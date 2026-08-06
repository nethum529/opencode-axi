use std::{
    fs,
    io::{BufRead, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
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
fn background_spawn_then_kill_closes_the_tab_and_exits_its_tui_process() {
    let home = tempfile::tempdir().unwrap();
    let socket = home.path().join("fake-herdr.sock");
    let tab_closed = Arc::new(AtomicBool::new(false));
    let fake_opencode = fake_attached_opencode(home.path());
    let herdr = spawn_herdr_with_attached_process(&socket, fake_opencode, Arc::clone(&tab_closed));
    let (port, opencode) = spawn_background_abort_opencode(Arc::clone(&tab_closed));
    prepare_dispatch_home(home.path(), &socket, port);
    fs::write(
        home.path().join(".oca/config.toml"),
        format!(
            "[herdr]\nsocket = {}\ntimeout_ms = 100\nclose_on_done = false\n",
            serde_json::to_string(&socket.display().to_string()).unwrap()
        ),
    )
    .unwrap();

    let background = Command::new(env!("CARGO_BIN_EXE_oca"))
        .args(["luna:h", "-b", "keep", "running", "until", "killed"])
        .env("HOME", home.path())
        .current_dir(home.path())
        .output()
        .unwrap();
    assert!(
        background.status.success(),
        "background dispatch failed: {}",
        String::from_utf8_lossy(&background.stderr)
    );
    let reference = String::from_utf8(background.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    wait_for_ref_tab(home.path(), &reference, Duration::from_secs(2));

    let started = Instant::now();
    let killed = Command::new(env!("CARGO_BIN_EXE_oca"))
        .args(["k", &reference])
        .env("HOME", home.path())
        .current_dir(home.path())
        .output()
        .unwrap();
    let elapsed = started.elapsed();
    assert!(
        killed.status.success(),
        "kill failed: {}",
        String::from_utf8_lossy(&killed.stderr)
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "kill exceeded its bounded display cleanup wait: {elapsed:?}"
    );

    let calls = herdr.join().expect("fake herdr completes");
    opencode.join().expect("fake OpenCode completes");
    assert!(tab_closed.load(Ordering::SeqCst));
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
    assert_eq!(calls[4]["params"]["tab_id"], "spawned-t1");
    assert_eq!(
        fs::read_to_string(home.path().join("attached-opencode-args"))
            .unwrap()
            .trim(),
        "--session ses_target"
    );
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

fn spawn_herdr_with_attached_process(
    socket: &Path,
    fake_opencode: PathBuf,
    tab_closed: Arc<AtomicBool>,
) -> thread::JoinHandle<Vec<Value>> {
    let listener = UnixListener::bind(socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    thread::spawn(move || {
        let mut calls = Vec::new();
        let mut attached = None;
        for index in 0..5 {
            let (mut stream, _) = accept_unix_before(&listener, Duration::from_secs(3));
            let request = read_unix_request(&stream);
            let request_id = request["id"].as_str().unwrap();
            let result = match index {
                0 => json!({"type":"workspace_list","workspaces":[]}),
                1 => json!({
                    "type":"workspace_created",
                    "workspace":{"workspace_id":"spawned-w1","label":"oca"}
                }),
                2 => json!({
                    "type":"tab_created",
                    "tab":{"tab_id":"spawned-t1"},
                    "root_pane":{"pane_id":"spawned-p1"}
                }),
                3 => {
                    let args = request["params"]["args"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|argument| argument.as_str().unwrap())
                        .collect::<Vec<_>>();
                    let mut child = Command::new(&fake_opencode).args(args).spawn().unwrap();
                    assert!(
                        child.try_wait().unwrap().is_none(),
                        "attached TUI exited early"
                    );
                    attached = Some(AttachedProcess(child));
                    json!({
                        "type":"agent_started",
                        "agent":{"terminal_id":"spawned-term1"}
                    })
                }
                4 => {
                    let child = &mut attached.as_mut().expect("agent.start spawned a TUI").0;
                    assert!(
                        child.try_wait().unwrap().is_none(),
                        "attached TUI exited before kill"
                    );
                    child.kill().unwrap();
                    child.wait().unwrap();
                    assert!(
                        child.try_wait().unwrap().is_some(),
                        "attached TUI was not reaped"
                    );
                    tab_closed.store(true, Ordering::SeqCst);
                    json!({"type":"ok"})
                }
                _ => unreachable!(),
            };
            writeln!(stream, "{}", json!({"id":request_id,"result":result})).unwrap();
            calls.push(request);
        }
        calls
    })
}

struct AttachedProcess(Child);

impl Drop for AttachedProcess {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn fake_attached_opencode(directory: &Path) -> PathBuf {
    let executable = directory.join("attached-opencode");
    fs::write(
        &executable,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > '{}'\nexec sleep 30\n",
            directory.join("attached-opencode-args").display()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&executable, permissions).unwrap();
    executable
}

fn spawn_background_abort_opencode(tab_closed: Arc<AtomicBool>) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let server = thread::spawn(move || {
        let event_count = Arc::new(AtomicUsize::new(0));
        let message_id = Arc::new(Mutex::new(None::<String>));
        let mut handlers = Vec::new();
        for _ in 0..6 {
            let (mut stream, _) = accept_tcp_before(&listener, Duration::from_secs(3));
            let event_count = Arc::clone(&event_count);
            let message_id = Arc::clone(&message_id);
            let tab_closed = Arc::clone(&tab_closed);
            handlers.push(thread::spawn(move || {
                let request = read_http_request(&mut stream);
                if request.path.starts_with("/session?") {
                    write_http_response(
                        &mut stream,
                        "200 OK",
                        "application/json",
                        r#"{"id":"ses_target"}"#,
                    );
                } else if request.path == "/event" {
                    let index = event_count.fetch_add(1, Ordering::SeqCst);
                    if index == 0 {
                        write_http_response(&mut stream, "200 OK", "text/event-stream", "");
                    } else {
                        write!(
                            stream,
                            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n"
                        )
                        .unwrap();
                        stream.flush().unwrap();
                        wait_for_flag(&tab_closed, Duration::from_secs(2));
                        let parent_id = message_id.lock().unwrap().clone().unwrap();
                        write!(stream, "{}", terminal_sse(&parent_id)).unwrap();
                    }
                } else if request.path == "/session/ses_target/prompt_async" {
                    *message_id.lock().unwrap() =
                        request.body["messageID"].as_str().map(ToOwned::to_owned);
                    write_http_response(&mut stream, "204 No Content", "text/plain", "");
                } else if request.path == "/session/ses_target/message" {
                    write_http_response(&mut stream, "200 OK", "application/json", "[]");
                } else if request.path == "/session/ses_target/abort" {
                    write_http_response(&mut stream, "200 OK", "application/json", "true");
                } else {
                    panic!("unexpected OpenCode request: {}", request.path);
                }
            }));
        }
        for handler in handlers {
            handler.join().unwrap();
        }
    });
    (port, server)
}

fn terminal_sse(parent_id: &str) -> String {
    let message = json!({
        "id":"evt_message",
        "type":"message.updated",
        "properties":{
            "sessionID":"ses_target",
            "info":{
                "id":"msg_assistant",
                "sessionID":"ses_target",
                "role":"assistant",
                "parentID":parent_id,
                "time":{"created":1,"completed":2},
                "structured":{"status":"done"}
            }
        }
    });
    let idle = json!({
        "id":"evt_idle",
        "type":"session.idle",
        "properties":{"sessionID":"ses_target"}
    });
    format!("id: evt_message\ndata: {message}\n\nid: evt_idle\ndata: {idle}\n\n")
}

fn accept_unix_before(
    listener: &UnixListener,
    timeout: Duration,
) -> (UnixStream, std::os::unix::net::SocketAddr) {
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

fn accept_tcp_before(
    listener: &TcpListener,
    timeout: Duration,
) -> (TcpStream, std::net::SocketAddr) {
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok(connection) => return connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for OpenCode call"
                );
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("fake OpenCode accept failed: {error}"),
        }
    }
}

fn wait_for_ref_tab(home: &Path, reference: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if ref_store(home)
            .resolve(reference)
            .unwrap()
            .is_some_and(|record| record.herdr_tab.as_deref() == Some("spawned-t1"))
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for attached tab"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for_flag(flag: &AtomicBool, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !flag.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "timed out waiting for tab close");
        thread::sleep(Duration::from_millis(5));
    }
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
