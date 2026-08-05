// Durable local storage for the short refs that identify OpenCode sessions.
// Every mutation takes an advisory lock, rereads `refs.json`, and replaces it
// atomically. Keeping the read inside the lock is important: it makes the
// collision check in `RefStore::allocate` meaningful across processes.

use std::{
    collections::HashSet,
    env, fmt,
    fs::{self, File, OpenOptions},
    io::{self, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use oca_core::RefId;
use serde::{Deserialize, Serialize};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

const REF_ID_WIDTH: usize = 5;
const REF_ID_SPACE: u64 = 60_466_176;
const DIRECTORY_SYNC_PENDING_MARKER: &[u8] = b"pending\n";

/// A stored ref. `id` is always `w` followed by five lowercase base-36 digits.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RefRecord {
    pub id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawner_tag: Option<String>,
    #[serde(default)]
    pub tombstoned: bool,
}

/// Values used to create a new ref.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewRef {
    pub session_id: String,
    pub message_id: Option<String>,
    pub repo: Option<String>,
    pub spawner_tag: Option<String>,
}

/// Store-level selection used by [`RefStore::list`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RefListFilter {
    pub spawner_tag: Option<String>,
    pub repo: Option<String>,
    pub all: bool,
    pub include_tombstones: bool,
}

/// The mutable fields of a ref. `None` leaves a field unchanged.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RefPatch {
    pub session_id: Option<String>,
    pub repo: Option<String>,
    pub spawner_tag: Option<String>,
}

impl RefPatch {
    #[must_use]
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    #[must_use]
    pub fn with_repo(mut self, repo: impl Into<String>) -> Self {
        self.repo = Some(repo.into());
        self
    }

    #[must_use]
    pub fn with_spawner_tag(mut self, spawner_tag: impl Into<String>) -> Self {
        self.spawner_tag = Some(spawner_tag.into());
        self
    }
}

impl RefListFilter {
    #[must_use]
    pub fn for_spawner(spawner_tag: impl Into<String>) -> Self {
        Self {
            spawner_tag: Some(spawner_tag.into()),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn across_spawners_and_repos() -> Self {
        Self {
            all: true,
            ..Self::default()
        }
    }
}

impl NewRef {
    #[must_use]
    pub fn for_session(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            message_id: None,
            repo: None,
            spawner_tag: None,
        }
    }

    #[must_use]
    pub fn with_message_id(mut self, message_id: impl Into<String>) -> Self {
        self.message_id = Some(message_id.into());
        self
    }

    #[must_use]
    pub fn with_repo(mut self, repo: impl Into<String>) -> Self {
        self.repo = Some(repo.into());
        self
    }

    #[must_use]
    pub fn with_spawner_tag(mut self, spawner_tag: impl Into<String>) -> Self {
        self.spawner_tag = Some(spawner_tag.into());
        self
    }
}

/// File locations used by a [`RefStore`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefStorePaths {
    pub refs_file: PathBuf,
    pub lock_file: PathBuf,
}

impl RefStorePaths {
    #[must_use]
    pub fn in_directory(directory: impl AsRef<Path>) -> Self {
        let directory = directory.as_ref();
        Self {
            refs_file: directory.join("refs.json"),
            lock_file: directory.join("refs.lock"),
        }
    }

    /// The standard `~/.oca` locations.
    ///
    /// # Errors
    ///
    /// Returns an error when neither supported home-directory environment
    /// variable is present.
    pub fn default_locations() -> Result<Self, RefStoreError> {
        let home = env::var_os("HOME")
            .or_else(|| env::var_os("USERPROFILE"))
            .ok_or(RefStoreError::HomeDirectoryUnavailable)?;
        Ok(Self::in_directory(PathBuf::from(home).join(".oca")))
    }
}

/// Source of candidate ref IDs. Supplying one makes collision behaviour testable.
pub trait RefIdSource: Send + Sync {
    fn next_id(&self) -> String;
}

/// Sequencing hooks around atomic ref replacement and deferred durability.
///
/// The hooks are primarily deterministic failure, observation, and latch seams
/// for crash-consistency tests.
pub trait AtomicWriteHook: Send + Sync {
    /// # Errors
    ///
    /// Returns an error to simulate failure to create the temporary file.
    fn before_temporary_file_create(&self) -> io::Result<()> {
        Ok(())
    }

    /// # Errors
    ///
    /// Returns an error to simulate failure to write the replacement.
    fn before_temporary_file_write(&self) -> io::Result<()> {
        Ok(())
    }

    /// # Errors
    ///
    /// Returns an error to simulate failure to sync the temporary file.
    fn before_temporary_file_sync(&self) -> io::Result<()> {
        Ok(())
    }

    /// # Errors
    ///
    /// Returns an error to simulate or report a failure before replacement.
    fn before_rename(&self) -> io::Result<()>;

    /// Observes the replacement after rename and before acknowledgement.
    fn after_rename(&self, _refs_file: &Path) {}

    /// # Errors
    ///
    /// Returns an error to block or fail a parent-directory sync attempt.
    fn before_directory_sync(&self) -> io::Result<()> {
        Ok(())
    }
}

struct NoopAtomicWriteHook;

impl AtomicWriteHook for NoopAtomicWriteHook {
    fn before_rename(&self) -> io::Result<()> {
        Ok(())
    }
}

/// An allocation whose replacement is visible and ready to acknowledge.
///
/// The handle retains the exclusive ref lock. Call [`Self::acknowledge_with`]
/// to emit and flush the caller's acknowledgement before the owning process
/// attempts the deferred parent-directory sync.
#[must_use = "the caller must finish deferred ref durability after acknowledgement"]
pub struct PendingRefAllocation {
    record: RefRecord,
    parent: PathBuf,
    lock: File,
    atomic_write_hook: Arc<dyn AtomicWriteHook>,
}

impl fmt::Debug for PendingRefAllocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingRefAllocation")
            .field("record", &self.record)
            .field("parent", &self.parent)
            .finish_non_exhaustive()
    }
}

