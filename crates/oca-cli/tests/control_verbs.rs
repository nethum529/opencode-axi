use std::{
    io::{BufRead, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::net::{UnixListener, UnixStream},
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
fn continuations_on_a_tooled_incompatible_alias_fail_with_zero_requests() {
    for verb in ["m", "q"] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fake server binds");
        let port = listener.local_addr().expect("fake server address").port();
        listener
            .set_nonblocking(true)
            .expect("listener becomes nonblocking");
        let home = prepared_home_on_alias(port, RefState::Idle, "high", "flash");

        let output = run_oca(home.path(), [verb, "w4f2a1", "continue"]);

        assert_eq!(output.status.code(), Some(2), "`oca {verb}` exit");
        let stderr = String::from_utf8(output.stderr).expect("stderr is utf-8");
        assert!(
            stderr.contains("code: model_unsupported_tooled"),
            "`oca {verb}`: {stderr}"
        );
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "`oca {verb}` must not send any HTTP request before rejecting the alias"
        );
    }
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
        let server = thread::spawn(move || serve_confirmed_prompt(&listener));
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
        assert!(
            request.body.get("format").is_none(),
            "text transport must omit the poisonable format field"
        );
        assert!(request.body.get("delivery").is_none());
        assert_eq!(
            stored_record(home.path()).last_state,
            Some(RefState::Running)
        );
    }
}

#[test]
fn schema_transport_escape_hatch_keeps_the_continuation_format_field() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fake server binds");
    let port = listener.local_addr().expect("fake server address").port();
    let server = thread::spawn(move || serve_confirmed_prompt(&listener));
    let home = prepared_home(port, RefState::Done, "high");
    std::fs::write(
        home.path().join(".oca/config.toml"),
        "[dispatch]\ntransport = \"schema\"\n",
    )
    .expect("schema transport config");

    let output = run_oca(home.path(), ["m", "w4f2a1", "continue in schema mode"]);
    let request = server.join().expect("fake server completes");

    assert_success(&output);
    assert_eq!(request.body["format"]["type"], "json_schema");
    assert!(request.body["format"]["schema"].is_object());
}

#[test]
fn silently_dropped_message_stays_prompt_uncertain_without_a_running_ghost() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fake server binds");
    let port = listener.local_addr().expect("fake server address").port();
    let server = thread::spawn(move || serve_silently_dropped_prompt(listener));
    let home = prepared_home(port, RefState::Done, "high");

    let output = run_oca(home.path(), ["m", "w4f2a1", "dropped", "resend"]);
    let requests = server.join().expect("fake server completes");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("stderr is utf-8");
    assert!(stderr.contains("code: prompt_uncertain"), "{stderr}");
    assert_eq!(
        stored_record(home.path()).last_state,
        Some(RefState::Done),
        "a dropped resend must not persist Running on the ref"
    );
    assert_eq!(
        stored_record(home.path()).message_id.as_deref(),
        Some("msg_prior_turn"),
        "a dropped resend must not replace the last evidenced message"
    );
    let intent: Value = serde_json::from_slice(
        &std::fs::read(home.path().join(".oca/intents/w4f2a1.json"))
            .expect("uncertain resend intent persists"),
    )
    .expect("intent is JSON");
    assert_eq!(intent["phase"], "prompt_uncertain");
    assert_eq!(intent["op"], "message");
    assert!(
        requests
            .iter()
            .any(|request| request.path == "/session/ses_prior_context/message"),
        "confirmation must consult history after the SSE stream closes"
    );
}

#[test]
fn message_sse_evidence_overrides_poisoned_history_and_marks_running() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fake server binds");
    let port = listener.local_addr().expect("fake server address").port();
    let server = thread::spawn(move || serve_poisoned_history_confirmed_prompt(listener));
    let home = prepared_home(port, RefState::Done, "high");

    let output = run_oca(home.path(), ["m", "w4f2a1", "landed", "resend"]);
    let requests = server.join().expect("fake server completes");

    assert_success(&output);
    assert_eq!(
        stored_record(home.path()).last_state,
        Some(RefState::Running)
    );
    assert!(
        requests
            .iter()
            .any(|request| request.path == "/session/ses_prior_context/message"),
        "the regression must exercise rejected poisoned history"
    );
}

