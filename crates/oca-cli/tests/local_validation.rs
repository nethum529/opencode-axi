//! Cut-level acceptance tests backfilled by the QA gate for T08 (#8).
//!
//! spec-cli-surface.md, "Local validation, before any socket opens", lists the
//! failures the parser owns. Ref syntax is one of them: a malformed ref must
//! fail here, not after a session is opened against the server.

use oca_cli::parse_from;
use oca_core::ErrorCode;

/// A ref is `w` followed by five lowercase ASCII base-36 characters. Each
/// parser arm must reject malformed refs locally, before execution can open a
/// transport.
#[test]
fn invalid_ref_syntax_fails_local_validation_on_every_ref_taking_verb() {
    let malformed = [
        "",        // empty
        "x4f2a1",  // wrong prefix
        "w4f2a",   // wrong length
        "w4f2a1x", // wrong length
        "w4F2a1",  // uppercase
        "w4f-a1",  // hyphen
        "w4f_a1",  // underscore
        "wé0000",  // non-ASCII
    ];

    for verb in ["m", "q", "f", "k", "events", "push", "pr", "__attach"] {
        for reference in malformed {
            let arguments = match verb {
                "m" | "q" => vec!["oca", verb, reference, "message"],
                "__attach" => vec!["oca", verb, reference, "ses_1", "/repo"],
                _ => vec!["oca", verb, reference],
            };
            let error = parse_from(arguments)
                .err()
                .unwrap_or_else(|| panic!("`oca {verb} {reference}` must fail local validation"));
            assert_eq!(
                error.code(),
                ErrorCode::Usage.as_str(),
                "`oca {verb} {reference}` must be a usage failure"
            );
            assert_eq!(
                error.exit_code(),
                2,
                "`oca {verb} {reference}` must exit with the usage status"
            );
        }
    }
}

/// Every canonical ref must get past its verb's ref validation to the next
/// normal parser step.
#[test]
fn canonical_ref_syntax_passes_local_validation_on_every_ref_taking_verb() {
    for reference in ["w00000", "w4f2a1", "wzzzzz"] {
        for verb in ["m", "q", "f", "k", "push", "pr"] {
            let arguments = match verb {
                "m" | "q" => vec!["oca", verb, reference, "message"],
                _ => vec!["oca", verb, reference],
            };
            parse_from(arguments).unwrap_or_else(|error| {
                panic!("`oca {verb} {reference}` must pass local validation, got {error}")
            });
        }

        for arguments in [
            vec!["oca", "events", reference],
            vec!["oca", "events", reference, "--since", "7"],
            vec!["oca", "__attach", reference, "ses_1", "/repo"],
        ] {
            parse_from(arguments).unwrap_or_else(|error| {
                panic!("canonical ref must pass local validation: {error}")
            });
        }
    }
}
