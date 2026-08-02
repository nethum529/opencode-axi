//! Cut-level acceptance tests backfilled by the QA gate.
//!
//! Each test names the acceptance criterion it covers. The tables below are
//! transcribed from the frozen spec, so a drift between `oca-core` and the spec
//! fails here rather than at release.

use oca_core::{ErrorCode, ModelCatalog, normalize_alias, resolve_model};

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
const SPEC_CODES: [(&str, i32); 23] = [
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
    ("events_corrupt", 1),
    ("follow_timeout", 4),
];

/// `FIX14c`: aliases are normalized through the public function and every
/// catalog/resolver lookup follows the same canonical form.
#[test]
fn aliases_are_normalized_without_a_public_wrapper_type() {
    assert_eq!(normalize_alias("  OpUs  "), "opus");

    let mut catalog = ModelCatalog::default();
    let definition = catalog
        .get("opus")
        .expect("the default opus definition must exist")
        .clone();
    catalog.insert("  CuStOm  ", definition);
    assert!(catalog.get("custom").is_some());

    let opus =
        resolve_model("  OpUs  ", "high", &catalog).expect("a normalized opus alias must resolve");
    assert_eq!(opus.alias, "opus");

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
