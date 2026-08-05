//! Process and source-level gates for the warm dispatch invocation budget.
//!
//! T28's first run with reqwest's default TLS backend measured p50 16.328 ms
//! and p95 19.288 ms on the development host. OpenCode is loopback-only, so
//! disabling that unused backend removes its per-process initialization cost.

#![cfg(unix)]

use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

use oca_state::{RefRecord, RefState, RefStore, RefStorePaths};
use serde_json::json;

const WARMUP_INVOCATIONS: usize = 10;
const MEASURED_INVOCATIONS: usize = 100;
const WARM_P95_LIMIT: Duration = Duration::from_millis(10);

#[test]
fn warm_subprocess_ack_p95_is_under_ten_milliseconds() {
    let fixture = WarmFixture::new();

    for _ in 0..WARMUP_INVOCATIONS {
        let sample = fixture.dispatch_to_ack();
        fixture.assert_permitted_pre_ack_requests(&sample);
    }

    let mut elapsed = Vec::with_capacity(MEASURED_INVOCATIONS);
    for _ in 0..MEASURED_INVOCATIONS {
        let sample = fixture.dispatch_to_ack();
        fixture.assert_permitted_pre_ack_requests(&sample);
        elapsed.push(sample.elapsed);
    }
    elapsed.sort_unstable();

    let p95_index = (elapsed.len() * 95).div_ceil(100) - 1;
    let p95 = elapsed[p95_index];
    let median = elapsed[elapsed.len() / 2];
    eprintln!(
        "warm dispatch timing: p50={:.3} ms, p95={:.3} ms over {} measured invocations",
        milliseconds(median),
        milliseconds(p95),
        elapsed.len()
    );
    assert!(
        p95 < WARM_P95_LIMIT,
        "warm dispatch exceeded the hard 10 ms ack gate: p50={:.3} ms, p95={:.3} ms, samples_ms={:?}",
        milliseconds(median),
        milliseconds(p95),
        elapsed
            .iter()
            .copied()
            .map(milliseconds)
            .collect::<Vec<_>>()
    );
}

#[test]
fn warm_ack_has_no_forbidden_network_process_or_herdr_operation() {
    let fixture = WarmFixture::new();
    let sample = fixture.dispatch_to_ack();

    fixture.assert_permitted_pre_ack_requests(&sample);
    assert_eq!(
        fixture.process_calls(),
        Vec::<String>::new(),
        "the warm path must not start git, OpenCode probes, interpreters, or SDK commands"
    );
    fixture.assert_no_herdr_call();
}

