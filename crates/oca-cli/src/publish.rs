//! Config-gated execution for `oca push` and `oca pr`.

use std::{fs, path::Path};

use oca_core::{EffortInput, ErrorCode, OcaError, resolve_model};
use oca_display::Acknowledgement;
use oca_git::{GitError, PublishRepository, branch_matches, is_protected_branch};
use oca_state::{OcaConfig, RefRecord, RefStore, RefStorePaths};

use crate::{
    RefCommand,
    pull_request::{GitHubProvider, ProviderError, PullRequestDraft, PullRequestProvider},
};

const PUSH_HELP: &str = "[publish] push = true";
const PR_HELP: &str = "[publish] pr = true";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PublishKind {
    Push,
    PullRequest,
}

/// Successful ref-scoped publication output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishCommandOutput {
    pub stdout: String,
}

pub fn execute_push(
    command: &RefCommand,
    home: impl AsRef<Path>,
) -> Result<PublishCommandOutput, OcaError> {
    execute(
        command,
        home.as_ref(),
        PublishKind::Push,
        &mut GitHubProvider,
    )
}

pub fn execute_pull_request(
    command: &RefCommand,
    home: impl AsRef<Path>,
) -> Result<PublishCommandOutput, OcaError> {
    execute(
        command,
        home.as_ref(),
        PublishKind::PullRequest,
        &mut GitHubProvider,
    )
}

fn execute(
    command: &RefCommand,
    home: &Path,
    kind: PublishKind,
    provider: &mut dyn PullRequestProvider,
) -> Result<PublishCommandOutput, OcaError> {
    let store = RefStore::with_paths(RefStorePaths::in_directory(home.join(".oca")));
    let record = store
        .resolve(&command.reference)
        .map_err(|error| state_error(&command.reference, error))?
        .ok_or_else(|| OcaError::new(ErrorCode::UnknownRef).with_ref(&command.reference))?;
    if record.tombstoned {
        return Err(OcaError::new(ErrorCode::SessionNotFound).with_ref(&command.reference));
    }

    // Publish grant evaluation step 1: resolve and canonicalize the ref's repository root.
    let repo_root = required(&record, record.repo.as_deref(), "repository root")?;
    let repo_root = fs::canonicalize(repo_root).map_err(|error| {
        publish_error(
            &command.reference,
            format!("could not resolve repository root: {error}"),
        )
    })?;

    // Step 2: select the matching override or the default publication block.
    let config = OcaConfig::load_from_home(home).map_err(|error| {
        OcaError::new(ErrorCode::Usage)
            .with_error(format!("failed to load configuration: {error}"))
            .with_help("fix ~/.oca/config.toml and retry")
    })?;
    let settings = config.effective_publish(&repo_root);

    // Step 3: enforce the verb-specific booleans before inspecting branches or remotes.
    if !settings.push {
        return Err(OcaError::publish_disabled(PUSH_HELP).with_ref(&command.reference));
    }
    if kind == PublishKind::PullRequest && !settings.pr {
        return Err(OcaError::publish_disabled(PR_HELP).with_ref(&command.reference));
    }

    let worktree = required(&record, record.worktree.as_deref(), "worktree")?;
    let branch = required(&record, record.branch.as_deref(), "branch")?;
    let repository = PublishRepository::new(worktree);
    let alias = required(&record, record.alias.as_deref(), "model alias")?;
    let effort = required(&record, record.effort.as_deref(), "model effort")?;
    let model = resolve_model(alias, EffortInput::one(effort), config.model_catalog())
        .map_err(|error| publish_error(&command.reference, error.to_string()))?;

    // Step 4: apply the configured branch glob.
    if !branch_matches(&settings.branches, branch) {
        return Err(forbidden_branch(&command.reference, branch));
    }

    // Step 5: literal protected names are rejected without needing a working remote.
    if is_protected_branch(branch, None) {
        return Err(forbidden_branch(&command.reference, branch));
    }

    // The remote value was selected only from config (step 7); it is never CLI input.
    repository
        .require_remote(&settings.remote)
        .map_err(|error| git_error(&command.reference, error))?;
    let remote_default = repository
        .remote_default_branch(&settings.remote)
        .map_err(|error| git_error(&command.reference, error))?;
    if is_protected_branch(branch, remote_default.as_deref()) {
        return Err(forbidden_branch(&command.reference, branch));
    }

    repository
        .require_clean()
        .map_err(|error| git_error(&command.reference, error))?;

    match kind {
        PublishKind::Push => repository
            .push_branch(&settings.remote, branch)
            .map_err(|error| git_error(&command.reference, error))?,
        PublishKind::PullRequest => {
            if !repository
                .remote_branch_exists(&settings.remote, branch)
                .map_err(|error| git_error(&command.reference, error))?
            {
                repository
                    .push_branch(&settings.remote, branch)
                    .map_err(|error| git_error(&command.reference, error))?;
            }
            open_pull_request(&record, worktree, branch, &settings.base, provider)?;
        }
    }

    let state = match kind {
        PublishKind::Push => "pushed",
        PublishKind::PullRequest => "pr_opened",
    };
    let acknowledgement = Acknowledgement::from_resolved(&command.reference, state, &model);
    Ok(PublishCommandOutput {
        stdout: if command.json {
            acknowledgement.render_json()
        } else {
            acknowledgement.render_toon()
        },
    })
}

