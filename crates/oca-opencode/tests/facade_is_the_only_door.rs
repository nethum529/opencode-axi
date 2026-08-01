//! Cut-level acceptance tests backfilled by the QA gate for T13 (#13) and
//! T10 (#10). These are source-level gates: they assert properties of the tree
//! that no runtime test can observe.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the workspace root resolves")
}

fn rust_sources(directory: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            rust_sources(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
}

/// T13 (#13), criterion 4: the facade is the only door. No crate other than
/// `oca-opencode` reaches the generated client's endpoint methods.
#[test]
fn no_crate_outside_oca_opencode_reaches_the_generated_client() {
    let root = workspace_root();
    let mut sources = Vec::new();
    for entry in fs::read_dir(root.join("crates")).expect("the crates directory exists") {
        let path = entry.expect("a readable crate directory").path();
        if path.file_name().is_some_and(|name| name == "oca-opencode") {
            continue;
        }
        rust_sources(&path, &mut sources);
    }
    rust_sources(&root.join("xtask"), &mut sources);

    let offenders: Vec<String> = sources
        .into_iter()
        .filter(|path| {
            let source = fs::read_to_string(path).unwrap_or_default();
            source.contains("generated::") || source.contains("oca_opencode::generated")
        })
        .map(|path| path.display().to_string())
        .collect();

    assert!(
        offenders.is_empty(),
        "these files bypass the facade: {offenders:?}"
    );
}

/// T13 (#13), criterion 5: no `steer` method exists in the facade or its tests.
/// Gate-0 verification found `delivery: steer` silently drops messages on the
/// legacy pipeline, so v1 must not carry one.
#[test]
fn no_steer_operation_exists_in_the_facade_or_its_tests() {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for relative in ["src/facade.rs", "src/lib.rs", "tests/facade.rs"] {
        let source = fs::read_to_string(crate_root.join(relative))
            .unwrap_or_else(|error| panic!("{relative} is readable, got {error}"));
        assert!(
            !source.contains("steer"),
            "{relative} must not mention a steer operation"
        );
    }
}

/// T10 (#10), criterion 1: the build script cannot reach the network, because
/// no build dependency can open a socket and the build source names no URL
/// scheme it could fetch from.
#[test]
fn the_build_script_declares_no_network_capable_dependency() {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = fs::read_to_string(crate_root.join("Cargo.toml")).expect("the manifest reads");
    let build_dependencies = manifest
        .split("[build-dependencies]")
        .nth(1)
        .expect("the manifest declares build dependencies")
        .split("\n[")
        .next()
        .expect("the build dependency table ends");

    for forbidden in [
        "reqwest",
        "hyper",
        "ureq",
        "curl",
        "tokio",
        "isahc",
        "attohttpc",
    ] {
        assert!(
            !build_dependencies.contains(forbidden),
            "{forbidden} would let the build script open a socket"
        );
    }

    for relative in ["build.rs", "build_support.rs"] {
        let source = fs::read_to_string(crate_root.join(relative))
            .unwrap_or_else(|error| panic!("{relative} is readable, got {error}"));
        assert!(
            !source.contains("http://") && !source.contains("https://"),
            "{relative} must not name a URL to fetch"
        );
    }
}
