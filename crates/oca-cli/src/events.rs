//! Point-in-time implementation of the debugging-only `oca events` verb.

use std::{io, path::Path};

use oca_core::{ErrorCode, OcaError, RefId};
use oca_display::{Event, EventPage};
use oca_state::{EventJournal, JournalError, RefStore, RefStorePaths};
use serde_json::Value;

use crate::EventsCommand;

const EVENT_PAGE_LIMIT: usize = 1_024;

/// Reads and renders one bounded page from the ref's current turn journal.
///
/// This path performs no server discovery, subscription, polling, or waiting.
///
/// # Errors
///
/// Returns a stable ref, state, journal-corruption, or filesystem error.
pub fn execute_events(command: &EventsCommand, home: impl AsRef<Path>) -> Result<String, OcaError> {
    let state_directory = home.as_ref().join(".oca");
    let store = RefStore::with_paths(RefStorePaths::in_directory(&state_directory));
    let record = store
        .resolve(&command.reference)
        .map_err(|error| state_error(&command.reference, &error.to_string()))?
        .ok_or_else(|| {
            OcaError::new(ErrorCode::UnknownRef)
                .with_ref(&command.reference)
                .with_help("Run `oca ls --all` to list refs")
        })?;
    if record.tombstoned {
        return Err(OcaError::new(ErrorCode::SessionNotFound).with_ref(&command.reference));
    }
    let reference = RefId::new(&command.reference).map_err(|error| {
        OcaError::new(ErrorCode::Usage)
            .with_error(error.to_string())
            .with_ref(&command.reference)
    })?;
    let since = command.since.unwrap_or(0);
    let Some(turn) = record.message_id else {
        return Ok(render_page(
            EventPage::new(&command.reference, Vec::new(), since, 0),
            command.json,
        ));
    };
    let journal = match EventJournal::open(&state_directory, &reference, &turn) {
        Ok(journal) => journal,
        Err(JournalError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            return Ok(render_page(
                EventPage::new(&command.reference, Vec::new(), since, 0),
                command.json,
            ));
        }
        Err(error) => return Err(journal_error(&command.reference, error)),
    };
    let page = journal
        .page(since, EVENT_PAGE_LIMIT)
        .map_err(|error| journal_error(&command.reference, error))?;
    let events = page
        .events
        .into_iter()
        .map(|record| Event::new(record.sequence, record.event, event_data(record.payload)))
        .collect();
    Ok(render_page(
        EventPage::new(&command.reference, events, page.cursor, page.total),
        command.json,
    ))
}

fn event_data(payload: Option<Value>) -> String {
    match payload {
        Some(Value::String(value)) => value,
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

fn render_page(page: EventPage, json: bool) -> String {
    if json {
        page.render_json()
    } else {
        page.render_toon()
    }
}

fn state_error(reference: &str, detail: &str) -> OcaError {
    OcaError::new(ErrorCode::UnknownRef)
        .with_ref(reference)
        .with_error(format!("could not read ref state: {detail}"))
}

fn journal_error(reference: &str, error: JournalError) -> OcaError {
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
    OcaError::new(code)
        .with_ref(reference)
        .with_error(format!("event journal failed: {error}"))
        .with_help(format!("Inspect `oca events {reference}` and retry"))
}

#[cfg(test)]
mod tests {
    use oca_core::OcaEvent;
    use oca_state::{EventJournal, RefRecord};
    use serde_json::json;

    use super::*;

    #[test]
    fn since_returns_only_strictly_newer_records() {
        let home = tempfile::tempdir().unwrap();
        let state = home.path().join(".oca");
        let store = RefStore::with_paths(RefStorePaths::in_directory(&state));
        store.insert(record()).unwrap();
        let reference = RefId::new("w4f2a1").unwrap();
        let mut journal = EventJournal::create(&state, &reference, "turn_1").unwrap();
        for number in 1..=3 {
            journal
                .append(&OcaEvent {
                    id: None,
                    cursor: None,
                    kind: "session.status".to_owned(),
                    session_id: Some("ses_target".to_owned()),
                    payload: Some(json!({ "number": number })),
                    message: None,
                    known: true,
                })
                .unwrap();
        }
        drop(journal);

        let rendered = execute_events(
            &EventsCommand {
                reference: "w4f2a1".to_owned(),
                since: Some(1),
                json: true,
            },
            home.path(),
        )
        .unwrap();
        let document: Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(document["events"].as_array().unwrap().len(), 2);
        assert_eq!(document["events"][0]["sequence"], 2);
        assert_eq!(document["events"][1]["sequence"], 3);
        assert_eq!(document["cursor"], 3);
        assert_eq!(document["total"], 3);
    }

    fn record() -> RefRecord {
        RefRecord {
            id: "w4f2a1".to_owned(),
            session_id: "ses_target".to_owned(),
            message_id: Some("turn_1".to_owned()),
            alias: Some("luna".to_owned()),
            effort: Some("high".to_owned()),
            role: Some("impl".to_owned()),
            cwd: Some("/repo/a".to_owned()),
            last_state: Some(oca_state::RefState::Running),
            repo: None,
            spawner_tag: None,
            tombstoned: false,
        }
    }
}
