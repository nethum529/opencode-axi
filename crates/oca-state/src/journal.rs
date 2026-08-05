//! Private append-only event journals used by `oca f`.

use std::{
    collections::BTreeMap,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use oca_core::{EventJournalWriter, OcaEvent, RefId};
use serde::Serialize;
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
}

impl fmt::Debug for EventJournal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EventJournal")
            .field("path", &self.path)
            .field("next_sequence", &self.next_sequence)
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
        let next_sequence = next_sequence(&path)?;
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        set_creation_mode(&mut options);
        let file = options.open(&path).map_err(|source| JournalError::Io {
            path: path.clone(),
            source,
        })?;
        set_file_mode(&path)?;
        Ok(Self {
            path,
            file,
            next_sequence,
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
        self.file.sync_all().map_err(|source| JournalError::Io {
            path: self.path.clone(),
            source,
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
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
}