impl PendingRefAllocation {
    /// The complete record that is visible in `refs.json` at acknowledgement.
    #[must_use]
    pub fn record(&self) -> &RefRecord {
        &self.record
    }

    /// Emits and flushes acknowledgement through the caller, then attempts
    /// deferred directory durability.
    ///
    /// The callback belongs to the dispatch layer, which keeps terminal output
    /// out of state storage. If it fails, no completion is returned and dropping
    /// this handle transfers pending directory durability to the next entrant.
    ///
    /// # Errors
    ///
    /// Returns the caller's acknowledgement or flush error without beginning
    /// the post-ack directory-sync attempt.
    pub fn acknowledge_with<E>(
        self,
        acknowledge: impl FnOnce(&RefRecord) -> Result<(), E>,
    ) -> Result<RefAllocationCompletion, E> {
        acknowledge(&self.record)?;
        Ok(self.finish_after_ack())
    }

    /// Attempts the deferred parent-directory sync while retaining `refs.lock`.
    ///
    /// This must be called only after the acknowledgement has been emitted and
    /// flushed. A failure is returned as a warning alongside the acknowledged
    /// record; it cannot retroactively turn the dispatch into a failure.
    #[must_use]
    pub fn finish_after_ack(mut self) -> RefAllocationCompletion {
        let sync_result = self
            .atomic_write_hook
            .before_directory_sync()
            .and_then(|()| sync_directory(&self.parent))
            .and_then(|()| clear_directory_sync_pending(&mut self.lock));
        let durability_warning = sync_result.err().map(|source| RefDurabilityWarning {
            path: self.parent.clone(),
            source,
        });

        RefAllocationCompletion {
            record: self.record,
            durability_warning,
        }
    }
}

/// The result after the first post-ack directory-durability attempt.
#[derive(Debug)]
pub struct RefAllocationCompletion {
    record: RefRecord,
    durability_warning: Option<RefDurabilityWarning>,
}

impl RefAllocationCompletion {
    #[must_use]
    pub fn record(&self) -> &RefRecord {
        &self.record
    }

    #[must_use]
    pub fn durability_warning(&self) -> Option<&RefDurabilityWarning> {
        self.durability_warning.as_ref()
    }

    #[must_use]
    pub fn into_parts(self) -> (RefRecord, Option<RefDurabilityWarning>) {
        (self.record, self.durability_warning)
    }
}

/// A parent-directory durability failure that happened after acknowledgement.
#[derive(Debug)]
pub struct RefDurabilityWarning {
    path: PathBuf,
    source: io::Error,
}

impl RefDurabilityWarning {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl fmt::Display for RefDurabilityWarning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "acknowledged refs update is not directory-durable at {}: {}",
            self.path.display(),
            self.source
        )
    }
}

impl std::error::Error for RefDurabilityWarning {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Default)]
struct DefaultRefIdSource;

impl RefIdSource for DefaultRefIdSource {
    fn next_id(&self) -> String {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        )
        .unwrap_or(u64::MAX);
        let value = nanos
            .wrapping_add(u64::from(std::process::id()))
            .wrapping_add(NEXT.fetch_add(1, Ordering::Relaxed))
            % REF_ID_SPACE;
        format!("w{}", encode_base36(value))
    }
}

/// Read-modify-write store for `~/.oca/refs.json`.
#[derive(Clone)]
pub struct RefStore {
    paths: RefStorePaths,
    id_source: Arc<dyn RefIdSource>,
    atomic_write_hook: Arc<dyn AtomicWriteHook>,
}

struct LockedRecords {
    parent: PathBuf,
    lock: File,
    records: Vec<RefRecord>,
}

impl fmt::Debug for RefStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RefStore")
            .field("paths", &self.paths)
            .finish_non_exhaustive()
    }
}

impl RefStore {
    #[must_use]
    pub fn with_paths(paths: RefStorePaths) -> Self {
        Self::with_id_source(paths, Arc::new(DefaultRefIdSource))
    }

    #[must_use]
    pub fn with_id_source(paths: RefStorePaths, id_source: Arc<dyn RefIdSource>) -> Self {
        Self::with_id_source_and_write_hook(paths, id_source, Arc::new(NoopAtomicWriteHook))
    }

    #[must_use]
    pub fn with_id_source_and_write_hook(
        paths: RefStorePaths,
        id_source: Arc<dyn RefIdSource>,
        atomic_write_hook: Arc<dyn AtomicWriteHook>,
    ) -> Self {
        Self {
            paths,
            id_source,
            atomic_write_hook,
        }
    }

    /// Opens the store at the standard `~/.oca` location.
    ///
    /// # Errors
    ///
    /// Returns an error when the home directory cannot be determined.
    pub fn open_default() -> Result<Self, RefStoreError> {
        Ok(Self::with_paths(RefStorePaths::default_locations()?))
    }

    #[must_use]
    pub fn paths(&self) -> &RefStorePaths {
        &self.paths
    }

    /// Allocates an unused ref ID while retaining the advisory lock through the
    /// collision check, pre-ack content sync, and atomic replacement.
    ///
    /// The returned handle owns `refs.lock` and exposes the complete renamed
    /// record at the acknowledgement boundary. The caller must acknowledge and
    /// finish deferred directory durability through that handle.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot be read, locked, or persisted.
    pub fn allocate(&self, new_ref: NewRef) -> Result<PendingRefAllocation, RefStoreError> {
        let mut locked = self.lock_and_read_records()?;
        let occupied: HashSet<_> = locked
            .records
            .iter()
            .map(|record| record.id.as_str())
            .collect();
        let id = loop {
            let candidate = self.id_source.next_id();
            if RefId::new(&candidate).is_ok() && !occupied.contains(candidate.as_str()) {
                break candidate;
            }
        };
        let record = RefRecord {
            id,
            session_id: new_ref.session_id,
            message_id: new_ref.message_id,
            repo: new_ref.repo,
            spawner_tag: new_ref.spawner_tag,
            tombstoned: false,
        };
        locked.records.push(record.clone());
        self.write_records_before_ack(&locked.records, &mut locked.lock)?;

        Ok(PendingRefAllocation {
            record,
            parent: locked.parent,
            lock: locked.lock,
            atomic_write_hook: Arc::clone(&self.atomic_write_hook),
        })
    }

