//! Single-snapshot implementation of the `oca ls` fleet inbox.

use std::{cmp::Ordering, io, path::Path, time::Duration};

use oca_core::{ErrorCode, OcaError, RefId};
use oca_display::{ListDocument, ListItem};
use oca_state::{
    EventJournal, JournalError, JournalEvent, OcaConfig, RefListFilter, RefRecord, RefStore,
    RefStorePaths, prune_expired_journals,
};
use serde_json::Value;

use crate::{ListCommand, scope::Scope};

const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

/// Executes one point-in-time fleet read plus bounded opportunistic pruning.
///
/// This function never subscribes, polls, sleeps, or waits for a state change.
///
/// # Errors
///
/// Returns a stable configuration, state, journal, or scope error.
pub fn execute_list(command: &ListCommand, home: impl AsRef<Path>) -> Result<String, OcaError> {
    let cwd = std::env::current_dir().map_err(runtime_error)?;
    let scope = crate::scope::current(home.as_ref(), &cwd).map_err(runtime_error)?;
    execute_list_with_scope(command, home.as_ref(), &scope)
}

fn execute_list_with_scope(
    command: &ListCommand,
    home: &Path,
    scope: &Scope,
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
    let mut workers = records
        .into_iter()
        .map(|record| worker_from_record(&state_directory, record))
        .collect::<Result<Vec<_>, _>>()?;
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
    Running,
    Partial,
    Done,
    Aborted,
}

impl ListState {
    const fn rank(self) -> u8 {
        match self {
            Self::Blocked => 0,
            Self::Running => 1,
            Self::Partial => 2,
            Self::Done => 3,
            Self::Aborted => 4,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Running => "running",
            Self::Partial => "partial",
            Self::Done => "done",
            Self::Aborted => "aborted",
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

fn worker_from_record(state_directory: &Path, record: RefRecord) -> Result<Worker, OcaError> {
    let state = cached_state(state_directory, &record)?;
    Ok(Worker {
        reference: record.id,
        state,
    })
}

fn cached_state(state_directory: &Path, record: &RefRecord) -> Result<ListState, OcaError> {
    let Some(turn) = record.message_id.as_deref() else {
        return Ok(ListState::Running);
    };
    let reference = RefId::new(&record.id).map_err(|error| runtime_error(error.to_string()))?;
    let journal = match EventJournal::open(state_directory, &reference, turn) {
        Ok(journal) => journal,
        Err(JournalError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            return Ok(ListState::Running);
        }
        Err(error) => return Err(journal_error(Some(&record.id), error)),
    };
    let page = journal
        .page(0, usize::MAX)
        .map_err(|error| journal_error(Some(&record.id), error))?;
    Ok(page.events.iter().fold(ListState::Running, apply_event))
}

fn apply_event(state: ListState, event: &JournalEvent) -> ListState {
    let kind = event.event.to_ascii_lowercase();
    if kind.contains("abort") {
        return ListState::Aborted;
    }
    if kind.ends_with("busy") || payload_type(event.payload.as_ref()) == Some("busy") {
        return ListState::Running;
    }
    terminal_state(event.payload.as_ref()).unwrap_or(state)
}

fn payload_type(payload: Option<&Value>) -> Option<&str> {
    payload?.as_object()?.get("type")?.as_str()
}

fn terminal_state(value: Option<&Value>) -> Option<ListState> {
    fn visit(value: &Value) -> Option<ListState> {
        match value {
            Value::Object(object) => {
                if let Some(status) = object.get("status").and_then(Value::as_str) {
                    match status {
                        "blocked" => return Some(ListState::Blocked),
                        "partial" => return Some(ListState::Partial),
                        "done" | "completed" => return Some(ListState::Done),
                        "aborted" => return Some(ListState::Aborted),
                        "running" => return Some(ListState::Running),
                        _ => {}
                    }
                }
                object.values().find_map(visit)
            }
            Value::Array(values) => values.iter().find_map(visit),
            _ => None,
        }
    }
    value.and_then(visit)
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
    use oca_core::OcaEvent;
    use serde_json::json;

    use super::*;

    #[test]
    fn mixed_fixture_has_exact_blocked_first_lifecycle_order() {
        let home = tempfile::tempdir().unwrap();
        let state = home.path().join(".oca");
        let scope = Scope {
            spawner_tag: "parent-a".to_owned(),
            repo: "/repo/a".to_owned(),
        };
        for (reference, state_name) in [
            ("w00008", "aborted"),
            ("w00004", "done"),
            ("w00003", "blocked"),
            ("w00006", "partial"),
            ("w00002", "running"),
            ("w00001", "blocked"),
        ] {
            insert_worker(&state, &scope, reference, state_name);
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
        insert_worker(&state, &selected, "w00001", "blocked");
        insert_worker(
            &state,
            &Scope {
                spawner_tag: "parent-b".to_owned(),
                repo: "/repo/a".to_owned(),
            },
            "w00002",
            "blocked",
        );
        insert_worker(
            &state,
            &Scope {
                spawner_tag: "parent-a".to_owned(),
                repo: "/repo/b".to_owned(),
            },
            "w00003",
            "blocked",
        );

        let count = execute_list_with_scope(
            &ListCommand {
                count: true,
                ..ListCommand::default()
            },
            home.path(),
            &selected,
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
        )
        .unwrap();
        assert_eq!(all.as_bytes(), b"3");
    }

    fn insert_worker(state: &Path, scope: &Scope, reference: &str, state_name: &str) {
        let store = RefStore::with_paths(RefStorePaths::in_directory(state));
        let turn = format!("turn_{reference}");
        store
            .insert(RefRecord {
                id: reference.to_owned(),
                session_id: format!("ses_{reference}"),
                message_id: Some(turn.clone()),
                repo: Some(scope.repo.clone()),
                spawner_tag: Some(scope.spawner_tag.clone()),
                tombstoned: false,
            })
            .unwrap();
        if state_name == "running" {
            return;
        }
        let reference = RefId::new(reference).unwrap();
        let mut journal = EventJournal::create(state, &reference, &turn).unwrap();
        let (kind, payload) = if state_name == "aborted" {
            ("session.aborted", json!({}))
        } else {
            (
                "message.updated",
                json!({ "properties": { "info": { "structured": { "status": state_name } } } }),
            )
        };
        journal
            .append(&OcaEvent {
                id: None,
                cursor: None,
                kind: kind.to_owned(),
                session_id: Some(format!("ses_{reference}")),
                payload: Some(payload),
                message: None,
                known: true,
            })
            .unwrap();
    }
}
