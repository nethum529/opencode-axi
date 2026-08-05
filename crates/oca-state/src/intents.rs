//! Durable write-ahead records for operations whose completion may be uncertain.

use std::{
    fmt, fs,
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use oca_core::{RefId, RoleReply};
use serde::{Deserialize, Serialize};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

pub const INTENT_SCHEMA_VERSION: u8 = 1;

/// The operation protected by an intent.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentOperation {
    Dispatch,
    Message,
    Queue,
    Push,
    PullRequest,
}

/// Every durable phase from `spec-data-state.md` section 9.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentPhase {
    Planned,
    WorktreeReady,
    SessionCreated,
    PromptUncertain,
    Running,
    TerminalObserved,
    Validated,
    Committed,
    PublishedUncertain,
}

/// Durability required for one atomic intent replacement.
///
/// Pre-ack writes deliberately stop after the atomic rename. Post-ack phase
/// transitions additionally flush the file and its parent directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntentDurability {
    PreAck,
    PostAck,
}

impl IntentPhase {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::WorktreeReady => "worktree_ready",
            Self::SessionCreated => "session_created",
            Self::PromptUncertain => "prompt_uncertain",
            Self::Running => "running",
            Self::TerminalObserved => "terminal_observed",
            Self::Validated => "validated",
            Self::Committed => "committed",
            Self::PublishedUncertain => "published_uncertain",
        }
    }
}

/// Locally resolved options sufficient to identify and safely clean a dispatch.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct IntentRequest {
    pub alias: String,
    pub effort: String,
    pub role: String,
    pub cwd: String,
    pub repo: String,
    pub worktree: bool,
}

/// One write-ahead crash-recovery record.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Intent {
    pub schema_version: u8,
    #[serde(rename = "ref")]
    pub reference: String,
    #[serde(rename = "op")]
    pub operation: IntentOperation,
    pub phase: IntentPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested: Option<IntentRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reply: Option<RoleReply>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_fingerprint: Option<String>,
    pub at_unix_ms: u64,
}

impl Intent {
    #[must_use]
    pub fn new(reference: impl Into<String>, operation: IntentOperation) -> Self {
        Self {
            schema_version: INTENT_SCHEMA_VERSION,
            reference: reference.into(),
            operation,
            phase: IntentPhase::Planned,
            requested: None,
            session_id: None,
            message_id: None,
            prompt_sha256: None,
            event_cursor: None,
            terminal_reply: None,
            changed_paths: Vec::new(),
            checks: Vec::new(),
            commit_id: None,
            remote_fingerprint: None,
            at_unix_ms: now_unix_ms(),
        }
    }

    #[must_use]
    pub fn with_requested(mut self, requested: IntentRequest) -> Self {
        self.requested = Some(requested);
        self
    }

    pub fn set_phase(&mut self, phase: IntentPhase) {
        self.phase = phase;
        self.at_unix_ms = now_unix_ms();
    }
}

/// Atomic storage rooted at `~/.oca/intents`.
#[derive(Clone, Debug)]
pub struct IntentStore {
    state_directory: PathBuf,
}

impl IntentStore {
    #[must_use]
    pub fn in_directory(state_directory: impl AsRef<Path>) -> Self {
        Self {
            state_directory: state_directory.as_ref().to_path_buf(),
        }
    }

    #[must_use]
    pub fn directory(&self) -> PathBuf {
        self.state_directory.join("intents")
    }

