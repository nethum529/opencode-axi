use std::{
    io::{BufRead, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Output},
    thread,
    time::Duration,
};

use oca_server::{ConnectOrStart, ServerRecord};
use serde_json::{Value, json};

struct CapturedRequest {
    path: String,
    body: Value,
}

#[test]
fn partial_dispatch_and_second_oca_m_create_two_original_prompt_commits() {
    let repository = TestRepository::new();
    let home = tempfile::tempdir().expect("temporary home");
    let listener = TcpListener::bind("127.0.0.1:0").expect("fake server binds");
    let port = listener.local_addr().unwrap().port();
    prepare_home(home.path(), port);
    let server = thread::spawn(move || serve_partial_then_review(listener, ReviewReply::Valid));

    let initial = run_oca(
        home.path(),
        repository.path(),
        [
            "luna:h",
            "-w",
            "--headless",
            "implement",
            "retry",
            "handling",
            "across",
            "transport",
            "boundaries!!!",
        ],
    );
    assert_success(&initial);
    let reference = String::from_utf8(initial.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let first_record = stored_ref(home.path(), &reference);
    let worktree = PathBuf::from(first_record["worktree"].as_str().unwrap());
    assert_eq!(first_record["last_state"], "partial");
    assert_eq!(first_record["display"], "headless");
    let first_commit = first_record["commit"].as_str().unwrap().to_owned();

    let message = run_oca(
        home.path(),
        repository.path(),
        ["m", &reference, "apply", "the", "review", "findings"],
    );
    assert_success(&message);
    let follow = run_oca(home.path(), repository.path(), ["f", &reference]);
    assert_success(&follow);
    server.join().expect("fake server completes");

    let final_record = stored_ref(home.path(), &reference);
    let second_commit = final_record["commit"].as_str().unwrap();
    assert_ne!(
        first_commit, second_commit,
        "oca m must create a new commit"
    );
    assert_eq!(final_record["last_state"], "done");
    let expected = format!("oca {reference}: implement retry handling across transport boundaries");
    let subjects = git_output(&worktree, ["log", "-2", "--format=%s"]);
    assert_eq!(
        subjects.lines().collect::<Vec<_>>(),
        [expected.as_str(), expected.as_str()]
    );
    assert!(!subjects.contains("WORKER-SUPPLIED"));
    assert_eq!(
        git_output(&worktree, ["rev-list", "--count", "HEAD"]).trim(),
        "3"
    );
}

#[test]
fn worktree_rate_limit_creates_no_commit() {
    let repository = TestRepository::new();
    let home = tempfile::tempdir().expect("temporary home");
    let listener = TcpListener::bind("127.0.0.1:0").expect("fake server binds");
    let port = listener.local_addr().unwrap().port();
    prepare_home(home.path(), port);
    let server = thread::spawn(move || serve_rate_limit(listener));

    let output = run_oca(
        home.path(),
        repository.path(),
        ["luna:h", "-w", "--headless", "make", "a", "change"],
    );
    server.join().expect("fake server completes");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("code: rate_limited"));
    let record = only_stored_ref(home.path());
    let worktree = PathBuf::from(record["worktree"].as_str().unwrap());
    assert!(record.get("commit").is_none());
    assert_eq!(
        git_output(&worktree, ["rev-list", "--count", "HEAD"]).trim(),
        "1"
    );
}

#[test]
fn contract_invalid_schema_and_floor_preserve_the_exact_worker_diff() {
    for invalid in [InvalidReply::Structural, InvalidReply::Floor] {
        let repository = TestRepository::new();
        let home = tempfile::tempdir().expect("temporary home");
        let listener = TcpListener::bind("127.0.0.1:0").expect("fake server binds");
        let port = listener.local_addr().unwrap().port();
        prepare_home(home.path(), port);
        let server = thread::spawn(move || serve_invalid_reply(listener, invalid));

        let output = run_oca(
            home.path(),
            repository.path(),
            ["luna:h", "-w", "--headless", "write", "worker", "output"],
        );
        server.join().expect("fake server completes");

        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("code: contract_invalid"));
        let record = only_stored_ref(home.path());
        let worktree = PathBuf::from(record["worktree"].as_str().unwrap());
        assert_eq!(record["last_state"], "running");
        assert_eq!(
            std::fs::read(worktree.join("worker.txt")).unwrap(),
            b"worker bytes\n"
        );
        assert_eq!(
            git_output(&worktree, ["status", "--porcelain"]),
            "?? worker.txt\n"
        );
        assert_eq!(
            git_output(&worktree, ["diff", "--cached", "--name-only"]),
            ""
        );
        assert_eq!(
            git_output(&worktree, ["rev-list", "--count", "HEAD"]).trim(),
            "1"
        );
        assert!(record.get("commit").is_none());
    }
}

