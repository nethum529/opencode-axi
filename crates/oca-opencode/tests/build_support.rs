#[path = "../build_support.rs"]
mod build_support;

use std::{fs, path::Path};

use build_support::{
    generate_client, generated_file, load_pinned_snapshot, verify_snapshot_hash,
    write_generated_file,
};

fn temporary_directory(name: &str) -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "oca-opencode-build-support-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).expect("temporary directory is created");
    directory
}

#[test]
fn verifies_the_committed_snapshot() {
    let workspace_directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let snapshot =
        load_pinned_snapshot(&workspace_directory).expect("the committed snapshot loads");

    verify_snapshot_hash(&snapshot).expect("the committed snapshot hash matches");
}

#[test]
fn rejects_a_snapshot_tampered_without_a_digest_update() {
    let root = temporary_directory("tampered");
    let snapshot_directory = root.join("compatibility/opencode/1.18.10");
    fs::create_dir_all(&snapshot_directory).expect("snapshot directory is created");
    fs::write(snapshot_directory.join("openapi.json"), b"tampered").expect("snapshot is written");
    fs::write(
        snapshot_directory.join("snapshot.toml"),
        "sha256 = \"d5a1bc7501cbd57d2fbd3303120a99d5a063346d82865ed0f2c218bd9f8593fc\"\n",
    )
    .expect("metadata is written");

    let snapshot = load_pinned_snapshot(&root).expect("tampered snapshot still loads");
    let error = verify_snapshot_hash(&snapshot).expect_err("tampered snapshot is rejected");

    assert!(
        error.contains("SHA-256 mismatch"),
        "unexpected error: {error}"
    );
}

#[test]
fn resolves_the_generated_file_beneath_out_dir() {
    let out_directory = temporary_directory("out");
    let generated = generated_file(&out_directory);

    assert_eq!(generated.parent(), Some(out_directory.as_path()));
    assert_eq!(
        generated.file_name().and_then(|name| name.to_str()),
        Some("opencode_client.rs")
    );
}

#[test]
fn generates_only_the_facade_operations() {
    let workspace_directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let snapshot =
        load_pinned_snapshot(&workspace_directory).expect("the committed snapshot loads");
    let generated = generate_client(&snapshot).expect("the OpenAPI client generates");

    assert!(
        generated.contains("pub async fn session_prompt_async"),
        "generated client must retain the facade operations"
    );
    assert!(
        !generated.contains("pub async fn session_prompt<'"),
        "generated client must omit non-facade operations"
    );
}

#[test]
fn writes_generated_source_only_at_the_out_dir_destination() {
    let out_directory = temporary_directory("write");
    let generated = write_generated_file(&out_directory, "pub struct Generated;")
        .expect("generated source is written");

    assert_eq!(generated, generated_file(&out_directory));
    assert_eq!(
        fs::read_to_string(&generated).unwrap(),
        "pub struct Generated;"
    );
    assert!(!out_directory.join("../opencode_client.rs").exists());
}
