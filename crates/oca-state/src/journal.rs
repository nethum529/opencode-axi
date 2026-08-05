//! Private append-only event journals used by `oca f`.

use std::{
    collections::BTreeMap,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use fs2::FileExt;
use oca_core::{EventJournalWriter, OcaEvent, RefId};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

/// Maximum encoded size of one JSONL record, including its newline.
pub const MAX_JOURNAL_RECORD_BYTES: usize = 8 * 1024;

/// The append-only writer for `~/.oca/events/<ref>.<turn>.jsonl`.
pub struct EventJournal {
    path: PathBuf,
    file: File,
    next_sequence: u64,
    access: JournalAccess,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JournalAccess {
    Reader,
    Writer,
}

/// One decoded public event from a journal page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalEvent {
    pub sequence: u64,
    pub event: String,
    pub session_id: Option<String>,
    pub payload: Option<Value>,
}

/// One bounded, point-in-time read of an event journal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalPage {
    pub events: Vec<JournalEvent>,
    pub cursor: u64,
    pub total: u64,
}

impl fmt::Debug for EventJournal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EventJournal")
            .field("path", &self.path)
            .field("next_sequence", &self.next_sequence)
            .field("access", &self.access)
            .finish_non_exhaustive()
    }
}

impl EventJournal {
    /// Opens the journal under an `~/.oca`-style state directory.
    ///
    /// Existing complete records are retained and sequencing resumes after the last one.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe turn id, corrupt complete record, or filesystem failure.
    pub fn create(
        state_directory: impl AsRef<Path>,
        reference: &RefId,
        turn: &str,
    ) -> Result<Self, JournalError> {
        validate_turn(turn)?;
        let events = state_directory.as_ref().join("events");
        fs::create_dir_all(&events).map_err(|source| JournalError::Io {
            path: events.clone(),
            source,
        })?;
        set_directory_mode(&events)?;
        let path = events.join(format!("{reference}.{turn}.jsonl"));
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        set_creation_mode(&mut options);
        let file = options.open(&path).map_err(|source| JournalError::Io {
            path: path.clone(),
            source,
        })?;
        FileExt::try_lock_exclusive(&file).map_err(|source| {
            if source.kind() == io::ErrorKind::WouldBlock {
                JournalError::WriterActive { path: path.clone() }
            } else {
                JournalError::Io {
                    path: path.clone(),
                    source,
                }
            }
        })?;
        let next_sequence = next_sequence(&path)?;
        set_file_mode(&path)?;
        Ok(Self {
            path,
            file,
            next_sequence,
            access: JournalAccess::Writer,
        })
    }

    /// Opens an existing journal for point-in-time page reads.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe turn id or filesystem failure. The
    /// reader never creates a missing journal.
    pub fn open(
        state_directory: impl AsRef<Path>,
        reference: &RefId,
        turn: &str,
    ) -> Result<Self, JournalError> {
        validate_turn(turn)?;
        let path = state_directory
            .as_ref()
            .join("events")
            .join(format!("{reference}.{turn}.jsonl"));
        let file = OpenOptions::new()
            .read(true)
            .open(&path)
            .map_err(|source| JournalError::Io {
                path: path.clone(),
                source,
            })?;
        Ok(Self {
            path,
            file,
            next_sequence: 0,
            access: JournalAccess::Reader,
        })
    }

    /// Appends and flushes one permitted public event.
    ///
    /// Reasoning events are dropped. Unknown events retain their type but omit their payload.
    /// Oversized public payloads are replaced by a bounded truncation marker.
    ///
    /// # Errors
    ///
    /// Returns an error when encoding, writing, or flushing the record fails.
    pub fn append(&mut self, event: &OcaEvent) -> Result<(), JournalError> {
        if self.access != JournalAccess::Writer {
            return Err(JournalError::ReadOnly {
                path: self.path.clone(),
            });
        }
        if event.is_reasoning() {
            return Ok(());
        }
        let sequence = self.next_sequence;
        let payload = event
            .known
            .then_some(event.payload.as_ref())
            .flatten()
            .and_then(public_payload);
        let record = JournalRecord {
            schema_version: 1,
            sequence,
            event: &event.kind,
            session_id: event.session_id.as_deref(),
            payload,
        };
        let mut encoded = encode_record(&record)?;
        if encoded.len() + 1 > MAX_JOURNAL_RECORD_BYTES {
            let truncated = JournalRecord {
                schema_version: 1,
                sequence,
                event: &event.kind,
                session_id: event.session_id.as_deref(),
                payload: Some(json!({
                    "truncated": true,
                    "original_bytes": encoded.len() + 1,
                })),
            };
            encoded = encode_record(&truncated)?;
        }
        debug_assert!(encoded.len() < MAX_JOURNAL_RECORD_BYTES);
        encoded.push(b'\n');
        self.file
            .write_all(&encoded)
            .and_then(|()| self.file.flush())
            .map_err(|source| JournalError::Io {
                path: self.path.clone(),
                source,
            })?;
        self.next_sequence = self.next_sequence.saturating_add(1);
        Ok(())
    }

