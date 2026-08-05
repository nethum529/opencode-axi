//! Single-snapshot implementation of the `oca ls` fleet inbox.

use std::{cmp::Ordering, collections::HashMap, path::Path, time::Duration};

use oca_core::{ErrorCode, OcaError};
use oca_display::{ListDocument, ListItem};
use oca_state::{
    JournalError, OcaConfig, RefListFilter, RefRecord, RefState, RefStore, RefStorePaths,
    prune_expired_journals,
};

use crate::{
    ListCommand,
    crash_recovery::{ReconciledState, reconcile_for_list},
    scope::Scope,
};

const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

/// Executes one point-in-time fleet read plus bounded opportunistic pruning.
///
/// This function never subscribes, polls, sleeps, or waits for a state change.
///
/// # Errors
///
/// Returns a stable configuration, state, journal, or scope error.
pub async fn execute_list(
    command: &ListCommand,
    home: impl AsRef<Path>,
) -> Result<String, OcaError> {
    let cwd = std::env::current_dir().map_err(runtime_error)?;
    let scope = crate::scope::current(home.as_ref(), &cwd).map_err(runtime_error)?;
    let reconciled = reconcile_for_list(home.as_ref()).await?;
    execute_list_with_scope(command, home.as_ref(), &scope, &reconciled)
}

fn execute_list_with_scope(
    command: &ListCommand,
    home: &Path,
    scope: &Scope,
    reconciled: &[(String, ReconciledState)],
) -> Result<String, OcaError> {
    let state_directory = home.join(".oca");
    let config = OcaConfig::load_from_home(home).map_err(|error| {
        OcaError::new(ErrorCode::Usage)
            .with_error(format!("failed to load configuration: {error}"))
            .with_help("fix ~/.oca/config.toml and retry")
    })?;
    let store = RefStore::with_paths(RefStorePaths::in_directory(&state_directory));
    store
        .prune_tombstones(retention(config.retention.tombstone_days))
        .map_err(|error| runtime_error(error.to_string()))?;
    prune_expired_journals(&state_directory, retention(config.retention.journal_days))
        .map_err(|error| journal_error(None, error))?;

    let filter = if command.all {
        RefListFilter::across_spawners_and_repos()
    } else {
        RefListFilter {
            spawner_tag: Some(scope.spawner_tag.clone()),
            repo: Some(scope.repo.clone()),
            ..RefListFilter::default()
        }
    };
    let records = store
        .list(&filter)
        .map_err(|error| runtime_error(error.to_string()))?;
    let overrides = reconciled.iter().cloned().collect::<HashMap<_, _>>();
    let mut workers = records
        .into_iter()
        .map(|record| worker_from_record(record, &overrides))
        .collect::<Vec<_>>();
    if command.blocked {
        workers.retain(|worker| worker.state == ListState::Blocked);
    }
    workers.sort_by(Worker::compare);

    if command.count {
        return Ok(workers.len().to_string());
    }
    let total = u64::try_from(workers.len()).unwrap_or(u64::MAX);
    let items = workers
        .into_iter()
        .map(|worker| ListItem::new(worker.reference, worker.state.as_str()))
        .collect();
    let document = ListDocument::new(items, 0, total);
    Ok(if command.json {
        document.render_json()
    } else {
        document.render_toon()
    })
}

