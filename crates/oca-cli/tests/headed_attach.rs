use std::{
    io::{BufRead, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    process::Command,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use oca_server::{ConnectOrStart, ServerRecord};
use oca_state::{RefRecord, RefState, RefStore, RefStorePaths};
use serde_json::{Value, json};

#[test]
fn headed_attach_records_and_closes_the_tab_after_terminal_state() {
    let home = tempfile::tempdir().unwrap();
    let socket = home.path().join("fake-herdr.sock");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let herdr = spawn_herdr_lifecycle(&socket, Arc::clone(&calls));
    let (port, opencode) = spawn_attach_opencode();
    prepare_attach_home(home.path(), &socket, port);

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
fn an_absent_herdr_socket_ends_the_attach_helper_quietly() {
    let home = tempfile::tempdir().unwrap();
    // Never created, so discovery must decline rather than dial it.
    let socket = home.path().join("herdr-was-never-started.sock");
    prepare_attach_home(home.path(), &socket, 1);

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
        "an absent herdr is the normal headless case, not a warning: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let record = ref_store(home.path()).resolve("wabc12").unwrap().unwrap();
    assert_eq!(record.display, None);
    assert_eq!(record.herdr_tab, None);
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

fn prepare_attach_home(home: &Path, socket: &Path, port: u16) {
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
            display: None,
            herdr_tab: None,
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
