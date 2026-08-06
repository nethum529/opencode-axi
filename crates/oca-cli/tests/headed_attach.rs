use std::{
    fs,
    io::{BufRead, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use oca_server::{ConnectOrStart, ServerRecord};
use oca_state::{RefRecord, RefState, RefStore, RefStorePaths};
use serde_json::{Value, json};

#[test]
fn headed_background_dispatch_lands_prompt_before_real_detached_attach_and_events_flow() {
    let home = tempfile::tempdir().unwrap();
    init_repository(home.path());
    let socket = home.path().join("fake-herdr.sock");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let herdr = spawn_herdr_lifecycle(&socket, Arc::clone(&calls));
    let (port, opencode) = spawn_headed_background_opencode();
    prepare_dispatch_home(home.path(), &socket, port);

    let output = Command::new(env!("CARGO_BIN_EXE_oca"))
        .args([
            "luna:h",
            "-w",
            "-b",
            "land",
            "the",
            "headed",
            "background",
            "prompt",
        ])
        .env("HOME", home.path())
        .current_dir(home.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "background dispatch failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let acknowledgement = String::from_utf8(output.stdout).unwrap();
    let reference = acknowledgement
        .split_whitespace()
        .next()
        .expect("background acknowledgement ref");

    herdr.join().unwrap();
    let requests = opencode.join().unwrap();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.path.as_str())
            .collect::<Vec<_>>(),
        [
            requests[0].path.as_str(),
            "/event",
            "/session/ses_headed_background/prompt_async",
            "/session/ses_headed_background/message",
            "/event",
            "/session/ses_headed_background/message",
        ],
        "headed background admission must confirm the user message before spawning the helper"
    );
    assert!(requests[0].path.starts_with("/session?directory="));
    let message_id = requests[2].body["messageID"]
        .as_str()
        .expect("prompt carries a caller message id");
    assert_eq!(requests[3].body, Value::Null);
    assert_eq!(requests[5].body, Value::Null);

    let calls = calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .map(|call| call["method"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "workspace.list",
            "workspace.create",
            "tab.create",
            "agent.start",
            "tab.close",
        ],
        "tab.close proves the detached helper consumed the attributed terminal events"
    );
    assert_eq!(
        calls[3]["params"]["args"],
        json!([
            "attach",
            format!("http://127.0.0.1:{port}"),
            "--session",
            "ses_headed_background"
        ])
    );

    let record = ref_store(home.path()).resolve(reference).unwrap().unwrap();
    assert_eq!(record.session_id, "ses_headed_background");
    assert_eq!(record.message_id.as_deref(), Some(message_id));
    assert_eq!(record.last_state, Some(RefState::Running));
    assert_eq!(record.display.as_deref(), Some("herdr"));
    assert_eq!(record.herdr_tab.as_deref(), Some("t1"));
}

#[test]
fn headed_attach_records_and_closes_the_tab_after_terminal_state() {
    let home = tempfile::tempdir().unwrap();
    let socket = home.path().join("fake-herdr.sock");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let herdr = spawn_herdr_lifecycle(&socket, Arc::clone(&calls));
    let (port, opencode) = spawn_attach_opencode();
    prepare_attach_home(home.path(), &socket, port, "herdr");

    let output = Command::new(env!("CARGO_BIN_EXE_oca"))
        .args(["__attach", "wabc12", "ses_target", "/worker"])
        .env("HOME", home.path())
        .current_dir(home.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "attach failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    herdr.join().unwrap();
    opencode.join().unwrap();

    let calls = calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .map(|call| call["method"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "workspace.list",
            "workspace.create",
            "tab.create",
            "agent.start",
            "tab.close"
        ]
    );
    assert_eq!(calls[1]["params"]["label"], "oca");
    assert_eq!(calls[1]["params"]["focus"], false);
    assert_eq!(calls[2]["params"]["label"], "wabc12");
    assert_eq!(calls[2]["params"]["cwd"], "/worker");
    assert_eq!(calls[2]["params"]["focus"], false);
    assert_eq!(calls[3]["params"]["name"], "opencode");
    assert_eq!(calls[3]["params"]["kind"], "opencode");
    assert_eq!(
        calls[3]["params"]["args"],
        json!(["--session", "ses_target"])
    );
    assert!(
        calls[3]["params"]["args"]
            .as_array()
            .unwrap()
            .iter()
            .all(|argument| argument != "--read-only"),
        "T03 passed, so headed attach must remain shared-input"
    );
    assert_eq!(calls[4]["params"]["tab_id"], "t1");

    let record = ref_store(home.path()).resolve("wabc12").unwrap().unwrap();
    assert_eq!(record.display.as_deref(), Some("herdr"));
    assert_eq!(record.herdr_tab.as_deref(), Some("t1"));
}

#[test]
fn a_headless_ref_ends_the_attach_helper_quietly() {
    let home = tempfile::tempdir().unwrap();
    let socket = home.path().join("herdr-was-never-started.sock");
    prepare_attach_home(home.path(), &socket, 1, "headless");

    let output = Command::new(env!("CARGO_BIN_EXE_oca"))
        .args(["__attach", "wabc12", "ses_target", "/worker"])
        .env("HOME", home.path())
        .current_dir(home.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        output.stderr.is_empty(),
        "headless display is not a warning: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let record = ref_store(home.path()).resolve("wabc12").unwrap().unwrap();
    assert_eq!(record.display.as_deref(), Some("headless"));
    assert_eq!(record.herdr_tab, None);
}

#[test]
fn explicit_headless_dispatch_never_calls_the_fake_herdr_socket() {
    let home = tempfile::tempdir().unwrap();
    let socket = home.path().join("fake-herdr.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let (port, opencode) = spawn_foreground_opencode();
    prepare_dispatch_home(home.path(), &socket, port);

    let output = Command::new(env!("CARGO_BIN_EXE_oca"))
        .args(["luna:h", "--headless", "skip", "display", "discovery"])
        .env("HOME", home.path())
        .env("TMUX", "fake-client")
        .current_dir(home.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "dispatch failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(matches!(
        listener.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    let record = only_ref(home.path());
    assert_eq!(record.display.as_deref(), Some("headless"));
    opencode.join().unwrap();
}

#[test]
fn no_herdr_and_no_tmux_is_a_silent_headless_http_dispatch() {
    let home = tempfile::tempdir().unwrap();
    let missing_socket = home.path().join("missing-herdr.sock");
    let (port, opencode) = spawn_foreground_opencode();
    prepare_dispatch_home(home.path(), &missing_socket, port);

    let output = Command::new(env!("CARGO_BIN_EXE_oca"))
        .args(["luna:h", "finish", "without", "a", "display"])
        .env("HOME", home.path())
        .env_remove("TMUX")
        .current_dir(home.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "dispatch failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("state: completed")
    );
    assert_eq!(only_ref(home.path()).display.as_deref(), Some("headless"));
    opencode.join().unwrap();
}

#[test]
fn no_herdr_inside_tmux_creates_and_cleans_up_a_ref_named_window() {
    let home = tempfile::tempdir().unwrap();
    let missing_socket = home.path().join("missing-herdr.sock");
    let tmux = FakeTmux::new(home.path());
    let (port, opencode) = spawn_tmux_foreground_opencode();
    prepare_dispatch_home(home.path(), &missing_socket, port);
    let path = prepend_path(home.path());

    let output = Command::new(env!("CARGO_BIN_EXE_oca"))
        .args(["luna:h", "finish", "inside", "tmux"])
        .env("HOME", home.path())
        .env("TMUX", "fake-client")
        .env("PATH", path)
        .current_dir(home.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "dispatch failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let record = only_ref(home.path());
    assert_eq!(record.display.as_deref(), Some("tmux"));
    opencode.join().unwrap();
    let calls = tmux.wait_for_calls(2);
    assert_eq!(
        calls,
        [
            format!(
                "new-window -d -n oca-{} -- opencode --session ses_target",
                record.id
            ),
            format!("kill-window -t =oca-{}", record.id),
        ]
    );
}

#[test]
fn a_never_responding_herdr_socket_does_not_delay_dispatch_ack_or_completion() {
    let home = tempfile::tempdir().unwrap();
    let socket = home.path().join("hung-herdr.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (accepted_tx, accepted_rx) = mpsc::channel();
    let herdr = thread::spawn(move || {
        let (_stream, _) = listener.accept().unwrap();
        accepted_tx.send(()).unwrap();
        thread::sleep(Duration::from_millis(850));
    });
    let (port, opencode) = spawn_foreground_opencode();
    prepare_dispatch_home(home.path(), &socket, port);

    let started = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_oca"))
        .args(["luna:h", "finish", "quickly"])
        .env("HOME", home.path())
        .current_dir(home.path())
        .output()
        .unwrap();
    let elapsed = started.elapsed();

    assert!(
        output.status.success(),
        "dispatch failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert!(
        elapsed < Duration::from_millis(700),
        "detached herdr deadline leaked onto dispatch path: {elapsed:?}"
    );
    accepted_rx
        .recv_timeout(Duration::from_millis(300))
        .expect("post-ack detached attach reached the fake herdr socket");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.lines().next().unwrap().contains(" running "));
    assert!(stdout.contains("state: completed"));

    opencode.join().unwrap();
    herdr.join().unwrap();
}

#[test]
fn a_malformed_herdr_envelope_never_fails_the_dispatch() {
    let home = tempfile::tempdir().unwrap();
    let socket = home.path().join("malformed-herdr.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let herdr = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_unix_request(&stream);
        writeln!(stream, "{}", json!({"id":request["id"],"result":{}})).unwrap();
    });
    let (port, opencode) = spawn_foreground_opencode();
    prepare_dispatch_home(home.path(), &socket, port);

    let output = Command::new(env!("CARGO_BIN_EXE_oca"))
        .args(["luna:h", "finish", "despite", "display", "failure"])
        .env("HOME", home.path())
        .current_dir(home.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "dispatch failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("state: completed")
    );
    assert_eq!(only_ref(home.path()).display.as_deref(), Some("herdr"));
    opencode.join().unwrap();
    herdr.join().unwrap();
}

fn spawn_herdr_lifecycle(socket: &Path, calls: Arc<Mutex<Vec<Value>>>) -> thread::JoinHandle<()> {
    let listener = UnixListener::bind(socket).unwrap();
    thread::spawn(move || {
        for index in 0..5 {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_unix_request(&stream);
            let request_id = request["id"].as_str().unwrap();
            let result = match index {
                0 => json!({"type":"workspace_list","workspaces":[]}),
                1 => json!({
                    "type":"workspace_created",
                    "workspace":{"workspace_id":"w1","label":"oca"}
                }),
                2 => json!({
                    "type":"tab_created",
                    "tab":{"tab_id":"t1"},
                    "root_pane":{"pane_id":"p1"}
                }),
                3 => json!({
                    "type":"agent_started",
                    "agent":{"terminal_id":"term1"},
                    "argv":["opencode","--session","ses_target"]
                }),
                4 => json!({"type":"ok"}),
                _ => unreachable!(),
            };
            writeln!(stream, "{}", json!({"id":request_id,"result":result})).unwrap();
            calls.lock().unwrap().push(request);
        }
    })
}

fn read_unix_request(stream: &UnixStream) -> Value {
    let mut line = String::new();
    std::io::BufReader::new(stream.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    serde_json::from_str(&line).unwrap()
}

fn spawn_attach_opencode() -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut event, _) = listener.accept().unwrap();
        assert_eq!(read_http_request(&mut event).path, "/event");
        write_http_response(&mut event, "200 OK", "text/event-stream", "");

        let (mut messages, _) = listener.accept().unwrap();
        assert_eq!(
            read_http_request(&mut messages).path,
            "/session/ses_target/message"
        );
        write_http_response(
            &mut messages,
            "200 OK",
            "application/json",
            &terminal_messages("msg_dispatch"),
        );
    });
    (port, server)
}

fn spawn_headed_background_opencode() -> (u16, thread::JoinHandle<Vec<HttpRequest>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let started = Instant::now();
        let mut last_request = None;
        let mut requests = Vec::new();
        let mut message_id = None;
        let mut prompt_text = None;
        let mut event_subscriptions = 0;

        while requests.len() < 6 && started.elapsed() < Duration::from_secs(5) {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if last_request
                        .is_some_and(|last: Instant| last.elapsed() >= Duration::from_millis(750))
                    {
                        break;
                    }
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("fake OpenCode accept failed: {error}"),
            };
            let request = read_http_request(&mut stream);
            if request.path.starts_with("/session?directory=") {
                write_http_response(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    r#"{"id":"ses_headed_background"}"#,
                );
            } else if request.path == "/event" {
                let body = if event_subscriptions == 0 {
                    String::new()
                } else {
                    terminal_sse(message_id.as_deref().expect("prompt precedes attach"))
                };
                event_subscriptions += 1;
                write_http_response(&mut stream, "200 OK", "text/event-stream", &body);
            } else if request.path == "/session/ses_headed_background/prompt_async" {
                message_id = request.body["messageID"].as_str().map(ToOwned::to_owned);
                prompt_text = request.body["parts"][0]["text"]
                    .as_str()
                    .map(ToOwned::to_owned);
                write_http_response(&mut stream, "204 No Content", "text/plain", "");
            } else if request.path == "/session/ses_headed_background/message" {
                let body = user_messages(
                    message_id.as_deref().expect("prompt precedes messages"),
                    prompt_text.as_deref().expect("prompt text was captured"),
                );
                write_http_response(&mut stream, "200 OK", "application/json", &body);
            } else {
                panic!("unexpected OpenCode request path: {}", request.path);
            }
            requests.push(request);
            last_request = Some(Instant::now());
        }
        requests
    });
    (port, server)
}

