// Seams under test: `validate_reply_floor`, `ImplReply`, `ReviewReply`,
// `ReviewFinding`, and `WorkerState`. These are deliberately pure so the
// dispatch decoder and commit gate can call the same check.
use oca_core::{ImplReply, ReviewFinding, ReviewReply, WorkerState, validate_reply_floor};

#[test]
fn impl_reply_floor_matrix_rejects_collapsed_notes_and_accepts_thresholds() {
    let valid_done_note = "Implemented the parser changes across the command handling module. \
        Added focused tests covering accepted aliases and invalid flag combinations for callers. \
        Updated the documentation so operators can understand the resulting validation behavior.";
    let valid_blocked_note = "Which configuration source should define the fallback alias before I continue this change?";

    for status in [WorkerState::Done, WorkerState::Partial] {
        let short = ImplReply {
            status,
            files: Vec::new(),
            note: "Too short. Still too short.".to_owned(),
        };
        assert_floor_failure(&short.into(), "note", "25 words and 2 sentences");

        let valid = ImplReply {
            status,
            files: Vec::new(),
            note: valid_done_note.to_owned(),
        };
        assert!(validate_reply_floor(&valid.into()).is_ok());
    }

    let short_blocked = ImplReply {
        status: WorkerState::Blocked,
        files: Vec::new(),
        note: "Which source should I use?".to_owned(),
    };
    assert_floor_failure(&short_blocked.into(), "note", "10 words");

    let valid_blocked = ImplReply {
        status: WorkerState::Blocked,
        files: Vec::new(),
        note: valid_blocked_note.to_owned(),
    };
    assert!(validate_reply_floor(&valid_blocked.into()).is_ok());

    let uncharacterized_change = ImplReply {
        status: WorkerState::Done,
        files: vec!["src/lib.rs".to_owned()],
        note: "This task is complete and the requested behavior is now available. \
            The requested result meets the acceptance criteria without further changes needed. \
            Please continue with the next requested task when convenient for the project."
            .to_owned(),
    };
    assert_floor_failure(
        &uncharacterized_change.into(),
        "note",
        "name a changed file or characterize the change",
    );
}

#[test]
fn review_reply_floor_matrix_covers_findings_and_clean_reviews() {
    let underspecified_finding = ReviewReply {
        status: WorkerState::Partial,
        findings: vec![ReviewFinding {
            file: "src/lib.rs".to_owned(),
            line: 12,
            severity: "major".to_owned(),
            summary: "Parser accepts invalid flags after prompt text".to_owned(),
            fix: "Reject flags.".to_owned(),
        }],
        note: None,
    };
    assert_floor_failure(&underspecified_finding.into(), "findings[0].fix", "6 words");

    let complete_finding = ReviewReply {
        status: WorkerState::Partial,
        findings: vec![ReviewFinding {
            file: "src/lib.rs".to_owned(),
            line: 12,
            severity: "major".to_owned(),
            summary: "Parser accepts unknown flags after prompts".to_owned(),
            fix: "Reject unknown flags before parsing prompt text".to_owned(),
        }],
        note: None,
    };
    assert!(validate_reply_floor(&complete_finding.into()).is_ok());

    for status in [
        WorkerState::Done,
        WorkerState::Blocked,
        WorkerState::Partial,
    ] {
        let complete_finding = ReviewReply {
            status,
            findings: vec![ReviewFinding {
                file: "src/lib.rs".to_owned(),
                line: 12,
                severity: "major".to_owned(),
                summary: "Parser accepts unknown flags after prompt text".to_owned(),
                fix: "Reject unknown flags before parsing prompt text".to_owned(),
            }],
            note: None,
        };
        assert!(validate_reply_floor(&complete_finding.into()).is_ok());
    }

    let missing_clean_note = ReviewReply {
        status: WorkerState::Done,
        findings: Vec::new(),
        note: None,
    };
    assert_floor_failure(&missing_clean_note.into(), "note", "required");

    let short_clean_note = ReviewReply {
        status: WorkerState::Done,
        findings: Vec::new(),
        note: Some(
            "Reviewed parser behavior and error formatting across command cases.".to_owned(),
        ),
    };
    assert_floor_failure(&short_clean_note.into(), "note", "15 words");

    let valid_clean_note = ReviewReply {
        status: WorkerState::Done,
        findings: Vec::new(),
        note: Some(
            "Reviewed command parsing, alias resolution, error envelopes, and representative CLI \
             failure cases for the documented behavior."
                .to_owned(),
        ),
    };
    assert!(validate_reply_floor(&valid_clean_note.into()).is_ok());

    for status in [WorkerState::Blocked, WorkerState::Partial] {
        let reply = ReviewReply {
            status,
            findings: Vec::new(),
            note: None,
        };
        assert!(validate_reply_floor(&reply.into()).is_ok());
    }
}

fn assert_floor_failure(reply: &oca_core::RoleReply, field: &str, floor: &str) {
    let error = validate_reply_floor(reply).expect_err("reply should miss its floor");

    assert_eq!(error.code(), "contract_invalid");
    assert!(error.error().contains(field));
    assert!(error.error().contains(floor));
}
