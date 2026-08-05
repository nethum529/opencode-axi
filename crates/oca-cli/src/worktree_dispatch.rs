//! Worktree-only dispatch preparation and terminal commit finalization.

use std::path::{Path, PathBuf};

use oca_core::{ErrorCode, ForegroundRequest, OcaError, WorkerPolicy, WorkerState};
use oca_git::{GitError, RefId, RelativePath, TaskSummary, WorktreeManager, commit};
use oca_state::{NewRef, RefPatch, RefState, RefStore};

use crate::scope::Scope;

const RESERVED_SESSION_ID: &str = "oca_worktree_session_pending";

/// State held between pre-session worktree creation and ref acknowledgement.
pub(crate) struct WorktreeDispatch {
    repo_root: PathBuf,
    summary: TaskSummary,
    reference: Option<String>,
}

impl WorktreeDispatch {
    pub(crate) fn new(repo_root: PathBuf, original_prompt: &str) -> Self {
        Self {
            repo_root,
            summary: TaskSummary::from_original_prompt(original_prompt),
            reference: None,
        }
    }

    /// Reserves the ref and creates its worktree before session creation.
    pub(crate) fn prepare(
        &mut self,
        refs: &RefStore,
        request: &mut ForegroundRequest,
        scope: &Scope,
    ) -> Result<(), OcaError> {
        let reservation = refs
            .reserve(
                NewRef::for_session(RESERVED_SESSION_ID)
                    .with_control_metadata(
                        &request.model.alias,
                        &request.model.effort,
                        &request.role,
                        self.repo_root.display().to_string(),
                        RefState::Running,
                    )
                    .with_repo(&scope.repo)
                    .with_spawner_tag(&scope.spawner_tag),
            )
            .map_err(|error| state_error("could not reserve worktree ref", error))?;
        let reference = RefId::new(&reservation.id).map_err(git_error)?;
        let worktree = match WorktreeManager::new().create(&reference, &self.repo_root) {
            Ok(worktree) => worktree,
            Err(error) => {
                let _ = refs.tombstone(&reservation.id);
                return Err(git_error(error));
            }
        };
        let worktree_path = worktree.path().to_path_buf();
        refs.patch(
            &reservation.id,
            RefPatch::default()
                .with_cwd(worktree_path.display().to_string())
                .with_worktree_metadata(
                    worktree_path.display().to_string(),
                    worktree.branch(),
                    self.summary.as_str(),
                ),
        )
        .map_err(|error| state_error("could not store worktree metadata", error))?;

        request.cwd = worktree_path.clone();
        request.policy = WorkerPolicy::restricted([worktree_path]);
        self.reference = Some(reservation.id);
        Ok(())
    }

    pub(crate) fn record_session(&self, refs: &RefStore, session_id: &str) -> Result<(), OcaError> {
        refs.patch(
            self.reference()?,
            RefPatch::default().with_session_id(session_id),
        )
        .map(|_| ())
        .map_err(|error| state_error("could not store worktree session", error))
    }

    pub(crate) fn finish_ref(
        &self,
        refs: &RefStore,
        session_id: &str,
        message_id: &str,
    ) -> Result<String, OcaError> {
        let reference = self.reference()?;
        refs.patch(
            reference,
            RefPatch::default()
                .with_session_id(session_id)
                .with_message_id(message_id)
                .with_last_state(RefState::Running),
        )
        .map_err(|error| state_error("could not complete worktree ref", error))?;
        Ok(reference.to_owned())
    }

    fn reference(&self) -> Result<&str, OcaError> {
        self.reference.as_deref().ok_or_else(|| {
            OcaError::new(ErrorCode::ServerUnavailable)
                .with_error("worktree dispatch was not prepared before session creation")
        })
    }
}

/// Finalizes one decoded and floor-valid turn.
///
/// The non-worktree branch patches state and returns before constructing a git
/// manager or running any git command.
pub(crate) fn finalize_turn(
    refs: &RefStore,
    reference: &str,
    state: WorkerState,
) -> Result<(), OcaError> {
    let record = refs
        .resolve(reference)
        .map_err(|error| state_error("could not read terminal ref", error))?
        .ok_or_else(|| OcaError::new(ErrorCode::UnknownRef).with_ref(reference))?;
    let ref_state = ref_state(state);
    let Some(worktree) = record.worktree.as_deref() else {
        return refs
            .patch(reference, RefPatch::default().with_last_state(ref_state))
            .map(|_| ())
            .map_err(|error| state_error("could not update terminal ref state", error));
    };

    let stored_summary = record.commit_subject.as_deref().ok_or_else(|| {
        OcaError::new(ErrorCode::ProtocolMismatch)
            .with_ref(reference)
            .with_error("worktree ref has no original-prompt commit summary")
    })?;
    let reference_id = RefId::new(reference).map_err(git_error)?;
    let manifest = [RelativePath::new(".").map_err(git_error)?];
    WorktreeManager::new()
        .validate(Path::new(worktree), &manifest)
        .map_err(git_error)?;
    let summary = TaskSummary::from_original_prompt(stored_summary);
    let committed = commit(Path::new(worktree), &reference_id, &summary).map_err(git_error)?;
    refs.patch(
        reference,
        RefPatch::default()
            .with_last_state(ref_state)
            .with_commit(committed.id()),
    )
    .map(|_| ())
    .map_err(|error| state_error("could not store committed turn", error))
}

