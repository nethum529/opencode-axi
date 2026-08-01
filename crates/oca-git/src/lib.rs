use std::{
    ffi::OsStr,
    fmt, fs, io,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Output},
    sync::Arc,
    thread,
    time::Duration,
};

const LOCK_RETRY_DELAY: Duration = Duration::from_millis(5);
type AfterScanHook = dyn Fn(&Path) + Send + Sync;

/// A validated identifier used to name an oca branch and worktree.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RefId(String);

impl RefId {
    /// Creates a reference identifier that is safe to use in oca-owned paths.
    ///
    /// # Errors
    ///
    /// Returns [`GitError::InvalidRef`] when the identifier is empty or contains
    /// characters that are unsafe in an oca-owned path.
    pub fn new(value: impl Into<String>) -> Result<Self, GitError> {
        let value = value.into();
        if value.is_empty()
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(GitError::InvalidRef { value });
        }
        Ok(Self(value))
    }

    /// Returns the identifier text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RefId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// An oca-owned worktree and its branch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeRecord {
    path: PathBuf,
    branch: String,
}

/// A validated path relative to a worktree root.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RelativePath(String);

impl RelativePath {
    /// Creates a path that cannot escape a worktree root.
    ///
    /// # Errors
    ///
    /// Returns [`GitError::InvalidRelativePath`] when the value is empty,
    /// absolute, or contains a parent-directory component.
    pub fn new(value: impl Into<String>) -> Result<Self, GitError> {
        let value = value.into();
        let path = Path::new(&value);
        if value.is_empty()
            || path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(GitError::InvalidRelativePath { value });
        }
        Ok(Self(value))
    }

    /// Returns the path relative to its worktree root.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }

    /// Returns the path text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn permits(&self, changed: &Self) -> bool {
        self.0 == "." || changed.0 == self.0 || changed.0.starts_with(&format!("{}/", self.0))
    }
}

impl fmt::Display for RelativePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The manifest-scoped paths staged for a later commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeSet {
    paths: Vec<RelativePath>,
}

impl ChangeSet {
    /// Returns the paths that passed the twice-checked validation sequence.
    #[must_use]
    pub fn paths(&self) -> &[RelativePath] {
        &self.paths
    }
}

impl WorktreeRecord {
    /// Returns the absolute worktree path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the oca-owned branch name.
    #[must_use]
    pub fn branch(&self) -> &str {
        &self.branch
    }
}

/// Errors raised while oca owns a git worktree.
#[derive(Debug)]
pub enum GitError {
    /// The supplied reference is not safe for an oca-owned path.
    InvalidRef { value: String },
    /// The supplied path is not relative to the worktree root.
    InvalidRelativePath { value: String },
    /// An oca branch or worktree already exists for the reference.
    WorktreeConflict { reference: RefId },
    /// A changed path is not owned by the manifest for this turn.
    OutOfScope { paths: Vec<RelativePath> },
    /// A changed regular file is empty.
    ZeroByteOutput { paths: Vec<RelativePath> },
    /// The worktree has no changes to stage.
    WorktreeEmpty,
    /// The worktree changed after its initial scan and before commit handoff.
    WorktreeChanged { paths: Vec<RelativePath> },
    /// An operating-system operation failed.
    Io(io::Error),
    /// Git returned an unexpected non-zero status.
    GitCommand {
        operation: &'static str,
        status: ExitStatus,
    },
}

impl GitError {
    /// Returns the stable error code consumed by the CLI.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRef { .. } => "invalid_ref",
            Self::InvalidRelativePath { .. } => "invalid_relative_path",
            Self::WorktreeConflict { .. } => "worktree_conflict",
            Self::OutOfScope { .. } => "out_of_scope",
            Self::ZeroByteOutput { .. } => "zero_byte_output",
            Self::WorktreeEmpty => "worktree_empty",
            Self::WorktreeChanged { .. } => "worktree_changed",
            Self::Io(_) | Self::GitCommand { .. } => "git_error",
        }
    }
}

