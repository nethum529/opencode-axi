//! Cut-level acceptance tests backfilled by the QA gate.
//!
//! Each test names the acceptance criterion it covers. The tables below are
//! transcribed from the frozen spec, so a drift between `oca-core` and the spec
//! fails here rather than at release.

use oca_core::{ErrorCode, FollowExit, ModelCatalog, OcaError, normalize_alias, resolve_model};

/// spec-data-state.md section 3, "Ladders": alias, provider, model, accepted efforts.
const SPEC_LADDERS: [(&str, &str, &str, &[&str]); 4] = [
    (
        "luna",
        "openai",
        "gpt-5.6-luna",
        &["low", "medium", "high", "xhigh", "max"],
    ),
    (
        "sol",
        "openai",
        "gpt-5.6-sol",
        &["low", "medium", "high", "xhigh", "max"],
    ),
    (
        "terra",
        "openai",
        "gpt-5.6-terra",
        &["low", "medium", "high", "xhigh", "max"],
    ),
    (
        "flash",
        "opencode",
        "deepseek-v4-flash-free",
        &["high", "max"],
    ),
];

/// spec-cli-surface.md, "Codes": code identifier and its frozen exit number.
/// `herdr_unavailable` is omitted: it carries no exit number because it never
/// fails a dispatch.
const SPEC_CODES: [(&str, i32); 27] = [
    ("effort_required", 2),
    ("effort_unsupported", 2),
    ("effort_conflict", 2),
    ("invalid_model", 2),
    ("invalid_usage", 2),
    ("ref_not_found", 1),
    ("session_not_found", 1),
    ("worker_busy", 1),
    ("server_unavailable", 5),
    ("server_start_timeout", 5),
    ("server_unreachable", 5),
    ("rate_limited", 1),
    ("protocol_mismatch", 1),
    ("contract_invalid", 1),
    ("worktree_conflict", 1),
    ("worktree_dirty", 1),
    ("worktree_empty", 1),
    ("out_of_scope", 1),
    ("zero_byte_output", 1),
    ("prompt_uncertain", 1),
    ("publish_disabled", 1),
    ("publish_branch_forbidden", 2),
    ("publish_remote_missing", 1),
    ("publish_failed", 1),
    ("events_corrupt", 1),
    ("follow_timeout", 1),
    ("interrupted", 130),
];

/// `FIX14c`: aliases are normalized through the public function and every
/// catalog/resolver lookup follows the same canonical form.
#[test]
fn aliases_are_normalized_without_a_public_wrapper_type() {
    assert_eq!(normalize_alias("  LuNa  "), "luna");

    let mut catalog = ModelCatalog::default();
    let definition = catalog
        .get("luna")
        .expect("the default luna definition must exist")
        .clone();
    catalog.insert("  CuStOm  ", definition);
    assert!(catalog.get("custom").is_some());

    let luna =
        resolve_model("  LuNa  ", "high", &catalog).expect("a normalized luna alias must resolve");
    assert_eq!(luna.alias, "luna");

    let deepseek = resolve_model("  DeEpSeEk  ", "high", &catalog)
        .expect("a normalized deepseek alias must resolve");
    let flash = resolve_model("flash", "high", &catalog).expect("flash:high must resolve");
    assert_eq!(deepseek, flash);
}

/// T04 (#4), criterion 1: the effort matrix resolves against the four aliases
/// the spec actually defines, with the provider and model each alias names.
#[test]
fn default_catalog_matches_the_spec_alias_and_ladder_table() {
    let catalog = ModelCatalog::default();
    let mut expected: Vec<&str> = SPEC_LADDERS.iter().map(|(alias, ..)| *alias).collect();
    expected.sort_unstable();

    let mut actual: Vec<&str> = catalog.aliases().collect();
    actual.sort_unstable();
    assert_eq!(actual, expected, "the default catalog must be the spec's");

    for (alias, provider, model, ladder) in SPEC_LADDERS {
        for effort in ladder {
            let resolved = resolve_model(alias, *effort, &catalog)
                .unwrap_or_else(|error| panic!("{alias}:{effort} must resolve, got {error}"));
            assert_eq!(resolved.provider, provider, "{alias} provider");
            assert_eq!(resolved.model, model, "{alias} model");
            assert_eq!(resolved.variant, *effort, "{alias}:{effort} variant");
        }
    }
}

/// T07 (#7), criterion 1: every code in the spec's "Codes" table exists with
/// its frozen exit number.
#[test]
fn every_spec_code_table_entry_has_an_error_code_and_frozen_exit_number() {
    let mut missing = Vec::new();
    let mut wrong_exit = Vec::new();

    for (code, exit) in SPEC_CODES {
        match ErrorCode::from_code(code) {
            None => missing.push(code),
            Some(error_code) => {
                if error_code.exit_code() != exit {
                    wrong_exit.push(format!("{code}: {} != {exit}", error_code.exit_code()));
                }
            }
        }
    }

    assert!(
        missing.is_empty(),
        "codes absent from oca-core: {missing:?}"
    );
    assert!(
        wrong_exit.is_empty(),
        "exit numbers off spec: {wrong_exit:?}"
    );
}

