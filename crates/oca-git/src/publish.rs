//! Fixed-shape Git operations used by the config-gated publish commands.

use std::{
    ffi::OsStr,
    path::PathBuf,
    process::{Command, Output},
};

use crate::GitError;

/// A repository worktree from which oca may perform a non-forced publication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishRepository {
    worktree: PathBuf,
}

impl PublishRepository {
    #[must_use]
    pub fn new(worktree: impl Into<PathBuf>) -> Self {
        Self {
            worktree: worktree.into(),
        }
    }

    /// Confirms that the configured remote exists locally.
    ///
    /// # Errors
    ///
    /// Returns [`GitError::RemoteMissing`] only for Git's documented missing
    /// remote status. Process startup and all other Git failures remain typed
    /// Git errors.
    pub fn require_remote(&self, remote: &str) -> Result<(), GitError> {
        let output = self.output([
            OsStr::new("remote"),
            OsStr::new("get-url"),
            OsStr::new(remote),
        ])?;
        if output.status.success() {
            return Ok(());
        }
        if output.status.code() == Some(2) {
            return Err(GitError::RemoteMissing {
                remote: remote.to_owned(),
            });
        }
        Err(command_error("remote get-url", output))
    }

    /// Returns the branch selected by the remote's symbolic `HEAD`, when one exists.
    ///
    /// This consults the remote itself rather than trusting a possibly stale
    /// local remote-tracking symbolic ref.
    ///
    /// # Errors
    ///
    /// Returns a Git error if the remote cannot be queried or its output is not UTF-8.
    pub fn remote_default_branch(&self, remote: &str) -> Result<Option<String>, GitError> {
        let output = self.output([
            OsStr::new("ls-remote"),
            OsStr::new("--symref"),
            OsStr::new(remote),
            OsStr::new("HEAD"),
        ])?;
        if !output.status.success() {
            return Err(command_error("ls-remote --symref", output));
        }
        let stdout = utf8_output(output.stdout)?;
        Ok(stdout.lines().find_map(|line| {
            let target = line.strip_prefix("ref: refs/heads/")?.split('\t').next()?;
            (!target.is_empty()).then(|| target.to_owned())
        }))
    }

    /// Fails only when Git successfully reports at least one uncommitted change.
    ///
    /// # Errors
    ///
    /// A failed status command is returned as [`GitError::GitCommand`], never
    /// as [`GitError::DirtyWorktree`].
    pub fn require_clean(&self) -> Result<(), GitError> {
        let output = self.output([
            OsStr::new("status"),
            OsStr::new("--porcelain"),
            OsStr::new("--untracked-files=all"),
        ])?;
        if !output.status.success() {
            return Err(command_error("status --porcelain", output));
        }
        if output.stdout.is_empty() {
            Ok(())
        } else {
            Err(GitError::DirtyWorktree)
        }
    }

    /// Returns whether the exact branch exists on the configured remote.
    ///
    /// # Errors
    ///
    /// Git status 2 means absent; every other non-success status is a publish failure.
    pub fn remote_branch_exists(&self, remote: &str, branch: &str) -> Result<bool, GitError> {
        let reference = format!("refs/heads/{branch}");
        let output = self.output([
            OsStr::new("ls-remote"),
            OsStr::new("--exit-code"),
            OsStr::new("--heads"),
            OsStr::new(remote),
            OsStr::new(&reference),
        ])?;
        if output.status.success() {
            return Ok(true);
        }
        if output.status.code() == Some(2) {
            return Ok(false);
        }
        Err(command_error("ls-remote --heads", output))
    }

    /// Pushes one exact local branch to the same remote branch and establishes tracking.
    ///
    /// The fixed argument shape deliberately offers no force mode.
    ///
    /// # Errors
    ///
    /// Returns a Git error when the push cannot be started or does not succeed.
    pub fn push_branch(&self, remote: &str, branch: &str) -> Result<(), GitError> {
        let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
        let output = self.output([
            OsStr::new("push"),
            OsStr::new("--set-upstream"),
            OsStr::new(remote),
            OsStr::new(&refspec),
        ])?;
        if output.status.success() {
            Ok(())
        } else {
            Err(command_error("push", output))
        }
    }

    fn output<const N: usize>(&self, arguments: [&OsStr; N]) -> Result<Output, GitError> {
        Command::new("git")
            .args([OsStr::new("-C"), self.worktree.as_os_str()])
            .args(arguments)
            .output()
            .map_err(GitError::from)
    }
}

/// Matches the configured branch glob. `*` spans any characters and `?`
/// spans one character; all other characters are literal.
#[must_use]
pub fn branch_matches(pattern: &str, branch: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = branch.as_bytes();
    let (mut pattern_index, mut value_index) = (0, 0);
    let (mut star_index, mut star_value_index) = (None, 0);

    while value_index < value.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == b'?' || pattern[pattern_index] == value[value_index])
        {
            pattern_index += 1;
            value_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star_index = Some(pattern_index);
            pattern_index += 1;
            star_value_index = value_index;
        } else if let Some(star) = star_index {
            pattern_index = star + 1;
            star_value_index += 1;
            value_index = star_value_index;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

/// Applies the structural protected-branch rule independently of any glob.
#[must_use]
pub fn is_protected_branch(branch: &str, remote_default: Option<&str>) -> bool {
    matches!(branch, "main" | "master" | "HEAD") || remote_default == Some(branch)
}

fn command_error(operation: &'static str, output: Output) -> GitError {
    GitError::GitCommand {
        operation,
        status: output.status,
    }
}

fn utf8_output(output: Vec<u8>) -> Result<String, GitError> {
    String::from_utf8(output)
        .map_err(|error| GitError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error)))
}

#[cfg(test)]
mod tests {
    use super::{branch_matches, is_protected_branch};

    #[test]
    fn branch_glob_supports_literal_star_and_single_character_segments() {
        assert!(branch_matches("oca/*", "oca/w4f2a1"));
        assert!(branch_matches("release/?.*", "release/v.1"));
        assert!(!branch_matches("oca/*", "feature/w4f2a1"));
        assert!(!branch_matches("release/?.*", "release/version.1"));
    }

    #[test]
    fn structural_protection_is_independent_of_the_glob() {
        for branch in ["main", "master", "HEAD", "trunk"] {
            let remote_default = (branch == "trunk").then_some("trunk");
            assert!(branch_matches("*", branch));
            assert!(is_protected_branch(branch, remote_default));
        }
        assert!(!is_protected_branch("oca/w4f2a1", Some("trunk")));
    }

    #[test]
    fn production_publish_source_contains_no_force_capability() {
        let production = include_str!("publish.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production source prefix");
        let long_option = ["--", "force"].concat();
        let short_option = ["-", "f"].concat();
        let forced_ref_prefix = ["+", "refs"].concat();

        assert!(!production.contains(&long_option));
        assert!(!production.contains(&forced_ref_prefix));
        for invocation in production.split("OsStr::new(\"push\")").skip(1) {
            let arguments = invocation.split("])").next().unwrap_or(invocation);
            assert!(!arguments.contains(&short_option));
        }
    }
}
