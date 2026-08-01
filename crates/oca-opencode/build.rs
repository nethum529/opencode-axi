mod build_support;

use std::{env, path::PathBuf};

fn main() {
    if let Err(error) = generate() {
        panic!("{error}");
    }
}

fn generate() -> Result<(), String> {
    let manifest_directory = PathBuf::from(
        env::var("CARGO_MANIFEST_DIR")
            .map_err(|error| format!("CARGO_MANIFEST_DIR is unavailable: {error}"))?,
    );
    let workspace_directory = manifest_directory.join("../..");
    let snapshot_directory = workspace_directory.join("compatibility/opencode/1.18.10");
    println!(
        "cargo::rerun-if-changed={}",
        snapshot_directory.join("openapi.json").display()
    );
    println!(
        "cargo::rerun-if-changed={}",
        snapshot_directory.join("snapshot.toml").display()
    );

    let snapshot = build_support::load_pinned_snapshot(&workspace_directory)?;
    build_support::verify_snapshot_hash(&snapshot)?;
    let generated = build_support::generate_client(&snapshot)?;
    let out_directory = PathBuf::from(
        env::var("OUT_DIR").map_err(|error| format!("OUT_DIR is unavailable: {error}"))?,
    );
    build_support::write_generated_file(&out_directory, &generated)?;
    Ok(())
}