fn user_messages(message_id: &str, prompt_text: &str) -> String {
    json!([{
        "info": {
            "id": message_id,
            "sessionID": "ses_headed_background",
            "role": "user",
            "time": {"created": 1}
        },
        "parts": [{"type": "text", "text": prompt_text}]
    }])
    .to_string()
}

fn terminal_sse(parent_id: &str) -> String {
    let message = json!({
        "id": "evt_headed_message",
        "type": "message.updated",
        "properties": {
            "sessionID": "ses_headed_background",
            "info": {
                "id": "msg_headed_assistant",
                "sessionID": "ses_headed_background",
                "role": "assistant",
                "parentID": parent_id,
                "time": {"created": 2, "completed": 3},
                "structured": {
                    "status": "done",
                    "files": [],
                    "note": "The headed background prompt landed in the authoritative server session, emitted an attributed terminal event, and completed through the detached production attach helper."
                }
            }
        }
    });
    let idle = json!({
        "id": "evt_headed_idle",
        "type": "session.idle",
        "properties": {"sessionID": "ses_headed_background"}
    });
    format!("id: evt_headed_message\ndata: {message}\n\nid: evt_headed_idle\ndata: {idle}\n\n")
}

fn spawn_foreground_opencode() -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let mut message_id = None;
        for index in 0..5 {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            match index {
                0 => {
                    assert!(request.path.starts_with("/session?directory="));
                    write_http_response(
                        &mut stream,
                        "200 OK",
                        "application/json",
                        r#"{"id":"ses_target"}"#,
                    );
                }
                1 => {
                    assert_eq!(request.path, "/event");
                    write_http_response(
                        &mut stream,
                        "200 OK",
                        "text/event-stream",
                        concat!(
                            "id: evt_terminal\n",
                            "event: session.idle\n",
                            "data: {\"sessionID\":\"ses_target\"}\n\n"
                        ),
                    );
                }
                2 => {
                    assert_eq!(request.path, "/session/ses_target/prompt_async");
                    message_id = request.body["messageID"].as_str().map(ToOwned::to_owned);
                    write_http_response(&mut stream, "204 No Content", "text/plain", "");
                }
                3 => {
                    assert_eq!(request.path, "/session/ses_target/message");
                    write_http_response(&mut stream, "200 OK", "application/json", "[]");
                }
                4 => {
                    assert_eq!(request.path, "/session/ses_target/message");
                    write_http_response(
                        &mut stream,
                        "200 OK",
                        "application/json",
                        &terminal_messages(message_id.as_deref().unwrap()),
                    );
                }
                _ => unreachable!(),
            }
        }
    });
    (port, server)
}

