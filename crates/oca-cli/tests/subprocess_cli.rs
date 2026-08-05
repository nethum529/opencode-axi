//! Acceptance coverage for the user-visible `oca` process contract.

use std::process::{Command, Output};

use oca_core::{ErrorCode, exit, parse_error_envelope};

fn run_oca(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oca"))
        .args(arguments)
        .output()
        .expect("Cargo-built oca executable must run")
}

#[test]
fn malformed_ref_json_is_one_usage_envelope_on_stderr() {
    let output = run_oca(&["--json", "f", "not-a-ref"]);

    assert_eq!(output.status.code(), Some(exit::USAGE));
    assert!(output.stdout.is_empty(), "stdout must remain empty");

    let stderr = std::str::from_utf8(&output.stderr).expect("stderr must be UTF-8");
    let document = stderr
        .strip_suffix('\n')
        .expect("JSON stderr must end in exactly one newline");
    assert!(
        !document.contains('\n'),
        "JSON stderr must contain exactly one output line"
    );
    let envelope = parse_error_envelope(document)
        .expect("the whole stderr stream must be exactly one valid JSON envelope");

    assert_eq!(envelope.code(), ErrorCode::Usage.as_str());
    assert!(envelope.reference().is_none(), "usage errors must omit ref");
    assert!(
        envelope.retry_after_ms().is_none(),
        "usage errors must omit retry_after_ms"
    );
}

#[test]
fn malformed_ref_toon_is_one_ordered_usage_envelope_on_stderr() {
    let output = run_oca(&["f", "not-a-ref"]);

    assert_eq!(output.status.code(), Some(exit::USAGE));
    assert!(output.stdout.is_empty(), "stdout must remain empty");

    let stderr = std::str::from_utf8(&output.stderr).expect("stderr must be UTF-8");
    let document = stderr
        .strip_suffix('\n')
        .expect("TOON stderr must end in exactly one newline");
    assert!(
        !document.ends_with('\n'),
        "TOON stderr has an extra newline"
    );

    let fields = document
        .lines()
        .map(|line| {
            line.split_once(": ")
                .expect("each TOON line must contain one named field")
        })
        .collect::<Vec<_>>();

    assert_eq!(
        fields.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        ["error", "code", "help"],
        "the envelope must contain only the three ordered fields"
    );
    assert!(
        fields.iter().all(|(_, value)| !value.is_empty()),
        "every required field must be non-empty"
    );
    assert_eq!(fields[1].1, ErrorCode::Usage.as_str());
}

#[test]
fn canonical_ref_is_not_rejected_by_the_process() {
    let output = run_oca(&["f", "w4f2a1"]);

    assert!(
        output.status.success(),
        "canonical ref failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
