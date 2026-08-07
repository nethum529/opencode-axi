#![cfg(unix)]

use std::{
    fs,
    io::{BufRead, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use oca_server::{ConnectOrStart, ServerRecord};
use oca_state::{RefStore, RefStorePaths};
use serde_json::{Value, json};

#[test]
fn headed_tmux_background_journals_events_while_detached_stream_is_live() {
    let home = tempfile::tempdir().expect("temporary home");
    init_repository(home.path());
    let tmux = FakeTmux::new(home.path());
    let server = FakeOpenCode::spawn();
    prepare_home(home.path(), server.port);

    let output = Command::new(env!("CARGO_BIN_EXE_oca"))
        .args([
            "luna:h", "-w", "-b", "journal", "the", "headed", "tmux", "stream",
        ])
        .env("HOME", home.path())
        .env("TMUX", "fake-client")
        .env("PATH", prepend_path(home.path()))
        .current_dir(home.path())
        .output()
        .expect("background dispatch runs");

    assert!(
        output.status.success(),
        "background dispatch failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "unexpected dispatch stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let acknowledgement = String::from_utf8(output.stdout).expect("UTF-8 acknowledgement");
    let reference = acknowledgement
        .split_whitespace()
        .next()
        .expect("background acknowledgement ref");
    let record = ref_store(home.path())
        .resolve(reference)
        .expect("ref lookup succeeds")
        .expect("background ref exists");
    assert_eq!(record.display.as_deref(), Some("tmux"));
    let message_id = record
        .message_id
        .as_deref()
        .expect("admitted ref carries its message id");
    let journal = home
        .path()
        .join(".oca/events")
        .join(format!("{reference}.{message_id}.jsonl"));

    wait_for_nonempty_file(&journal, Duration::from_secs(2));
    let jsonl = fs::read_to_string(&journal).expect("journal is readable");
    assert!(!jsonl.trim().is_empty(), "journal must contain an event");
    for line in jsonl.lines() {
        serde_json::from_str::<Value>(line).expect("each journal line is JSON");
    }

    let events = Command::new(env!("CARGO_BIN_EXE_oca"))
        .args(["events", reference, "--json"])
        .env("HOME", home.path())
        .current_dir(home.path())
        .output()
        .expect("mid-run events command runs");
    assert!(
        events.status.success(),
        "mid-run event read failed: {}",
        String::from_utf8_lossy(&events.stderr)
    );
    let page: Value = serde_json::from_slice(&events.stdout).expect("events output is JSON");
    assert_eq!(page["total"], 1);
    assert_eq!(page["events"][0]["kind"], "session.busy");

    // The terminal boundary is withheld until both journal read paths succeed,
    // proving the detached attach helper still owns a live stream here.
    server.release_terminal.store(true, Ordering::SeqCst);
    let requests = server.join();
    assert_eq!(
        requests,
        [
            "agents",
            "session",
            "event",
            "prompt_async",
            "messages",
            "messages",
            "event",
            "messages",
        ]
    );
    assert_eq!(
        tmux.wait_for_calls(2),
        [
            format!(
                "new-window -d -n oca-{reference} | impl | openai/gpt-5.6-luna | high | DO NOT TYPE: composer unbound -- opencode --session ses_tmux_journal"
            ),
            format!(
                "kill-window -t =oca-{reference} | impl | openai/gpt-5.6-luna | high | DO NOT TYPE: composer unbound"
            ),
        ]
    );
}

struct FakeOpenCode {
    port: u16,
    release_terminal: Arc<AtomicBool>,
    worker: thread::JoinHandle<Vec<&'static str>>,
}

impl FakeOpenCode {
    fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fake OpenCode binds");
        listener
            .set_nonblocking(true)
            .expect("listener becomes nonblocking");
        let port = listener.local_addr().expect("listener address").port();
        let release_terminal = Arc::new(AtomicBool::new(false));
        let server_release = Arc::clone(&release_terminal);
        let worker = thread::spawn(move || {
            let started = Instant::now();
            let mut requests = Vec::new();
            let mut last_request = None;
            let mut message_id = None::<String>;
            let mut prompt_text = None::<String>;
            let mut subscriptions = 0;
            let mut event_handler = None;

            while requests.len() < 8 && started.elapsed() < Duration::from_secs(5) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if last_request.is_some_and(|last: Instant| {
                            last.elapsed() >= Duration::from_millis(750)
                        }) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("fake OpenCode accept failed: {error}"),
                };
                let request = read_request(&mut stream);
                let route = route(&request.path);
                match route {
                    "agents" => write_response(
                        &mut stream,
                        "200 OK",
                        "application/json",
                        r#"[{"name":"impl"}]"#,
                    ),
                    "session" => write_response(
                        &mut stream,
                        "200 OK",
                        "application/json",
                        r#"{"id":"ses_tmux_journal"}"#,
                    ),
                    "event" if subscriptions == 0 => {
                        write_response(&mut stream, "200 OK", "text/event-stream", "");
                        subscriptions += 1;
                    }
                    "event" => {
                        subscriptions += 1;
                        let parent_id = message_id
                            .as_deref()
                            .expect("prompt precedes detached attach")
                            .to_owned();
                        let release = Arc::clone(&server_release);
                        event_handler = Some(thread::spawn(move || {
                            write!(
                                stream,
                                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{}",
                                busy_sse()
                            )
                            .expect("busy event writes");
                            stream.flush().expect("busy event flushes");
                            wait_for_flag(&release, Duration::from_secs(3));
                            write!(stream, "{}", terminal_sse(&parent_id))
                                .expect("terminal events write");
                            stream.flush().expect("terminal events flush");
                        }));
                    }
                    "prompt_async" => {
                        message_id = request.body["messageID"].as_str().map(ToOwned::to_owned);
                        prompt_text = request.body["parts"][0]["text"]
                            .as_str()
                            .map(ToOwned::to_owned);
                        write_response(&mut stream, "204 No Content", "text/plain", "");
                    }
                    "messages" => write_response(
                        &mut stream,
                        "200 OK",
                        "application/json",
                        &user_messages(
                            message_id.as_deref().expect("messages follow prompt"),
                            prompt_text.as_deref().expect("prompt text was captured"),
                        ),
                    ),
                    unexpected => panic!("unexpected OpenCode route: {unexpected}"),
                }
                requests.push(route);
                last_request = Some(Instant::now());
            }
            if let Some(handler) = event_handler {
                handler.join().expect("event handler exits");
            }
            requests
        });
        Self {
            port,
            release_terminal,
            worker,
        }
    }

    fn join(self) -> Vec<&'static str> {
        self.worker.join().expect("fake OpenCode exits")
    }
}