impl fmt::Display for GitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRef { value } => write!(formatter, "invalid oca reference: {value}"),
            Self::InvalidRelativePath { value } => {
                write!(formatter, "invalid relative path: {value}")
            }
            Self::WorktreeConflict { reference } => {
                write!(formatter, "worktree conflict for reference {reference}")
            }
            Self::OutOfScope { paths } => {
                write!(formatter, "paths outside the manifest: {paths:?}")
            }
            Self::ZeroByteOutput { paths } => {
                write!(formatter, "zero-byte output paths: {paths:?}")
            }
            Self::WorktreeEmpty => formatter.write_str("worktree has no changes"),
            Self::WorktreeChanged { paths } => {
                write!(formatter, "worktree changed during validation: {paths:?}")
            }
            Self::Io(error) => write!(formatter, "filesystem operation failed: {error}"),
            Self::GitCommand { operation, status } => {
                write!(formatter, "git {operation} failed with status {status}")
            }
        }
    }
}

impl std::error::Error for GitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::InvalidRef { .. }
            | Self::InvalidRelativePath { .. }
            | Self::WorktreeConflict { .. }
            | Self::OutOfScope { .. }
            | Self::ZeroByteOutput { .. }
            | Self::WorktreeEmpty
            | Self::WorktreeChanged { .. }
            | Self::GitCommand { .. } => None,
        }
    }
}

impl From<io::Error> for GitError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Creates and validates oca-owned Git worktrees.
pub struct WorktreeManager {
    after_scan_hook: Option<Arc<AfterScanHook>>,
}

impl fmt::Debug for WorktreeManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorktreeManager")
            .field("has_after_scan_hook", &self.after_scan_hook.is_some())
            .finish()
    }
}

impl Default for WorktreeManager {
    fn default() -> Self {
        Self::new()
    }
}

impl WorktreeManager {
    /// Creates a manager that performs only fixed-argument-array git invocations.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            after_scan_hook: None,
        }
    }

    /// Creates a manager with a hook run after the first change scan.
    ///
    /// This seam exists for deterministic TOCTOU tests.
    #[doc(hidden)]
    #[must_use]
    pub fn with_after_scan_hook(hook: Arc<AfterScanHook>) -> Self {
        Self {
            after_scan_hook: Some(hook),
        }
    }

    /// Creates `.oca/wt/<ref>` on branch `oca/<ref>` under a repository lock.
    ///
    /// # Errors
    ///
    /// Returns [`GitError::WorktreeConflict`] when the oca branch or worktree
    /// already exists, or a git/filesystem error when creation cannot complete.
    pub fn create(&self, reference: &RefId, base: &Path) -> Result<WorktreeRecord, GitError> {
        let _lock = RepositoryLock::acquire(base)?;
        let branch = format!("oca/{reference}");
        let path = base.join(".oca").join("wt").join(reference.as_str());

        if path.exists() || branch_exists(base, &branch)? {
            return Err(GitError::WorktreeConflict {
                reference: reference.clone(),
            });
        }

        let status = git_status(
            base,
            [
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("-b"),
                OsStr::new(&branch),
                path.as_os_str(),
            ],
        )?;
        if !status.success() {
            return Err(GitError::GitCommand {
                operation: "worktree add",
                status,
            });
        }

        Ok(WorktreeRecord { path, branch })
    }

    /// Validates and stages manifest-owned changes before commit handoff.
    ///
    /// # Errors
    ///
    /// Returns a stable validation error for an empty worktree, an out-of-scope
    /// path, a zero-byte regular file, or a worktree that changes during the
    /// validation sequence. Git and filesystem errors are returned unchanged.
    pub fn validate(
        &self,
        worktree: &Path,
        manifest: &[RelativePath],
    ) -> Result<ChangeSet, GitError> {
        let initial_paths = changed_paths(worktree)?;
        if initial_paths.is_empty() {
            return Err(GitError::WorktreeEmpty);
        }
        validate_manifest(&initial_paths, manifest)?;
        reject_zero_byte_files(worktree, &initial_paths)?;

        if let Some(hook) = &self.after_scan_hook {
            hook(worktree);
        }

        stage_paths(worktree, &initial_paths)?;

        let rechecked_paths = changed_paths(worktree)?;
        validate_manifest(&rechecked_paths, manifest)?;
        reject_zero_byte_files(worktree, &rechecked_paths)?;
        if rechecked_paths != initial_paths || worktree_differs_from_index(worktree)? {
            return Err(GitError::WorktreeChanged {
                paths: rechecked_paths,
            });
        }
        if staged_diff_is_empty(worktree)? {
            return Err(GitError::WorktreeEmpty);
        }

        Ok(ChangeSet {
            paths: initial_paths,
        })
    }
}

struct RepositoryLock {
    path: PathBuf,
}