#[test]
fn message_history_evidence_marks_the_resend_running() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fake server binds");
    let port = listener.local_addr().expect("fake server address").port();
    let server = thread::spawn(move || serve_history_confirmed_prompt(listener));
    let home = prepared_home(port, RefState::Done, "high");

    let output = run_oca(home.path(), ["m", "w4f2a1", "history", "evidence"]);
    let requests = server.join().expect("fake server completes");

    assert_success(&output);
    let record = stored_record(home.path());
    assert_eq!(record.last_state, Some(RefState::Running));
    assert_eq!(
        record.message_id.as_deref(),
        requests[1].body["messageID"].as_str(),
        "Running must retain the message ID proven by history"
    );
}

#[test]
fn message_effort_override_applies_now_and_persists_for_later_turns() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fake server binds");
    let port = listener.local_addr().expect("fake server address").port();
    let server = thread::spawn(move || {
        [
            serve_confirmed_prompt(&listener),
            serve_confirmed_prompt(&listener),
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
fn queue_uses_plain_legacy_admission_without_state_or_effort_change() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fake server binds");
    let port = listener.local_addr().expect("fake server address").port();
    let server = thread::spawn(move || serve_prompt(listener, "204 No Content", ""));
    let home = prepared_home(port, RefState::Running, "high");

    let output = run_oca(home.path(), ["q", "w4f2a1", "after", "this", "turn"]);
    let request = server.join().expect("fake server completes");

    assert_success(&output);
    assert_eq!(request.path, "/session/ses_prior_context/prompt_async");
    assert!(request.body.get("delivery").is_none());
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
            serve_confirmed_prompt(&listener),
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
    assert!(requests[0].body.get("delivery").is_none());
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
fn abort_closes_the_persisted_herdr_tab() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fake server binds");
    let port = listener.local_addr().expect("fake server address").port();
    let server = thread::spawn(move || serve_prompt(listener, "200 OK", "true"));
    let home = prepared_home(port, RefState::Running, "high");
    let socket = configure_herdr(home.path(), Duration::from_millis(100));
    ref_store(home.path())
        .patch("w4f2a1", RefPatch::default().with_herdr_tab("persisted-t1"))
        .expect("record herdr tab");
    let herdr = thread::spawn(move || {
        let listener = UnixListener::bind(socket).expect("fake herdr binds");
        accept_discovery_probe(&listener);
        let (mut stream, _) = accept_unix_before(&listener, Duration::from_secs(1));
        let request = read_unix_request(&stream);
        let request_id = request["id"].as_str().expect("herdr request id");
        writeln!(
            stream,
            "{}",
            serde_json::json!({"id":request_id,"result":{"type":"ok"}})
        )
        .expect("fake herdr responds");
        request
    });
    wait_for_path(&home.path().join("herdr.sock"), Duration::from_secs(1));

    let output = run_oca(home.path(), ["k", "w4f2a1"]);
    let request = herdr.join().expect("fake herdr completes");
    server.join().expect("fake server completes");

    assert_success(&output);
    assert_eq!(request["method"], "tab.close");
    assert_eq!(request["params"]["tab_id"], "persisted-t1");
    assert_eq!(
        stored_record(home.path()).last_state,
        Some(RefState::Aborted)
    );
}

#[test]
fn abort_is_idempotent_when_the_herdr_tab_or_server_is_already_gone() {
    for server_gone in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fake server binds");
        let port = listener.local_addr().expect("fake server address").port();
        let server = thread::spawn(move || serve_prompt(listener, "200 OK", "true"));
        let home = prepared_home(port, RefState::Running, "high");
        let socket = configure_herdr(home.path(), Duration::from_millis(100));
        ref_store(home.path())
            .patch("w4f2a1", RefPatch::default().with_herdr_tab("closed-t1"))
            .expect("record closed herdr tab");

        let herdr = (!server_gone).then(|| {
            thread::spawn(move || {
                let listener = UnixListener::bind(socket).expect("fake herdr binds");
                accept_discovery_probe(&listener);
                let (mut stream, _) = accept_unix_before(&listener, Duration::from_secs(1));
                let request = read_unix_request(&stream);
                let request_id = request["id"].as_str().expect("herdr request id");
                writeln!(
                    stream,
                    "{}",
                    serde_json::json!({
                        "id":request_id,
                        "error":{"code":"tab_not_found","message":"tab is already closed"}
                    })
                )
                .expect("fake herdr rejects stale tab");
            })
        });
        if herdr.is_some() {
            wait_for_path(&home.path().join("herdr.sock"), Duration::from_secs(1));
        }

        let output = run_oca(home.path(), ["k", "w4f2a1"]);
        server.join().expect("fake server completes");
        if let Some(herdr) = herdr {
            herdr.join().expect("fake herdr completes");
        }

        assert_success(&output);
        assert_eq!(
            stored_record(home.path()).last_state,
            Some(RefState::Aborted)
        );
    }
}

#[test]
fn abort_never_contacts_herdr_for_headless_or_tmux_refs() {
    for display in ["headless", "tmux"] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fake server binds");
        let port = listener.local_addr().expect("fake server address").port();
        let server = thread::spawn(move || serve_prompt(listener, "200 OK", "true"));
        let home = prepared_home(port, RefState::Running, "high");
        let socket = configure_herdr(home.path(), Duration::from_millis(100));
        let herdr = UnixListener::bind(socket).expect("fake herdr binds");
        herdr
            .set_nonblocking(true)
            .expect("fake herdr is nonblocking");
        let mut patch = RefPatch::default().with_herdr_tab("stale-t1");
        patch.display = Some(display.to_owned());
        ref_store(home.path())
            .patch("w4f2a1", patch)
            .expect("record non-herdr display");

        let output = run_oca(home.path(), ["k", "w4f2a1"]);
        server.join().expect("fake server completes");

        assert_success(&output);
        assert!(
            matches!(herdr.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "{display} abort must not contact herdr"
        );
    }
}

#[test]
fn aborts_herdr_cleanup_wait_is_bounded_by_the_configured_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fake server binds");
    let port = listener.local_addr().expect("fake server address").port();
    let server = thread::spawn(move || serve_prompt(listener, "200 OK", "true"));
    let home = prepared_home(port, RefState::Running, "high");
    let socket = configure_herdr(home.path(), Duration::from_millis(30));
    ref_store(home.path())
        .patch("w4f2a1", RefPatch::default().with_herdr_tab("hung-t1"))
        .expect("record herdr tab");
    let herdr = thread::spawn(move || {
        let listener = UnixListener::bind(socket).expect("fake herdr binds");
        accept_discovery_probe(&listener);
        let (stream, _) = accept_unix_before(&listener, Duration::from_secs(1));
        let _request = read_unix_request(&stream);
        thread::sleep(Duration::from_millis(100));
    });
    wait_for_path(&home.path().join("herdr.sock"), Duration::from_secs(1));

    let started = Instant::now();
    let output = run_oca(home.path(), ["k", "w4f2a1"]);
    let elapsed = started.elapsed();
    server.join().expect("fake server completes");
    herdr.join().expect("fake herdr completes");

    assert_success(&output);
    assert!(
        elapsed < Duration::from_millis(500),
        "abort exceeded its configured herdr deadline: {elapsed:?}"
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
        let (mut events, _) = listener
            .accept()
            .expect("confirmation subscription arrives");
        let subscription = read_request(&mut events);
        assert_eq!(subscription.path, "/event?directory=%2Frepo");
        write_sse_headers(&mut events);
        let (mut stream, _) = listener.accept().expect("first message arrives");
        let first = read_request(&mut stream);
        admitted_tx.send(()).expect("signal first admission");
        release_rx.recv().expect("release first response");
        let message_id = first.body["messageID"].as_str().expect("message id");
        write_confirmation_event(&mut events, message_id);
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
    prepared_home_on_alias(port, state, effort, "luna")
}

fn prepared_home_on_alias(
    port: u16,
    state: RefState,
    effort: &str,
    alias: &str,
) -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("temporary home");
    let state_directory = home.path().join(".oca");
    std::fs::create_dir(&state_directory).expect("state directory");
    std::fs::write(state_directory.join("config.toml"), "").expect("default config");
    ref_store(home.path())
        .insert(RefRecord {
            id: "w4f2a1".to_owned(),
            session_id: "ses_prior_context".to_owned(),
            message_id: Some("msg_prior_turn".to_owned()),
            alias: Some(alias.to_owned()),
            effort: Some(effort.to_owned()),
            role: Some("impl".to_owned()),
            cwd: Some(home.path().display().to_string()),
            last_state: Some(state),
            repo: Some("/repo".to_owned()),
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

fn configure_herdr(home: &std::path::Path, timeout: Duration) -> std::path::PathBuf {
    let socket = home.join("herdr.sock");
    std::fs::write(
        home.join(".oca/config.toml"),
        format!(
            "[herdr]\nsocket = {:?}\ntimeout_ms = {}\n",
            socket.display().to_string(),
            timeout.as_millis()
        ),
    )
    .expect("herdr config");
    socket
}

fn wait_for_path(path: &std::path::Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {path:?}");
        thread::sleep(Duration::from_millis(5));
    }
}

fn accept_unix_before(
    listener: &UnixListener,
    timeout: Duration,
) -> (UnixStream, std::os::unix::net::SocketAddr) {
    listener
        .set_nonblocking(true)
        .expect("fake herdr is nonblocking");
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok(connection) => return connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for herdr call"
                );
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("fake herdr accept failed: {error}"),
        }
    }
}

