//! Deterministic, tool-owned commit messages and commit execution.

use std::{
    ffi::OsStr,
    path::Path,
    process::{Command, Output},
};

use crate::{GitError, RefId};

const SUMMARY_LIMIT: usize = 60;

/// The normalized summary derived from an initial dispatch prompt.
///
/// The inner value is intentionally private. Git commit execution accepts this
/// type instead of an arbitrary string so terminal worker output cannot become
/// a commit message by accident.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSummary(String);

impl TaskSummary {
    /// Derives the stable task summary from the first meaningful prompt line.
    #[must_use]
    pub fn from_original_prompt(prompt: &str) -> Self {
        let meaningful_line = prompt
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("");
        let collapsed = meaningful_line
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let stripped = collapsed.trim_end_matches(is_trailing_punctuation);
        let source = if stripped.is_empty() {
            "task"
        } else {
            stripped
        };
        let truncated = truncate_on_word_boundary(source, SUMMARY_LIMIT);
        let normalized = truncated.trim_end_matches(is_trailing_punctuation);
        Self(if normalized.is_empty() {
            "task".to_owned()
        } else {
            normalized.to_owned()
        })
    }

    /// Returns the persisted normalized summary.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The commit created at one validated worktree turn boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitRecord {
    id: String,
    subject: String,
}

impl CommitRecord {
    /// Returns the full commit object ID.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the deterministic commit subject.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }
}

/// Creates one new commit without amend or worker-controlled message input.
///
/// # Errors
///
/// Returns a git or process error when commit creation or object-id lookup
/// fails. The staged diff is left intact when commit creation fails.
pub fn commit(
    worktree: &Path,
    reference: &RefId,
    summary: &TaskSummary,
) -> Result<CommitRecord, GitError> {
    let subject = format!("oca {reference}: {}", summary.as_str());
    let status = Command::new("git")
        .args([
            OsStr::new("-C"),
            worktree.as_os_str(),
            OsStr::new("commit"),
            OsStr::new("--quiet"),
            OsStr::new("-m"),
            OsStr::new(&subject),
        ])
        .status()
        .map_err(GitError::from)?;
    if !status.success() {
        return Err(GitError::GitCommand {
            operation: "commit",
            status,
        });
    }

    let output = git_output(
        worktree,
        [
            OsStr::new("rev-parse"),
            OsStr::new("--verify"),
            OsStr::new("HEAD"),
        ],
    )?;
    if !output.status.success() {
        return Err(GitError::GitCommand {
            operation: "rev-parse HEAD",
            status: output.status,
        });
    }
    let id = String::from_utf8(output.stdout)
        .map_err(|error| GitError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error)))?
        .trim()
        .to_owned();

    Ok(CommitRecord { id, subject })
}

fn git_output<const N: usize>(base: &Path, arguments: [&OsStr; N]) -> Result<Output, GitError> {
    Command::new("git")
        .args([OsStr::new("-C"), base.as_os_str()])
        .args(arguments)
        .output()
        .map_err(GitError::from)
}

fn is_trailing_punctuation(character: char) -> bool {
    character.is_ascii_punctuation() || matches!(character, '…' | '‘' | '’' | '“' | '”' | '–' | '—')
}

fn truncate_on_word_boundary(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_owned();
    }

    let mut summary = String::new();
    for word in value.split_whitespace() {
        let separator = usize::from(!summary.is_empty());
        if summary.chars().count() + separator + word.chars().count() > limit {
            break;
        }
        if !summary.is_empty() {
            summary.push(' ');
        }
        summary.push_str(word);
    }

    if summary.is_empty() {
        value.chars().take(limit).collect()
    } else {
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::TaskSummary;

    #[test]
    fn summary_uses_first_meaningful_line_and_normalizes_it() {
        let summary = TaskSummary::from_original_prompt(
            "\n  add   retry\thandling!!!  \nworker-authored later line",
        );

        assert_eq!(summary.as_str(), "add retry handling");
    }

    #[test]
    fn summary_truncates_to_sixty_characters_at_a_word_boundary() {
        let summary = TaskSummary::from_original_prompt(
            "implement deterministic worktree commit handling with review checkpoints and tests",
        );

        assert!(summary.as_str().chars().count() <= 60);
        assert_eq!(
            summary.as_str(),
            "implement deterministic worktree commit handling with review"
        );
    }

    #[test]
    fn truncation_does_not_reintroduce_trailing_punctuation() {
        let summary = TaskSummary::from_original_prompt(
            "one two three four five six seven eight nine ten eleven, followed by excluded words",
        );

        assert!(!summary.as_str().ends_with(','));
        assert!(summary.as_str().chars().count() <= 60);
    }

    #[test]
    fn punctuation_only_prompt_has_a_stable_tool_owned_fallback() {
        assert_eq!(TaskSummary::from_original_prompt("...?!").as_str(), "task");
    }

    #[test]
    fn commit_message_boundary_accepts_a_typed_summary_not_worker_reply_text() {
        let production = include_str!("commit.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();

        assert!(production.contains("summary: &TaskSummary"));
        assert!(production.contains("OsStr::new(&subject)"));
        assert!(!production.contains("RoleReply"));
        assert!(!production.contains("ImplReply"));
        assert!(!production.contains("ReviewReply"));
    }
}