impl RepositoryLock {
    fn acquire(base: &Path) -> Result<Self, GitError> {
        let oca_directory = base.join(".oca");
        fs::create_dir_all(&oca_directory)?;
        let path = oca_directory.join("worktree.lock");

        loop {
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    thread::sleep(LOCK_RETRY_DELAY);
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

impl Drop for RepositoryLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}

fn branch_exists(base: &Path, branch: &str) -> Result<bool, GitError> {
    let reference = format!("refs/heads/{branch}");
    let status = git_status(
        base,
        [
            OsStr::new("show-ref"),
            OsStr::new("--verify"),
            OsStr::new("--quiet"),
            OsStr::new(&reference),
        ],
    )?;
    if status.success() {
        return Ok(true);
    }
    if status.code() == Some(1) {
        return Ok(false);
    }
    Err(GitError::GitCommand {
        operation: "show-ref --verify",
        status,
    })
}

fn changed_paths(worktree: &Path) -> Result<Vec<RelativePath>, GitError> {
    let output = git_output(
        worktree,
        [
            OsStr::new("status"),
            OsStr::new("--porcelain"),
            OsStr::new("--untracked-files=all"),
            OsStr::new("-z"),
        ],
    )?;
    let mut records = output.stdout.split(|byte| *byte == b'\0');
    let mut paths = Vec::new();
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        if record.len() < 4 || record[2] != b' ' {
            return Err(GitError::GitCommand {
                operation: "status --porcelain",
                status: output.status,
            });
        }
        let status = &record[..2];
        let path = std::str::from_utf8(&record[3..]).map_err(|_| GitError::GitCommand {
            operation: "status --porcelain",
            status: output.status,
        })?;
        paths.push(RelativePath::new(path.to_owned())?);
        if status.iter().any(|byte| matches!(*byte, b'R' | b'C')) {
            let Some(original) = records.next() else {
                return Err(GitError::GitCommand {
                    operation: "status --porcelain",
                    status: output.status,
                });
            };
            let original = std::str::from_utf8(original).map_err(|_| GitError::GitCommand {
                operation: "status --porcelain",
                status: output.status,
            })?;
            paths.push(RelativePath::new(original.to_owned())?);
        }
    }
    paths.sort_unstable();
    paths.dedup();
    Ok(paths)
}

fn validate_manifest(paths: &[RelativePath], manifest: &[RelativePath]) -> Result<(), GitError> {
    let outside_manifest: Vec<_> = paths
        .iter()
        .filter(|path| !manifest.iter().any(|allowed| allowed.permits(path)))
        .cloned()
        .collect();
    if outside_manifest.is_empty() {
        Ok(())
    } else {
        Err(GitError::OutOfScope {
            paths: outside_manifest,
        })
    }
}

fn reject_zero_byte_files(worktree: &Path, paths: &[RelativePath]) -> Result<(), GitError> {
    let zero_byte_paths = paths
        .iter()
        .filter_map(
            |path| match fs::symlink_metadata(worktree.join(path.as_path())) {
                Ok(metadata) if metadata.file_type().is_file() && metadata.len() == 0 => {
                    Some(Ok(path.clone()))
                }
                Ok(_) => None,
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => Some(Err(GitError::Io(error))),
            },
        )
        .collect::<Result<Vec<_>, _>>()?;
    if zero_byte_paths.is_empty() {
        Ok(())
    } else {
        Err(GitError::ZeroByteOutput {
            paths: zero_byte_paths,
        })
    }
}

fn stage_paths(worktree: &Path, paths: &[RelativePath]) -> Result<(), GitError> {
    let mut arguments = Vec::with_capacity(paths.len() + 2);
    arguments.extend([OsStr::new("add"), OsStr::new("--")]);
    arguments.extend(paths.iter().map(|path| path.as_path().as_os_str()));
    let status = git_status_slice(worktree, &arguments)?;
    if status.success() {
        Ok(())
    } else {
        Err(GitError::GitCommand {
            operation: "add",
            status,
        })
    }
}

fn worktree_differs_from_index(worktree: &Path) -> Result<bool, GitError> {
    git_quiet_diff_has_changes(worktree, [OsStr::new("diff"), OsStr::new("--quiet")])
}

fn staged_diff_is_empty(worktree: &Path) -> Result<bool, GitError> {
    Ok(!git_quiet_diff_has_changes(
        worktree,
        [
            OsStr::new("diff"),
            OsStr::new("--cached"),
            OsStr::new("--quiet"),
        ],
    )?)
}

fn git_quiet_diff_has_changes<const N: usize>(
    worktree: &Path,
    arguments: [&OsStr; N],
) -> Result<bool, GitError> {
    let status = git_status(worktree, arguments)?;
    match status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(GitError::GitCommand {
            operation: "diff --quiet",
            status,
        }),
    }
}