/// T07 (#7), criterion 1: no code is emitted that the spec's table does not
/// name, so the public contract has exactly one source of truth.
#[test]
fn no_error_code_is_emitted_outside_the_spec_code_table() {
    let extra: Vec<&str> = ErrorCode::all()
        .iter()
        .map(|code| code.as_str())
        .filter(|code| !SPEC_CODES.iter().any(|(spec, _)| spec == code))
        .collect();

    assert!(
        extra.is_empty(),
        "codes absent from the spec table: {extra:?}"
    );
}

/// T07 (#7), criterion 1: the code table round-trips, so a code parsed back off
/// the wire is the same code that was serialized.
#[test]
fn every_error_code_round_trips_through_its_wire_string() {
    for code in ErrorCode::all() {
        assert_eq!(
            ErrorCode::from_code(code.as_str()),
            Some(*code),
            "{} must round-trip",
            code.as_str()
        );
    }
    assert_eq!(ErrorCode::from_code("not_a_code"), None);
    assert_eq!(ErrorCode::from_code(""), None);
}

/// T07 (#7), criterion 1: additions to the code table are real serializable
/// errors, not unimplemented strings in a registry.
#[test]
fn error_catalogue_includes_previously_absent_contract_codes() {
    for wire_code in [
        "session_not_found",
        "server_start_timeout",
        "worktree_dirty",
        "out_of_scope",
        "prompt_uncertain",
        "follow_timeout",
    ] {
        let error_code = ErrorCode::from_code(wire_code)
            .unwrap_or_else(|| panic!("{wire_code} must be a catalogue entry"));
        let error = OcaError::new(error_code);
        assert_eq!(error.code(), wire_code);
        assert_eq!(ErrorCode::from_code(error.code()), Some(error_code));
        assert!(oca_core::parse_error_envelope(&error.to_json()).is_ok());
    }
}

/// T07 (#7), criterion 2: the public code catalogue and the canonical table
/// have the same members, including codes which are not emitted by every CLI.
#[test]
fn canonical_error_contract_has_no_missing_or_extra_codes() {
    for required in [
        ("server_unreachable", 5),
        ("publish_remote_missing", 1),
        ("publish_failed", 1),
        ("interrupted", 130),
    ] {
        assert!(SPEC_CODES.contains(&required), "missing {required:?}");
    }

    let canonical: Vec<&str> = SPEC_CODES.iter().map(|(code, _)| *code).collect();
    for code in ErrorCode::all() {
        assert!(
            canonical.contains(&code.as_str()),
            "extra {}",
            code.as_str()
        );
    }
}

/// T07 (#7), criterion 3: renamed wire strings are a deliberate breaking
/// change, so parsers only accept their canonical replacements.
#[test]
fn breaking_wire_string_renames_are_one_way() {
    for (error_code, wire_code) in [
        (ErrorCode::Usage, "invalid_usage"),
        (ErrorCode::AliasUnknown, "invalid_model"),
        (ErrorCode::EffortMissing, "effort_required"),
        (ErrorCode::UnknownRef, "ref_not_found"),
        (ErrorCode::ProtectedBranch, "publish_branch_forbidden"),
    ] {
        assert_eq!(OcaError::new(error_code).code(), wire_code);
        assert_eq!(ErrorCode::from_code(wire_code), Some(error_code));
    }

    for retired in [
        "usage",
        "alias_unknown",
        "effort_missing",
        "unknown_ref",
        "protected_branch",
    ] {
        assert_eq!(ErrorCode::from_code(retired), None, "{retired} is retired");
    }
}

/// T07 (#7), criterion 4: error outcomes use general failure unless they are
/// specifically a usage error, a server reachability error, or interruption.
#[test]
fn error_exit_statuses_match_the_contract() {
    assert_eq!(ErrorCode::RateLimited.exit_code(), 1);
    assert_eq!(ErrorCode::ProtectedBranch.exit_code(), 2);

    for (wire_code, exit_code) in SPEC_CODES {
        let error_code = ErrorCode::from_code(wire_code)
            .unwrap_or_else(|| panic!("{wire_code} must be a catalogue entry"));
        assert_eq!(error_code.exit_code(), exit_code, "{wire_code}");
    }
}

/// T07 (#7), criterion 5: a timed-out follow operation reported as an error
/// is a failure; exits 3 and 4 identify only follow command outcomes.
#[test]
fn follow_timeout_is_reallocated_to_failure() {
    assert_eq!(ErrorCode::FollowTimeout.exit_code(), 1);
    assert!(
        ErrorCode::all()
            .iter()
            .all(|code| !matches!(code.exit_code(), 3 | 4))
    );
    assert_eq!(FollowExit::Blocked.code(), 3);
    assert_eq!(FollowExit::Timeout.code(), 4);
}