#[test]
fn worktree_list_never_runs_dispatch_git_or_network_operations() {
    let fixture = WarmFixture::new();
    fixture.insert_worktree_ref();
    let requests_before = fixture.server.request_count();

    let output = fixture
        .command(["ls", "--all"])
        .output()
        .expect("oca ls runs");
    fixture.server.barrier();

    assert!(
        output.status.success(),
        "oca ls failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.server.request_count(), requests_before);
    assert_eq!(fixture.process_calls(), Vec::<String>::new());
    fixture.assert_no_herdr_call();
}

#[test]
fn warm_prefix_sources_reject_forbidden_operation_families() {
    let root = workspace_root();
    let core = fs::read_to_string(root.join("crates/oca-core/src/foreground.rs"))
        .expect("core foreground source is readable");
    let production = fs::read_to_string(root.join("crates/oca-cli/src/foreground.rs"))
        .expect("production foreground source is readable");
    let policy = fs::read_to_string(root.join("crates/oca-core/src/policy.rs"))
        .expect("policy source is readable");
    let refs = fs::read_to_string(root.join("crates/oca-state/src/refs.rs"))
        .expect("ref-store source is readable");
    let intents = fs::read_to_string(root.join("crates/oca-state/src/intents.rs"))
        .expect("intent-store source is readable");
    let recovery = fs::read_to_string(root.join("crates/oca-cli/src/crash_recovery.rs"))
        .expect("crash-recovery source is readable");

    let dispatch_prefix =
        source_between(&core, "pub(crate) async fn start_dispatch", "#[cfg(test)]");
    let production_prefix = source_between(
        &production,
        "impl ForegroundBackend for ProductionBackend",
        "    fn spawn_attach(",
    );
    let production_prepare = source_between(
        &production,
        "pub(crate) fn prepare_dispatch",
        "pub(crate) struct ProductionBackend",
    );
    let policy_production = policy.split("#[cfg(test)]").next().unwrap_or(&policy);
    let acknowledgement_durability = source_between(
        &refs,
        "    pub fn acknowledge_with<E>(",
        "/// The result after the first post-ack directory-durability attempt.",
    );
    let intent_write = source_between(
        &intents,
        "    pub fn write(",
        "    /// Reads one intent without changing it.",
    );
    let intent_pre_ack_write = intent_write
        .split("            if durability == IntentDurability::PostAck {")
        .next()
        .expect("pre-ack intent write precedes the explicit post-ack flush");
    let intent_persistence = source_between(
        &recovery,
        "pub(crate) fn persist_intent(",
        "pub(crate) fn remove_intent(",
    );

    for (name, source) in [
        ("core dispatch prefix", dispatch_prefix),
        ("production dispatch prefix", production_prefix),
        ("production preparation", production_prepare),
        ("worker policy construction", policy_production),
    ] {
        for forbidden in [
            "ProcessCommand::",
            "std::process::Command",
            "HerdrClient",
            ".messages(",
            ".status(",
            "health_check",
            "health_probe",
            "\"/doc\"",
            "\"--version\"",
            "\"doctor\"",
            "WorktreeManager::validate",
            ".validate(",
            ".sync_all(",
            ".sync_data(",
            "thread::spawn",
            "watcher",
            "relay",
            "broker",
            "registry",
            "help_text(",
            "render_failure(",
        ] {
            assert!(
                !source.contains(forbidden),
                "{name} contains forbidden warm-path operation `{forbidden}`"
            );
        }
    }

    assert_ordered(
        dispatch_prefix,
        &[
            "backend.prepare(",
            "backend.create_session(",
            "backend.subscribe(",
            "backend.prompt_async(",
            "backend.write_ref(",
            "backend.acknowledge(",
            "backend.spawn_attach(",
        ],
    );
    assert_ordered(
        acknowledgement_durability,
        &["acknowledge(&self.record)?", "self.finish_after_ack()"],
    );
    let before_finish = acknowledgement_durability
        .split("pub fn finish_after_ack")
        .next()
        .expect("acknowledgement precedes deferred durability");
    assert!(
        !before_finish.contains("sync_directory("),
        "parent-directory fsync must remain after the acknowledgement callback"
    );
    assert!(
        acknowledgement_durability.contains("sync_directory(&self.parent)"),
        "the post-ack durability owner must retain the parent-directory sync"
    );
    for forbidden in [".sync_all(", ".sync_data(", "sync_directory("] {
        assert!(
            !intent_pre_ack_write.contains(forbidden),
            "pre-ack intent replacement contains forbidden durability operation `{forbidden}`"
        );
    }
    assert_eq!(
        intent_write.matches(".sync_all(").count(),
        1,
        "intent replacement must contain exactly one explicitly post-ack file sync"
    );
    assert_eq!(
        intent_write.matches(".sync_data(").count(),
        0,
        "intent replacement must not introduce another file-sync operation"
    );
    assert_eq!(
        intent_write.matches("sync_directory(").count(),
        1,
        "intent replacement must contain exactly one explicitly post-ack directory sync"
    );
    assert_eq!(
        intent_write
            .matches("if durability == IntentDurability::PostAck {")
            .count(),
        2,
        "file and directory syncs must remain guarded by explicit post-ack durability"
    );
    assert!(
        intent_persistence.contains("intent.phase >= IntentPhase::TerminalObserved")
            && intent_persistence.contains("IntentDurability::PostAck")
            && intent_persistence.contains("IntentDurability::PreAck"),
        "intent persistence must select durability explicitly at terminal_observed"
    );
}

struct DispatchSample {
    elapsed: Duration,
    acknowledgement: String,
    pre_ack_requests: Vec<String>,
}

struct WarmFixture {
    home: tempfile::TempDir,
    repo: tempfile::TempDir,
    fake_bin: PathBuf,
    process_log: PathBuf,
    herdr: UnixListener,
    server: FakeServer,
}

impl WarmFixture {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("temporary home");
        let repo = tempfile::tempdir().expect("temporary repository");
        fs::create_dir(repo.path().join(".git")).expect("repository marker");

        let server = FakeServer::start();
        let state = home.path().join(".oca");
        fs::create_dir(&state).expect("state directory");
        fs::write(
            state.join("server.json"),
            serde_json::to_vec(&json!({
                "port": server.port(),
                "version": "1.18.10",
                "environment_hash": "warm-path-gate"
            }))
            .expect("server record serializes"),
        )
        .expect("server record writes");

        let herdr_path = home.path().join("fake-herdr.sock");
        let herdr = UnixListener::bind(&herdr_path).expect("fake herdr binds");
        herdr
            .set_nonblocking(true)
            .expect("fake herdr is nonblocking");
        fs::write(
            state.join("config.toml"),
            format!("[herdr]\nsocket = {:?}\n", herdr_path.display().to_string()),
        )
        .expect("warm config writes");

        let fake_bin = home.path().join("fake-bin");
        fs::create_dir(&fake_bin).expect("fake executable directory");
        let process_log = home.path().join("process-calls.log");
        install_process_spies(&fake_bin);

        Self {
            home,
            repo,
            fake_bin,
            process_log,
            herdr,
            server,
        }
    }

    fn command<const N: usize>(&self, arguments: [&str; N]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oca"));
        command
            .args(arguments)
            .env("HOME", self.home.path())
            .env("OCA_SPAWNER", "warm-gate")
            .env("OCA_WARM_PROCESS_LOG", &self.process_log)
            .env("PATH", &self.fake_bin)
            .current_dir(self.repo.path());
        command
    }

    fn dispatch_to_ack(&self) -> DispatchSample {
        let mut command = self.command(["luna:h", "--headless", "warm", "dispatch"]);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());

        let started = Instant::now();
        let mut child = command.spawn().expect("warm oca subprocess starts");
        let mut stdout = BufReader::new(child.stdout.take().expect("child stdout is piped"));
        let mut acknowledgement = String::new();
        stdout
            .read_line(&mut acknowledgement)
            .expect("acknowledgement reads");
        let elapsed = started.elapsed();

        let pre_ack_requests = self.server.acknowledge();
        finish_after_ack(&mut child);
        self.server.barrier();

        assert!(
            acknowledgement.starts_with('w') && acknowledgement.ends_with('\n'),
            "expected one flushed acknowledgement, got {acknowledgement:?}"
        );
        DispatchSample {
            elapsed,
            acknowledgement,
            pre_ack_requests,
        }
    }

    fn assert_permitted_pre_ack_requests(&self, sample: &DispatchSample) {
        assert!(
            sample
                .acknowledgement
                .contains(" running openai/gpt-5.6-luna:high"),
            "unexpected acknowledgement: {:?}",
            sample.acknowledgement
        );
        assert_eq!(
            sample
                .pre_ack_requests
                .iter()
                .map(|path| route(path))
                .collect::<Vec<_>>(),
            ["session.create", "event.subscribe", "session.prompt_async"],
            "the warm path made a forbidden request before acknowledgement"
        );
        assert!(
            sample.pre_ack_requests.iter().all(|path| {
                !path.contains("health")
                    && !path.contains("status")
                    && !path.ends_with("/message")
                    && path != "/doc"
            }),
            "health, status, documentation, and full-history reads are forbidden before ack: {:?}",
            sample.pre_ack_requests
        );
    }

    fn insert_worktree_ref(&self) {
        let store =
            RefStore::with_paths(RefStorePaths::in_directory(self.home.path().join(".oca")));
        store
            .insert(RefRecord {
                id: "w4f2a1".to_owned(),
                session_id: "ses_worktree".to_owned(),
                message_id: Some("msg_worktree".to_owned()),
                alias: Some("luna".to_owned()),
                effort: Some("high".to_owned()),
                role: Some("impl".to_owned()),
                cwd: Some(self.repo.path().display().to_string()),
                last_state: Some(RefState::Running),
                repo: Some(self.repo.path().display().to_string()),
                spawner_tag: Some("warm-gate".to_owned()),
                worktree: Some(self.repo.path().display().to_string()),
                branch: Some("oca/w4f2a1".to_owned()),
                commit: None,
                commit_subject: Some("Warm path gate".to_owned()),
                display: None,
                herdr_tab: None,
                completion: None,
                tombstoned: false,
            })
            .expect("worktree ref inserts");
    }

    fn process_calls(&self) -> Vec<String> {
        fs::read_to_string(&self.process_log)
            .unwrap_or_default()
            .lines()
            .map(ToOwned::to_owned)
            .collect()
    }

    fn assert_no_herdr_call(&self) {
        let error = self
            .herdr
            .accept()
            .expect_err("attachment-disabled commands must not connect to herdr");
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    }
}

