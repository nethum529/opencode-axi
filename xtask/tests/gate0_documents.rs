//! Cut-level acceptance tests backfilled by the QA gate for T03 (#3).
//!
//! T03's deliverables are documents. These tests hold the documents to the
//! shape the ticket demanded, so a later edit cannot quietly drop a case or a
//! verdict that downstream tickets read.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()
        .expect("the workspace root resolves")
}

fn read(relative: &str) -> String {
    let path = workspace_root().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{relative} reads: {error}"))
}

/// T03 (#3), criterion 1: every case 1-8 has a recorded result.
#[test]
fn the_tui_coexistence_document_records_every_case_from_one_to_eight() {
    let document = read("docs/experiments/tui-coexistence.md");

    let results = document
        .split("## Case results")
        .nth(1)
        .expect("the document has a case results section");

    for case in 1..=8 {
        let row = results
            .lines()
            .find(|line| line.trim_start().starts_with(&format!("| {case} |")))
            .unwrap_or_else(|| panic!("case {case} has no result row"));
        assert!(
            row.contains("PASS") || row.contains("FAIL"),
            "case {case} records no observed result"
        );
    }
}

/// T03 (#3), criterion 2: the headed-mode verdict is an explicit sentence, not
/// an implication left in the case table.
#[test]
fn the_tui_coexistence_document_states_its_verdict_explicitly() {
    let document = read("docs/experiments/tui-coexistence.md");

    assert!(document.contains("## Verdict"), "no verdict section");
    let verdict = document
        .split("## Verdict")
        .nth(1)
        .expect("the verdict section has a body");
    let lowered = verdict.to_lowercase();
    assert!(
        lowered.contains("shared-input") || lowered.contains("observer-only"),
        "the verdict must name the selected headed mode"
    );
}

/// T03 (#3), criterion 3: case 8, the spurious `session.idle` at TUI attach
/// boot, carries its own pass/fail line.
#[test]
fn case_eight_has_its_own_pass_or_fail_line() {
    let document = read("docs/experiments/tui-coexistence.md").to_lowercase();
    let section = document
        .split("case 8 — dedicated pass/fail line")
        .nth(1)
        .expect("case 8 has a dedicated section");

    assert!(
        section.contains("pass") || section.contains("fail"),
        "case 8 must record an explicit pass or fail"
    );
    assert!(
        section.contains("session.idle"),
        "case 8 must name the event it rules on"
    );
}

/// T03 (#3), criterion 4: the gate-0 document answers all three open verify
/// items with an explicit yes or no.
#[test]
fn the_gate0_document_answers_each_verify_item_yes_or_no() {
    let document = read("docs/experiments/gate0-verify.md");

    for item in [
        "permission profile",
        "message id echo",
        "variant behavioral effect",
    ] {
        let normalized = document
            .to_lowercase()
            .replace('-', " ")
            .replace('`', "")
            .replace('_', " ");
        let position = normalized
            .find(item)
            .unwrap_or_else(|| panic!("{item} is not covered"));
        let headline = &normalized[position..normalized.len().min(position + 160)];
        assert!(
            headline.contains("yes") || headline.contains("no"),
            "{item} has no explicit yes/no answer"
        );
    }
}