pub(crate) const fn reply_state(reply: &oca_core::RoleReply) -> WorkerState {
    match reply {
        oca_core::RoleReply::Impl(reply) => reply.status,
        oca_core::RoleReply::Review(reply) => reply.status,
    }
}

const fn ref_state(state: WorkerState) -> RefState {
    match state {
        WorkerState::Done => RefState::Done,
        WorkerState::Blocked => RefState::Blocked,
        WorkerState::Partial => RefState::Partial,
    }
}

fn git_error(error: GitError) -> OcaError {
    let code = match error {
        GitError::WorktreeConflict { .. } => ErrorCode::WorktreeConflict,
        GitError::OutOfScope { .. } => ErrorCode::OutOfScope,
        GitError::ZeroByteOutput { .. } => ErrorCode::ZeroByteOutput,
        GitError::WorktreeEmpty => ErrorCode::WorktreeEmpty,
        GitError::InvalidRef { .. } | GitError::InvalidRelativePath { .. } => ErrorCode::Usage,
        GitError::WorktreeChanged { .. }
        | GitError::LockTimeout
        | GitError::Io(_)
        | GitError::GitCommand { .. } => ErrorCode::WorktreeDirty,
    };
    OcaError::new(code).with_error(error.to_string())
}

fn state_error(context: &str, error: impl std::fmt::Display) -> OcaError {
    OcaError::new(ErrorCode::ServerUnavailable).with_error(format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use std::{path::Path, process::Command};

    use oca_core::WorkerState;
    use oca_state::{NewRef, RefPatch, RefState, RefStore, RefStorePaths};

    use super::finalize_turn;

    #[test]
    fn non_worktree_turn_never_requires_a_git_repository() {
        let home = tempfile::tempdir().expect("temporary state root");
        let store = RefStore::with_paths(RefStorePaths::in_directory(home.path()));
        let allocation = store
            .allocate(NewRef::for_session("ses_non_git").with_control_metadata(
                "luna",
                "high",
                "impl",
                home.path().display().to_string(),
                RefState::Running,
            ))
            .expect("non-worktree ref allocation");
        let reference = allocation.record().id.clone();
        allocation
            .acknowledge_with(|_| Ok::<(), std::io::Error>(()))
            .expect("finish ref allocation");

        finalize_turn(&store, &reference, WorkerState::Done)
            .expect("non-worktree state finalization must not invoke git");

        assert_eq!(
            store.resolve(&reference).unwrap().unwrap().last_state,
            Some(RefState::Done)
        );
    }

    #[test]
    fn partial_and_review_turns_create_distinct_commits_with_one_subject_prefix() {
        let repository = tempfile::tempdir().expect("temporary repository");
        run_git(repository.path(), ["init", "--quiet"]);
        run_git(repository.path(), ["config", "user.name", "oca test"]);
        run_git(
            repository.path(),
            ["config", "user.email", "oca@example.test"],
        );
        std::fs::write(repository.path().join("README.md"), "base\n").unwrap();
        run_git(repository.path(), ["add", "README.md"]);
        run_git(repository.path(), ["commit", "--quiet", "-m", "base"]);

        let state = tempfile::tempdir().expect("temporary state root");
        let store = RefStore::with_paths(RefStorePaths::in_directory(state.path()));
        let allocation = store
            .allocate(
                NewRef::for_session("ses_worktree")
                    .with_control_metadata(
                        "luna",
                        "high",
                        "impl",
                        repository.path().display().to_string(),
                        RefState::Running,
                    )
                    .with_worktree_metadata(
                        repository.path().display().to_string(),
                        "oca/w4f2a1",
                        "implement stable review checkpoints",
                    ),
            )
            .unwrap();
        let reference = allocation.record().id.clone();
        allocation
            .acknowledge_with(|_| Ok::<(), std::io::Error>(()))
            .unwrap();

        std::fs::write(repository.path().join("first.txt"), "partial\n").unwrap();
        finalize_turn(&store, &reference, WorkerState::Partial).unwrap();
        let first = store.resolve(&reference).unwrap().unwrap().commit.unwrap();

        store
            .patch(
                &reference,
                RefPatch::default().with_last_state(RefState::Running),
            )
            .unwrap();
        std::fs::write(repository.path().join("second.txt"), "review\n").unwrap();
        finalize_turn(&store, &reference, WorkerState::Done).unwrap();
        let second = store.resolve(&reference).unwrap().unwrap().commit.unwrap();

        assert_ne!(first, second, "the review checkpoint must be a new commit");
        let subjects = git_output(repository.path(), ["log", "-2", "--format=%s"]);
        let expected = format!("oca {reference}: implement stable review checkpoints");
        assert_eq!(
            subjects.lines().collect::<Vec<_>>(),
            [expected.as_str(), expected.as_str()]
        );
        assert_eq!(
            git_output(repository.path(), ["rev-list", "--count", "HEAD"]).trim(),
            "3"
        );
    }

    fn run_git<const N: usize>(directory: &Path, arguments: [&str; N]) {
        let status = Command::new("git")
            .args(["-C", directory.to_str().unwrap()])
            .args(arguments)
            .status()
            .unwrap();
        assert!(status.success(), "git {arguments:?} failed");
    }

    fn git_output<const N: usize>(directory: &Path, arguments: [&str; N]) -> String {
        let output = Command::new("git")
            .args(["-C", directory.to_str().unwrap()])
            .args(arguments)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {arguments:?} failed");
        String::from_utf8(output.stdout).unwrap()
    }
}