fn spawn_tmux_foreground_opencode() -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let mut message_id = None;
        for _ in 0..6 {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            if request.path.starts_with("/session?directory=") {
                write_http_response(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    r#"{"id":"ses_target"}"#,
                );
            } else if request.path == "/event" {
                write_http_response(
                    &mut stream,
                    "200 OK",
                    "text/event-stream",
                    concat!(
                        "id: evt_terminal\n",
                        "event: session.idle\n",
                        "data: {\"sessionID\":\"ses_target\"}\n\n"
                    ),
                );
            } else if request.path == "/session/ses_target/prompt_async" {
                message_id = request.body["messageID"].as_str().map(ToOwned::to_owned);
                write_http_response(&mut stream, "204 No Content", "text/plain", "");
            } else if request.path == "/session/ses_target/message" {
                write_http_response(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    &terminal_messages(message_id.as_deref().unwrap()),
                );
            } else {
                panic!("unexpected OpenCode request path: {}", request.path);
            }
        }
    });
    (port, server)
}

fn terminal_messages(parent_id: &str) -> String {
    json!([{
        "info": {
            "id": "msg_assistant",
            "sessionID": "ses_target",
            "role": "assistant",
            "parentID": parent_id,
            "time": {"created":1,"completed":2},
            // Long enough to clear T27a's per-role reply floor, so this fixture
            // stays valid once the floor lands on the integration branch.
            "structured": {"status":"done","files":[],"note":"Implemented the requested change and verified it end to end against the fake server fixture. All assertions pass, no regressions were observed, and the worker finished with no outstanding follow-up work."}
        },
        "parts": []
    }])
    .to_string()
}