fn git_status<const N: usize>(base: &Path, arguments: [&OsStr; N]) -> Result<ExitStatus, GitError> {
    git_status_slice(base, &arguments)
}

fn git_status_slice(base: &Path, arguments: &[&OsStr]) -> Result<ExitStatus, GitError> {
    Command::new("git")
        .args([OsStr::new("-C"), base.as_os_str()])
        .args(arguments)
        .status()
        .map_err(GitError::from)
}

fn git_output<const N: usize>(base: &Path, arguments: [&OsStr; N]) -> Result<Output, GitError> {
    Command::new("git")
        .args([OsStr::new("-C"), base.as_os_str()])
        .args(arguments)
        .output()
        .map_err(GitError::from)
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::{Path, PathBuf},
        process::Command,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{GitError, RefId, RelativePath, WorktreeManager};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestRepository {
        path: PathBuf,
    }

    impl TestRepository {
        fn new() -> Self {
            let unique = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("the system clock should be after the Unix epoch")
                .as_nanos();
            let path = env::temp_dir().join(format!(
                "oca-git-test-{}-{timestamp}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("the test repository directory should be created");
            run_git(&path, ["init", "--quiet"]);
            run_git(&path, ["config", "user.name", "oca-git test"]);
            run_git(&path, ["config", "user.email", "oca-git@example.test"]);
            fs::write(path.join("README.md"), "initial\n")
                .expect("the initial file should be written");
            run_git(&path, ["add", "README.md"]);
            run_git(&path, ["commit", "--quiet", "-m", "initial"]);
            Self { path }
        }
    }

    impl Drop for TestRepository {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.path).expect("the test repository should be removed");
        }
    }

    fn run_git<const N: usize>(directory: &Path, arguments: [&str; N]) {
        let status = Command::new("git")
            .args(["-C", directory.to_str().expect("test paths are UTF-8")])
            .args(arguments)
            .status()
            .expect("git should start");
        assert!(status.success(), "git {arguments:?} should succeed");
    }

    fn git_output<const N: usize>(directory: &Path, arguments: [&str; N]) -> String {
        let output = Command::new("git")
            .args(["-C", directory.to_str().expect("test paths are UTF-8")])
            .args(arguments)
            .output()
            .expect("git should start");
        assert!(output.status.success(), "git {arguments:?} should succeed");
        String::from_utf8(output.stdout).expect("git output should be UTF-8")
    }

    #[test]
    fn create_allocates_the_ref_worktree_on_its_oca_branch() {
        let repository = TestRepository::new();
        let manager = WorktreeManager::new();
        let reference = RefId::new("w4f2a1").expect("the reference should be valid");

        let worktree = manager
            .create(&reference, &repository.path)
            .expect("worktree creation should succeed");

        assert_eq!(worktree.path(), repository.path.join(".oca/wt/w4f2a1"));
        assert_eq!(worktree.branch(), "oca/w4f2a1");
        assert!(worktree.path().is_dir());
        run_git(worktree.path(), ["rev-parse", "--verify", "oca/w4f2a1"]);
    }

    #[test]
    fn validate_stages_only_the_changed_manifest_paths() {
        let repository = TestRepository::new();
        fs::write(repository.path.join("allowed.txt"), "changed\n")
            .expect("the changed file should be written");
        let manager = WorktreeManager::new();
        let manifest = [RelativePath::new("allowed.txt").expect("the path should be valid")];

        let changes = manager
            .validate(&repository.path, &manifest)
            .expect("the manifest-scoped change should validate");

        assert_eq!(changes.paths(), &manifest);
        assert_eq!(
            git_output(&repository.path, ["diff", "--cached", "--name-only"]),
            "allowed.txt\n"
        );
    }

    #[test]
    fn validate_rechecks_after_staging_and_rejects_a_late_out_of_scope_file() {
        let repository = TestRepository::new();
        fs::write(repository.path.join("allowed.txt"), "changed\n")
            .expect("the initial changed file should be written");
        let manager = WorktreeManager::with_after_scan_hook(std::sync::Arc::new(|worktree| {
            fs::write(worktree.join("late.txt"), "late\n")
                .expect("the injected late file should be written");
        }));
        let manifest = [RelativePath::new("allowed.txt").expect("the path should be valid")];

        let error = manager
            .validate(&repository.path, &manifest)
            .expect_err("the post-stage re-check should catch the late file");

        match error {
            GitError::OutOfScope { paths } => {
                assert_eq!(
                    paths,
                    vec![RelativePath::new("late.txt").expect("the path should be valid")]
                );
            }
            other => panic!("expected out_of_scope, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_out_of_scope_paths_and_leaves_the_diff_intact() {
        let repository = TestRepository::new();
        fs::write(repository.path.join("allowed.txt"), "allowed\n")
            .expect("the allowed file should be written");
        fs::write(repository.path.join("outside.txt"), "outside\n")
            .expect("the outside file should be written");
        let manager = WorktreeManager::new();
        let manifest = [RelativePath::new("allowed.txt").expect("the path should be valid")];

        let error = manager
            .validate(&repository.path, &manifest)
            .expect_err("an out-of-scope path should fail validation");

        match error {
            GitError::OutOfScope { paths } => {
                assert_eq!(
                    paths,
                    vec![RelativePath::new("outside.txt").expect("the path should be valid")]
                );
            }
            other => panic!("expected out_of_scope, got {other:?}"),
        }
        assert_eq!(
            git_output(&repository.path, ["diff", "--cached", "--name-only"]),
            ""
        );
        assert!(repository.path.join("outside.txt").is_file());
    }

    #[test]
    fn validate_rejects_zero_byte_regular_files_by_name() {
        let repository = TestRepository::new();
        fs::write(repository.path.join("empty.txt"), "").expect("the empty file should be written");
        let manager = WorktreeManager::new();
        let manifest = [RelativePath::new("empty.txt").expect("the path should be valid")];

        let error = manager
            .validate(&repository.path, &manifest)
            .expect_err("a zero-byte file should fail validation");

        match error {
            GitError::ZeroByteOutput { paths } => {
                assert_eq!(
                    paths,
                    vec![RelativePath::new("empty.txt").expect("the path should be valid")]
                );
            }
            other => panic!("expected zero_byte_output, got {other:?}"),
        }
        assert!(repository.path.join("empty.txt").is_file());
    }

    #[test]
    fn validate_rejects_an_empty_worktree() {
        let repository = TestRepository::new();
        let manager = WorktreeManager::new();

        let error = manager
            .validate(&repository.path, &[])
            .expect_err("an unchanged worktree should fail validation");

        assert!(matches!(error, GitError::WorktreeEmpty));
    }

    #[test]
    fn create_rejects_an_unrelated_preexisting_oca_branch() {
        let repository = TestRepository::new();
        let reference = RefId::new("w4f2a1").expect("the reference should be valid");
        run_git(&repository.path, ["branch", "oca/w4f2a1"]);

        let error = WorktreeManager::new()
            .create(&reference, &repository.path)
            .expect_err("an existing unrelated oca branch should conflict");

        assert!(matches!(error, GitError::WorktreeConflict { .. }));
        assert_eq!(error.code(), "worktree_conflict");
    }

    #[test]
    fn concurrent_create_calls_never_allocate_the_same_worktree() {
        let repository = TestRepository::new();
        let reference = RefId::new("w4f2a1").expect("the reference should be valid");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let first_path = repository.path.clone();
        let second_path = repository.path.clone();
        let first_reference = reference.clone();
        let second_reference = reference.clone();
        let first_barrier = std::sync::Arc::clone(&barrier);
        let second_barrier = std::sync::Arc::clone(&barrier);

        let first = std::thread::spawn(move || {
            first_barrier.wait();
            WorktreeManager::new().create(&first_reference, &first_path)
        });
        let second = std::thread::spawn(move || {
            second_barrier.wait();
            WorktreeManager::new().create(&second_reference, &second_path)
        });

        let results = [
            first
                .join()
                .expect("the first create thread should not panic"),
            second
                .join()
                .expect("the second create thread should not panic"),
        ];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(GitError::WorktreeConflict { .. })))
                .count(),
            1
        );
    }

    #[test]
    fn git_shell_outs_are_fixed_argument_arrays_not_shell_strings() {
        let source = include_str!("lib.rs");
        let production_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("the production source should precede the tests");

        assert_eq!(
            production_source.matches("Command::new(\"git\")").count(),
            2
        );
        assert!(!production_source.contains(".arg("));
        assert!(!production_source.contains("Command::new(\"sh\")"));
        assert!(!production_source.contains("Command::new(\"bash\")"));
        assert!(!production_source.contains(" -c "));
    }
}
