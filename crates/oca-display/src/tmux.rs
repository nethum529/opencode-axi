//! Fixed-argv client for tmux display windows.

use std::{
    ffi::OsString,
    path::Path,
    process::{Command, ExitStatus},
};

use thiserror::Error;

use crate::opencode_attach_argv;

const PANE_BANNER_SCRIPT: &str = r#"printf '%s\n' "$1"; shift; exec "$@""#;
const PANE_BANNER_ARGV0: &str = "oca-headed-attach";

/// A tmux window owned by one oca ref.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TmuxWindow {
    id: String,
    name: String,
}

impl TmuxWindow {
    /// Returns the task-derived visible window name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the exact tmux window identifier used for cleanup.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// Failures from invoking tmux with the fixed display commands.
#[derive(Debug, Error)]
pub enum TmuxError {
    #[error("could not invoke tmux for {operation}: {source}")]
    Invoke {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("tmux {operation} exited with {status}")]
    CommandFailed {
        operation: &'static str,
        status: ExitStatus,
    },
    #[error("tmux new-window returned an invalid window id: {output:?}")]
    InvalidWindowId { output: String },
}

/// Process client for the tmux fallback.
#[derive(Clone, Debug)]
pub struct TmuxClient {
    executable: OsString,
}

impl Default for TmuxClient {
    fn default() -> Self {
        Self::new("tmux")
    }
}

impl TmuxClient {
    /// Builds a client for an explicit executable.
    ///
    /// Production uses [`Self::default`]. Supplying an executable keeps tests
    /// independent of a live tmux server.
    #[must_use]
    pub fn new(executable: impl Into<OsString>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    /// Creates a detached ref-owned window running the shared-input TUI.
    ///
    /// # Errors
    ///
    /// Returns an invocation or non-zero-exit failure from tmux.
    pub fn new_window(
        &self,
        reference: &str,
        name: &str,
        worker_identity: &str,
        base_url: &str,
        session_id: &str,
        cwd: &Path,
    ) -> Result<TmuxWindow, TmuxError> {
        let attach_argv = opencode_attach_argv(base_url, session_id);
        let output = Command::new(&self.executable)
            .args(["new-window", "-d", "-P", "-F", "#{window_id}", "-n"])
            .arg(name)
            .arg("--")
            .args(["sh", "-c", PANE_BANNER_SCRIPT, PANE_BANNER_ARGV0])
            .arg(worker_identity)
            .args(attach_argv)
            .current_dir(cwd)
            .output()
            .map_err(|source| TmuxError::Invoke {
                operation: "new-window",
                source,
            })?;
        ensure_success("new-window", output.status)?;
        let id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !is_window_id(&id) {
            return Err(TmuxError::InvalidWindowId { output: id });
        }
        let window = TmuxWindow {
            id,
            name: name.to_owned(),
        };

        let option_status = Command::new(&self.executable)
            .args(["set-option", "-w", "-t"])
            .arg(window.id())
            .args(["@oca-ref", reference])
            .status();
        let option_status = match option_status {
            Ok(status) => status,
            Err(source) => {
                let _ = self.close_window(&window);
                return Err(TmuxError::Invoke {
                    operation: "set-option",
                    source,
                });
            }
        };
        if let Err(error) = ensure_success("set-option", option_status) {
            let _ = self.close_window(&window);
            return Err(error);
        }
        Ok(window)
    }

    /// Closes exactly the ref-owned window rather than a fuzzy tmux target.
    ///
    /// # Errors
    ///
    /// Returns an invocation or non-zero-exit failure from tmux.
    pub fn close_window(&self, window: &TmuxWindow) -> Result<(), TmuxError> {
        let status = Command::new(&self.executable)
            .args(["kill-window", "-t"])
            .arg(window.id())
            .status()
            .map_err(|source| TmuxError::Invoke {
                operation: "kill-window",
                source,
            })?;
        ensure_success("kill-window", status)
    }
}

fn is_window_id(value: &str) -> bool {
    value.strip_prefix('@').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn ensure_success(operation: &'static str, status: ExitStatus) -> Result<(), TmuxError> {
    if status.success() {
        Ok(())
    } else {
        Err(TmuxError::CommandFailed { operation, status })
    }
}