fn prepare_attach_home(home: &Path, socket: &Path, port: u16, display: &str) {
    prepare_dispatch_home(home, socket, port);
    ref_store(home)
        .insert(RefRecord {
            id: "wabc12".to_owned(),
            session_id: "ses_target".to_owned(),
            message_id: Some("msg_dispatch".to_owned()),
            alias: Some("luna".to_owned()),
            effort: Some("high".to_owned()),
            role: Some("impl".to_owned()),
            cwd: Some("/worker".to_owned()),
            last_state: Some(RefState::Running),
            repo: None,
            spawner_tag: None,
            worktree: None,
            branch: None,
            commit: None,
            commit_subject: None,
            display: Some(display.to_owned()),
            herdr_tab: None,
            completion: None,
            tombstoned: false,
        })
        .unwrap();
}

fn prepare_dispatch_home(home: &Path, socket: &Path, port: u16) {
    let state = home.join(".oca");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("config.toml"),
        format!(
            "[herdr]\nsocket = {}\n",
            serde_json::to_string(&socket.display().to_string()).unwrap()
        ),
    )
    .unwrap();
    ConnectOrStart::new(&state, port, [], Duration::from_secs(1))
        .write_record(&ServerRecord::new(
            port,
            installed_opencode_version(),
            environment_hash_for(home),
        ))
        .unwrap();
}

