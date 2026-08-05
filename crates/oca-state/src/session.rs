//! Durable session-control state and cross-process turn serialization.

use std::{
    fs::{self, File, OpenOptions},
    io,
    path::Path,
};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

/// The last locally observed lifecycle state for a persistent worker session.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RefState {
    Idle,
    Running,
    Done,
    Blocked,
    Partial,
    Aborted,
}

impl RefState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Partial => "partial",
            Self::Aborted => "aborted",
        }
    }

    #[must_use]
    pub const fn accepts_new_turn(self) -> bool {
        matches!(
            self,
            Self::Idle | Self::Done | Self::Blocked | Self::Partial
        )
    }
}

/// An exclusive per-ref lock held across admission and the resulting ref update.
///
/// The lock is advisory and process-scoped. Dropping it releases the lock.
pub struct SessionTurnLock {
    _file: File,
}

impl SessionTurnLock {
    /// Waits until this process exclusively owns the ref's turn-admission lock.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the private lock directory or lock file cannot
    /// be created, secured, or locked.
    pub fn acquire(state_directory: &Path, reference: &str) -> io::Result<Self> {
        let directory = state_directory.join("session-locks");
        fs::create_dir_all(&directory)?;
        #[cfg(unix)]
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;

        let path = directory.join(format!("{reference}.lock"));
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(path)?;
        file.lock_exclusive()?;
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::RefState;

    #[test]
    fn only_terminal_continuable_states_accept_a_message_turn() {
        for state in [
            RefState::Idle,
            RefState::Done,
            RefState::Blocked,
            RefState::Partial,
        ] {
            assert!(state.accepts_new_turn());
        }
        for state in [RefState::Running, RefState::Aborted] {
            assert!(!state.accepts_new_turn());
        }
    }
}
