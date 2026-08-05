use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Output},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use oca_core::is_opencode_message_id;
use oca_server::{ConnectOrStart, ServerRecord};
use oca_state::{RefPatch, RefRecord, RefState, RefStore, RefStorePaths};
use serde_json::Value;

struct CapturedRequest {
    path: String,
    body: Value,
}

#[test]
fn message_on_running_session_is_worker_busy_with_zero_requests() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fake server binds");
    let port = listener.local_addr().expect("fake server address").port();
    listener
        .set_nonblocking(true)
        .expect("listener becomes nonblocking");
    let home = prepared_home(port, RefState::Running, "high");

    let output = run_oca(home.path(), ["m", "w4f2a1", "continue"]);

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("stderr is utf-8");
    assert!(stderr.contains("code: worker_busy"), "{stderr}");
    assert!(
        stderr.contains("oca q w4f2a1"),
        "busy recovery names the concrete queue command: {stderr}"
    );
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "busy rejection must not send any HTTP request"
    );
}

#[test]
fn message_on_terminal_sessions_reuses_the_session_and_sends_full_turn_context() {
    for state in [
        RefState::Idle,
        RefState::Blocked,
        RefState::Partial,
        RefState::Done,
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fake server binds");
        let port = listener.local_addr().expect("fake server address").port();
        let server = thread::spawn(move || serve_prompt(listener, "204 No Content", ""));
        let home = prepared_home(port, state, "high");

        let output = run_oca(
            home.path(),
            ["m", "w4f2a1", "use", "the", "prior", "analysis"],
        );
        let request = server.join().expect("fake server completes");

        assert_success(&output);
        assert_eq!(request.path, "/session/ses_prior_context/prompt_async");
        assert_eq!(request.body["agent"], "impl");
        assert_eq!(request.body["variant"], "high");
        assert_eq!(request.body["model"]["providerID"], "openai");
        assert_eq!(request.body["model"]["modelID"], "gpt-5.6-luna");
        assert_eq!(
            request.body["parts"],
            serde_json::json!([{ "type": "text", "text": "use the prior analysis" }])
        );
        assert_eq!(request.body["format"]["type"], "json_schema");
        assert!(request.body.get("delivery").is_none());
        assert_eq!(
            stored_record(home.path()).last_state,
            Some(RefState::Running)
        );
    }
}

#[test]
fn message_effort_override_applies_now_and_persists_for_later_turns() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fake server binds");
    let port = listener.local_addr().expect("fake server address").port();
    let server = thread::spawn(move || {
        [
            serve_one_prompt(&listener, "204 No Content", ""),
            serve_one_prompt(&listener, "204 No Content", ""),
        ]
    });
    let home = prepared_home(port, RefState::Done, "high");

    let first = run_oca(
        home.path(),
        ["m", "w4f2a1", "-e", "low", "first", "continuation"],
    );
    assert_success(&first);
    assert_eq!(stored_record(home.path()).effort.as_deref(), Some("low"));
    ref_store(home.path())
        .patch(
            "w4f2a1",
            RefPatch::default().with_last_state(RefState::Done),
        )
        .expect("test observes a terminal boundary");

    let second = run_oca(home.path(), ["m", "w4f2a1", "later", "continuation"]);
    assert_success(&second);
    let requests = server.join().expect("fake server completes");

    assert_eq!(requests[0].body["variant"], "low");
    assert_eq!(requests[1].body["variant"], "low");
    assert_eq!(stored_record(home.path()).effort.as_deref(), Some("low"));
}

#[test]
fn queue_sends_queue_delivery_returns_on_acceptance_without_state_or_effort_change() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fake server binds");
    let port = listener.local_addr().expect("fake server address").port();
    let server = thread::spawn(move || serve_prompt(listener, "204 No Content", ""));
    let home = prepared_home(port, RefState::Running, "high");

    let output = run_oca(home.path(), ["q", "w4f2a1", "after", "this", "turn"]);
    let request = server.join().expect("fake server completes");

    assert_success(&output);
    assert_eq!(request.path, "/session/ses_prior_context/prompt_async");
    assert_eq!(request.body["delivery"], "queue");
    assert_eq!(request.body["variant"], "high");
    assert_eq!(
        String::from_utf8(output.stdout).expect("stdout is utf-8"),
        "w4f2a1 queued openai/gpt-5.6-luna:high\n"
    );
    let record = stored_record(home.path());
    assert_eq!(record.last_state, Some(RefState::Running));
    let patched_message_id = record
        .message_id
        .as_deref()
        .expect("queue patches message_id");
    assert_ne!(patched_message_id, "msg_prior_turn");
    assert!(is_opencode_message_id(patched_message_id));
    assert_eq!(record.effort.as_deref(), Some("high"));
}

#[test]
fn queue_on_idle_session_leaves_state_unchanged_so_message_can_follow() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fake server binds");
    let port = listener.local_addr().expect("fake server address").port();
    let server = thread::spawn(move || {
        [
            serve_one_prompt(&listener, "204 No Content", ""),
            serve_one_prompt(&listener, "204 No Content", ""),
        ]
    });
    let home = prepared_home(port, RefState::Idle, "high");

    let queued = run_oca(home.path(), ["q", "w4f2a1", "first", "message"]);
    assert_success(&queued);
    assert_eq!(stored_record(home.path()).last_state, Some(RefState::Idle));

    let message = run_oca(home.path(), ["m", "w4f2a1", "follow-up", "message"]);
    assert_success(&message);
    assert_eq!(
        stored_record(home.path()).last_state,
        Some(RefState::Running)
    );

    let requests = server.join().expect("fake server completes");
    assert_eq!(requests[0].body["delivery"], "queue");
    assert!(requests[1].body.get("delivery").is_none());
}