#[test]
fn contract_invalid_review_turn_leaves_the_first_commit_alone() {
    let repository = TestRepository::new();
    let home = tempfile::tempdir().expect("temporary home");
    let listener = TcpListener::bind("127.0.0.1:0").expect("fake server binds");
    let port = listener.local_addr().unwrap().port();
    prepare_home(home.path(), port);
    let server =
        thread::spawn(move || serve_partial_then_review(listener, ReviewReply::FloorInvalid));

    let initial = run_oca(
        home.path(),
        repository.path(),
        [
            "luna:h",
            "-w",
            "--headless",
            "implement",
            "retry",
            "handling",
        ],
    );
    assert_success(&initial);
    let reference = String::from_utf8(initial.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let first_record = stored_ref(home.path(), &reference);
    let worktree = PathBuf::from(first_record["worktree"].as_str().unwrap());
    let first_commit = first_record["commit"].as_str().unwrap().to_owned();

    assert_success(&run_oca(
        home.path(),
        repository.path(),
        ["m", &reference, "apply", "the", "review", "findings"],
    ));
    let follow = run_oca(home.path(), repository.path(), ["f", &reference]);
    server.join().expect("fake server completes");

    assert!(!follow.status.success());
    assert!(
        String::from_utf8_lossy(&follow.stderr).contains("code: contract_invalid"),
        "{}",
        String::from_utf8_lossy(&follow.stderr)
    );
    assert_eq!(
        stored_ref(home.path(), &reference)["commit"].as_str(),
        Some(first_commit.as_str()),
        "a rejected review reply must not record a new commit"
    );
    assert_eq!(
        git_output(&worktree, ["rev-list", "--count", "HEAD"]).trim(),
        "2",
        "the review turn must add no commit"
    );
}

#[derive(Clone, Copy)]
enum InvalidReply {
    Structural,
    Floor,
}

#[derive(Clone, Copy)]
enum ReviewReply {
    Valid,
    FloorInvalid,
}

fn serve_partial_then_review(listener: TcpListener, review: ReviewReply) {
    let mut worktree = None;
    let mut first_message = None;
    let mut second_message = None;
    for index in 0..8 {
        let (mut stream, _) = listener.accept().expect("fake accepts request");
        let request = read_request(&mut stream);
        match index {
            0 => {
                let directory = session_directory(&request.path);
                assert!(
                    directory.join(".git").exists(),
                    "worktree must predate session creation"
                );
                worktree = Some(directory);
                write_response(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    r#"{"id":"ses_worktree"}"#,
                );
            }
            1 => write_response(
                &mut stream,
                "200 OK",
                "text/event-stream",
                "id: evt_one\nevent: session.idle\ndata: {\"sessionID\":\"ses_worktree\"}\n\n",
            ),
            2 => {
                first_message = request.body["messageID"].as_str().map(ToOwned::to_owned);
                std::fs::write(worktree.as_ref().unwrap().join("first.txt"), "partial\n").unwrap();
                write_response(&mut stream, "204 No Content", "text/plain", "");
            }
            3 => write_response(&mut stream, "200 OK", "application/json", "[]"),
            4 => {
                let body = assistant_reply(
                    first_message.as_deref().unwrap(),
                    "partial",
                    "Implemented the first checkpoint while preserving the requested worktree boundaries and deterministic validation behavior. Verified this useful partial result is ready for a stable review commit.",
                );
                write_response(&mut stream, "200 OK", "application/json", &body.to_string());
            }
            5 => {
                second_message = request.body["messageID"].as_str().map(ToOwned::to_owned);
                std::fs::write(worktree.as_ref().unwrap().join("second.txt"), "review\n").unwrap();
                write_response(&mut stream, "204 No Content", "text/plain", "");
            }
            6 => write_response(&mut stream, "200 OK", "text/event-stream", ""),
            7 => {
                let note = match review {
                    ReviewReply::Valid => {
                        "Applied every review finding in the existing worktree without changing the original task identity or commit message source. Verified the second checkpoint is complete and independently reviewable. WORKER-SUPPLIED"
                    }
                    ReviewReply::FloorInvalid => "Too short.",
                };
                let body = assistant_reply(second_message.as_deref().unwrap(), "done", note);
                write_response(&mut stream, "200 OK", "application/json", &body.to_string());
            }
            _ => unreachable!(),
        }
    }
}

fn serve_rate_limit(listener: TcpListener) {
    for index in 0..3 {
        let (mut stream, _) = listener.accept().expect("fake accepts request");
        let request = read_request(&mut stream);
        match index {
            0 => {
                let directory = session_directory(&request.path);
                assert!(directory.join(".git").exists());
                write_response(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    r#"{"id":"ses_limited"}"#,
                );
            }
            1 => write_response(&mut stream, "200 OK", "text/event-stream", ""),
            2 => write_response(
                &mut stream,
                "429 Too Many Requests",
                "text/plain",
                "slow down",
            ),
            _ => unreachable!(),
        }
    }
}

fn serve_invalid_reply(listener: TcpListener, invalid: InvalidReply) {
    let mut worktree = None;
    let mut message_id = None;
    for index in 0..5 {
        let (mut stream, _) = listener.accept().expect("fake accepts request");
        let request = read_request(&mut stream);
        match index {
            0 => {
                let directory = session_directory(&request.path);
                assert!(directory.join(".git").exists());
                worktree = Some(directory);
                write_response(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    r#"{"id":"ses_invalid"}"#,
                );
            }
            1 => write_response(
                &mut stream,
                "200 OK",
                "text/event-stream",
                "id: evt_invalid\nevent: session.idle\ndata: {\"sessionID\":\"ses_invalid\"}\n\n",
            ),
            2 => {
                message_id = request.body["messageID"].as_str().map(ToOwned::to_owned);
                std::fs::write(
                    worktree.as_ref().unwrap().join("worker.txt"),
                    "worker bytes\n",
                )
                .unwrap();
                write_response(&mut stream, "204 No Content", "text/plain", "");
            }
            3 => write_response(&mut stream, "200 OK", "application/json", "[]"),
            4 => {
                let structured = match invalid {
                    InvalidReply::Structural => json!({
                        "status":"done", "files":[], "note":"This structurally invalid report deliberately carries an unexpected field after the worker writes its output. The client must reject it before staging or committing any byte.", "unexpected":true
                    }),
                    InvalidReply::Floor => json!({
                        "status":"done", "files":[], "note":"Too short."
                    }),
                };
                let body = json!([{
                    "info": {
                        "id":"msg_assistant_invalid",
                        "sessionID":"ses_invalid",
                        "role":"assistant",
                        "parentID":message_id.as_deref().unwrap(),
                        "structured":structured
                    },
                    "parts":[]
                }]);
                write_response(&mut stream, "200 OK", "application/json", &body.to_string());
            }
            _ => unreachable!(),
        }
    }
}

fn assistant_reply(parent: &str, status: &str, note: &str) -> Value {
    json!([{
        "info": {
            "id":format!("msg_assistant_{status}"),
            "sessionID":"ses_worktree",
            "role":"assistant",
            "parentID":parent,
            "time":{"created":1,"completed":2},
            "structured":{"status":status,"files":[],"note":note}
        },
        "parts":[]
    }])
}

struct TestRepository(tempfile::TempDir);

impl TestRepository {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("temporary repository");
        run_git(directory.path(), ["init", "--quiet"]);
        run_git(directory.path(), ["config", "user.name", "oca test"]);
        run_git(
            directory.path(),
            ["config", "user.email", "oca@example.test"],
        );
        std::fs::write(directory.path().join("README.md"), "base\n").unwrap();
        run_git(directory.path(), ["add", "README.md"]);
        run_git(directory.path(), ["commit", "--quiet", "-m", "base"]);
        Self(directory)
    }

    fn path(&self) -> &Path {
        self.0.path()
    }
}