struct Request {
    path: String,
    body: Value,
}

fn read_request(stream: &mut TcpStream) -> Request {
    let mut reader = std::io::BufReader::new(stream.try_clone().expect("request stream clones"));
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .expect("request line reads");
    let path = request_line
        .split_whitespace()
        .nth(1)
        .expect("request path")
        .to_owned();
    let mut content_length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect("header reads");
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = value.trim().parse().expect("numeric content length");
        }
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).expect("request body reads");
    Request {
        path,
        body: if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body).expect("request body is JSON")
        },
    }
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

fn write_response(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .expect("response writes");
    stream.flush().expect("response flushes");
}

fn user_messages(message_id: &str, prompt_text: &str) -> String {
    json!([{
        "info": {
            "id": message_id,
            "sessionID": "ses_tmux_journal",
            "role": "user",
            "time": {"created": 1}
        },
        "parts": [{"type": "text", "text": prompt_text}]
    }])
    .to_string()
}

fn busy_sse() -> String {
    let busy = json!({
        "id": "evt_tmux_busy",
        "type": "session.busy",
        "properties": {"sessionID": "ses_tmux_journal"}
    });
    format!("id: evt_tmux_busy\ndata: {busy}\n\n")
}

fn terminal_sse(parent_id: &str) -> String {
    let message = json!({
        "id": "evt_tmux_message",
        "type": "message.updated",
        "properties": {
            "sessionID": "ses_tmux_journal",
            "info": {
                "id": "msg_tmux_assistant",
                "sessionID": "ses_tmux_journal",
                "role": "assistant",
                "parentID": parent_id,
                "time": {"created": 2, "completed": 3},
                "structured": {
                    "status": "done",
                    "files": [],
                    "note": "The detached tmux helper retained the live worker stream, journaled its events during execution, and observed the attributed terminal boundary without leaving follow-up work."
                }
            }
        }
    });
    let idle = json!({
        "id": "evt_tmux_idle",
        "type": "session.idle",
        "properties": {"sessionID": "ses_tmux_journal"}
    });
    format!("id: evt_tmux_message\ndata: {message}\n\nid: evt_tmux_idle\ndata: {idle}\n\n")
}

fn wait_for_nonempty_file(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for non-empty journal {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for_flag(flag: &AtomicBool, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !flag.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "terminal release timed out");
        thread::sleep(Duration::from_millis(5));
    }
}

fn prepare_home(home: &Path, port: u16) {
    let state = home.join(".oca");
    fs::create_dir_all(&state).expect("state directory");
    fs::write(
        state.join("config.toml"),
        format!(
            "[herdr]\nsocket = {}\n",
            serde_json::to_string(&home.join("missing-herdr.sock").display().to_string())
                .expect("socket path serializes")
        ),
    )
    .expect("config writes");
    ConnectOrStart::new(&state, port, [], Duration::from_secs(1))
        .write_record(&ServerRecord::new(
            port,
            installed_opencode_version(),
            environment_hash_for(home, &prepend_path(home)),
        ))
        .expect("server record");
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
                .expect("git runs")
                .success()
        );
    }
    fs::write(path.join("README.md"), "base\n").expect("fixture file writes");
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["add", "README.md"])
            .status()
            .expect("git add runs")
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["commit", "--quiet", "-m", "base"])
            .status()
            .expect("git commit runs")
            .success()
    );
}

fn ref_store(home: &Path) -> RefStore {
    RefStore::with_paths(RefStorePaths::in_directory(home.join(".oca")))
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

fn environment_hash_for(home: &Path, path: &std::ffi::OsStr) -> String {
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
        } else if key == "PATH" {
            hasher.update(path.to_string_lossy().as_bytes());
        } else if let Some(value) = std::env::var_os(key) {
            hasher.update(value.to_string_lossy().as_bytes());
        }
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

fn prepend_path(directory: &Path) -> std::ffi::OsString {
    let mut paths = vec![directory.to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::env::join_paths(paths).expect("PATH joins")
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
        .expect("fake tmux writes");
        let mut permissions = fs::metadata(&executable)
            .expect("fake tmux metadata")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&executable, permissions).expect("fake tmux becomes executable");
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