fn open_pull_request(
    record: &RefRecord,
    worktree: &str,
    branch: &str,
    base: &str,
    provider: &mut dyn PullRequestProvider,
) -> Result<(), OcaError> {
    if provider
        .has_pull_request(Path::new(worktree), branch)
        .map_err(|error| provider_error(&record.id, error))?
    {
        return Ok(());
    }
    let summary = required(record, record.commit_subject.as_deref(), "task summary")?;
    let commit = required(record, record.commit.as_deref(), "commit")?;
    let completion = record.completion.as_ref().ok_or_else(|| {
        publish_error(
            &record.id,
            "the ref has no validated completion record for a pull request",
        )
    })?;
    let draft = PullRequestDraft::from_completion(&record.id, summary, completion, branch, commit);
    provider
        .create_pull_request(Path::new(worktree), branch, base, &draft)
        .map_err(|error| provider_error(&record.id, error))
}

fn required<'a>(
    record: &RefRecord,
    value: Option<&'a str>,
    field: &str,
) -> Result<&'a str, OcaError> {
    value.ok_or_else(|| publish_error(&record.id, format!("the ref has no stored {field}")))
}

fn forbidden_branch(reference: &str, branch: &str) -> OcaError {
    OcaError::new(ErrorCode::ProtectedBranch)
        .with_ref(reference)
        .with_error(format!(
            "Branch `{branch}` is not permitted for publication"
        ))
}

fn git_error(reference: &str, error: GitError) -> OcaError {
    let code = match error {
        GitError::DirtyWorktree => ErrorCode::WorktreeDirty,
        GitError::RemoteMissing { .. } => ErrorCode::PublishRemoteMissing,
        _ => ErrorCode::PublishFailed,
    };
    OcaError::new(code)
        .with_ref(reference)
        .with_error(error.to_string())
}

fn provider_error(reference: &str, error: ProviderError) -> OcaError {
    publish_error(reference, error.to_string())
}

fn publish_error(reference: &str, error: impl Into<String>) -> OcaError {
    OcaError::new(ErrorCode::PublishFailed)
        .with_ref(reference)
        .with_error(error)
}

fn state_error(reference: &str, error: impl std::fmt::Display) -> OcaError {
    publish_error(reference, format!("could not read ref state: {error}"))
}

#[cfg(test)]
mod tests {
    use std::{io, path::Path, process::Command};

    use oca_core::{ErrorCode, ImplReply, RoleReply, WorkerState};
    use oca_git::GitError;
    use oca_state::{RefRecord, RefStore, RefStorePaths};

    use crate::{
        RefCommand,
        pull_request::{ProviderError, PullRequestDraft, PullRequestProvider},
    };

    use super::{PUSH_HELP, PublishKind, execute, git_error};

    struct TestRepository {
        worktree: tempfile::TempDir,
        remote: tempfile::TempDir,
    }

    #[derive(Default)]
    struct RecordingProvider {
        existing: bool,
        create_calls: Vec<PullRequestDraft>,
    }

    impl PullRequestProvider for RecordingProvider {
        fn has_pull_request(
            &mut self,
            _worktree: &Path,
            _branch: &str,
        ) -> Result<bool, ProviderError> {
            Ok(self.existing)
        }

        fn create_pull_request(
            &mut self,
            _worktree: &Path,
            _branch: &str,
            _base: &str,
            draft: &PullRequestDraft,
        ) -> Result<(), ProviderError> {
            self.create_calls.push(draft.clone());
            self.existing = true;
            Ok(())
        }
    }

    impl TestRepository {
        fn new(branch: &str) -> Self {
            let worktree = tempfile::tempdir().expect("worktree");
            let remote = tempfile::tempdir().expect("bare remote");
            run_git(worktree.path(), ["init", "--quiet"]);
            run_git(worktree.path(), ["config", "user.name", "oca test"]);
            run_git(
                worktree.path(),
                ["config", "user.email", "oca@example.test"],
            );
            std::fs::write(worktree.path().join("README.md"), "base\n").unwrap();
            run_git(worktree.path(), ["add", "README.md"]);
            run_git(worktree.path(), ["commit", "--quiet", "-m", "base"]);
            run_git(worktree.path(), ["branch", "-M", branch]);
            run_git(remote.path(), ["init", "--bare", "--quiet"]);
            run_git(
                worktree.path(),
                ["remote", "add", "origin", remote.path().to_str().unwrap()],
            );
            Self { worktree, remote }
        }
    }