#[test]
fn abort_posts_once_and_marks_the_ref_aborted_without_display_work() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fake server binds");
    let port = listener.local_addr().expect("fake server address").port();
    let server = thread::spawn(move || serve_prompt(listener, "200 OK", "true"));
    let home = prepared_home(port, RefState::Running, "high");

    let output = run_oca(home.path(), ["k", "w4f2a1"]);
    let request = server.join().expect("fake server completes");

    assert_success(&output);
    assert_eq!(request.path, "/session/ses_prior_context/abort");
    assert_eq!(
        String::from_utf8(output.stdout).expect("stdout is utf-8"),
        "w4f2a1 aborted openai/gpt-5.6-luna:high\n"
    );
    assert_eq!(
        stored_record(home.path()).last_state,
        Some(RefState::Aborted)
    );
}

#[test]
fn concurrent_messages_are_serialized_and_never_create_parallel_turns() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fake server binds");
    let port = listener.local_addr().expect("fake server address").port();
    let home = prepared_home(port, RefState::Done, "high");
    let (admitted_tx, admitted_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("first message arrives");
        let first = read_request(&mut stream);
        admitted_tx.send(()).expect("signal first admission");
        release_rx.recv().expect("release first response");
        write_response(&mut stream, "204 No Content", "");
        listener
            .set_nonblocking(true)
            .expect("listener becomes nonblocking");
        let deadline = Instant::now() + Duration::from_millis(300);
        loop {
            match listener.accept() {
                Ok(_) => panic!("a parallel message reached the server"),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        break;
                    }
                    thread::yield_now();
                }
                Err(error) => panic!("listener failed: {error}"),
            }
        }
        first
    });

    let first_home = home.path().to_owned();
    let first = thread::spawn(move || run_oca(&first_home, ["m", "w4f2a1", "first"]));
    admitted_rx.recv().expect("first request reaches server");
    let second_home = home.path().to_owned();
    let second = thread::spawn(move || run_oca(&second_home, ["m", "w4f2a1", "second"]));
    thread::sleep(Duration::from_millis(100));
    release_tx.send(()).expect("release first request");

    let first = first.join().expect("first process completes");
    let second = second.join().expect("second process completes");
    let first_request = server.join().expect("fake server completes");

    assert_success(&first);
    assert_eq!(first_request.body["parts"][0]["text"], "first");
    assert_eq!(second.status.code(), Some(1));
    let stderr = String::from_utf8(second.stderr).expect("stderr is utf-8");
    assert!(stderr.contains("code: worker_busy"), "{stderr}");
}

fn prepared_home(port: u16, state: RefState, effort: &str) -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("temporary home");
    let state_directory = home.path().join(".oca");
    std::fs::create_dir(&state_directory).expect("state directory");
    std::fs::write(state_directory.join("config.toml"), "").expect("default config");
    ref_store(home.path())
        .insert(RefRecord {
            id: "w4f2a1".to_owned(),
            session_id: "ses_prior_context".to_owned(),
            message_id: Some("msg_prior_turn".to_owned()),
            alias: Some("luna".to_owned()),
            effort: Some(effort.to_owned()),
            role: Some("impl".to_owned()),
            cwd: Some(home.path().display().to_string()),
            last_state: Some(state),
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
        .expect("seed ref");
    ConnectOrStart::new(
        &state_directory,
        port,
        [],
        std::time::Duration::from_secs(1),
    )
    .write_record(&ServerRecord::new(port, "1.18.10", "test"))
    .expect("server record");
    home
}

fn ref_store(home: &std::path::Path) -> RefStore {
    RefStore::with_paths(RefStorePaths::in_directory(home.join(".oca")))
}

fn stored_record(home: &std::path::Path) -> RefRecord {
    ref_store(home)
        .resolve("w4f2a1")
        .expect("read ref")
        .expect("stored ref")
}

fn run_oca<const N: usize>(home: &std::path::Path, arguments: [&str; N]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oca"))
        .args(arguments)
        .env("HOME", home)
        .current_dir(home)
        .output()
        .expect("oca runs")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "oca failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
}

fn serve_prompt(listener: TcpListener, status: &str, body: &str) -> CapturedRequest {
    serve_one_prompt(&listener, status, body)
}

fn serve_one_prompt(listener: &TcpListener, status: &str, body: &str) -> CapturedRequest {
    let (mut stream, _) = listener.accept().expect("fake accepts request");
    let request = read_request(&mut stream);
    write_response(&mut stream, status, body);
    request
}

fn read_request(stream: &mut TcpStream) -> CapturedRequest {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut buffer).expect("request headers");
        assert_ne!(read, 0, "request closed before headers arrived");
        request.extend_from_slice(&buffer[..read]);
    }
    let headers_end = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("complete request headers")
        + 4;
    let headers = String::from_utf8_lossy(&request[..headers_end]);
    let content_length = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length: "))
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_default();
    while request.len() - headers_end < content_length {
        let read = stream.read(&mut buffer).expect("request body");
        assert_ne!(read, 0, "request closed before body arrived");
        request.extend_from_slice(&buffer[..read]);
    }
    let request = String::from_utf8(request).expect("request is utf-8");
    let (headers, body) = request.split_once("\r\n\r\n").expect("request shape");
    let path = headers
        .lines()
        .next()
        .expect("request line")
        .split_whitespace()
        .nth(1)
        .expect("request path")
        .to_owned();
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(body).expect("request body is JSON")
    };
    CapturedRequest { path, body }
}

fn write_response(stream: &mut TcpStream, status: &str, body: &str) {
    let content_type = if body.is_empty() {
        "text/plain"
    } else {
        "application/json"
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
    .expect("fake response");
}
