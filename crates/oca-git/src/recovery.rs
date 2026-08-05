//! Recovery-only cleanup for pre-session oca-owned worktrees.

use std::{ffi::OsStr, path::Path};

use crate::{
    DEFAULT_LOCK_ACQUISITION_TIMEOUT, GitError, RefId, RepositoryLock, branch_exists, git_status,
};

/// Removes the deterministic worktree and branch owned by a stranded ref.
///
/// This is intentionally narrower than a general worktree delete API: callers
/// cannot provide either target path or branch name.
pub fn cleanup_orphaned_worktree(base: &Path, reference: &RefId) -> Result<(), GitError> {
    let _lock = RepositoryLock::acquire(base, DEFAULT_LOCK_ACQUISITION_TIMEOUT, None)?;
    let branch = format!("oca/{reference}");
    let path = base.join(".oca").join("wt").join(reference.as_str());
    if path.exists() {
        let status = git_status(
            base,
            [
                OsStr::new("worktree"),
                OsStr::new("remove"),
                OsStr::new("-f"),
                path.as_os_str(),
            ],
        )?;
        if !status.success() {
            return Err(GitError::GitCommand {
                operation: "worktree remove",
                status,
            });
        }
    }
    if branch_exists(base, &branch)? {
        let status = git_status(
            base,
            [
                OsStr::new("branch"),
                OsStr::new("-D"),
                OsStr::new("--quiet"),
                OsStr::new(&branch),
            ],
        )?;
        if !status.success() {
            return Err(GitError::GitCommand {
                operation: "branch delete",
                status,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, process::Command};

    use super::*;
    use crate::WorktreeManager;

    #[test]
    fn cleanup_targets_only_the_ref_owned_branch_and_worktree() {
        let repository = tempfile::tempdir().unwrap();
        run_git(repository.path(), ["init", "--quiet"]);
        run_git(repository.path(), ["config", "user.name", "oca test"]);
        run_git(
            repository.path(),
            ["config", "user.email", "oca@example.test"],
        );
        fs::write(repository.path().join("README.md"), "base\n").unwrap();
        run_git(repository.path(), ["add", "README.md"]);
        run_git(repository.path(), ["commit", "--quiet", "-m", "base"]);
        let reference = RefId::new("w4f2a1").unwrap();
        let worktree = WorktreeManager::new()
            .create(&reference, repository.path())
            .unwrap();
        assert!(worktree.path().exists());

        cleanup_orphaned_worktree(repository.path(), &reference).unwrap();

        assert!(!worktree.path().exists());
        let branches = Command::new("git")
            .args([
                "-C",
                repository.path().to_str().unwrap(),
                "branch",
                "--list",
            ])
            .output()
            .unwrap();
        assert!(!String::from_utf8_lossy(&branches.stdout).contains("oca/w4f2a1"));
    }

    fn run_git<const N: usize>(directory: &Path, arguments: [&str; N]) {
        assert!(
            Command::new("git")
                .args(["-C", directory.to_str().unwrap()])
                .args(arguments)
                .status()
                .unwrap()
                .success()
        );
    }
}