    #[test]
    fn only_confirmed_dirt_maps_to_worktree_dirty() {
        assert_eq!(
            git_error("w00001", GitError::DirtyWorktree).code(),
            ErrorCode::WorktreeDirty.as_str()
        );
        assert_eq!(
            git_error(
                "w00001",
                GitError::Io(io::Error::new(io::ErrorKind::NotFound, "git missing")),
            )
            .code(),
            ErrorCode::PublishFailed.as_str()
        );
    }

    #[test]
    fn disabled_push_help_is_the_exact_config_line() {
        let error = oca_core::OcaError::publish_disabled(PUSH_HELP);
        assert_eq!(error.help(), "[publish] push = true");
    }

    #[test]
    fn push_defaults_off_before_branch_or_remote_inspection() {
        let repository = TestRepository::new("oca/w00001");
        let home = tempfile::tempdir().unwrap();
        insert_ref(home.path(), &repository, "oca/w00001");
        let error = execute(
            &command(),
            home.path(),
            PublishKind::Push,
            &mut RecordingProvider::default(),
        )
        .unwrap_err();

        assert_eq!(error.code(), ErrorCode::PublishDisabled.as_str());
        assert_eq!(error.help(), "[publish] push = true");
    }

    #[test]
    fn a_disabled_grant_refuses_before_the_glob_protected_and_remote_steps() {
        let repository = TestRepository::new("main");
        run_git(repository.worktree.path(), ["remote", "remove", "origin"]);
        let home = tempfile::tempdir().unwrap();
        insert_ref(home.path(), &repository, "main");
        write_config(
            home.path(),
            "[publish]\npush = false\nbranches = \"nothing/*\"\nremote = \"missing\"\n",
        );

        let error = execute(
            &command(),
            home.path(),
            PublishKind::Push,
            &mut RecordingProvider::default(),
        )
        .unwrap_err();

        // Every later step would also refuse this ref, so only the spec's
        // step-3-first ordering can produce `publish_disabled` here.
        assert_eq!(error.code(), ErrorCode::PublishDisabled.as_str());
        assert_eq!(error.help(), "[publish] push = true");
    }

    #[test]
    fn canonical_repository_override_wins_over_disabled_default() {
        let repository = TestRepository::new("oca/w00001");
        let home = tempfile::tempdir().unwrap();
        insert_ref(home.path(), &repository, "oca/w00001");
        write_config(
            home.path(),
            &format!(
                "[publish]\npush = false\npr = false\nbranches = \"oca/*\"\nremote = \"origin\"\n\n[publish.overrides.\"{}\"]\npush = true\n",
                repository.worktree.path().canonicalize().unwrap().display()
            ),
        );

        let output = execute(
            &command(),
            home.path(),
            PublishKind::Push,
            &mut RecordingProvider::default(),
        )
        .unwrap();

        assert!(output.stdout.contains(" pushed "));
        assert!(remote_oid(&repository, "oca/w00001").is_some());
    }

    #[test]
    fn actual_remote_default_is_protected_even_when_glob_matches() {
        let repository = TestRepository::new("trunk");
        run_git(repository.worktree.path(), ["push", "origin", "trunk"]);
        run_git(
            repository.remote.path(),
            ["symbolic-ref", "HEAD", "refs/heads/trunk"],
        );
        let home = tempfile::tempdir().unwrap();
        insert_ref(home.path(), &repository, "trunk");
        write_config(
            home.path(),
            "[publish]\npush = true\nbranches = \"*\"\nremote = \"origin\"\n",
        );

        let error = execute(
            &command(),
            home.path(),
            PublishKind::Push,
            &mut RecordingProvider::default(),
        )
        .unwrap_err();

        assert_eq!(error.code(), ErrorCode::ProtectedBranch.as_str());
    }

    #[test]
    fn literal_protected_branches_do_not_depend_on_remote_discovery() {
        for branch in ["main", "master", "HEAD"] {
            let local_branch = if branch == "HEAD" {
                "oca/w00001"
            } else {
                branch
            };
            let repository = TestRepository::new(local_branch);
            run_git(repository.worktree.path(), ["remote", "remove", "origin"]);
            let home = tempfile::tempdir().unwrap();
            insert_ref(home.path(), &repository, branch);
            write_config(
                home.path(),
                "[publish]\npush = true\nbranches = \"*\"\nremote = \"missing\"\n",
            );

            let error = execute(
                &command(),
                home.path(),
                PublishKind::Push,
                &mut RecordingProvider::default(),
            )
            .unwrap_err();
            assert_eq!(error.code(), ErrorCode::ProtectedBranch.as_str());
        }
    }

