//! Full-CLI proof that a server-side SSE connection death is transport loss.

use std::{process::Command, thread, time::Duration};

use oca_core::{ErrorCode, parse_error_envelope};
use oca_server::{ConnectOrStart, ServerRecord};
use oca_testkit::{FailureAction, FailureHttpServer, HttpResponse};
use serde_json::json;

#[test]
fn server_dying_after_valid_sse_exits_server_unreachable() {
    let busy = json!({
        "id": "evt_before_server_death",
        "type": "session.busy",
        "properties": {"sessionID": "ses_server_dies"}
    });
    let stream_prefix = format!("id: evt_before_server_death\ndata: {busy}\n\n");
    let actions = [
        response(200, "application/json", br#"[{"name":"impl"}]"#),
        response(200, "application/json", br#"{"id":"ses_server_dies"}"#),
        FailureAction::RespondThenDrop(HttpResponse::new(
            200,
            [("content-type", "text/event-stream")],
            [stream_prefix.into_bytes()],
        )),
        response(204, "text/plain", b""),
        FailureAction::EchoPrompt {
            session_id: "ses_server_dies".to_owned(),
        },
        response(200, "application/json", b"[]"),
        response(200, "application/json", b"[]"),
    ];
    let server =
        FailureHttpServer::bind("127.0.0.1:0", actions).expect("killed-server listener binds");
    let port = server
        .local_addr()
        .expect("killed-server listener address")
        .port();
    let home = tempfile::tempdir().expect("temporary home");
    prepare_home(home.path(), port);
    let server = thread::spawn(move || server.serve().expect("failure script completes"));

    let output = Command::new(env!("CARGO_BIN_EXE_oca"))
        .args([
            "--json",
            "luna:h",
            "--headless",
            "survive",
            "server",
            "death",
        ])
        .env("HOME", home.path())
        .current_dir(home.path())
        .output()
        .expect("oca process runs");
    let requests = server.join().expect("killed-server thread exits");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(5),
        "server death must use exit 5; stderr: {stderr}"
    );
    let error = parse_error_envelope(stderr.trim())
        .unwrap_or_else(|parse| panic!("invalid error envelope ({parse}): {stderr:?}"));
    assert_eq!(error.code(), ErrorCode::ServerUnreachable.as_str());
    assert!(
        stderr.contains("SSE source failed: error decoding response body"),
        "the real response-body failure must remain visible: {stderr}"
    );
    assert_eq!(
        requests
            .iter()
            .map(|request| route(&request.path))
            .collect::<Vec<_>>(),
        [
            "agents",
            "session",
            "event",
            "prompt_async",
            "messages",
            "messages",
            "messages",
        ]
    );
}

fn response(status: u16, content_type: &str, body: &[u8]) -> FailureAction {
    FailureAction::Respond(HttpResponse::new(
        status,
        [("content-type", content_type)],
        [body.to_vec()],
    ))
}

fn prepare_home(home: &std::path::Path, port: u16) {
    let state = home.join(".oca");
    std::fs::create_dir(&state).expect("state directory");
    std::fs::write(state.join("config.toml"), "").expect("default config");
    ConnectOrStart::new(&state, port, [], Duration::from_secs(1))
        .write_record(&ServerRecord::new(
            port,
            installed_opencode_version(),
            environment_hash_for(home),
        ))
        .expect("server record");
}

fn route(path: &str) -> &'static str {
    let path = path.split_once('?').map_or(path, |(path, _)| path);
    if path == "/agent" {
        "agents"
    } else if path == "/session" {
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