fn accept_discovery_probe(listener: &UnixListener) {
    let (mut stream, _) = accept_unix_before(listener, Duration::from_secs(1));
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("fake herdr probe read timeout");
    let mut byte = [0];
    assert_eq!(
        stream.read(&mut byte).expect("read herdr discovery probe"),
        0,
        "herdr discovery probe must not send a protocol request"
    );
}

fn read_unix_request(stream: &UnixStream) -> Value {
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("fake herdr read timeout");
    let mut line = String::new();
    std::io::BufReader::new(stream.try_clone().expect("clone fake herdr stream"))
        .read_line(&mut line)
        .expect("read herdr request");
    serde_json::from_str(&line).expect("herdr request JSON")
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

fn serve_confirmed_prompt(listener: &TcpListener) -> CapturedRequest {
    let (mut events, subscription) = accept_request(listener, "SSE subscription");
    assert_eq!(subscription.path, "/event?directory=%2Frepo");
    write_sse_headers(&mut events);

    let (mut prompt_stream, request) = accept_request(listener, "prompt");
    let message_id = request.body["messageID"]
        .as_str()
        .expect("prompt carries message id");
    write_confirmation_event(&mut events, message_id);
    write_response(&mut prompt_stream, "204 No Content", "");
    request
}

fn serve_poisoned_history_confirmed_prompt(listener: TcpListener) -> Vec<CapturedRequest> {
    let (mut events, _) = listener.accept().expect("fake accepts SSE subscription");
    let subscription = read_request(&mut events);
    assert_eq!(subscription.path, "/event?directory=%2Frepo");
    write_sse_headers(&mut events);

    let prompt = serve_one_prompt(&listener, "204 No Content", "");
    let message_id = prompt.body["messageID"]
        .as_str()
        .expect("prompt carries message id")
        .to_owned();

    let (mut history_stream, _) = listener.accept().expect("history fallback arrives");
    let history = read_request(&mut history_stream);
    assert_eq!(history.path, "/session/ses_prior_context/message");
    write_response(
        &mut history_stream,
        "400 Bad Request",
        r#"{"error":"Expected OutputFormatJsonSchema, got retryCount"}"#,
    );
    write_confirmation_event(&mut events, &message_id);
    vec![subscription, prompt, history]
}

fn serve_history_confirmed_prompt(listener: TcpListener) -> Vec<CapturedRequest> {
    let (mut events, _) = listener.accept().expect("fake accepts SSE subscription");
    let subscription = read_request(&mut events);
    assert_eq!(subscription.path, "/event?directory=%2Frepo");
    write!(
        events,
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    )
    .expect("fake closes empty SSE stream");

    let prompt = serve_one_prompt(&listener, "204 No Content", "");
    let message_id = prompt.body["messageID"]
        .as_str()
        .expect("prompt carries message id");
    let prompt_text = prompt.body["parts"][0]["text"]
        .as_str()
        .expect("prompt carries text");
    let history_body = serde_json::json!([{
        "info": {
            "id": message_id,
            "sessionID": "ses_prior_context",
            "role": "user",
            "time": {"created": 1}
        },
        "parts": [{"type": "text", "text": prompt_text}]
    }])
    .to_string();

    let (mut history_stream, _) = listener.accept().expect("history fallback arrives");
    let history = read_request(&mut history_stream);
    assert_eq!(history.path, "/session/ses_prior_context/message");
    write_response(&mut history_stream, "200 OK", &history_body);
    vec![subscription, prompt, history]
}

fn serve_silently_dropped_prompt(listener: TcpListener) -> Vec<CapturedRequest> {
    let mut requests = Vec::new();
    let (mut events, _) = listener.accept().expect("fake accepts SSE subscription");
    requests.push(read_request(&mut events));
    assert_eq!(requests[0].path, "/event?directory=%2Frepo");
    write!(
        events,
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    )
    .expect("fake closes empty SSE stream");

    requests.push(serve_one_prompt(&listener, "204 No Content", ""));
    listener
        .set_nonblocking(true)
        .expect("fake listener becomes nonblocking");
    let started = Instant::now();
    let mut last_request = Instant::now();
    while started.elapsed() < Duration::from_secs(4) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let request = read_request(&mut stream);
                assert_eq!(request.path, "/session/ses_prior_context/message");
                write_response(&mut stream, "200 OK", "[]");
                requests.push(request);
                last_request = Instant::now();
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if started.elapsed() >= Duration::from_secs(2)
                    && last_request.elapsed() >= Duration::from_millis(300)
                {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("fake listener failed: {error}"),
        }
    }
    requests
}

