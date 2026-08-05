//! Process-boundary acceptance coverage for the list and events verbs.

use std::process::{Command, Output};

use oca_core::{OcaEvent, RefId};
use oca_state::{EventJournal, RefRecord, RefState, RefStore, RefStorePaths};
use serde_json::{Value, json};

#[test]
fn blocked_count_is_a_byte_exact_bare_integer() {
    let fixture = Fixture::new();
    fixture.insert("w00001", RefState::Blocked);
    fixture.insert("w00002", RefState::Done);

    let output = fixture.run(&["ls", "--blocked", "--count"]);

    assert!(output.status.success());
    assert_eq!(output.stdout, b"1");
    assert!(output.stderr.is_empty());
}

#[test]
fn events_since_is_a_single_debugging_page() {
    let fixture = Fixture::new();
    fixture.insert("w00001", RefState::Done);
    fixture.append_event("w00001", "session.idle", json!({ "type": "idle" }));

    let output = fixture.run(&["--json", "events", "w00001", "--since", "0"]);

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let page: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(page["ref"], "w00001");
    assert_eq!(page["events"].as_array().unwrap().len(), 1);
    assert_eq!(page["events"][0]["sequence"], 1);
    assert_eq!(page["cursor"], 1);
    assert_eq!(page["total"], 1);
}

#[test]
fn completed_tool_part_does_not_make_a_running_worker_done() {
    let fixture = Fixture::new();
    fixture.insert("w00001", RefState::Running);
    fixture.append_event(
        "w00001",
        "message.part.updated",
        json!({
            "properties": {
                "part": {
                    "type": "tool",
                    "state": { "status": "completed" }
                }
            }
        }),
    );

    let output = fixture.run(&["ls", "--json"]);

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let page: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(page["items"][0]["ref"], "w00001");
    assert_eq!(page["items"][0]["state"], "running");
}

#[test]
fn corrupt_worker_journal_does_not_fail_the_fleet_listing() {
    let fixture = Fixture::new();
    fixture.insert("w00001", RefState::Running);
    fixture.insert("w00002", RefState::Blocked);
    fixture.corrupt_journal("w00001");

    let output = fixture.run(&["ls", "--json"]);

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let page: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(page["total"], 2);
    assert_eq!(page["items"][0]["ref"], "w00002");
    assert_eq!(page["items"][0]["state"], "blocked");
    assert_eq!(page["items"][1]["ref"], "w00001");
    assert_eq!(page["items"][1]["state"], "running");
}

struct Fixture {
    home: tempfile::TempDir,
    repo: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        Self { home, repo }
    }

    fn insert(&self, reference: &str, worker_state: RefState) {
        let state = self.home.path().join(".oca");
        let turn = format!("turn_{reference}");
        let store = RefStore::with_paths(RefStorePaths::in_directory(&state));
        store
            .insert(RefRecord {
                id: reference.to_owned(),
                session_id: format!("ses_{reference}"),
                message_id: Some(turn.clone()),
                alias: Some("luna".to_owned()),
                effort: Some("high".to_owned()),
                role: Some("impl".to_owned()),
                cwd: Some(self.repo.path().display().to_string()),
                last_state: Some(worker_state),
                repo: Some(self.repo.path().display().to_string()),
                spawner_tag: Some("test-spawner".to_owned()),
                tombstoned: false,
            })
            .unwrap();
    }

    fn append_event(&self, reference: &str, kind: &str, payload: Value) {
        let state = self.home.path().join(".oca");
        let turn = format!("turn_{reference}");
        let reference = RefId::new(reference).unwrap();
        let mut journal = EventJournal::create(&state, &reference, &turn).unwrap();
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

    fn corrupt_journal(&self, reference: &str) {
        let events = self.home.path().join(".oca/events");
        std::fs::create_dir_all(&events).unwrap();
        std::fs::write(
            events.join(format!("{reference}.turn_{reference}.jsonl")),
            b"{\"schema_version\":1,\"sequence\":",
        )
        .unwrap();
    }

    fn run(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oca"))
            .args(arguments)
            .env("HOME", self.home.path())
            .env("OCA_SPAWNER", "test-spawner")
            .current_dir(self.repo.path())
            .output()
            .unwrap()
    }
}