    /// Atomically creates or replaces one intent at the requested durability.
    pub fn write(
        &self,
        intent: &Intent,
        durability: IntentDurability,
    ) -> Result<(), IntentStoreError> {
        validate_reference(&intent.reference)?;
        let directory = self.ensure_directory()?;
        let _lock = self.lock()?;
        let path = directory.join(format!("{}.json", intent.reference));
        let temporary = directory.join(format!(
            ".{}.{}.{}.tmp",
            intent.reference,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let bytes = serde_json::to_vec_pretty(intent).map_err(IntentStoreError::Serialize)?;
        let result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options
                .open(&temporary)
                .map_err(|source| IntentStoreError::Io {
                    path: temporary.clone(),
                    source,
                })?;
            file.write_all(&bytes)
                .map_err(|source| IntentStoreError::Io {
                    path: temporary.clone(),
                    source,
                })?;
            if durability == IntentDurability::PostAck {
                file.sync_all().map_err(|source| IntentStoreError::Io {
                    path: temporary.clone(),
                    source,
                })?;
            }
            fs::rename(&temporary, &path).map_err(|source| IntentStoreError::Io {
                path: path.clone(),
                source,
            })?;
            if durability == IntentDurability::PostAck {
                sync_directory(&directory).map_err(|source| IntentStoreError::Io {
                    path: directory.clone(),
                    source,
                })?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }

    /// Reads one intent without changing it.
    pub fn read(&self, reference: &str) -> Result<Option<Intent>, IntentStoreError> {
        validate_reference(reference)?;
        let path = self.directory().join(format!("{reference}.json"));
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(IntentStoreError::Io { path, source }),
        };
        let intent: Intent =
            serde_json::from_slice(&bytes).map_err(|source| IntentStoreError::Deserialize {
                path: path.clone(),
                source,
            })?;
        validate_loaded(&intent, reference)?;
        secure_file(&path)?;
        Ok(Some(intent))
    }

    /// Reads every intent in lexical ref order.
    pub fn list(&self) -> Result<Vec<Intent>, IntentStoreError> {
        let directory = self.directory();
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(IntentStoreError::Io {
                    path: directory,
                    source,
                });
            }
        };
        let mut references = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                (path.extension().and_then(|value| value.to_str()) == Some("json"))
                    .then(|| path.file_stem()?.to_str().map(str::to_owned))
                    .flatten()
            })
            .collect::<Vec<_>>();
        references.sort();
        references
            .into_iter()
            .map(|reference| {
                self.read(&reference)?.ok_or_else(|| IntentStoreError::Io {
                    path: self.directory().join(format!("{reference}.json")),
                    source: io::Error::new(io::ErrorKind::NotFound, "intent disappeared"),
                })
            })
            .collect()
    }

    /// Removes a settled intent and makes the directory update durable.
    pub fn remove(&self, reference: &str) -> Result<bool, IntentStoreError> {
        validate_reference(reference)?;
        let directory = self.directory();
        let path = directory.join(format!("{reference}.json"));
        let _lock = match self.lock() {
            Ok(lock) => lock,
            Err(IntentStoreError::Io { source, .. })
                if source.kind() == io::ErrorKind::NotFound =>
            {
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        match fs::remove_file(&path) {
            Ok(()) => {
                sync_directory(&directory).map_err(|source| IntentStoreError::Io {
                    path: directory,
                    source,
                })?;
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(IntentStoreError::Io { path, source }),
        }
    }

    fn ensure_directory(&self) -> Result<PathBuf, IntentStoreError> {
        let directory = self.directory();
        fs::create_dir_all(&directory).map_err(|source| IntentStoreError::Io {
            path: directory.clone(),
            source,
        })?;
        #[cfg(unix)]
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).map_err(|source| {
            IntentStoreError::Io {
                path: directory.clone(),
                source,
            }
        })?;
        Ok(directory)
    }

    fn lock(&self) -> Result<File, IntentStoreError> {
        let directory = self.ensure_directory()?;
        let path = directory.join("intents.lock");
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(&path).map_err(|source| IntentStoreError::Io {
            path: path.clone(),
            source,
        })?;
        file.lock_exclusive()
            .map_err(|source| IntentStoreError::Io { path, source })?;
        Ok(file)
    }
}

fn validate_loaded(intent: &Intent, reference: &str) -> Result<(), IntentStoreError> {
    if intent.schema_version != INTENT_SCHEMA_VERSION {
        return Err(IntentStoreError::UnsupportedSchema(intent.schema_version));
    }
    if intent.reference != reference {
        return Err(IntentStoreError::ReferenceMismatch {
            file: reference.to_owned(),
            record: intent.reference.clone(),
        });
    }
    Ok(())
}

fn validate_reference(reference: &str) -> Result<(), IntentStoreError> {
    RefId::new(reference)
        .map(|_| ())
        .map_err(|_| IntentStoreError::InvalidRef(reference.to_owned()))
}

fn now_unix_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

fn secure_file(path: &Path) -> Result<(), IntentStoreError> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| {
        IntentStoreError::Io {
            path: path.to_path_buf(),
            source,
        }
    })?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> io::Result<()> {
    File::open(directory).and_then(|file| file.sync_all())
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) -> io::Result<()> {
    Ok(())
}

#[derive(Debug)]
pub enum IntentStoreError {
    InvalidRef(String),
    UnsupportedSchema(u8),
    ReferenceMismatch {
        file: String,
        record: String,
    },
    Io {
        path: PathBuf,
        source: io::Error,
    },
    Serialize(serde_json::Error),
    Deserialize {
        path: PathBuf,
        source: serde_json::Error,
    },
}

impl fmt::Display for IntentStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRef(reference) => write!(formatter, "invalid intent ref `{reference}`"),
            Self::UnsupportedSchema(version) => {
                write!(formatter, "unsupported intent schema version {version}")
            }
            Self::ReferenceMismatch { file, record } => {
                write!(formatter, "intent file `{file}` contains ref `{record}`")
            }
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::Serialize(source) => write!(formatter, "could not encode intent: {source}"),
            Self::Deserialize { path, source } => {
                write!(
                    formatter,
                    "could not decode intent {}: {source}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for IntentStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Serialize(source) | Self::Deserialize { source, .. } => Some(source),
            Self::InvalidRef(_) | Self::UnsupportedSchema(_) | Self::ReferenceMismatch { .. } => {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_covers_the_full_phase_table_and_settled_removal() {
        let state = tempfile::tempdir().unwrap();
        let store = IntentStore::in_directory(state.path());
        let mut intent = Intent::new("w4f2a1", IntentOperation::Dispatch);
        for phase in [
            IntentPhase::Planned,
            IntentPhase::WorktreeReady,
            IntentPhase::SessionCreated,
            IntentPhase::PromptUncertain,
            IntentPhase::Running,
            IntentPhase::TerminalObserved,
            IntentPhase::Validated,
            IntentPhase::Committed,
            IntentPhase::PublishedUncertain,
        ] {
            intent.set_phase(phase);
            let durability = if phase >= IntentPhase::TerminalObserved {
                IntentDurability::PostAck
            } else {
                IntentDurability::PreAck
            };
            store.write(&intent, durability).unwrap();
            assert_eq!(store.read("w4f2a1").unwrap().unwrap().phase, phase);
        }
        assert_eq!(store.list().unwrap(), [intent]);
        assert!(store.remove("w4f2a1").unwrap());
        assert!(store.list().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn intent_directory_and_files_are_private() {
        let state = tempfile::tempdir().unwrap();
        let store = IntentStore::in_directory(state.path());
        store
            .write(
                &Intent::new("w4f2a1", IntentOperation::Dispatch),
                IntentDurability::PreAck,
            )
            .unwrap();
        let directory_mode = fs::metadata(store.directory())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let file_mode = fs::metadata(store.directory().join("w4f2a1.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }
}