fn finish_after_ack(child: &mut Child) {
    let status = child
        .wait()
        .expect("warm subprocess exits after fake SSE closes");
    assert!(
        !status.success(),
        "the deliberately closed post-ack event stream should end the foreground process"
    );
    let mut stderr = Vec::new();
    child
        .stderr
        .take()
        .expect("child stderr is piped")
        .read_to_end(&mut stderr)
        .expect("child stderr reads");
    assert!(
        String::from_utf8_lossy(&stderr).contains("server_unreachable"),
        "unexpected post-ack failure: {}",
        String::from_utf8_lossy(&stderr)
    );
}

fn install_process_spies(directory: &Path) {
    let script = b"#!/bin/sh\nprintf '%s\\n' \"$0 $*\" >> \"$OCA_WARM_PROCESS_LOG\"\nexit 97\n";
    for executable in [
        "git", "opencode", "python", "python3", "node", "ruby", "perl", "deno", "bun",
    ] {
        let path = directory.join(executable);
        fs::write(&path, script).expect("process spy writes");
        let mut permissions = fs::metadata(&path)
            .expect("process spy metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("process spy is executable");
    }
}

struct FakeServer {
    port: u16,
    shared: Arc<(Mutex<ServerState>, Condvar)>,
    worker: Option<thread::JoinHandle<()>>,
}

#[derive(Default)]
struct ServerState {
    shutdown: bool,
    next_session: usize,
    current: Option<CurrentDispatch>,
    requests: Vec<String>,
    barriers: usize,
}

struct CurrentDispatch {
    awaiting_ack: bool,
    pre_ack_requests: Vec<String>,
}

impl FakeServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("warm fake server binds");
        let port = listener.local_addr().expect("fake server address").port();
        let shared = Arc::new((Mutex::new(ServerState::default()), Condvar::new()));
        let server_shared = Arc::clone(&shared);
        let worker = thread::spawn(move || serve(listener, &server_shared));
        Self {
            port,
            shared,
            worker: Some(worker),
        }
    }

    fn port(&self) -> u16 {
        self.port
    }

    fn acknowledge(&self) -> Vec<String> {
        let (lock, ready) = &*self.shared;
        let mut state = lock.lock().expect("server state locks");
        let current = state
            .current
            .as_mut()
            .expect("a dispatch reached the fake server before ack");
        assert!(current.awaiting_ack, "dispatch was already acknowledged");
        current.awaiting_ack = false;
        let requests = current.pre_ack_requests.clone();
        ready.notify_all();
        requests
    }

    fn barrier(&self) {
        let target = {
            let (lock, _) = &*self.shared;
            lock.lock().expect("server state locks").barriers + 1
        };
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).expect("barrier connects");
        stream
            .write_all(b"GET /__warm_test_barrier HTTP/1.1\r\nhost: localhost\r\n\r\n")
            .expect("barrier request writes");
        let mut response = [0_u8; 64];
        let _ = stream.read(&mut response).expect("barrier response reads");

        let (lock, ready) = &*self.shared;
        let state = lock.lock().expect("server state locks");
        drop(
            ready
                .wait_while(state, |state| state.barriers < target)
                .expect("barrier wait locks"),
        );
    }

    fn request_count(&self) -> usize {
        let (lock, _) = &*self.shared;
        lock.lock().expect("server state locks").requests.len()
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        let (lock, ready) = &*self.shared;
        {
            let mut state = lock.lock().expect("server state locks");
            state.shutdown = true;
            if let Some(current) = &mut state.current {
                current.awaiting_ack = false;
            }
            ready.notify_all();
        }
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(worker) = self.worker.take() {
            worker.join().expect("fake server exits");
        }
    }
}