fn prepare_home(home: &Path, port: u16) {
    let state = home.join(".oca");
    std::fs::create_dir(&state).unwrap();
    std::fs::write(state.join("config.toml"), "").unwrap();
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

fn run_oca<const N: usize>(home: &Path, cwd: &Path, arguments: [&str; N]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oca"))
        .args(arguments)
        .env("HOME", home)
        .current_dir(cwd)
        .output()
        .expect("oca runs")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "oca failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stored_ref(home: &Path, reference: &str) -> Value {
    let refs: Vec<Value> =
        serde_json::from_slice(&std::fs::read(home.join(".oca/refs.json")).unwrap()).unwrap();
    refs.into_iter()
        .find(|record| record["id"] == reference)
        .unwrap()
}

fn only_stored_ref(home: &Path) -> Value {
    let refs: Vec<Value> =
        serde_json::from_slice(&std::fs::read(home.join(".oca/refs.json")).unwrap()).unwrap();
    assert_eq!(refs.len(), 1);
    refs.into_iter().next().unwrap()
}

fn session_directory(path: &str) -> PathBuf {
    url::Url::parse(&format!("http://localhost{path}"))
        .unwrap()
        .query_pairs()
        .find_map(|(key, value)| (key == "directory").then(|| PathBuf::from(value.as_ref())))
        .expect("session directory query")
}

fn read_request(stream: &mut TcpStream) -> CapturedRequest {
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
    CapturedRequest { path, body }
}

fn write_response(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
}

fn run_git<const N: usize>(directory: &Path, arguments: [&str; N]) {
    let status = Command::new("git")
        .args(["-C", directory.to_str().unwrap()])
        .args(arguments)
        .status()
        .unwrap();
    assert!(status.success(), "git {arguments:?} failed");
}

fn git_output<const N: usize>(directory: &Path, arguments: [&str; N]) -> String {
    let output = Command::new("git")
        .args(["-C", directory.to_str().unwrap()])
        .args(arguments)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {arguments:?} failed");
    String::from_utf8(output.stdout).unwrap()
}
