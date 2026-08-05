//! Fixture-driven acceptance coverage for transport cuts and HTTP 429 handling.

use std::{process::Command, thread};

use oca_core::{ErrorCode, exit, parse_error_envelope};
use oca_server::{ConnectOrStart, ServerRecord};
use oca_state::{RefRecord, RefState, RefStore, RefStorePaths};
use oca_testkit::{FailureAction, FailureHttpServer, HttpResponse};

#[test]
fn pre_transmit_connection_cut_retries_session_creation_once_and_prompts_once() {
    let (home, server) = fixture([
        FailureAction::DropBeforeRequest,
        respond(200, "application/json", br#"{"id":"ses_recovered"}"#),
        respond(200, "text/event-stream", b""),
        respond(204, "text/plain", b""),
    ]);

    let output = run(&home, ["luna:h", "-b", "--headless", "do", "the", "work"]);
    let requests = server.join().expect("failure server thread");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert_eq!(routes(&requests), ["session", "event", "prompt_async"]);
    assert_eq!(
        routes(&requests)
            .iter()
            .filter(|route| **route == "prompt_async")
            .count(),
        1
    );
    let records = stored_refs(&home);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].session_id, "ses_recovered");
    assert_eq!(records[0].last_state, Some(RefState::Running));
}

#[test]
fn post_transmit_pre_response_cut_never_replays_and_persists_unknown_ref() {
    let (home, server) = fixture([
        respond(200, "application/json", br#"{"id":"ses_uncertain"}"#),
        respond(200, "text/event-stream", b""),
        FailureAction::DropAfterRequest,
    ]);

    let output = run(
        &home,
        ["--json", "luna:h", "-b", "--headless", "one", "shot"],
    );
    let requests = server.join().expect("failure server thread");

    assert_eq!(output.status.code(), Some(exit::FAILURE));
    assert!(
        output.stdout.is_empty(),
        "an uncertain prompt is never acknowledged"
    );
    let error = parsed_error(&output);
    assert_eq!(error.code(), ErrorCode::PromptUncertain.as_str());
    assert!(error.reference().is_some());
    assert_eq!(routes(&requests), ["session", "event", "prompt_async"]);
    assert_eq!(
        routes(&requests)
            .iter()
            .filter(|route| **route == "prompt_async")
            .count(),
        1
    );
    let records = stored_refs(&home);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].last_state, Some(RefState::Unknown));
    assert_eq!(records[0].id, error.reference().unwrap());

    let reference = error.reference().unwrap().to_owned();
    let recovery = FailureHttpServer::bind(
        "127.0.0.1:0",
        [
            respond(200, "text/event-stream", b""),
            respond(200, "application/json", b"[]"),
        ],
    )
    .expect("recovery server binds");
    replace_server_record(&home, recovery.local_addr().unwrap().port());
    let recovery = thread::spawn(move || recovery.serve().expect("recovery script completes"));
    let follow = run(&home, ["--json", "f", &reference]);
    let recovery_requests = recovery.join().expect("recovery server thread");

    let recovered_error = parsed_error(&follow);
    assert_eq!(recovered_error.code(), ErrorCode::PromptUncertain.as_str());
    assert!(
        recovered_error
            .help()
            .contains(&format!("oca m {reference}")),
        "the explicit resend command must be named"
    );
    assert_eq!(routes(&recovery_requests), ["event", "messages"]);
    assert!(
        !routes(&recovery_requests).contains(&"prompt_async"),
        "fresh reconciliation must never resend the uncertain prompt"
    );
}

#[test]
fn fake_500_after_worktree_prepare_leaves_no_ref_intent_branch_or_worktree() {
    let (home, server) = fixture([respond(500, "application/json", br#"{"error":"injected"}"#)]);
    init_repository(home.path());

    let output = run(
        &home,
        [
            "luna:h",
            "-w",
            "-b",
            "--headless",
            "prepare",
            "then",
            "fail",
        ],
    );
    let requests = server.join().expect("failure server thread");

    assert!(!output.status.success());
    assert_eq!(routes(&requests), ["session"]);
    assert!(stored_refs(&home).is_empty());
    assert!(intent_json_files(home.path()).is_empty());
    assert!(!git_branches(home.path()).contains("oca/"));
    assert!(
        !home.path().join(".oca/wt").exists()
            || std::fs::read_dir(home.path().join(".oca/wt"))
                .unwrap()
                .next()
                .is_none()
    );
}

#[test]
fn killed_worktree_ready_phase_is_cleaned_by_a_fresh_ls() {
    let home = tempfile::tempdir().expect("temporary home");
    std::fs::create_dir(home.path().join(".oca")).unwrap();
    std::fs::write(home.path().join(".oca/config.toml"), "").unwrap();
    init_repository(home.path());

    let killed = Command::new(env!("CARGO_BIN_EXE_oca"))
        .args([
            "luna:h",
            "-w",
            "-b",
            "--headless",
            "crash",
            "after",
            "worktree",
        ])
        .env("HOME", home.path())
        .env("OCA_FAILPOINT", "worktree_ready")
        .current_dir(home.path())
        .output()
        .unwrap();
    assert_eq!(killed.status.code(), Some(86));
    assert_eq!(intent_json_files(home.path()).len(), 1);
    assert!(git_branches(home.path()).contains("oca/"));

    let listed = run(&home, ["ls", "--all", "--json"]);
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let list: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid list output ({error}): {:?}",
            String::from_utf8_lossy(&listed.stdout)
        )
    });
    assert_eq!(list["total"], 0);
    assert!(intent_json_files(home.path()).is_empty());
    assert!(!git_branches(home.path()).contains("oca/"));
}

