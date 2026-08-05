//! Process-boundary acceptance coverage for the list and events verbs.

use std::process::{Command, Output};

use oca_core::{OcaEvent, RefId};
use oca_state::{EventJournal, RefRecord, RefStore, RefStorePaths};
use serde_json::{Value, json};

#[test]
fn blocked_count_is_a_byte_exact_bare_integer() {
    let fixture = Fixture::new();
    fixture.insert("w00001", "blocked");
    fixture.insert("w00002", "done");

    let output = fixture.run(&["ls", "--blocked", "--count"]);

    assert!(output.status.success());
    assert_eq!(output.stdout, b"1");
    assert!(output.stderr.is_empty());
}

#[test]
fn events_since_is_a_single_debugging_page() {
    let fixture = Fixture::new();
    fixture.insert("w00001", "done");

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

    fn insert(&self, reference: &str, status: &str) {
        let state = self.home.path().join(".oca");
        let turn = format!("turn_{reference}");
        let store = RefStore::with_paths(RefStorePaths::in_directory(&state));
        store
            .insert(RefRecord {
                id: reference.to_owned(),
                session_id: format!("ses_{reference}"),
                message_id: Some(turn.clone()),
                repo: Some(self.repo.path().display().to_string()),
                spawner_tag: Some("test-spawner".to_owned()),
                tombstoned: false,
            })
            .unwrap();
        let reference = RefId::new(reference).unwrap();
        let mut journal = EventJournal::create(&state, &reference, &turn).unwrap();
        journal
            .append(&OcaEvent {
                id: None,
                cursor: None,
                kind: "message.updated".to_owned(),
                session_id: Some(format!("ses_{reference}")),
                payload: Some(json!({ "structured": { "status": status } })),
                message: None,
                known: true,
            })
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