    /// Lists active refs, optionally narrowed to a spawner tag and repository.
    /// `all` deliberately ignores those two scopes, which is the store-side
    /// implementation behind the CLI's future `--all` flag.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot be read or locked.
    pub fn list(&self, filter: &RefListFilter) -> Result<Vec<RefRecord>, RefStoreError> {
        self.with_locked_records(|records| {
            let refs = records
                .iter()
                .filter(|record| filter.include_tombstones || !record.tombstoned)
                .filter(|record| {
                    filter.all
                        || (filter
                            .spawner_tag
                            .as_ref()
                            .is_none_or(|tag| record.spawner_tag.as_ref() == Some(tag))
                            && filter
                                .repo
                                .as_ref()
                                .is_none_or(|repo| record.repo.as_ref() == Some(repo)))
                })
                .cloned()
                .collect();
            Ok((refs, false))
        })
    }

    /// Resolves a ref without hiding a tombstone. The caller can therefore
    /// distinguish a stale session from a ref that was never allocated.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot be read or locked.
    pub fn resolve(&self, id: &str) -> Result<Option<RefRecord>, RefStoreError> {
        self.with_locked_records(|records| {
            Ok((
                records.iter().find(|record| record.id == id).cloned(),
                false,
            ))
        })
    }

    /// Inserts a caller-provided ref. Existing refs, including tombstones, are
    /// never overwritten or made allocatable again by this method.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid or existing IDs, or if persistence fails.
    pub fn insert(&self, record: RefRecord) -> Result<(), RefStoreError> {
        if RefId::new(&record.id).is_err() {
            return Err(RefStoreError::InvalidRefId(record.id));
        }
        self.with_locked_records(|records| {
            if records.iter().any(|existing| existing.id == record.id) {
                return Err(RefStoreError::RefAlreadyExists(record.id));
            }
            records.push(record);
            Ok(((), true))
        })
    }

    /// Applies a patch to an existing ref.
    ///
    /// # Errors
    ///
    /// Returns an error if the ref is absent or persistence fails.
    pub fn patch(&self, id: &str, patch: RefPatch) -> Result<RefRecord, RefStoreError> {
        self.with_locked_records(|records| {
            let record = records
                .iter_mut()
                .find(|record| record.id == id)
                .ok_or_else(|| RefStoreError::RefNotFound(id.to_owned()))?;
            if let Some(session_id) = patch.session_id {
                record.session_id = session_id;
            }
            if let Some(repo) = patch.repo {
                record.repo = Some(repo);
            }
            if let Some(spawner_tag) = patch.spawner_tag {
                record.spawner_tag = Some(spawner_tag);
            }
            Ok((record.clone(), true))
        })
    }

    /// Marks a ref stale without deleting its ID from the allocation set.
    ///
    /// # Errors
    ///
    /// Returns an error if the ref is absent or persistence fails.
    pub fn tombstone(&self, id: &str) -> Result<RefRecord, RefStoreError> {
        self.with_locked_records(|records| {
            let record = records
                .iter_mut()
                .find(|record| record.id == id)
                .ok_or_else(|| RefStoreError::RefNotFound(id.to_owned()))?;
            record.tombstoned = true;
            Ok((record.clone(), true))
        })
    }

    fn with_locked_records<T>(
        &self,
        operation: impl FnOnce(&mut Vec<RefRecord>) -> Result<(T, bool), RefStoreError>,
    ) -> Result<T, RefStoreError> {
        let mut locked = self.lock_and_read_records()?;
        let (value, changed) = operation(&mut locked.records)?;
        if changed {
            self.write_records_before_ack(&locked.records, &mut locked.lock)?;
            self.atomic_write_hook
                .before_directory_sync()
                .and_then(|()| sync_directory(&locked.parent))
                .map_err(|source| RefStoreError::Io {
                    path: locked.parent.clone(),
                    source,
                })?;
            clear_directory_sync_pending(&mut locked.lock).map_err(|source| RefStoreError::Io {
                path: self.paths.lock_file.clone(),
                source,
            })?;
        }
        FileExt::unlock(&locked.lock).map_err(|source| RefStoreError::Io {
            path: self.paths.lock_file.clone(),
            source,
        })?;
        Ok(value)
    }

