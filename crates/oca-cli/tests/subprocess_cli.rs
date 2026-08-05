//! Acceptance coverage for the user-visible `oca` process contract.

use std::{
    collections::BTreeMap,
    process::{Command, Output},
    thread,
};

use oca_core::{ErrorCode, exit, parse_error_envelope};
use oca_server::{ConnectOrStart, ServerRecord};
use oca_state::{RefRecord, RefStore, RefStorePaths};
use oca_testkit::{
    HttpRequest, HttpResponse, RecordedExchange, Recording, ReplayHttpServer, ReplayServer,
};

fn run_oca(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oca"))
        .args(arguments)
        .output()
        .expect("Cargo-built oca executable must run")
}

fn run_oca_in_home(home: &std::path::Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oca"))
        .env("HOME", home)
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
fn follow_exit_codes_are_frozen_at_the_real_process_boundary() {
    for (status, expected_exit) in [("done", exit::SUCCESS), ("blocked", exit::FOLLOW_BLOCKED)] {
        let fixture = FollowFixture::terminal(status);
        let output = run_oca_in_home(fixture.home.path(), &["f", "w4f2a1"]);
        fixture
            .server
            .join()
            .expect("fake server thread must finish");

        assert_eq!(output.status.code(), Some(expected_exit));
        assert!(output.stderr.is_empty());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(stdout.matches("oca wake").count(), 1);
        assert_eq!(stdout.matches("status:").count(), 1);
        assert!(stdout.starts_with(&format!("oca wake ref=w4f2a1 state={status}\n")));
    }

    let timeout = FollowFixture::disconnected();
    let refs_before = std::fs::read(timeout.home.path().join(".oca/refs.json")).unwrap();
    let output = run_oca_in_home(timeout.home.path(), &["f", "w4f2a1", "-t", "1"]);
    timeout
        .server
        .join()
        .expect("fake server thread must finish");
    assert_eq!(output.status.code(), Some(exit::FOLLOW_TIMEOUT));
    assert_eq!(output.stdout, b"oca wake ref=w4f2a1 state=timeout\n");
    assert!(output.stderr.is_empty());
    assert_eq!(
        std::fs::read(timeout.home.path().join(".oca/refs.json")).unwrap(),
        refs_before,
        "exit 4 must not mark the ref aborted or otherwise mutate it"
    );

    let home = prepared_home(unused_loopback_port());
    let refs_before = std::fs::read(home.path().join(".oca/refs.json")).unwrap();
    let output = run_oca_in_home(home.path(), &["--json", "f", "w4f2a1"]);
    assert_eq!(output.status.code(), Some(exit::SERVER_UNREACHABLE));
    assert!(output.stdout.is_empty());
    let error = parse_error_envelope(String::from_utf8_lossy(&output.stderr).trim()).unwrap();
    assert_eq!(error.code(), "server_unreachable");
    assert_eq!(error.reference(), Some("w4f2a1"));
    assert_eq!(
        std::fs::read(home.path().join(".oca/refs.json")).unwrap(),
        refs_before,
        "exit 5 must not mark the ref failed or otherwise mutate it"
    );
}

struct FollowFixture {
    home: tempfile::TempDir,
    server: thread::JoinHandle<()>,
}

impl FollowFixture {
    fn terminal(status: &str) -> Self {
        let sse = terminal_sse(status);
        Self::with_sse(vec![sse.into_bytes()])
    }

    fn disconnected() -> Self {
        Self::with_sse(Vec::new())
    }

    fn with_sse(chunks: Vec<Vec<u8>>) -> Self {
        let recording = Recording {
            exchanges: vec![
                RecordedExchange {
                    request: HttpRequest::new("GET", "/event", no_headers(), []),
                    response: HttpResponse::new(
                        200,
                        [("content-type", "text/event-stream")],
                        chunks,
                    ),
                },
                RecordedExchange {
                    request: HttpRequest::new(
                        "GET",
                        "/session/ses_target/message",
                        no_headers(),
                        [],
                    ),
                    response: HttpResponse::new(
                        200,
                        [("content-type", "application/json")],
                        [b"[]".to_vec()],
                    ),
                },
            ],
        };
        let replay = ReplayServer::pinned(recording).expect("pinned fake server surface");
        let mut server = ReplayHttpServer::bind("127.0.0.1:0", replay).expect("bind fake server");
        let port = server.local_addr().unwrap().port();
        let home = prepared_home(port);
        let server = thread::spawn(move || {
            server.serve_one().expect("serve event subscription");
            server.serve_one().expect("serve one-shot reconciliation");
        });
        Self { home, server }
    }
}

fn prepared_home(port: u16) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let state = home.path().join(".oca");
    std::fs::create_dir(&state).unwrap();
    std::fs::write(state.join("config.toml"), "").unwrap();
    let store = RefStore::with_paths(RefStorePaths::in_directory(&state));
    store
        .insert(RefRecord {
            id: "w4f2a1".to_owned(),
            session_id: "ses_target".to_owned(),
            message_id: Some("msg_dispatch".into()),
            alias: Some("luna".to_owned()),
            effort: Some("high".to_owned()),
            role: Some("impl".to_owned()),
            cwd: Some(home.path().display().to_string()),
            last_state: Some(oca_state::RefState::Running),
            repo: None,
            spawner_tag: None,
            worktree: None,
            branch: None,
            commit: None,
            commit_subject: None,
            tombstoned: false,
        })
        .unwrap();
    ConnectOrStart::new(&state, port, [], std::time::Duration::from_secs(1))
        .write_record(&ServerRecord::new(port, "1.18.10", "test"))
        .unwrap();
    home
}

fn terminal_sse(status: &str) -> String {
    let message = serde_json::json!({
        "id": "evt_message",
        "type": "message.updated",
        "properties": {
            "sessionID": "ses_target",
            "info": {
                "id": "msg_assistant",
                "sessionID": "ses_target",
                "role": "assistant",
                "parentID": "msg_dispatch",
                "time": { "created": 1, "completed": 2 },
                "structured": {
                    "status": status,
                    "files": [],
                    "note": "Returned one attributed terminal report for the process-level acceptance path without duplicating any session event. Verified the follow command preserves the expected stable output and exit behavior."
                }
            }
        }
    });
    let idle = serde_json::json!({
        "id": "evt_idle",
        "type": "session.idle",
        "properties": { "sessionID": "ses_target" }
    });
    let duplicate_idle = serde_json::json!({
        "id": "evt_idle_duplicate",
        "type": "session.idle",
        "properties": { "sessionID": "ses_target" }
    });
    format!(
        "id: evt_message\ndata: {message}\n\nid: evt_idle\ndata: {idle}\n\nid: evt_idle_duplicate\ndata: {duplicate_idle}\n\n"
    )
}

fn no_headers() -> BTreeMap<String, String> {
    BTreeMap::new()
}

fn unused_loopback_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