    /// Syncs all appended records to stable storage.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be synchronized.
    pub fn finish(&self) -> Result<(), JournalError> {
        if self.access != JournalAccess::Writer {
            return Ok(());
        }
        self.file.sync_all().map_err(|source| JournalError::Io {
            path: self.path.clone(),
            source,
        })
    }

    /// Reads one bounded page containing only records whose sequence is
    /// greater than `since`.
    ///
    /// A live writer is detected through its exclusive file lock. While that
    /// lock is held, an incomplete or undecodable suffix is ignored. Once a
    /// shared lock proves that the writer is gone, the same suffix is reported
    /// as corruption.
    ///
    /// # Errors
    ///
    /// Returns an error for filesystem failures or corruption confirmed after
    /// the writer has gone away.
    pub fn page(&self, since: u64, limit: usize) -> Result<JournalPage, JournalError> {
        let writer_live = self.writer_is_live()?;
        let bytes = fs::read(&self.path).map_err(|source| JournalError::Io {
            path: self.path.clone(),
            source,
        })?;
        let complete_length = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        let has_incomplete_suffix = complete_length != bytes.len();
        let mut decoded = Vec::new();
        for line in bytes[..complete_length]
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            match serde_json::from_slice::<OwnedJournalRecord>(line) {
                Ok(record) => decoded.push(record),
                Err(_) if writer_live => break,
                Err(source) => {
                    return Err(JournalError::Corrupt {
                        path: self.path.clone(),
                        source,
                    });
                }
            }
        }
        if has_incomplete_suffix && !writer_live {
            return Err(JournalError::TrailingIncomplete {
                path: self.path.clone(),
            });
        }

