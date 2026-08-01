//! Cut-level acceptance tests backfilled by the QA gate for T08 (#8).
//!
//! spec-cli-surface.md, "Local validation, before any socket opens", lists the
//! failures the parser owns. Ref syntax is one of them: a malformed ref must
//! fail here, not after a session is opened against the server.

use oca_cli::parse_from;
use oca_core::ErrorCode;

/// T08 (#8), criterion 3: invalid ref syntax is a local validation failure.
/// A ref is `w` followed by five base36 characters (spec-data-state.md
/// section 4, "Ref identifiers").
#[test]
fn invalid_ref_syntax_fails_local_validation_on_every_ref_taking_verb() {
    let malformed = ["", "not-a-ref", "W4F2A1", "w4f2", "w4f2a1x", "x4f2a1"];

    for verb in ["f", "k", "push", "pr"] {
        for reference in malformed {
            let error = parse_from(["oca", verb, reference])
                .err()
                .unwrap_or_else(|| panic!("`oca {verb} {reference}` must fail local validation"));
            assert_eq!(
                error.code(),
                ErrorCode::Usage.as_str(),
                "`oca {verb} {reference}` must be a usage failure"
            );
        }
    }
}

/// T08 (#8), criterion 3: a well-formed ref still parses, so the syntax check
/// above cannot be satisfied by rejecting every ref.
#[test]
fn a_well_formed_ref_passes_local_validation() {
    for verb in ["f", "k", "push", "pr"] {
        parse_from(["oca", verb, "w4f2a1"])
            .unwrap_or_else(|error| panic!("`oca {verb} w4f2a1` must parse, got {error}"));
    }
}
