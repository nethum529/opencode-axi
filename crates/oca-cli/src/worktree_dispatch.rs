//! Worktree-only dispatch preparation and terminal commit finalization.

use std::path::{Path, PathBuf};

use oca_core::{ErrorCode, ForegroundRequest, OcaError, RoleReply, WorkerPolicy, WorkerState};
use oca_git::{
    GitError, RefId, RelativePath, TaskSummary, WorktreeManager, cleanup_orphaned_worktree, commit,
};
use oca_state::{Intent, IntentPhase, IntentStore, RefPatch, RefState, RefStore};

use crate::crash_recovery::{persist_intent, remove_intent};

/// State held between pre-session worktree creation and ref acknowledgement.
pub(crate) struct WorktreeDispatch {
    repo_root: PathBuf,
    summary: TaskSummary,
}

impl WorktreeDispatch {
    pub(crate) fn new(repo_root: PathBuf, original_prompt: &str) -> Self {
        Self {
            repo_root,
            summary: TaskSummary::from_original_prompt(original_prompt),
        }
    }

    /// Reserves the ref and creates its worktree before session creation.
    pub(crate) fn prepare(
        &mut self,
        refs: &RefStore,
        request: &mut ForegroundRequest,
        reference: &str,
        intents: &IntentStore,
        intent: &mut Intent,
    ) -> Result<(), OcaError> {
        let reference_id = RefId::new(reference).map_err(git_error)?;
        let worktree = match WorktreeManager::new().create(&reference_id, &self.repo_root) {
            Ok(worktree) => worktree,
            Err(error) => {
                let _ = refs.tombstone(reference);
                let _ = remove_intent(intents, reference);
                return Err(git_error(error));
            }
        };
        let worktree_path = worktree.path().to_path_buf();
        intent.set_phase(IntentPhase::WorktreeReady);
        if let Err(error) = persist_intent(intents, intent) {
            let _ = cleanup_orphaned_worktree(&self.repo_root, &reference_id);
            let _ = refs.tombstone(reference);
            return Err(error);
        }
        refs.patch(
            reference,
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
        Ok(())
    }

    pub(crate) fn record_session(
        &self,
        refs: &RefStore,
        reference: &str,
        session_id: &str,
    ) -> Result<(), OcaError> {
        refs.patch(reference, RefPatch::default().with_session_id(session_id))
            .map(|_| ())
            .map_err(|error| state_error("could not store worktree session", error))
    }

    pub(crate) fn cleanup(&self, reference: &str) -> Result<(), OcaError> {
        let reference = RefId::new(reference).map_err(git_error)?;
        cleanup_orphaned_worktree(&self.repo_root, &reference).map_err(git_error)
    }

    pub(crate) fn finish_ref(
        &self,
        refs: &RefStore,
        reference: &str,
        session_id: &str,
        message_id: &str,
    ) -> Result<String, OcaError> {
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
}

/// Finalizes one decoded and floor-valid turn.
///
/// The non-worktree branch patches state and returns before constructing a git
/// manager or running any git command.
pub(crate) fn finalize_turn(
    refs: &RefStore,
    reference: &str,
    reply: &RoleReply,
) -> Result<(), OcaError> {
    let state_directory = refs
        .paths()
        .refs_file
        .parent()
        .ok_or_else(|| {
            OcaError::new(ErrorCode::ServerUnavailable)
                .with_error("refs file has no state directory")
        })?
        .to_path_buf();
    let intents = IntentStore::in_directory(state_directory);
    let mut intent = intents
        .read(reference)
        .map_err(|error| state_error("could not read terminal intent", error))?;
    if let Some(intent) = intent.as_mut() {
        if intent.phase < IntentPhase::TerminalObserved {
            intent.set_phase(IntentPhase::TerminalObserved);
        }
        intent.terminal_reply = Some(reply.clone());
    }
    let record = refs
        .resolve(reference)
        .map_err(|error| state_error("could not read terminal ref", error))?
        .ok_or_else(|| OcaError::new(ErrorCode::UnknownRef).with_ref(reference))?;
    let ref_state = ref_state(reply_state(reply));
    let Some(worktree) = record.worktree.as_deref() else {
        if let Some(intent) = intent.as_mut() {
            intent.set_phase(IntentPhase::Validated);
            persist_intent(&intents, intent)?;
        }
        refs.patch(
            reference,
            RefPatch::default()
                .with_last_state(ref_state)
                .with_completion(reply.clone()),
        )
        .map_err(|error| state_error("could not update terminal ref state", error))?;
        remove_intent(&intents, reference)?;
        return Ok(());
    };

    let stored_summary = record.commit_subject.as_deref().ok_or_else(|| {
        OcaError::new(ErrorCode::ProtocolMismatch)
            .with_ref(reference)
            .with_error("worktree ref has no original-prompt commit summary")
    })?;
    let reference_id = RefId::new(reference).map_err(git_error)?;
    let manifest = [RelativePath::new(".").map_err(git_error)?];
    let changes = WorktreeManager::new()
        .validate(Path::new(worktree), &manifest)
        .map_err(git_error)?;
    if let Some(intent) = intent.as_mut() {
        intent.changed_paths = changes
            .paths()
            .iter()
            .map(|path| path.as_str().to_owned())
            .collect();
        intent.checks = vec![
            "manifest_scope".to_owned(),
            "nonzero_files".to_owned(),
            "stable_diff".to_owned(),
        ];
        intent.set_phase(IntentPhase::Validated);
        persist_intent(&intents, intent)?;
    }
    let summary = TaskSummary::from_original_prompt(stored_summary);
    let committed = commit(Path::new(worktree), &reference_id, &summary).map_err(git_error)?;
    if let Some(intent) = intent.as_mut() {
        intent.commit_id = Some(committed.id().to_owned());
        intent.set_phase(IntentPhase::Committed);
        persist_intent(&intents, intent)?;
    }
    refs.patch(
        reference,
        RefPatch::default()
            .with_last_state(ref_state)
            .with_commit(committed.id())
            .with_completion(reply.clone()),
    )
    .map_err(|error| state_error("could not store committed turn", error))?;
    remove_intent(&intents, reference)
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
        | GitError::DirtyWorktree
        | GitError::LockTimeout
        | GitError::Io(_)
        | GitError::GitCommand { .. } => ErrorCode::WorktreeDirty,
        GitError::RemoteMissing { .. } => ErrorCode::ServerUnavailable,
    };
    OcaError::new(code).with_error(error.to_string())
}

fn state_error(context: &str, error: impl std::fmt::Display) -> OcaError {
    OcaError::new(ErrorCode::ServerUnavailable).with_error(format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use std::{path::Path, process::Command};

    use oca_core::{ImplReply, RoleReply, WorkerState};
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

        finalize_turn(&store, &reference, &reply(WorkerState::Done))
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
        finalize_turn(&store, &reference, &reply(WorkerState::Partial)).unwrap();
        let first = store.resolve(&reference).unwrap().unwrap().commit.unwrap();

        store
            .patch(
                &reference,
                RefPatch::default().with_last_state(RefState::Running),
            )
            .unwrap();
        std::fs::write(repository.path().join("second.txt"), "review\n").unwrap();
        finalize_turn(&store, &reference, &reply(WorkerState::Done)).unwrap();
        let second = store.resolve(&reference).unwrap().unwrap().commit.unwrap();
        assert!(
            store
                .resolve(&reference)
                .unwrap()
                .unwrap()
                .completion
                .is_some(),
            "the validated completion record must be available to publication"
        );

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

    fn reply(status: WorkerState) -> RoleReply {
        ImplReply {
            status,
            files: Vec::new(),
            note: "A deterministic completion note is persisted for later publication. The test verifies commit finalization remains coupled to that validated record.".to_owned(),
        }
        .into()
    }
}