    fn lock_and_read_records(&self) -> Result<LockedRecords, RefStoreError> {
        let parent = self.refs_parent()?;
        fs::create_dir_all(parent).map_err(|source| RefStoreError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        secure_path(parent, 0o700)?;
        let mut lock_options = OpenOptions::new();
        lock_options
            .read(true)
            .write(true)
            .create(true)
            .truncate(false);
        #[cfg(unix)]
        lock_options.mode(0o600);
        let mut lock =
            lock_options
                .open(&self.paths.lock_file)
                .map_err(|source| RefStoreError::Io {
                    path: self.paths.lock_file.clone(),
                    source,
                })?;
        secure_path(&self.paths.lock_file, 0o600)?;
        lock.lock_exclusive().map_err(|source| RefStoreError::Io {
            path: self.paths.lock_file.clone(),
            source,
        })?;

        if directory_sync_pending(&lock).map_err(|source| RefStoreError::Io {
            path: self.paths.lock_file.clone(),
            source,
        })? {
            self.atomic_write_hook
                .before_directory_sync()
                .and_then(|()| sync_directory(parent))
                .map_err(|source| RefStoreError::Durability {
                    path: parent.to_path_buf(),
                    source,
                })?;
            clear_directory_sync_pending(&mut lock).map_err(|source| {
                RefStoreError::Durability {
                    path: self.paths.lock_file.clone(),
                    source,
                }
            })?;
        }

        Ok(LockedRecords {
            parent: parent.to_path_buf(),
            lock,
            records: self.read_records()?,
        })
    }

    fn refs_parent(&self) -> Result<&Path, RefStoreError> {
        self.paths
            .refs_file
            .parent()
            .ok_or_else(|| RefStoreError::MissingParent(self.paths.refs_file.clone()))
    }

    fn read_records(&self) -> Result<Vec<RefRecord>, RefStoreError> {
        match fs::read(&self.paths.refs_file) {
            Ok(bytes) => {
                secure_path(&self.paths.refs_file, 0o600)?;
                serde_json::from_slice(&bytes).map_err(|source| RefStoreError::Deserialize {
                    path: self.paths.refs_file.clone(),
                    source,
                })
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(source) => Err(RefStoreError::Io {
                path: self.paths.refs_file.clone(),
                source,
            }),
        }
    }

    fn write_records_before_ack(
        &self,
        records: &[RefRecord],
        lock: &mut File,
    ) -> Result<(), RefStoreError> {
        let bytes = serde_json::to_vec_pretty(records).map_err(RefStoreError::Serialize)?;
        let parent = self.refs_parent()?;
        let temporary = parent.join(format!(
            ".refs.json.{}.{}.tmp",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        mark_directory_sync_pending(lock).map_err(|source| RefStoreError::Io {
            path: self.paths.lock_file.clone(),
            source,
        })?;
        let write_result = (|| -> Result<(), RefStoreError> {
            self.atomic_write_hook
                .before_temporary_file_create()
                .map_err(|source| RefStoreError::Io {
                    path: temporary.clone(),
                    source,
                })?;
            let mut temporary_options = OpenOptions::new();
            temporary_options.write(true).create_new(true);
            #[cfg(unix)]
            temporary_options.mode(0o600);
            let mut file =
                temporary_options
                    .open(&temporary)
                    .map_err(|source| RefStoreError::Io {
                        path: temporary.clone(),
                        source,
                    })?;
            #[cfg(unix)]
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|source| RefStoreError::Io {
                    path: temporary.clone(),
                    source,
                })?;
            self.atomic_write_hook
                .before_temporary_file_write()
                .map_err(|source| RefStoreError::Io {
                    path: temporary.clone(),
                    source,
                })?;
            file.write_all(&bytes).map_err(|source| RefStoreError::Io {
                path: temporary.clone(),
                source,
            })?;
            self.atomic_write_hook
                .before_temporary_file_sync()
                .map_err(|source| RefStoreError::Io {
                    path: temporary.clone(),
                    source,
                })?;
            file.sync_all().map_err(|source| RefStoreError::Io {
                path: temporary.clone(),
                source,
            })?;
            self.atomic_write_hook
                .before_rename()
                .map_err(|source| RefStoreError::Io {
                    path: temporary.clone(),
                    source,
                })?;
            fs::rename(&temporary, &self.paths.refs_file).map_err(|source| RefStoreError::Io {
                path: self.paths.refs_file.clone(),
                source,
            })?;
            self.atomic_write_hook.after_rename(&self.paths.refs_file);
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
            let _ = clear_directory_sync_pending(lock);
        }
        write_result
    }
}

fn directory_sync_pending(lock: &File) -> io::Result<bool> {
    Ok(lock.metadata()?.len() != 0)
}

// The marker is written before replacement while the advisory lock is held.
// It transfers an interrupted or failed directory-sync attempt to the next
// entrant without imposing an unconditional directory sync on every warm path.
fn mark_directory_sync_pending(lock: &mut File) -> io::Result<()> {
    lock.set_len(0)?;
    lock.seek(SeekFrom::Start(0))?;
    lock.write_all(DIRECTORY_SYNC_PENDING_MARKER)
}

fn clear_directory_sync_pending(lock: &mut File) -> io::Result<()> {
    lock.set_len(0)?;
    lock.seek(SeekFrom::Start(0)).map(|_| ())
}

fn secure_path(path: &Path, mode: u32) -> Result<(), RefStoreError> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|source| {
        RefStoreError::Io {
            path: path.to_path_buf(),
            source,
        }
    })?;

    #[cfg(not(unix))]
    let _ = (path, mode);

    Ok(())
}

fn encode_base36(mut value: u64) -> String {
    let mut digits = [b'0'; REF_ID_WIDTH];
    for digit in digits.iter_mut().rev() {
        let index = usize::try_from(value % 36).expect("base-36 digit always fits usize");
        *digit = b"0123456789abcdefghijklmnopqrstuvwxyz"[index];
        value /= 36;
    }
    String::from_utf8(digits.to_vec()).expect("base-36 alphabet is valid UTF-8")
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> io::Result<()> {
    File::open(directory).and_then(|directory| directory.sync_all())
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) -> io::Result<()> {
    Ok(())
}

/// Errors raised by [`RefStore`].
#[derive(Debug)]
pub enum RefStoreError {
    HomeDirectoryUnavailable,
    MissingParent(PathBuf),
    InvalidRefId(String),
    RefAlreadyExists(String),
    RefNotFound(String),
    Io {
        path: PathBuf,
        source: io::Error,
    },
    Deserialize {
        path: PathBuf,
        source: serde_json::Error,
    },
    Durability {
        path: PathBuf,
        source: io::Error,
    },
    Serialize(serde_json::Error),
}

impl fmt::Display for RefStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HomeDirectoryUnavailable => {
                write!(formatter, "cannot determine the home directory")
            }
            Self::MissingParent(path) => {
                write!(formatter, "{} has no parent directory", path.display())
            }
            Self::InvalidRefId(id) => write!(formatter, "{id} is not a valid ref ID"),
            Self::RefAlreadyExists(id) => write!(formatter, "ref {id} already exists"),
            Self::RefNotFound(id) => write!(formatter, "ref {id} does not exist"),
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::Deserialize { path, source } => {
                write!(
                    formatter,
                    "cannot deserialize refs from {}: {source}",
                    path.display()
                )
            }
            Self::Durability { path, source } => write!(
                formatter,
                "pending refs directory durability at {}: {source}",
                path.display()
            ),
            Self::Serialize(source) => write!(formatter, "cannot serialize refs: {source}"),
        }
    }
}

