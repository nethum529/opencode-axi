use std::{
    io::{BufRead, Read, Write},
    net::{TcpListener, TcpStream},
    process::Command,
    thread,
};

use oca_core::is_opencode_message_id;
use serde_json::{Value, json};

struct CapturedRequest {
    path: String,
    body: Value,
}

#[test]
fn background_then_separate_follow_reconciles_an_already_finished_turn() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fake server binds");
    let port = listener.local_addr().expect("fake server address").port();
    let server = thread::spawn(move || serve_background_then_follow(listener));

    let home = tempfile::tempdir().expect("temporary home");
    let state = home.path().join(".oca");
    std::fs::create_dir(&state).expect("state directory");
    std::fs::write(
        state.join("server.json"),
        serde_json::to_vec(&json!({
            "port": port,
            "version": installed_opencode_version(),
            "environment_hash": environment_hash_for(home.path())
        }))
        .unwrap(),
    )
    .expect("server record");

    let background = Command::new(env!("CARGO_BIN_EXE_oca"))
        .args(["luna:h", "-b", "--headless", "finish", "immediately"])
        .env("HOME", home.path())
        .current_dir(home.path())
        .output()
        .expect("background oca runs");

    assert!(
        background.status.success(),
        "background dispatch failed: {}",
        String::from_utf8_lossy(&background.stderr)
    );
    assert!(background.stderr.is_empty());
    let acknowledgement = String::from_utf8(background.stdout).expect("UTF-8 acknowledgement");
    assert_eq!(acknowledgement.lines().count(), 1);
    let reference = acknowledgement
        .split_whitespace()
        .next()
        .expect("acknowledgement ref")
        .to_owned();
    assert_eq!(
        acknowledgement,
        format!("{reference} running openai/gpt-5.6-luna:high\n")
    );
    assert_eq!(
        std::fs::read(state.join("refs.lock")).expect("ref lock marker"),
        b"pending\n",
        "background dispatch transfers directory durability without a post-ack fsync"
    );

    // This is a new process after dispatch has already exited. The fake server
    // reports the turn as completed only through the one-shot messages read.
    let follow = Command::new(env!("CARGO_BIN_EXE_oca"))
        .args(["f", &reference])
        .env("HOME", home.path())
        .current_dir(home.path())
        .output()
        .expect("follow oca runs");
    let trace = server.join().expect("fake server completes");

    assert!(
        follow.status.success(),
        "follow failed: {}",
        String::from_utf8_lossy(&follow.stderr)
    );
    assert!(follow.stderr.is_empty());
    let follow_stdout = String::from_utf8(follow.stdout).expect("UTF-8 follow output");
    assert!(follow_stdout.starts_with(&format!("oca wake ref={reference} state=done\n")));
    assert!(follow_stdout.contains("status: done\n"));
    assert!(follow_stdout.ends_with(&format!("ref: {reference}\n")));
    assert!(
        std::fs::read(state.join("refs.lock"))
            .expect("ref lock after follow")
            .is_empty(),
        "follow's ref-store entry completes transferred durability"
    );

    assert_eq!(
        trace
            .iter()
            .map(|request| route(&request.path))
            .collect::<Vec<_>>(),
        ["session", "event", "prompt_async", "event", "messages"]
    );
    let message_id = trace[2].body["messageID"]
        .as_str()
        .expect("caller message id");
    assert!(is_opencode_message_id(message_id));

    let refs: Vec<Value> = serde_json::from_slice(
        &std::fs::read(state.join("refs.json")).expect("ref record was written"),
    )
    .expect("refs JSON");
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0]["id"], reference);
    assert_eq!(refs[0]["session_id"], "ses_background");
    assert_eq!(refs[0]["message_id"], message_id);
}

fn serve_background_then_follow(listener: TcpListener) -> Vec<CapturedRequest> {
    let mut captured = Vec::new();
    let mut message_id = None;
    for index in 0..5 {
        let (mut stream, _) = listener.accept().expect("fake accepts request");
        let request = read_request(&mut stream);
        match index {
            0 => {
                assert!(request.path.starts_with("/session?directory="));
                write_response(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    r#"{"id":"ses_background"}"#,
                );
            }
            1 | 3 => {
                assert_eq!(request.path, "/event");
                write_response(&mut stream, "200 OK", "text/event-stream", "");
            }
            2 => {
                assert_eq!(request.path, "/session/ses_background/prompt_async");
                message_id = request.body["messageID"].as_str().map(ToOwned::to_owned);
                write_response(&mut stream, "204 No Content", "text/plain", "");
            }
            4 => {
                assert_eq!(request.path, "/session/ses_background/message");
                let body = json!([{
                    "info": {
                        "id": "msg_assistant",
                        "sessionID": "ses_background",
                        "role": "assistant",
                        "parentID": message_id.as_deref().expect("prompt preceded follow"),
                        "time": { "created": 1, "completed": 2 },
                        "structured": {
                            "status": "done",
                            "files": [],
                            "note": "The already-finished background turn was reconciled."
                        }
                    },
                    "parts": []
                }]);
                write_response(&mut stream, "200 OK", "application/json", &body.to_string());
            }
            _ => unreachable!(),
        }
        captured.push(request);
    }
    captured
}

fn route(path: &str) -> &'static str {
    if path.starts_with("/session?") {
        "session"
    } else if path == "/event" {
        // Request order distinguishes dispatch ownership from follow ownership.
        "event"
    } else if path.ends_with("/prompt_async") {
        "prompt_async"
    } else if path.ends_with("/message") {
        "messages"
    } else {
        "unknown"
    }
}

fn read_request(stream: &mut TcpStream) -> CapturedRequest {
    let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone stream"));
    let mut request_line = String::new();
    reader.read_line(&mut request_line).expect("request line");
    let path = request_line
        .split_whitespace()
        .nth(1)
        .expect("request path")
        .to_owned();
    let mut content_length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect("request header");
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = value.trim().parse().expect("content length");
        }
    }
    let mut bytes = vec![0; content_length];
    reader.read_exact(&mut bytes).expect("request body");
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("JSON request body")
    };
    CapturedRequest { path, body }
}

fn write_response(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .expect("fake response");
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