        let total = u64::try_from(decoded.len()).unwrap_or(u64::MAX);
        let events = decoded
            .into_iter()
            .filter(|record| record.sequence > since)
            .take(limit)
            .map(JournalEvent::from)
            .collect::<Vec<_>>();
        let cursor = events.last().map_or(since, |event| event.sequence);
        Ok(JournalPage {
            events,
            cursor,
            total,
        })
    }

    fn writer_is_live(&self) -> Result<bool, JournalError> {
        if self.access == JournalAccess::Writer {
            return Ok(true);
        }
        match FileExt::try_lock_shared(&self.file) {
            // Keep the shared lock until the reader is dropped. This closes
            // the check/read race: a new writer cannot begin after we have
            // confirmed the previous writer is gone but before the snapshot
            // has been read and validated.
            Ok(()) => Ok(false),
            Err(source) if source.kind() == io::ErrorKind::WouldBlock => Ok(true),
            Err(source) => Err(JournalError::Io {
                path: self.path.clone(),
                source,
            }),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Removes expired, inactive journal files from an `~/.oca`-style state root.
///
/// A journal whose writer still owns the exclusive lock is retained even when
/// its modification time is older than the configured retention duration.
///
/// # Errors
///
/// Returns an error when directory inspection, metadata access, locking, or
/// removal fails.
pub fn prune_expired_journals(
    state_directory: impl AsRef<Path>,
    retention: Duration,
) -> Result<usize, JournalError> {
    let events = state_directory.as_ref().join("events");
    let entries = match fs::read_dir(&events) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(source) => {
            return Err(JournalError::Io {
                path: events,
                source,
            });
        }
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry.map_err(|source| JournalError::Io {
            path: events.clone(),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("jsonl")
            || !entry
                .file_type()
                .map_err(|source| JournalError::Io {
                    path: path.clone(),
                    source,
                })?
                .is_file()
        {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .map_err(|source| JournalError::Io {
                path: path.clone(),
                source,
            })?;
        if modified.elapsed().unwrap_or_default() < retention {
            continue;
        }
        let file = OpenOptions::new()
            .read(true)
            .open(&path)
            .map_err(|source| JournalError::Io {
                path: path.clone(),
                source,
            })?;
        match FileExt::try_lock_shared(&file) {
            Ok(()) => {
                fs::remove_file(&path).map_err(|source| JournalError::Io {
                    path: path.clone(),
                    source,
                })?;
                removed += 1;
            }
            Err(source) if source.kind() == io::ErrorKind::WouldBlock => {}
            Err(source) => {
                return Err(JournalError::Io {
                    path: path.clone(),
                    source,
                });
            }
        }
    }
    Ok(removed)
}

impl EventJournalWriter for EventJournal {
    fn append(&mut self, event: &OcaEvent) -> Result<(), String> {
        Self::append(self, event).map_err(|error| error.to_string())
    }
}

#[derive(Serialize)]
struct JournalRecord<'a> {
    schema_version: u32,
    sequence: u64,
    event: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<Value>,
}

#[derive(Deserialize)]
struct OwnedJournalRecord {
    #[serde(rename = "schema_version")]
    _schema_version: u32,
    sequence: u64,
    event: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    payload: Option<Value>,
}

impl From<OwnedJournalRecord> for JournalEvent {
    fn from(record: OwnedJournalRecord) -> Self {
        Self {
            sequence: record.sequence,
            event: record.event,
            session_id: record.session_id,
            payload: record.payload,
        }
    }
}

fn encode_record(record: &JournalRecord<'_>) -> Result<Vec<u8>, JournalError> {
    serde_json::to_vec(record).map_err(JournalError::Encode)
}

fn public_payload(value: &Value) -> Option<Value> {
    match value {
        Value::Object(object)
            if object.get("type").and_then(Value::as_str) == Some("reasoning") =>
        {
            None
        }
        Value::Object(object) => {
            let filtered = object
                .iter()
                .filter(|(key, _)| !is_reasoning_key(key))
                .filter_map(|(key, value)| public_payload(value).map(|value| (key.clone(), value)))
                .collect::<Map<_, _>>();
            Some(Value::Object(filtered))
        }
        Value::Array(values) => Some(Value::Array(
            values.iter().filter_map(public_payload).collect(),
        )),
        _ => Some(value.clone()),
    }
}

fn is_reasoning_key(key: &str) -> bool {
    key.to_ascii_lowercase().contains("reasoning")
}

fn next_sequence(path: &Path) -> Result<u64, JournalError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(1),
        Err(source) => {
            return Err(JournalError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let complete_length = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    if complete_length != bytes.len() {
        return Err(JournalError::TrailingIncomplete {
            path: path.to_path_buf(),
        });
    }
    let mut last = None;
    for line in bytes[..complete_length]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let fields: BTreeMap<String, Value> =
            serde_json::from_slice(line).map_err(|source| JournalError::Corrupt {
                path: path.to_path_buf(),
                source,
            })?;
        let sequence = fields
            .get("sequence")
            .and_then(Value::as_u64)
            .ok_or_else(|| JournalError::MissingSequence {
                path: path.to_path_buf(),
            })?;
        last = Some(sequence);
    }
    Ok(last.map_or(1, |sequence| sequence.saturating_add(1)))
}

fn validate_turn(turn: &str) -> Result<(), JournalError> {
    if turn.is_empty()
        || !turn
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(JournalError::UnsafeTurn(turn.to_owned()));
    }
    Ok(())
}

#[cfg(unix)]
fn set_creation_mode(options: &mut OpenOptions) {
    options.mode(0o600);
}

#[cfg(not(unix))]
fn set_creation_mode(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn set_directory_mode(path: &Path) -> Result<(), JournalError> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| {
        JournalError::Io {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn set_directory_mode(_path: &Path) -> Result<(), JournalError> {
    Ok(())
}

#[cfg(unix)]
fn set_file_mode(path: &Path) -> Result<(), JournalError> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| {
        JournalError::Io {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn set_file_mode(_path: &Path) -> Result<(), JournalError> {
    Ok(())
}

/// Failures produced by the event journal writer.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("journal I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("journal record encoding failed: {0}")]
    Encode(serde_json::Error),
    #[error("journal {path} contains a corrupt complete record: {source}")]
    Corrupt {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("journal {path} contains a record without a sequence")]
    MissingSequence { path: PathBuf },
    #[error("journal {path} ends with an incomplete record")]
    TrailingIncomplete { path: PathBuf },
    #[error("journal writer is already active at {path}")]
    WriterActive { path: PathBuf },
    #[error("journal {path} was opened read-only")]
    ReadOnly { path: PathBuf },
    #[error("unsafe journal turn id `{0}`")]
    UnsafeTurn(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &str, payload: Option<Value>, known: bool) -> OcaEvent {
        OcaEvent {
            id: Some("evt_1".to_owned()),
            cursor: Some("evt_1".to_owned()),
            kind: kind.to_owned(),
            session_id: Some("ses_target".to_owned()),
            payload,
            message: None,
            known,
        }
    }

    fn lines(path: &Path) -> Vec<Value> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn writer_is_private_append_only_and_flushes_each_record() {
        let state = tempfile::tempdir().unwrap();
        let reference = RefId::new("w4f2a1").unwrap();
        let path;
        {
            let mut journal = EventJournal::create(state.path(), &reference, "msg_turn").unwrap();
            path = journal.path().to_path_buf();
            journal
                .append(&event(
                    "session.idle",
                    Some(json!({ "properties": { "sessionID": "ses_target" } })),
                    true,
                ))
                .unwrap();
            assert_eq!(lines(&path).len(), 1, "append flushes before returning");
            journal.finish().unwrap();
        }
        let mut reopened = EventJournal::create(state.path(), &reference, "msg_turn").unwrap();
        reopened
            .append(&event("session.busy", Some(json!({})), true))
            .unwrap();
        assert_eq!(lines(&path)[1]["sequence"], 2);

        #[cfg(unix)]
        {
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn reasoning_is_removed_unknown_payloads_are_omitted_and_large_records_are_bounded() {
        let state = tempfile::tempdir().unwrap();
        let reference = RefId::new("w4f2a1").unwrap();
        let mut journal = EventJournal::create(state.path(), &reference, "msg_turn").unwrap();
        journal
            .append(&event(
                "message.updated",
                Some(json!({
                    "public": "kept",
                    "reasoning": "secret",
                    "nested": { "reasoning_content": "secret", "answer": "kept" }
                })),
                true,
            ))
            .unwrap();
        journal
            .append(&event(
                "future.event",
                Some(json!({ "secret": "omitted" })),
                false,
            ))
            .unwrap();
        journal
            .append(&event(
                "message.part.updated",
                Some(json!({ "blob": "x".repeat(10_000) })),
                true,
            ))
            .unwrap();
        journal
            .append(&event(
                "session.next.reasoning.delta",
                Some(json!({ "text": "never written" })),
                true,
            ))
            .unwrap();

        let bytes = fs::read(journal.path()).unwrap();
        assert!(
            bytes
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .all(|line| line.len() < MAX_JOURNAL_RECORD_BYTES)
        );
        let records = lines(journal.path());
        assert_eq!(records.len(), 3, "reasoning event is dropped entirely");
        assert_eq!(records[0]["payload"]["public"], "kept");
        assert!(records[0].to_string().find("reasoning").is_none());
        assert!(records[1].get("payload").is_none());
        assert_eq!(records[2]["payload"]["truncated"], true);
    }

    #[test]
    fn writer_refuses_to_append_after_a_trailing_incomplete_line() {
        let state = tempfile::tempdir().unwrap();
        let events = state.path().join("events");
        fs::create_dir(&events).unwrap();
        let path = events.join("w4f2a1.msg_turn.jsonl");
        fs::write(
            &path,
            b"{\"schema_version\":1,\"sequence\":7,\"event\":\"session.idle\"}\n{\"sequence\":",
        )
        .unwrap();
        let reference = RefId::new("w4f2a1").unwrap();
        assert!(matches!(
            EventJournal::create(state.path(), &reference, "msg_turn"),
            Err(JournalError::TrailingIncomplete { .. })
        ));
    }

    #[test]
    fn page_tolerates_live_trailing_line_and_reports_it_after_writer_exit() {
        let state = tempfile::tempdir().unwrap();
        let reference = RefId::new("w4f2a1").unwrap();
        let mut writer = EventJournal::create(state.path(), &reference, "msg_turn").unwrap();
        writer
            .append(&event("session.busy", Some(json!({})), true))
            .unwrap();
        OpenOptions::new()
            .append(true)
            .open(writer.path())
            .unwrap()
            .write_all(b"{\"schema_version\":1,\"sequence\":2")
            .unwrap();

        let reader = EventJournal::open(state.path(), &reference, "msg_turn").unwrap();
        let live_page = reader.page(0, 100).unwrap();
        assert_eq!(live_page.events.len(), 1);
        assert_eq!(live_page.cursor, 1);
        drop(reader);
        drop(writer);

        let reader = EventJournal::open(state.path(), &reference, "msg_turn").unwrap();
        assert!(matches!(
            reader.page(0, 100),
            Err(JournalError::TrailingIncomplete { .. })
        ));
    }

    #[test]
    fn page_filters_strictly_after_since_and_retention_skips_live_writers() {
        let state = tempfile::tempdir().unwrap();
        let reference = RefId::new("w4f2a1").unwrap();
        let mut writer = EventJournal::create(state.path(), &reference, "msg_turn").unwrap();
        writer
            .append(&event("session.busy", Some(json!({ "n": 1 })), true))
            .unwrap();
        writer
            .append(&event("session.idle", Some(json!({ "n": 2 })), true))
            .unwrap();
        let page = writer.page(1, 100).unwrap();
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].sequence, 2);
        assert_eq!(page.total, 2);

        assert_eq!(
            prune_expired_journals(state.path(), Duration::ZERO).unwrap(),
            0
        );
        let path = writer.path().to_path_buf();
        drop(writer);
        assert_eq!(
            prune_expired_journals(state.path(), Duration::ZERO).unwrap(),
            1
        );
        assert!(!path.exists());
    }
}