#[test]
fn killed_session_created_phase_is_queried_without_a_prompt() {
    let (home, server) = fixture([
        respond(
            200,
            "application/json",
            br#"{"id":"ses_created_before_crash"}"#,
        ),
        respond(200, "application/json", b"[]"),
    ]);

    let killed = Command::new(env!("CARGO_BIN_EXE_oca"))
        .args(["luna:h", "-b", "--headless", "stop", "before", "prompt"])
        .env("HOME", home.path())
        .env("OCA_FAILPOINT", "session_created")
        .current_dir(home.path())
        .output()
        .unwrap();
    assert_eq!(killed.status.code(), Some(86));

    let listed = run(&home, ["ls", "--all", "--json"]);
    let requests = server.join().expect("failure server thread");
    assert!(listed.status.success());
    let list: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(list["items"][0]["state"], "session_created");
    assert_eq!(routes(&requests), ["session", "messages"]);
    assert!(!routes(&requests).contains(&"prompt_async"));
}

#[test]
fn mid_sse_cut_keeps_the_admitted_prompt_exactly_once() {
    let (home, server) = fixture([
        respond(200, "application/json", br#"{"id":"ses_stream_cut"}"#),
        FailureAction::RespondThenDrop(HttpResponse::new(
            200,
            [("content-type", "text/event-stream")],
            [b": connected\n\n".to_vec()],
        )),
        respond(204, "text/plain", b""),
        respond(200, "application/json", b"[]"),
    ]);

    let output = run(&home, ["--json", "luna:h", "--headless", "stream", "once"]);
    let requests = server.join().expect("failure server thread");

    assert_eq!(output.status.code(), Some(exit::SERVER_UNREACHABLE));
    let error = parsed_error(&output);
    assert_eq!(error.code(), ErrorCode::ServerUnreachable.as_str());
    assert_eq!(
        routes(&requests),
        ["session", "event", "prompt_async", "messages"]
    );
    assert_eq!(
        routes(&requests)
            .iter()
            .filter(|route| **route == "prompt_async")
            .count(),
        1
    );
    let records = stored_refs(&home);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].last_state, Some(RefState::Running));
}

