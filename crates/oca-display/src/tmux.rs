//! Fixed-argv client for tmux display windows.

use std::{
    ffi::OsString,
    path::Path,
    process::{Command, ExitStatus},
};

use thiserror::Error;

/// A tmux window owned by one oca ref.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TmuxWindow {
    name: String,
}

impl TmuxWindow {
    /// Returns the ref-derived window name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
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
        session_id: &str,
        cwd: &Path,
    ) -> Result<TmuxWindow, TmuxError> {
        let window = TmuxWindow {
            name: format!("oca-{reference}"),
        };
        let status = Command::new(&self.executable)
            .args(["new-window", "-d", "-n"])
            .arg(window.name())
            .args(["--", "opencode", "--session", session_id])
            .current_dir(cwd)
            .status()
            .map_err(|source| TmuxError::Invoke {
                operation: "new-window",
                source,
            })?;
        ensure_success("new-window", status)?;
        Ok(window)
    }

    /// Closes exactly the ref-owned window rather than a fuzzy tmux target.
    ///
    /// # Errors
    ///
    /// Returns an invocation or non-zero-exit failure from tmux.
    pub fn close_window(&self, window: &TmuxWindow) -> Result<(), TmuxError> {
        let exact_target = format!("={}", window.name());
        let status = Command::new(&self.executable)
            .args(["kill-window", "-t"])
            .arg(exact_target)
            .status()
            .map_err(|source| TmuxError::Invoke {
                operation: "kill-window",
                source,
            })?;
        ensure_success("kill-window", status)
    }
}

fn ensure_success(operation: &'static str, status: ExitStatus) -> Result<(), TmuxError> {
    if status.success() {
        Ok(())
    } else {
        Err(TmuxError::CommandFailed { operation, status })
    }
}
