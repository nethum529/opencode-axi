//! Deterministic GitHub pull-request adapter for published completion records.

use std::{
    fmt,
    path::Path,
    process::{Command, Output},
};

use oca_core::RoleReply;

/// Provider-ready pull-request content derived without model-written shell.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PullRequestDraft {
    pub(crate) title: String,
    pub(crate) body: String,
}

impl PullRequestDraft {
    pub(crate) fn from_completion(
        reference: &str,
        task_summary: &str,
        completion: &RoleReply,
        branch: &str,
        commit: &str,
    ) -> Self {
        let title = format!("oca {reference}: {task_summary}");
        let (note, files, findings) = completion_parts(completion);
        let mut body = format!(
            "## Completion\n\n{note}\n\n## Changed files\n\n{}\n\n## Publication\n\n- Branch: `{branch}`\n- Commit: `{commit}`\n",
            bullet_files(&files),
        );
        if !findings.is_empty() {
            body.push_str("\n## Review findings\n\n");
            body.push_str(&findings.join("\n"));
            body.push('\n');
        }
        Self { title, body }
    }
}

pub(crate) trait PullRequestProvider {
    fn has_pull_request(&mut self, worktree: &Path, branch: &str) -> Result<bool, ProviderError>;
    fn create_pull_request(
        &mut self,
        worktree: &Path,
        branch: &str,
        base: &str,
        draft: &PullRequestDraft,
    ) -> Result<(), ProviderError>;
}

/// GitHub CLI adapter. GitHub repository and credentials are inherited from
/// the user's checked-out worktree and environment.
pub(crate) struct GitHubProvider;

impl PullRequestProvider for GitHubProvider {
    fn has_pull_request(&mut self, worktree: &Path, branch: &str) -> Result<bool, ProviderError> {
        let output = Command::new("gh")
            .current_dir(worktree)
            .args([
                "pr", "list", "--head", branch, "--state", "open", "--limit", "1", "--json",
                "number",
            ])
            .output()
            .map_err(ProviderError::Start)?;
        require_success("list pull requests", &output)?;
        let values: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)
            .map_err(|error| ProviderError::Output(error.to_string()))?;
        Ok(!values.is_empty())
    }

    fn create_pull_request(
        &mut self,
        worktree: &Path,
        branch: &str,
        base: &str,
        draft: &PullRequestDraft,
    ) -> Result<(), ProviderError> {
        let mut command = Command::new("gh");
        command.current_dir(worktree).args([
            "pr",
            "create",
            "--head",
            branch,
            "--title",
            &draft.title,
            "--body",
            &draft.body,
        ]);
        if !base.is_empty() {
            command.args(["--base", base]);
        }
        let output = command.output().map_err(ProviderError::Start)?;
        require_success("create pull request", &output)
    }
}

#[derive(Debug)]
pub(crate) enum ProviderError {
    Start(std::io::Error),
    Command {
        operation: &'static str,
        stderr: String,
    },
    Output(String),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Start(error) => write!(formatter, "could not start provider adapter: {error}"),
            Self::Command { operation, stderr } if stderr.is_empty() => {
                write!(formatter, "provider could not {operation}")
            }
            Self::Command { operation, stderr } => {
                write!(formatter, "provider could not {operation}: {stderr}")
            }
            Self::Output(error) => write!(formatter, "invalid provider response: {error}"),
        }
    }
}

fn require_success(operation: &'static str, output: &Output) -> Result<(), ProviderError> {
    if output.status.success() {
        Ok(())
    } else {
        Err(ProviderError::Command {
            operation,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

fn completion_parts(completion: &RoleReply) -> (String, Vec<String>, Vec<String>) {
    match completion {
        RoleReply::Impl(reply) => (reply.note.clone(), reply.files.clone(), Vec::new()),
        RoleReply::Review(reply) => {
            let files = reply
                .findings
                .iter()
                .map(|finding| finding.file.clone())
                .collect();
            let findings = reply
                .findings
                .iter()
                .map(|finding| {
                    format!(
                        "- `{}` line {} [{}]: {} Fix: {}",
                        finding.file, finding.line, finding.severity, finding.summary, finding.fix
                    )
                })
                .collect();
            (
                reply
                    .note
                    .clone()
                    .unwrap_or_else(|| "No completion note was recorded.".to_owned()),
                files,
                findings,
            )
        }
    }
}

fn bullet_files(files: &[String]) -> String {
    let mut files = files.to_vec();
    files.sort();
    files.dedup();
    if files.is_empty() {
        return "- None recorded".to_owned();
    }
    files
        .into_iter()
        .map(|file| format!("- `{file}`"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use oca_core::{ImplReply, ReviewFinding, ReviewReply, WorkerState};

    use super::PullRequestDraft;

    #[test]
    fn implementation_completion_has_a_stable_title_and_body() {
        let completion = ImplReply {
            status: WorkerState::Done,
            files: vec!["z.rs".to_owned(), "a.rs".to_owned(), "a.rs".to_owned()],
            note: "Implemented the requested publication gate and verified each boundary. The result is ready for a human-owned merge.".to_owned(),
        }
        .into();
        let draft = PullRequestDraft::from_completion(
            "w4f2a1",
            "implement publish gate",
            &completion,
            "oca/w4f2a1",
            "abc123",
        );

        assert_eq!(draft.title, "oca w4f2a1: implement publish gate");
        assert_eq!(
            draft.body,
            "## Completion\n\nImplemented the requested publication gate and verified each boundary. The result is ready for a human-owned merge.\n\n## Changed files\n\n- `a.rs`\n- `z.rs`\n\n## Publication\n\n- Branch: `oca/w4f2a1`\n- Commit: `abc123`\n"
        );
    }

    #[test]
    fn review_findings_are_rendered_without_shell_interpolation() {
        let completion = ReviewReply {
            status: WorkerState::Blocked,
            findings: vec![ReviewFinding {
                file: "src/lib.rs".to_owned(),
                line: 17,
                severity: "high".to_owned(),
                summary: "Git failure is incorrectly reported as dirt".to_owned(),
                fix: "Preserve the typed git command failure instead".to_owned(),
            }],
            note: None,
        }
        .into();
        let draft = PullRequestDraft::from_completion(
            "w00001",
            "review publish gate",
            &completion,
            "oca/w00001",
            "def456",
        );

        assert!(draft.body.contains("## Review findings"));
        assert!(draft.body.contains("`src/lib.rs` line 17 [high]"));
        assert!(draft.body.contains("No completion note was recorded."));
    }
}