impl std::error::Error for RefStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } | Self::Durability { source, .. } => Some(source),
            Self::Deserialize { source, .. } | Self::Serialize(source) => Some(source),
            Self::HomeDirectoryUnavailable
            | Self::MissingParent(_)
            | Self::InvalidRefId(_)
            | Self::RefAlreadyExists(_)
            | Self::RefNotFound(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashSet, VecDeque},
        fs, io,
        sync::{
            Arc, Barrier, Condvar, Mutex,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        thread,
        time::Duration,
    };

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::path::PathBuf;

    use super::{
        AtomicWriteHook, NewRef, RefIdSource, RefListFilter, RefPatch, RefRecord, RefStore,
        RefStoreError, RefStorePaths,
    };

    #[derive(Default)]
    struct SequenceIdSource(Mutex<VecDeque<String>>);

    impl SequenceIdSource {
        fn new(ids: &[&str]) -> Self {
            Self(Mutex::new(ids.iter().map(ToString::to_string).collect()))
        }
    }

    impl RefIdSource for SequenceIdSource {
        fn next_id(&self) -> String {
            self.0.lock().unwrap().pop_front().unwrap()
        }
    }

    fn finish(allocation: super::PendingRefAllocation) -> RefRecord {
        let completion = allocation
            .acknowledge_with(|_| Ok::<(), std::convert::Infallible>(()))
            .unwrap();
        assert!(completion.durability_warning().is_none());
        completion.into_parts().0
    }

    #[test]
    fn allocate_retries_a_colliding_id_while_holding_the_store_lock() {
        let directory = tempfile::tempdir().unwrap();
        let paths = RefStorePaths::in_directory(directory.path());
        let store = RefStore::with_id_source(
            paths,
            Arc::new(SequenceIdSource::new(&["w00000", "w00000", "w00001"])),
        );

        let first = finish(store.allocate(NewRef::for_session("session-one")).unwrap());
        let second = finish(store.allocate(NewRef::for_session("session-two")).unwrap());

        assert_eq!(first.id, "w00000");
        assert_eq!(second.id, "w00001");
    }

    #[test]
    fn allocate_skips_noncanonical_candidates() {
        let directory = tempfile::tempdir().unwrap();
        let paths = RefStorePaths::in_directory(directory.path());
        let store = RefStore::with_id_source(
            paths,
            Arc::new(SequenceIdSource::new(&[
                "x4f2a1", "w4f2a", "w4f2a10", "w4F2a1", "w4f-a1", "w4f_a1", "wé0000", "w4f2a1",
            ])),
        );

        let allocated = finish(store.allocate(NewRef::for_session("session-one")).unwrap());

        assert_eq!(allocated.id, "w4f2a1");
    }

    #[test]
    fn ref_id_suffix_is_zero_padded_base36() {
        assert_eq!(super::encode_base36(35), "0000z");
        assert_eq!(super::encode_base36(60_466_175), "zzzzz");
    }

    #[test]
    fn concurrent_allocations_never_duplicate_a_ref_id() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(RefStore::with_paths(RefStorePaths::in_directory(
            directory.path(),
        )));
        let start = Arc::new(Barrier::new(17));
        let workers: Vec<_> = (0..16)
            .map(|number| {
                let store = Arc::clone(&store);
                let start = Arc::clone(&start);
                thread::spawn(move || {
                    start.wait();
                    finish(
                        store
                            .allocate(NewRef::for_session(format!("session-{number}")))
                            .unwrap(),
                    )
                })
            })
            .collect();
        start.wait();

        let ids: HashSet<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap().id)
            .collect();

        assert_eq!(ids.len(), 16);
        assert_eq!(store.list(&RefListFilter::default()).unwrap().len(), 16);
    }

    #[test]
    fn ref_lifecycle_preserves_a_tombstone_and_filters_by_spawner_tag() {
        let directory = tempfile::tempdir().unwrap();
        let store = RefStore::with_paths(RefStorePaths::in_directory(directory.path()));
        let original = RefRecord {
            id: "w0a1b2".to_string(),
            session_id: "session-one".to_string(),
            message_id: None,
            repo: Some("repo-a".to_string()),
            spawner_tag: Some("parent-a".to_string()),
            tombstoned: false,
        };

        store.insert(original).unwrap();
        store
            .insert(RefRecord {
                id: "w0a1b3".to_string(),
                session_id: "session-two".to_string(),
                message_id: None,
                repo: Some("repo-b".to_string()),
                spawner_tag: Some("parent-c".to_string()),
                tombstoned: false,
            })
            .unwrap();
        let patched = store
            .patch("w0a1b2", RefPatch::default().with_spawner_tag("parent-b"))
            .unwrap();
        let tombstone = store.tombstone("w0a1b2").unwrap();

        assert_eq!(patched.spawner_tag.as_deref(), Some("parent-b"));
        assert!(tombstone.tombstoned);
        assert!(store.resolve("w0a1b2").unwrap().unwrap().tombstoned);
        assert!(
            store
                .list(&RefListFilter::for_spawner("parent-b"))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .list(&RefListFilter {
                    include_tombstones: true,
                    ..RefListFilter::for_spawner("parent-b")
                })
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .list(&RefListFilter {
                    spawner_tag: Some("parent-b".to_string()),
                    all: true,
                    ..RefListFilter::default()
                })
                .unwrap()
                .len(),
            1
        );
        assert!(matches!(
            store.insert(RefRecord { id: "w0a1b2".to_string(), session_id: "new-session".to_string(), message_id: None, repo: None, spawner_tag: None, tombstoned: false }),
            Err(RefStoreError::RefAlreadyExists(id)) if id == "w0a1b2"
        ));
    }

    #[test]
    fn insert_rejects_noncanonical_ids() {
        let directory = tempfile::tempdir().unwrap();
        let store = RefStore::with_paths(RefStorePaths::in_directory(directory.path()));

        for value in [
            "x4f2a1", "w4f2a", "w4f2a10", "w4F2a1", "w4f-a1", "w4f_a1", "wé0000",
        ] {
            let result = store.insert(RefRecord {
                id: value.to_string(),
                session_id: "session-one".to_string(),
                message_id: None,
                repo: None,
                spawner_tag: None,
                tombstoned: false,
            });

            assert!(
                matches!(result, Err(RefStoreError::InvalidRefId(id)) if id == value),
                "{value} should be rejected"
            );
        }
    }

    #[test]
    fn malformed_refs_return_the_original_deserialize_error_without_replacing_them() {
        let directory = tempfile::tempdir().unwrap();
        let paths = RefStorePaths::in_directory(directory.path());
        let payload = b"not json";
        fs::write(&paths.refs_file, payload).unwrap();
        let store = RefStore::with_paths(paths.clone());

        let error = store.list(&RefListFilter::default()).unwrap_err();

        match error {
            RefStoreError::Deserialize { path, source } => {
                assert_eq!(path, paths.refs_file);
                assert_eq!(
                    source.to_string(),
                    serde_json::from_slice::<Vec<RefRecord>>(payload)
                        .unwrap_err()
                        .to_string()
                );
            }
            error => panic!("expected deserialize error, got {error:?}"),
        }
        assert_eq!(fs::read(&paths.refs_file).unwrap(), payload);
        assert!(!directory.path().join("corrupt").exists());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
    }

    #[test]
    fn type_invalid_refs_return_the_original_deserialize_error_without_replacing_them() {
        let directory = tempfile::tempdir().unwrap();
        let paths = RefStorePaths::in_directory(directory.path());
        let payload = br#"[{"id":"w00000","session_id":42}]"#;
        fs::write(&paths.refs_file, payload).unwrap();
        let store = RefStore::with_paths(paths.clone());

        let error = store.list(&RefListFilter::default()).unwrap_err();

        match error {
            RefStoreError::Deserialize { path, source } => {
                assert_eq!(path, paths.refs_file);
                assert_eq!(
                    source.to_string(),
                    serde_json::from_slice::<Vec<RefRecord>>(payload)
                        .unwrap_err()
                        .to_string()
                );
            }
            error => panic!("expected deserialize error, got {error:?}"),
        }
        assert_eq!(fs::read(&paths.refs_file).unwrap(), payload);
        assert!(!directory.path().join("corrupt").exists());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
    }

    #[cfg(unix)]
    fn mode(path: &std::path::Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn operations_secure_preexisting_refs_and_lock_files() {
        let directory = tempfile::tempdir().unwrap();
        let paths = RefStorePaths::in_directory(directory.path());
        fs::write(&paths.refs_file, b"[]").unwrap();
        fs::write(&paths.lock_file, b"").unwrap();
        fs::set_permissions(&paths.refs_file, fs::Permissions::from_mode(0o644)).unwrap();
        fs::set_permissions(&paths.lock_file, fs::Permissions::from_mode(0o644)).unwrap();

        RefStore::with_paths(paths.clone())
            .list(&RefListFilter::default())
            .unwrap();

        assert_eq!(mode(&paths.refs_file), 0o600);
        assert_eq!(mode(&paths.lock_file), 0o600);
    }

    #[cfg(unix)]
    struct CaptureTemporaryFileMode {
        directory: PathBuf,
        temporary_mode: Mutex<Option<u32>>,
    }

    #[cfg(unix)]
    impl AtomicWriteHook for CaptureTemporaryFileMode {
        fn before_rename(&self) -> io::Result<()> {
            let temporary = fs::read_dir(&self.directory)?.find_map(|entry| {
                let entry = entry.ok()?;
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".refs.json.")
                    .then_some(entry.path())
            });
            *self.temporary_mode.lock().unwrap() = temporary.map(|path| mode(&path));
            Ok(())
        }
    }

    #[cfg(unix)]
    #[test]
    fn first_mutation_creates_private_store_directory_and_files() {
        let directory = tempfile::tempdir().unwrap();
        let store_directory = directory.path().join("state");
        let paths = RefStorePaths::in_directory(&store_directory);
        let hook = Arc::new(CaptureTemporaryFileMode {
            directory: store_directory.clone(),
            temporary_mode: Mutex::new(None),
        });
        let store = RefStore::with_id_source_and_write_hook(
            paths.clone(),
            Arc::new(SequenceIdSource::new(&["w00000"])),
            hook.clone(),
        );

        finish(store.allocate(NewRef::for_session("session-one")).unwrap());

        assert_eq!(mode(&store_directory), 0o700);
        assert_eq!(mode(&paths.refs_file), 0o600);
        assert_eq!(mode(&paths.lock_file), 0o600);
        assert_eq!(*hook.temporary_mode.lock().unwrap(), Some(0o600));
    }

    #[cfg(unix)]
    #[test]
    fn first_mutation_is_private_under_an_owner_read_blocking_umask() {
        const CHILD_ENV: &str = "OCA_STATE_REFS_UMASK_CHILD";
        const TEST_NAME: &str =
            "refs::tests::first_mutation_is_private_under_an_owner_read_blocking_umask";

        if std::env::var_os(CHILD_ENV).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST_NAME])
                .env(CHILD_ENV, "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success(),
                "umask child failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            // A stale `TEST_NAME` would match nothing and still exit zero.
            assert!(
                stdout.contains("1 passed"),
                "umask child ran no test, so {TEST_NAME} is stale: {stdout}"
            );
            return;
        }

        // The temporary directory is created before the umask changes so that
        // its own mode still permits the recursive cleanup on drop.
        let directory = tempfile::tempdir().unwrap();

        // SAFETY: this test executes in a dedicated child process, so changing
        // the process-global umask cannot affect concurrently running tests.
        let original_umask = unsafe { libc::umask(0o400) };
        let _restore_umask = UmaskGuard(original_umask);

        let store_directory = directory.path().join("state");
        let paths = RefStorePaths::in_directory(&store_directory);
        let hook = Arc::new(CaptureTemporaryFileMode {
            directory: store_directory.clone(),
            temporary_mode: Mutex::new(None),
        });
        let store = RefStore::with_id_source_and_write_hook(
            paths.clone(),
            Arc::new(SequenceIdSource::new(&["w00000"])),
            hook.clone(),
        );

        finish(store.allocate(NewRef::for_session("session-one")).unwrap());

        assert_eq!(mode(&store_directory), 0o700);
        assert_eq!(mode(&paths.refs_file), 0o600);
        assert_eq!(mode(&paths.lock_file), 0o600);
        assert_eq!(*hook.temporary_mode.lock().unwrap(), Some(0o600));
    }

    #[cfg(unix)]
    struct UmaskGuard(libc::mode_t);

    #[cfg(unix)]
    impl Drop for UmaskGuard {
        fn drop(&mut self) {
            // SAFETY: `self.0` is the umask returned by `libc::umask` above.
            unsafe {
                libc::umask(self.0);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn operations_secure_an_existing_store_directory() {
        let directory = tempfile::tempdir().unwrap();
        let paths = RefStorePaths::in_directory(directory.path());
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();

        RefStore::with_paths(paths)
            .list(&RefListFilter::default())
            .unwrap();

        assert_eq!(mode(directory.path()), 0o700);
    }

    #[derive(Clone, Copy, Debug)]
    enum PreAckFailure {
        TemporaryCreate,
        TemporaryWrite,
        TemporarySync,
        Rename,
    }

    struct FailPreAckOperation(PreAckFailure);

    impl FailPreAckOperation {
        fn fail(&self, operation: PreAckFailure) -> io::Result<()> {
            (std::mem::discriminant(&self.0) != std::mem::discriminant(&operation))
                .then_some(())
                .ok_or_else(|| io::Error::other(format!("simulated {operation:?} failure")))
        }
    }

    impl AtomicWriteHook for FailPreAckOperation {
        fn before_temporary_file_create(&self) -> io::Result<()> {
            self.fail(PreAckFailure::TemporaryCreate)
        }

        fn before_temporary_file_write(&self) -> io::Result<()> {
            self.fail(PreAckFailure::TemporaryWrite)
        }

        fn before_temporary_file_sync(&self) -> io::Result<()> {
            self.fail(PreAckFailure::TemporarySync)
        }

        fn before_rename(&self) -> io::Result<()> {
            self.fail(PreAckFailure::Rename)
        }
    }

    #[test]
    fn every_fallible_pre_ack_replacement_operation_suppresses_acknowledgement() {
        for failure in [
            PreAckFailure::TemporaryCreate,
            PreAckFailure::TemporaryWrite,
            PreAckFailure::TemporarySync,
            PreAckFailure::Rename,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let paths = RefStorePaths::in_directory(directory.path());
            let initial = RefStore::with_id_source(
                paths.clone(),
                Arc::new(SequenceIdSource::new(&["w00000"])),
            );
            finish(
                initial
                    .allocate(NewRef::for_session("durable-session"))
                    .unwrap(),
            );
            let prior_contents = fs::read(&paths.refs_file).unwrap();
            let failing = RefStore::with_id_source_and_write_hook(
                paths.clone(),
                Arc::new(SequenceIdSource::new(&["w00001"])),
                Arc::new(FailPreAckOperation(failure)),
            );

            let result = failing.allocate(NewRef::for_session("unacknowledged-session"));

            assert!(result.is_err(), "{failure:?} unexpectedly reached ack");
            assert_eq!(fs::read(&paths.refs_file).unwrap(), prior_contents);
            assert_eq!(initial.list(&RefListFilter::default()).unwrap().len(), 1);
            assert_eq!(
                fs::read_dir(directory.path())
                    .unwrap()
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
                    .count(),
                0
            );
            #[cfg(unix)]
            {
                assert_eq!(mode(directory.path()), 0o700, "{failure:?}");
                assert_eq!(mode(&paths.refs_file), 0o600, "{failure:?}");
                assert_eq!(mode(&paths.lock_file), 0o600, "{failure:?}");
            }
        }
    }

    struct SequencingHook {
        observed_records: Mutex<Option<Vec<RefRecord>>>,
        sync_started: Mutex<Option<mpsc::Sender<()>>>,
        released: (Mutex<bool>, Condvar),
    }

    impl SequencingHook {
        fn release(&self) {
            *self.released.0.lock().unwrap() = true;
            self.released.1.notify_all();
        }
    }

    impl AtomicWriteHook for SequencingHook {
        fn before_rename(&self) -> io::Result<()> {
            Ok(())
        }

        fn after_rename(&self, refs_file: &std::path::Path) {
            let records = serde_json::from_slice(&fs::read(refs_file).unwrap()).unwrap();
            *self.observed_records.lock().unwrap() = Some(records);
        }

        fn before_directory_sync(&self) -> io::Result<()> {
            if let Some(sender) = self.sync_started.lock().unwrap().take() {
                sender.send(()).unwrap();
            }
            let mut released = self.released.0.lock().unwrap();
            while !*released {
                released = self.released.1.wait(released).unwrap();
            }
            Ok(())
        }
    }

    #[test]
    fn acknowledgement_observes_renamed_refs_before_deferred_sync_and_lock_release() {
        let directory = tempfile::tempdir().unwrap();
        let paths = RefStorePaths::in_directory(directory.path());
        let (sync_started_tx, sync_started_rx) = mpsc::channel();
        let hook = Arc::new(SequencingHook {
            observed_records: Mutex::new(None),
            sync_started: Mutex::new(Some(sync_started_tx)),
            released: (Mutex::new(false), Condvar::new()),
        });
        let first = RefStore::with_id_source_and_write_hook(
            paths.clone(),
            Arc::new(SequenceIdSource::new(&["w00000"])),
            hook.clone(),
        );
        let (ack_tx, ack_rx) = mpsc::channel();
        let first_worker = thread::spawn(move || {
            let pending = first
                .allocate(NewRef::for_session("acknowledged-session"))
                .unwrap();
            pending
                .acknowledge_with(|record| ack_tx.send(record.clone()).map_err(|_| ()))
                .unwrap()
        });

        let acknowledged = ack_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(acknowledged.id, "w00000");
        assert_eq!(
            hook.observed_records.lock().unwrap().as_ref(),
            Some(&vec![acknowledged.clone()])
        );
        sync_started_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap();

        let second = RefStore::with_id_source(paths, Arc::new(SequenceIdSource::new(&["w00001"])));
        let (second_started_tx, second_started_rx) = mpsc::channel();
        let (second_entered_tx, second_entered_rx) = mpsc::channel();
        let second_worker = thread::spawn(move || {
            second_started_tx.send(()).unwrap();
            let pending = second
                .allocate(NewRef::for_session("concurrent-session"))
                .unwrap();
            second_entered_tx.send(()).unwrap();
            pending
        });
        second_started_rx.recv().unwrap();
        assert_eq!(
            second_entered_rx.recv_timeout(Duration::from_millis(150)),
            Err(mpsc::RecvTimeoutError::Timeout),
            "the concurrent allocation entered while deferred durability held refs.lock"
        );

        hook.release();
        let first_completion = first_worker.join().unwrap();
        assert!(first_completion.durability_warning().is_none());
        second_entered_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        let second_pending = second_worker.join().unwrap();
        assert!(
            second_pending
                .acknowledge_with(|_| Ok::<(), std::convert::Infallible>(()))
                .unwrap()
                .durability_warning()
                .is_none()
        );
    }

    struct FailDirectorySync {
        remaining_failures: AtomicUsize,
        calls: AtomicUsize,
    }

    impl FailDirectorySync {
        fn new(failures: usize) -> Self {
            Self {
                remaining_failures: AtomicUsize::new(failures),
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl AtomicWriteHook for FailDirectorySync {
        fn before_rename(&self) -> io::Result<()> {
            Ok(())
        }

        fn before_directory_sync(&self) -> io::Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self
                .remaining_failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                Err(io::Error::other("simulated directory sync failure"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn post_ack_sync_failure_returns_a_warning_and_the_next_entrant_retries() {
        let directory = tempfile::tempdir().unwrap();
        let paths = RefStorePaths::in_directory(directory.path());
        let hook = Arc::new(FailDirectorySync::new(1));
        let store = RefStore::with_id_source_and_write_hook(
            paths.clone(),
            Arc::new(SequenceIdSource::new(&["w00000"])),
            hook.clone(),
        );

        let pending = store
            .allocate(NewRef::for_session("acknowledged-session"))
            .unwrap();
        let acknowledged = pending.record().clone();
        let completion = pending.finish_after_ack();

        assert_eq!(completion.record(), &acknowledged);
        let warning = completion.durability_warning().unwrap();
        assert_eq!(warning.path(), directory.path());
        assert!(warning.to_string().contains("not directory-durable"));
        assert_eq!(hook.calls.load(Ordering::SeqCst), 1);

        assert_eq!(
            store.list(&RefListFilter::default()).unwrap(),
            vec![acknowledged]
        );
        assert_eq!(hook.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn retry_failure_prevents_reliance_and_remains_retryable_by_a_later_entrant() {
        let directory = tempfile::tempdir().unwrap();
        let paths = RefStorePaths::in_directory(directory.path());
        let first_hook = Arc::new(FailDirectorySync::new(1));
        let first = RefStore::with_id_source_and_write_hook(
            paths.clone(),
            Arc::new(SequenceIdSource::new(&["w00000"])),
            first_hook,
        );
        let completion = first
            .allocate(NewRef::for_session("acknowledged-session"))
            .unwrap()
            .finish_after_ack();
        assert!(completion.durability_warning().is_some());

        let retry_hook = Arc::new(FailDirectorySync::new(1));
        let retrying = RefStore::with_id_source_and_write_hook(
            paths.clone(),
            Arc::new(SequenceIdSource::new(&[])),
            retry_hook.clone(),
        );
        assert!(matches!(
            retrying.list(&RefListFilter::default()),
            Err(RefStoreError::Durability { path, .. }) if path == directory.path()
        ));
        assert_eq!(retry_hook.calls.load(Ordering::SeqCst), 1);

        let later = RefStore::with_paths(paths);
        assert_eq!(later.list(&RefListFilter::default()).unwrap().len(), 1);
    }

    #[test]
    fn an_unfinished_post_ack_attempt_is_transferred_to_the_next_entrant() {
        let directory = tempfile::tempdir().unwrap();
        let paths = RefStorePaths::in_directory(directory.path());
        let first_hook = Arc::new(FailDirectorySync::new(0));
        let first = RefStore::with_id_source_and_write_hook(
            paths.clone(),
            Arc::new(SequenceIdSource::new(&["w00000"])),
            first_hook.clone(),
        );
        let pending = first
            .allocate(NewRef::for_session("acknowledged-session"))
            .unwrap();

        drop(pending);
        assert_eq!(first_hook.calls.load(Ordering::SeqCst), 0);

        let retry_hook = Arc::new(FailDirectorySync::new(0));
        let next = RefStore::with_id_source_and_write_hook(
            paths,
            Arc::new(SequenceIdSource::new(&[])),
            retry_hook.clone(),
        );
        assert_eq!(next.list(&RefListFilter::default()).unwrap().len(), 1);
        assert_eq!(retry_hook.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn acknowledgement_flush_failure_suppresses_completion_and_defers_retry() {
        let directory = tempfile::tempdir().unwrap();
        let paths = RefStorePaths::in_directory(directory.path());
        let first_hook = Arc::new(FailDirectorySync::new(0));
        let first = RefStore::with_id_source_and_write_hook(
            paths.clone(),
            Arc::new(SequenceIdSource::new(&["w00000"])),
            first_hook.clone(),
        );
        let pending = first
            .allocate(NewRef::for_session("unacknowledged-session"))
            .unwrap();

        let result = pending.acknowledge_with(|_| {
            Err::<(), _>(io::Error::other("simulated acknowledgement flush failure"))
        });

        assert!(result.is_err());
        assert_eq!(first_hook.calls.load(Ordering::SeqCst), 0);
        let retry_hook = Arc::new(FailDirectorySync::new(0));
        let next = RefStore::with_id_source_and_write_hook(
            paths,
            Arc::new(SequenceIdSource::new(&[])),
            retry_hook.clone(),
        );
        assert_eq!(next.list(&RefListFilter::default()).unwrap().len(), 1);
        assert_eq!(retry_hook.calls.load(Ordering::SeqCst), 1);
    }
}