fn init_repository(path: &Path) {
    for arguments in [
        ["init", "--quiet"].as_slice(),
        ["config", "user.name", "oca test"].as_slice(),
        ["config", "user.email", "oca@example.test"].as_slice(),
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
    fs::write(path.join("README.md"), "base\n").unwrap();
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

fn installed_opencode_version() -> String {
    Command::new("opencode")
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| "1.18.10".to_owned())
}

fn environment_hash_for(home: &Path) -> String {
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

fn ref_store(home: &Path) -> RefStore {
    RefStore::with_paths(RefStorePaths::in_directory(home.join(".oca")))
}

fn only_ref(home: &Path) -> RefRecord {
    let refs = ref_store(home)
        .list(&oca_state::RefListFilter::across_spawners_and_repos())
        .unwrap();
    assert_eq!(refs.len(), 1);
    refs.into_iter().next().unwrap()
}

fn prepend_path(directory: &Path) -> std::ffi::OsString {
    let mut paths = vec![directory.to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::env::join_paths(paths).unwrap()
}

struct FakeTmux {
    log: PathBuf,
}

impl FakeTmux {
    fn new(directory: &Path) -> Self {
        let executable = directory.join("tmux");
        let log = directory.join("tmux-calls");
        fs::write(
            &executable,
            format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display()),
        )
        .unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&executable, permissions).unwrap();
        Self { log }
    }

    fn wait_for_calls(&self, expected: usize) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let calls = fs::read_to_string(&self.log)
                .unwrap_or_default()
                .lines()
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>();
            if calls.len() >= expected {
                return calls;
            }
            assert!(Instant::now() < deadline, "timed out waiting for fake tmux");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

struct HttpRequest {
    path: String,
    body: Value,
}

fn read_http_request(stream: &mut TcpStream) -> HttpRequest {
    let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    reader.read_line(&mut request_line).unwrap();
    let path = request_line.split_whitespace().nth(1).unwrap().to_owned();
    let mut content_length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap();
        }
    }
    let mut bytes = vec![0; content_length];
    reader.read_exact(&mut bytes).unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    HttpRequest { path, body }
}

fn write_http_response(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
}
