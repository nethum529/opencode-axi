// Durable local storage for the short refs that identify OpenCode sessions.
// Every mutation takes an advisory lock, rereads `refs.json`, and replaces it
// atomically. Keeping the read inside the lock is important: it makes the
// collision check in `RefStore::allocate` meaningful across processes.

use std::{
    collections::HashSet,
    env, fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

const REF_ID_WIDTH: usize = 5;
const REF_ID_SPACE: u64 = 60_466_176;

/// A stored ref. `id` is always `w` followed by five lowercase base-36 digits.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RefRecord {
    pub id: String,
    pub session_id: String,
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
            repo: None,
            spawner_tag: None,
        }
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
    pub corrupt_directory: PathBuf,
}

impl RefStorePaths {
    #[must_use]
    pub fn in_directory(directory: impl AsRef<Path>) -> Self {
        let directory = directory.as_ref();
        Self {
            refs_file: directory.join("refs.json"),
            lock_file: directory.join("refs.lock"),
            corrupt_directory: directory.join("corrupt"),
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

/// Hook called after the temporary file is durable and before it replaces
/// `refs.json`. It is primarily a deterministic crash-test seam.
pub trait AtomicWriteHook: Send + Sync {
    /// # Errors
    ///
    /// Returns an error to simulate or report a failure before replacement.
    fn before_rename(&self) -> io::Result<()>;
}

struct NoopAtomicWriteHook;

impl AtomicWriteHook for NoopAtomicWriteHook {
    fn before_rename(&self) -> io::Result<()> {
        Ok(())
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
    corruption_notice: Arc<Mutex<Option<PathBuf>>>,
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
            corruption_notice: Arc::new(Mutex::new(None)),
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

    /// Returns the quarantined path once, so the command layer can report it
    /// to stderr without making state persistence responsible for terminal IO.
    #[must_use]
    pub fn take_corruption_notice(&self) -> Option<PathBuf> {
        self.corruption_notice
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// Allocates an unused ref ID while retaining the advisory lock through the
    /// collision check and atomic persistence.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot be read, locked, or persisted.
    pub fn allocate(&self, new_ref: NewRef) -> Result<RefRecord, RefStoreError> {
        self.with_locked_records(|records| {
            let occupied: HashSet<_> = records.iter().map(|record| record.id.as_str()).collect();
            let id = loop {
                let candidate = self.id_source.next_id();
                if is_ref_id(&candidate) && !occupied.contains(candidate.as_str()) {
                    break candidate;
                }
            };
            let record = RefRecord {
                id,
                session_id: new_ref.session_id,
                repo: new_ref.repo,
                spawner_tag: new_ref.spawner_tag,
                tombstoned: false,
            };
            records.push(record.clone());
            Ok((record, true))
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
        if !is_ref_id(&record.id) {
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
        let parent = self.refs_parent()?;
        fs::create_dir_all(parent).map_err(|source| RefStoreError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.paths.lock_file)
            .map_err(|source| RefStoreError::Io {
                path: self.paths.lock_file.clone(),
                source,
            })?;
        lock.lock_exclusive().map_err(|source| RefStoreError::Io {
            path: self.paths.lock_file.clone(),
            source,
        })?;

        let mut records = self.read_records()?;
        let (value, changed) = operation(&mut records)?;
        if changed {
            self.write_records_atomically(&records)?;
        }
        lock.unlock().map_err(|source| RefStoreError::Io {
            path: self.paths.lock_file.clone(),
            source,
        })?;
        Ok(value)
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
                if let Ok(records) = serde_json::from_slice(&bytes) {
                    Ok(records)
                } else {
                    let quarantine = self.quarantine_corrupt_file()?;
                    self.write_records_atomically(&[])?;
                    let mut notice = self
                        .corruption_notice
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if notice.is_none() {
                        *notice = Some(quarantine);
                    }
                    Ok(Vec::new())
                }
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(source) => Err(RefStoreError::Io {
                path: self.paths.refs_file.clone(),
                source,
            }),
        }
    }

    fn quarantine_corrupt_file(&self) -> Result<PathBuf, RefStoreError> {
        fs::create_dir_all(&self.paths.corrupt_directory).map_err(|source| RefStoreError::Io {
            path: self.paths.corrupt_directory.clone(),
            source,
        })?;
        let milliseconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let quarantine = self
            .paths
            .corrupt_directory
            .join(format!("refs.json.{milliseconds}"));
        fs::rename(&self.paths.refs_file, &quarantine).map_err(|source| RefStoreError::Io {
            path: self.paths.refs_file.clone(),
            source,
        })?;
        sync_directory(&self.paths.corrupt_directory)?;
        Ok(quarantine)
    }

    fn write_records_atomically(&self, records: &[RefRecord]) -> Result<(), RefStoreError> {
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
        let write_result = (|| -> Result<(), RefStoreError> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|source| RefStoreError::Io {
                    path: temporary.clone(),
                    source,
                })?;
            file.write_all(&bytes).map_err(|source| RefStoreError::Io {
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
            sync_directory(parent)?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        write_result
    }
}

fn is_ref_id(id: &str) -> bool {
    id.len() == REF_ID_WIDTH + 1
        && id.starts_with('w')
        && id
            .bytes()
            .skip(1)
            .all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase())
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
fn sync_directory(directory: &Path) -> Result<(), RefStoreError> {
    File::open(directory)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| RefStoreError::Io {
            path: directory.to_path_buf(),
            source,
        })
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) -> Result<(), RefStoreError> {
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
    Io { path: PathBuf, source: io::Error },
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
            Self::Serialize(source) => write!(formatter, "cannot serialize refs: {source}"),
        }
    }
}

impl std::error::Error for RefStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Serialize(source) => Some(source),
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
        sync::{Arc, Barrier, Mutex},
        thread,
    };

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

    #[test]
    fn allocate_retries_a_colliding_id_while_holding_the_store_lock() {
        let directory = tempfile::tempdir().unwrap();
        let paths = RefStorePaths::in_directory(directory.path());
        let store = RefStore::with_id_source(
            paths,
            Arc::new(SequenceIdSource::new(&["w00000", "w00000", "w00001"])),
        );

        let first = store.allocate(NewRef::for_session("session-one")).unwrap();
        let second = store.allocate(NewRef::for_session("session-two")).unwrap();

        assert_eq!(first.id, "w00000");
        assert_eq!(second.id, "w00001");
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
                    store
                        .allocate(NewRef::for_session(format!("session-{number}")))
                        .unwrap()
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
            repo: Some("repo-a".to_string()),
            spawner_tag: Some("parent-a".to_string()),
            tombstoned: false,
        };

        store.insert(original).unwrap();
        store
            .insert(RefRecord {
                id: "w0a1b3".to_string(),
                session_id: "session-two".to_string(),
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
            store.insert(RefRecord { id: "w0a1b2".to_string(), session_id: "new-session".to_string(), repo: None, spawner_tag: None, tombstoned: false }),
            Err(RefStoreError::RefAlreadyExists(id)) if id == "w0a1b2"
        ));
    }

    #[test]
    fn corrupt_refs_are_quarantined_and_reported_once() {
        let directory = tempfile::tempdir().unwrap();
        let paths = RefStorePaths::in_directory(directory.path());
        fs::write(&paths.refs_file, b"not json").unwrap();
        let store = RefStore::with_paths(paths.clone());

        assert!(store.list(&RefListFilter::default()).unwrap().is_empty());
        let quarantine_notice = store.take_corruption_notice().unwrap();

        assert!(quarantine_notice.starts_with(&paths.corrupt_directory));
        assert!(
            quarantine_notice
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("refs.json.")
        );
        assert_eq!(fs::read(&paths.refs_file).unwrap(), b"[]");
        assert!(store.take_corruption_notice().is_none());
    }

    struct FailBeforeRename;

    impl AtomicWriteHook for FailBeforeRename {
        fn before_rename(&self) -> io::Result<()> {
            Err(io::Error::other("simulated crash before rename"))
        }
    }

    #[test]
    fn a_failure_before_rename_leaves_the_prior_refs_file_intact() {
        let directory = tempfile::tempdir().unwrap();
        let paths = RefStorePaths::in_directory(directory.path());
        let initial =
            RefStore::with_id_source(paths.clone(), Arc::new(SequenceIdSource::new(&["w00000"])));
        initial
            .allocate(NewRef::for_session("durable-session"))
            .unwrap();
        let prior_contents = fs::read(&paths.refs_file).unwrap();
        let failing = RefStore::with_id_source_and_write_hook(
            paths.clone(),
            Arc::new(SequenceIdSource::new(&["w00001"])),
            Arc::new(FailBeforeRename),
        );

        assert!(
            failing
                .allocate(NewRef::for_session("lost-session"))
                .is_err()
        );

        assert_eq!(fs::read(&paths.refs_file).unwrap(), prior_contents);
        assert_eq!(initial.list(&RefListFilter::default()).unwrap().len(), 1);
    }
}
