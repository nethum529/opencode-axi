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
fn end_to_end_foreground_has_one_turn_one_terminal_and_one_golden_final_result() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fake server binds");
    let port = listener.local_addr().expect("fake server address").port();
    let server = thread::spawn(move || serve_foreground(listener));

    let home = tempfile::tempdir().expect("temporary home");
    let state = home.path().join(".oca");
    std::fs::create_dir(&state).expect("state directory");
    std::fs::write(
        state.join("server.json"),
        serde_json::to_vec(&json!({
            "port": port,
            "version": "1.18.10",
            "environment_hash": "fake"
        }))
        .unwrap(),
    )
    .expect("server record");

    let output = Command::new(env!("CARGO_BIN_EXE_oca"))
        .args(["luna:h", "--headless", "implement", "the", "ticket"])
        .env("HOME", home.path())
        .current_dir(home.path())
        .output()
        .expect("oca runs");
    let trace = server.join().expect("fake server completes");

    assert!(
        output.status.success(),
        "foreground failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert_eq!(
        trace
            .iter()
            .map(|request| route(&request.path))
            .collect::<Vec<_>>(),
        ["session", "event", "prompt_async", "messages", "messages"]
    );

    let create = &trace[0].body;
    let permission = create["permission"].as_array().expect("permission ruleset");
    assert_eq!(permission.len(), 5);
    assert!(permission.iter().all(|rule| rule["action"] == "deny"));

    let prompt = &trace[2].body;
    let message_id = prompt["messageID"].as_str().expect("caller message id");
    assert!(is_opencode_message_id(message_id));
    assert!(!message_id.starts_with("msg_oca_"));
    assert_eq!(prompt["variant"], "high");
    assert_eq!(prompt["agent"], "impl");
    assert_eq!(prompt["parts"].as_array().unwrap().len(), 1);

    let stdout = String::from_utf8(output.stdout).expect("UTF-8 output");
    let mut lines = stdout.lines();
    let ack = lines.next().expect("ack line");
    let reference = ack.split_whitespace().next().expect("ack ref");
    assert_eq!(ack, format!("{reference} running openai/gpt-5.6-luna:high"));
    let final_output = lines.collect::<Vec<_>>().join("\n") + "\n";
    let golden = include_str!("../../oca-display/tests/goldens/completion-direct.toon")
        .replace("w00002", reference);
    assert_eq!(final_output, golden);

    let refs: Vec<Value> = serde_json::from_slice(
        &std::fs::read(state.join("refs.json")).expect("ref record was written"),
    )
    .expect("refs JSON");
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0]["id"], reference);
    assert_eq!(refs[0]["session_id"], "ses_target");
    assert_eq!(refs[0]["message_id"], message_id);
}

fn serve_foreground(listener: TcpListener) -> Vec<CapturedRequest> {
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
                    r#"{"id":"ses_target"}"#,
                );
            }
            1 => {
                assert_eq!(request.path, "/event");
                let event = concat!(
                    "id: evt_terminal\n",
                    "event: session.idle\n",
                    "data: {\"sessionID\":\"ses_target\"}\n\n"
                );
                write_response(&mut stream, "200 OK", "text/event-stream", event);
            }
            2 => {
                assert_eq!(request.path, "/session/ses_target/prompt_async");
                message_id = request.body["messageID"].as_str().map(ToOwned::to_owned);
                write_response(&mut stream, "204 No Content", "text/plain", "");
            }
            3 => {
                assert_eq!(request.path, "/session/ses_target/message");
                write_response(&mut stream, "200 OK", "application/json", "[]");
            }
            4 => {
                assert_eq!(request.path, "/session/ses_target/message");
                let body = json!([{
                    "info": {
                        "id": "msg_f9a4a7b00001BBBBBBBBBBBBBB",
                        "sessionID": "ses_target",
                        "role": "assistant",
                        "parentID": message_id.as_deref().expect("prompt preceded terminal read"),
                        "structured": {"status":"done","files":[],"note":"Done."}
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
