//! Cut-level acceptance tests backfilled by the QA gate for T01 (#1).
//!
//! The scaffold is a contract like any other: the binary name, the pinned
//! toolchain, the CI entry points and the per-crate dependency allowlist are
//! all things a later ticket can silently break.

use std::fs;
use std::path::{Path, PathBuf};

/// spec-architecture.md section 2: the eight crates plus `xtask`.
const WORKSPACE_CRATES: [&str; 8] = [
    "oca-cli",
    "oca-core",
    "oca-display",
    "oca-git",
    "oca-opencode",
    "oca-server",
    "oca-state",
    "oca-testkit",
];

/// The only crates the responsibility table lets reach the network directly.
const HTTP_CLIENT_CRATES: [&str; 2] = ["oca-opencode", "oca-cli"];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()
        .expect("the workspace root resolves")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("{} reads: {error}", path.display()))
}

/// T01 (#1), criterion 1: every crate in the section 2 tree is present and is a
/// workspace member.
#[test]
fn the_workspace_contains_all_eight_crates_and_xtask() {
    let root = workspace_root();

    for crate_name in WORKSPACE_CRATES {
        let manifest = root.join("crates").join(crate_name).join("Cargo.toml");
        assert!(manifest.is_file(), "{crate_name} has no manifest");
    }
    assert!(root.join("xtask/Cargo.toml").is_file(), "xtask is missing");

    let workspace = read(&root.join("Cargo.toml"));
    assert!(workspace.contains("crates/*"), "crates are not members");
    assert!(workspace.contains("xtask"), "xtask is not a member");
}

/// T01 (#1), criterion 2: `oca-cli` declares the `oca` binary name.
#[test]
fn oca_cli_declares_the_oca_binary_name() {
    let manifest = read(&workspace_root().join("crates/oca-cli/Cargo.toml"));
    let binary = manifest
        .split("[[bin]]")
        .nth(1)
        .expect("oca-cli declares a [[bin]] target");

    assert!(
        binary.contains("name = \"oca\""),
        "the binary must be named oca"
    );
}

/// T01 (#1), criterion 3: no crate outside the responsibility table's allowance
/// pulls in an HTTP client.
#[test]
fn only_the_allowed_crates_declare_an_http_client_dependency() {
    let root = workspace_root();
    let mut offenders = Vec::new();

    for crate_name in WORKSPACE_CRATES {
        if HTTP_CLIENT_CRATES.contains(&crate_name) {
            continue;
        }
        let manifest = read(&root.join("crates").join(crate_name).join("Cargo.toml"));
        if manifest.contains("reqwest") || manifest.contains("hyper") {
            offenders.push(crate_name);
        }
    }

    assert!(
        offenders.is_empty(),
        "these crates must not depend on an HTTP client: {offenders:?}"
    );
}

/// T01 (#1), criterion 4: the toolchain is pinned to a specific stable version,
/// not to a floating channel.
#[test]
fn the_toolchain_pins_a_specific_stable_version() {
    let toolchain = read(&workspace_root().join("rust-toolchain.toml"));
    let channel = toolchain
        .lines()
        .find_map(|line| line.trim().strip_prefix("channel = "))
        .expect("rust-toolchain.toml declares a channel")
        .trim_matches('"');

    let parts: Vec<&str> = channel.split('.').collect();
    assert_eq!(parts.len(), 3, "channel {channel} is not a pinned x.y.z");
    for part in parts {
        assert!(
            part.chars().all(|character| character.is_ascii_digit()),
            "channel {channel} is not a pinned stable version"
        );
    }
}

/// T01 (#1), criterion 5: CI builds and tests the whole workspace on push.
#[test]
fn ci_builds_and_tests_the_whole_workspace_on_push() {
    let workflow = read(&workspace_root().join(".github/workflows/ci.yml"));

    assert!(workflow.contains("on:"), "the workflow declares triggers");
    assert!(workflow.contains("push"), "the workflow runs on push");
    assert!(workflow.contains("cargo build --workspace"));
    assert!(workflow.contains("cargo test --workspace"));
}