fn retention(days: u16) -> Duration {
    Duration::from_secs(u64::from(days).saturating_mul(SECONDS_PER_DAY))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ListState {
    Blocked,
    PromptUncertain,
    PublishedUncertain,
    SessionCreated,
    TerminalObserved,
    Running,
    Unknown,
    Partial,
    Done,
    Idle,
    Aborted,
}

impl ListState {
    const fn rank(self) -> u8 {
        match self {
            Self::Blocked => 0,
            Self::PromptUncertain | Self::PublishedUncertain => 1,
            Self::SessionCreated | Self::Running | Self::Unknown | Self::TerminalObserved => 2,
            Self::Partial => 3,
            Self::Done | Self::Idle => 4,
            Self::Aborted => 5,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::PromptUncertain => "prompt_uncertain",
            Self::PublishedUncertain => "published_uncertain",
            Self::SessionCreated => "session_created",
            Self::TerminalObserved => "terminal_observed",
            Self::Running => "running",
            Self::Unknown => "unknown",
            Self::Partial => "partial",
            Self::Done => "done",
            Self::Idle => "idle",
            Self::Aborted => "aborted",
        }
    }
}

impl From<Option<RefState>> for ListState {
    fn from(state: Option<RefState>) -> Self {
        match state.unwrap_or(RefState::Running) {
            RefState::Blocked => Self::Blocked,
            RefState::Running => Self::Running,
            RefState::Unknown => Self::Unknown,
            RefState::Partial => Self::Partial,
            RefState::Done => Self::Done,
            RefState::Idle => Self::Idle,
            RefState::Aborted => Self::Aborted,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct Worker {
    reference: String,
    state: ListState,
}

impl Worker {
    fn compare(left: &Self, right: &Self) -> Ordering {
        left.state
            .rank()
            .cmp(&right.state.rank())
            .then_with(|| left.reference.cmp(&right.reference))
    }
}

fn worker_from_record(record: RefRecord, overrides: &HashMap<String, ReconciledState>) -> Worker {
    let state = match overrides.get(&record.id) {
        Some(ReconciledState::PromptUncertain) => ListState::PromptUncertain,
        Some(ReconciledState::PublishedUncertain) => ListState::PublishedUncertain,
        Some(ReconciledState::SessionCreated) => ListState::SessionCreated,
        Some(ReconciledState::TerminalObserved) => ListState::TerminalObserved,
        None => record.last_state.into(),
    };
    Worker {
        reference: record.id,
        state,
    }
}

fn runtime_error(detail: impl std::fmt::Display) -> OcaError {
    OcaError::new(ErrorCode::ServerUnavailable).with_error(detail.to_string())
}

fn journal_error(reference: Option<&str>, error: JournalError) -> OcaError {
    let code = match error {
        JournalError::Corrupt { .. }
        | JournalError::MissingSequence { .. }
        | JournalError::TrailingIncomplete { .. } => ErrorCode::EventsCorrupt,
        JournalError::Io { .. }
        | JournalError::Encode(_)
        | JournalError::WriterActive { .. }
        | JournalError::ReadOnly { .. }
        | JournalError::UnsafeTurn(_) => ErrorCode::ServerUnavailable,
    };
    let mut error = OcaError::new(code).with_error(format!("event journal failed: {error}"));
    if let Some(reference) = reference {
        error = error.with_ref(reference);
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_fixture_has_exact_blocked_first_lifecycle_order() {
        let home = tempfile::tempdir().unwrap();
        let state = home.path().join(".oca");
        let scope = Scope {
            spawner_tag: "parent-a".to_owned(),
            repo: "/repo/a".to_owned(),
        };
        for (reference, worker_state) in [
            ("w00008", RefState::Aborted),
            ("w00004", RefState::Done),
            ("w00003", RefState::Blocked),
            ("w00006", RefState::Partial),
            ("w00002", RefState::Running),
            ("w00001", RefState::Blocked),
        ] {
            insert_worker(&state, &scope, reference, worker_state);
        }

        let output = execute_list_with_scope(
            &ListCommand {
                all: false,
                blocked: false,
                count: false,
                json: true,
            },
            home.path(),
            &scope,
            &[],
        )
        .unwrap();
        assert_eq!(
            output,
            "{\"items\":[{\"ref\":\"w00001\",\"state\":\"blocked\"},{\"ref\":\"w00003\",\"state\":\"blocked\"},{\"ref\":\"w00002\",\"state\":\"running\"},{\"ref\":\"w00006\",\"state\":\"partial\"},{\"ref\":\"w00004\",\"state\":\"done\"},{\"ref\":\"w00008\",\"state\":\"aborted\"}],\"cursor\":0,\"total\":6}\n"
        );
    }

    #[test]
    fn default_scopes_by_spawner_and_repo_while_all_crosses_both() {
        let home = tempfile::tempdir().unwrap();
        let state = home.path().join(".oca");
        let selected = Scope {
            spawner_tag: "parent-a".to_owned(),
            repo: "/repo/a".to_owned(),
        };
        insert_worker(&state, &selected, "w00001", RefState::Blocked);
        insert_worker(
            &state,
            &Scope {
                spawner_tag: "parent-b".to_owned(),
                repo: "/repo/a".to_owned(),
            },
            "w00002",
            RefState::Blocked,
        );
        insert_worker(
            &state,
            &Scope {
                spawner_tag: "parent-a".to_owned(),
                repo: "/repo/b".to_owned(),
            },
            "w00003",
            RefState::Blocked,
        );

        let count = execute_list_with_scope(
            &ListCommand {
                count: true,
                ..ListCommand::default()
            },
            home.path(),
            &selected,
            &[],
        )
        .unwrap();
        assert_eq!(count.as_bytes(), b"1", "count has no newline or decoration");

        let all = execute_list_with_scope(
            &ListCommand {
                all: true,
                count: true,
                ..ListCommand::default()
            },
            home.path(),
            &selected,
            &[],
        )
        .unwrap();
        assert_eq!(all.as_bytes(), b"3");
    }

    fn insert_worker(state: &Path, scope: &Scope, reference: &str, worker_state: RefState) {
        let store = RefStore::with_paths(RefStorePaths::in_directory(state));
        store
            .insert(RefRecord {
                id: reference.to_owned(),
                session_id: format!("ses_{reference}"),
                message_id: Some(format!("turn_{reference}")),
                alias: Some("luna".to_owned()),
                effort: Some("high".to_owned()),
                role: Some("impl".to_owned()),
                cwd: Some("/repo/a".to_owned()),
                last_state: Some(worker_state),
                repo: Some(scope.repo.clone()),
                spawner_tag: Some(scope.spawner_tag.clone()),
                worktree: None,
                branch: None,
                commit: None,
                commit_subject: None,
                display: None,
                herdr_tab: None,
                completion: None,
                tombstoned: false,
            })
            .unwrap();
    }
}