    #[test]
    fn pull_request_requires_its_own_grant_in_addition_to_push() {
        let repository = TestRepository::new("oca/w00001");
        let home = tempfile::tempdir().unwrap();
        insert_ref(home.path(), &repository, "oca/w00001");
        write_config(
            home.path(),
            "[publish]\npush = true\npr = false\nbranches = \"oca/*\"\nremote = \"origin\"\n",
        );

        let error = execute(
            &command(),
            home.path(),
            PublishKind::PullRequest,
            &mut RecordingProvider::default(),
        )
        .unwrap_err();

        assert_eq!(error.code(), ErrorCode::PublishDisabled.as_str());
        assert_eq!(error.help(), "[publish] pr = true");
        assert!(remote_oid(&repository, "oca/w00001").is_none());
    }

    #[test]
    fn pull_request_pushes_only_an_absent_remote_branch_and_is_idempotent() {
        let repository = TestRepository::new("oca/w00001");
        let home = tempfile::tempdir().unwrap();
        insert_ref(home.path(), &repository, "oca/w00001");
        write_config(
            home.path(),
            "[publish]\npush = true\npr = true\nbranches = \"oca/*\"\nremote = \"origin\"\n",
        );
        let mut provider = RecordingProvider::default();

        execute(
            &command(),
            home.path(),
            PublishKind::PullRequest,
            &mut provider,
        )
        .unwrap();
        let first_remote_oid = remote_oid(&repository, "oca/w00001").unwrap();
        assert_eq!(provider.create_calls.len(), 1);

        std::fs::write(repository.worktree.path().join("later.txt"), "later\n").unwrap();
        run_git(repository.worktree.path(), ["add", "later.txt"]);
        run_git(
            repository.worktree.path(),
            ["commit", "--quiet", "-m", "later local commit"],
        );
        execute(
            &command(),
            home.path(),
            PublishKind::PullRequest,
            &mut provider,
        )
        .unwrap();

        assert_eq!(provider.create_calls.len(), 1);
        assert_eq!(
            remote_oid(&repository, "oca/w00001").as_deref(),
            Some(first_remote_oid.as_str()),
            "an existing remote branch is not pushed again"
        );
    }

    fn command() -> RefCommand {
        RefCommand {
            reference: "w00001".to_owned(),
            json: false,
        }
    }

    fn insert_ref(home: &Path, repository: &TestRepository, branch: &str) {
        let commit = git_output(repository.worktree.path(), ["rev-parse", "HEAD"]);
        RefStore::with_paths(RefStorePaths::in_directory(home.join(".oca")))
            .insert(RefRecord {
                id: "w00001".to_owned(),
                session_id: "ses_publish".to_owned(),
                message_id: Some("msg_publish".to_owned()),
                alias: Some("luna".to_owned()),
                effort: Some("high".to_owned()),
                role: Some("impl".to_owned()),
                cwd: Some(repository.worktree.path().display().to_string()),
                last_state: Some(oca_state::RefState::Done),
                repo: Some(repository.worktree.path().display().to_string()),
                spawner_tag: Some("tests".to_owned()),
                worktree: Some(repository.worktree.path().display().to_string()),
                branch: Some(branch.to_owned()),
                commit: Some(commit),
                commit_subject: Some("implement publish gate".to_owned()),
                display: None,
                herdr_tab: None,
                completion: Some(completion()),
                tombstoned: false,
            })
            .unwrap();
    }

    fn completion() -> RoleReply {
        ImplReply {
            status: WorkerState::Done,
            files: vec!["README.md".to_owned()],
            note: "Implemented the requested publication gate with deterministic provider input. Verified each protected branch and remote publication boundary.".to_owned(),
        }
        .into()
    }

    fn write_config(home: &Path, contents: &str) {
        std::fs::write(home.join(".oca/config.toml"), contents).unwrap();
    }

    fn remote_oid(repository: &TestRepository, branch: &str) -> Option<String> {
        let output = Command::new("git")
            .args([
                "-C",
                repository.remote.path().to_str().unwrap(),
                "rev-parse",
                "--verify",
                &format!("refs/heads/{branch}"),
            ])
            .output()
            .unwrap();
        output
            .status
            .success()
            .then(|| String::from_utf8(output.stdout).unwrap().trim().to_owned())
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
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
}