fn write_sse_headers(stream: &mut TcpStream) {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n"
    )
    .expect("fake writes SSE headers");
    stream.flush().expect("fake flushes SSE headers");
}

fn write_confirmation_event(stream: &mut TcpStream, message_id: &str) {
    let event = serde_json::json!({
        "type": "message.updated",
        "properties": {
            "info": {
                "id": message_id,
                "sessionID": "ses_prior_context",
                "role": "user",
                "time": {"created": 1}
            }
        }
    });
    write!(stream, "id: evt_prompt\ndata: {event}\n\n").expect("fake writes prompt confirmation");
    stream.flush().expect("fake flushes prompt confirmation");
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

fn accept_request(listener: &TcpListener, kind: &str) -> (TcpStream, CapturedRequest) {
    loop {
        let (mut stream, _) = listener.accept().unwrap_or_else(|error| {
            panic!("fake accepts {kind}: {error}");
        });
        if let Some(request) = try_read_request(&mut stream) {
            return (stream, request);
        }
    }
}

fn read_request(stream: &mut TcpStream) -> CapturedRequest {
    try_read_request(stream).expect("request closed before headers arrived")
}

fn try_read_request(stream: &mut TcpStream) -> Option<CapturedRequest> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut buffer).expect("request headers");
        if read == 0 {
            return None;
        }
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
    Some(CapturedRequest { path, body })
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