#[test]
fn every_429_is_terminal_rate_limited_with_retry_metadata_and_no_replay() {
    let (home, server) = fixture([FailureAction::Respond(HttpResponse::new(
        429,
        [("content-type", "application/json"), ("retry-after", "2")],
        [br#"{"error":"slow down"}"#.to_vec()],
    ))]);
    let output = run(&home, ["--json", "luna:h", "-b", "--headless", "limited"]);
    let requests = server.join().expect("failure server thread");
    assert_rate_limit(&output, Some(2_000));
    assert_eq!(routes(&requests), ["session"]);
    assert!(stored_refs(&home).is_empty());

    let (home, server) = fixture([
        respond(200, "application/json", br#"{"id":"ses_limited"}"#),
        respond(200, "text/event-stream", b""),
        FailureAction::Respond(HttpResponse::new(
            429,
            [
                ("content-type", "application/json"),
                ("x-ratelimit-reset-requests", "1750ms"),
            ],
            [br#"{"error":"provider limit"}"#.to_vec()],
        )),
    ]);
    let output = run(&home, ["--json", "luna:h", "-b", "--headless", "limited"]);
    let requests = server.join().expect("failure server thread");
    assert_rate_limit(&output, Some(1_750));
    assert_eq!(routes(&requests), ["session", "event", "prompt_async"]);
    assert_eq!(
        routes(&requests)
            .iter()
            .filter(|route| **route == "prompt_async")
            .count(),
        1
    );
    assert!(stored_refs(&home).is_empty());

    let (home, server) = fixture([FailureAction::Respond(HttpResponse::new(
        429,
        [("content-type", "application/json"), ("retry-after", "4")],
        [br#"{"error":"follow limited"}"#.to_vec()],
    ))]);
    RefStore::with_paths(RefStorePaths::in_directory(home.path().join(".oca")))
        .insert(RefRecord {
            id: "w4f2a1".to_owned(),
            session_id: "ses_follow_limited".to_owned(),
            message_id: Some("msg_dispatch".to_owned()),
            alias: Some("luna".to_owned()),
            effort: Some("high".to_owned()),
            role: Some("impl".to_owned()),
            cwd: Some(home.path().display().to_string()),
            last_state: Some(RefState::Running),
            repo: None,
            spawner_tag: None,
            worktree: None,
            branch: None,
            commit: None,
            commit_subject: None,
            display: None,
            herdr_tab: None,
            completion: None,
            tombstoned: false,
        })
        .expect("follow ref");
    let output = run(&home, ["--json", "f", "w4f2a1"]);
    let requests = server.join().expect("failure server thread");
    assert_rate_limit(&output, Some(4_000));
    assert_eq!(routes(&requests), ["event"]);
}

fn fixture(
    actions: impl IntoIterator<Item = FailureAction>,
) -> (
    tempfile::TempDir,
    thread::JoinHandle<Vec<oca_testkit::HttpRequest>>,
) {
    let server = FailureHttpServer::bind("127.0.0.1:0", actions).expect("failure server binds");
    let port = server.local_addr().expect("failure server address").port();
    let home = tempfile::tempdir().expect("temporary home");
    let state = home.path().join(".oca");
    std::fs::create_dir(&state).expect("state directory");
    std::fs::write(state.join("config.toml"), "").expect("default config");
    ConnectOrStart::new(&state, port, [], std::time::Duration::from_secs(1))
        .write_record(&ServerRecord::new(
            port,
            installed_opencode_version(),
            environment_hash_for(home.path()),
        ))
        .expect("server record");
    let server = thread::spawn(move || server.serve().expect("failure script completes"));
    (home, server)
}

fn respond(status: u16, content_type: &str, body: &[u8]) -> FailureAction {
    FailureAction::Respond(HttpResponse::new(
        status,
        [("content-type", content_type)],
        [body.to_vec()],
    ))
}

fn replace_server_record(home: &tempfile::TempDir, port: u16) {
    ConnectOrStart::new(
        home.path().join(".oca"),
        port,
        [],
        std::time::Duration::from_secs(1),
    )
    .write_record(&ServerRecord::new(
        port,
        installed_opencode_version(),
        environment_hash_for(home.path()),
    ))
    .unwrap();
}

fn init_repository(path: &std::path::Path) {
    for arguments in [
        vec!["init", "--quiet"],
        vec!["config", "user.name", "oca test"],
        vec!["config", "user.email", "oca@example.test"],
    ] {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(path)
                .args(arguments)
                .status()
                .unwrap()
                .success()
        );
    }
    std::fs::write(path.join("README.md"), "base\n").unwrap();
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["add", "README.md"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["commit", "--quiet", "-m", "base"])
            .status()
            .unwrap()
            .success()
    );
}

fn git_branches(path: &std::path::Path) -> String {
    String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["branch", "--list"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
}

fn intent_json_files(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    match std::fs::read_dir(home.join(".oca/intents")) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("could not list intents: {error}"),
    }
}

fn run<const N: usize>(home: &tempfile::TempDir, arguments: [&str; N]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_oca"))
        .args(arguments)
        .env("HOME", home.path())
        .current_dir(home.path())
        .output()
        .expect("oca process runs")
}

fn routes(requests: &[oca_testkit::HttpRequest]) -> Vec<&'static str> {
    requests
        .iter()
        .map(|request| {
            let path = request
                .path
                .split_once('?')
                .map_or(request.path.as_str(), |pair| pair.0);
            if path == "/session" {
                "session"
            } else if path == "/event" {
                "event"
            } else if path.ends_with("/prompt_async") {
                "prompt_async"
            } else if path.ends_with("/message") {
                "messages"
            } else {
                "unknown"
            }
        })
        .collect()
}

fn stored_refs(home: &tempfile::TempDir) -> Vec<RefRecord> {
    match std::fs::read(home.path().join(".oca/refs.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).expect("valid refs JSON"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("could not read refs: {error}"),
    }
}

fn assert_rate_limit(output: &std::process::Output, retry_after_ms: Option<u64>) {
    assert_eq!(output.status.code(), Some(exit::FAILURE));
    assert!(output.stdout.is_empty());
    let error = parsed_error(output);
    assert_eq!(error.code(), ErrorCode::RateLimited.as_str());
    assert_eq!(error.retry_after_ms(), retry_after_ms);
}

fn parsed_error(output: &std::process::Output) -> oca_core::ErrorEnvelope {
    let stderr = String::from_utf8_lossy(&output.stderr);
    parse_error_envelope(stderr.trim())
        .unwrap_or_else(|error| panic!("invalid error envelope ({error}): {stderr:?}"))
}

fn installed_opencode_version() -> String {
    Command::new("opencode")
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| "1.18.10".to_owned())
}

fn environment_hash_for(home: &std::path::Path) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    for key in [
        "HOME",
        "OPENCODE_CONFIG",
        "OPENCODE_CONFIG_DIR",
        "PATH",
        "XDG_CONFIG_HOME",
    ] {
        hasher.update(key.as_bytes());
        hasher.update([0]);
        if key == "HOME" {
            hasher.update(home.as_os_str().to_string_lossy().as_bytes());
        } else if let Some(value) = std::env::var_os(key) {
            hasher.update(value.to_string_lossy().as_bytes());
        }
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}