fn serve(listener: TcpListener, shared: &Arc<(Mutex<ServerState>, Condvar)>) {
    loop {
        let (mut stream, _) = listener.accept().expect("fake server accepts");
        {
            let (lock, _) = &**shared;
            if lock.lock().expect("server state locks").shutdown {
                return;
            }
        }
        let Some(path) = read_request_path(&mut stream) else {
            continue;
        };
        if path == "/__warm_test_barrier" {
            write_response(&mut stream, "204 No Content", "text/plain", "");
            let (lock, ready) = &**shared;
            lock.lock().expect("server state locks").barriers += 1;
            ready.notify_all();
            continue;
        }

        let (response_status, response_type, response_body, wait_for_ack) = {
            let (lock, _) = &**shared;
            let mut state = lock.lock().expect("server state locks");
            state.requests.push(path.clone());

            if path.starts_with("/session?") {
                state.next_session += 1;
                let session_id = format!("ses_warm_{}", state.next_session);
                state.current = Some(CurrentDispatch {
                    awaiting_ack: true,
                    pre_ack_requests: vec![path.clone()],
                });
                (
                    "200 OK",
                    "application/json",
                    format!(r#"{{"id":"{session_id}"}}"#),
                    false,
                )
            } else {
                // Every pre-ack request is recorded, including one naming a
                // session this dispatch never created: a poll against any
                // session is exactly what the warm-path budget forbids. Each
                // dispatch waits for its child to exit and barriers the server
                // before the next one starts, so no earlier process can still
                // be in flight here.
                if let Some(current) = state
                    .current
                    .as_mut()
                    .filter(|current| current.awaiting_ack)
                {
                    current.pre_ack_requests.push(path.clone());
                }

                if path == "/event" {
                    ("200 OK", "text/event-stream", String::new(), false)
                } else if path.ends_with("/prompt_async") {
                    ("204 No Content", "text/plain", String::new(), true)
                } else if path.ends_with("/message") {
                    ("200 OK", "application/json", "[]".to_owned(), false)
                } else {
                    ("200 OK", "application/json", "{}".to_owned(), false)
                }
            }
        };
        write_response(&mut stream, response_status, response_type, &response_body);

        if wait_for_ack {
            let (lock, ready) = &**shared;
            let state = lock.lock().expect("server state locks");
            drop(
                ready
                    .wait_while(state, |state| {
                        !state.shutdown
                            && state
                                .current
                                .as_ref()
                                .is_some_and(|current| current.awaiting_ack)
                    })
                    .expect("ack wait locks"),
            );
        }
    }
}

fn read_request_path(stream: &mut TcpStream) -> Option<String> {
    let mut reader = BufReader::new(stream.try_clone().expect("request stream clones"));
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .expect("request line reads");
    request_line
        .split_whitespace()
        .nth(1)
        .map(ToOwned::to_owned)
}

fn write_response(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .expect("fake response writes");
}

fn route(path: &str) -> &'static str {
    if path.starts_with("/session?") {
        "session.create"
    } else if path == "/event" {
        "event.subscribe"
    } else if path.ends_with("/prompt_async") {
        "session.prompt_async"
    } else if path.ends_with("/message") {
        "messages"
    } else if path.contains("health") {
        "health"
    } else if path.contains("status") {
        "status"
    } else {
        "forbidden"
    }
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("oca-cli belongs to the workspace")
        .to_path_buf()
}

fn source_between<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    let (_, after_start) = source
        .split_once(start)
        .unwrap_or_else(|| panic!("source marker `{start}` exists"));
    let (body, _) = after_start
        .split_once(end)
        .unwrap_or_else(|| panic!("source marker `{end}` exists after `{start}`"));
    body
}

fn assert_ordered(source: &str, operations: &[&str]) {
    let mut cursor = 0;
    for operation in operations {
        let offset = source[cursor..]
            .find(operation)
            .unwrap_or_else(|| panic!("warm dispatch prefix must contain `{operation}`"));
        cursor += offset + operation.len();
    }
}
