//! Fixture-driven acceptance coverage for transport cuts and HTTP 429 handling.

use std::{process::Command, thread};

use oca_core::{ErrorCode, exit, parse_error_envelope};
use oca_server::{ConnectOrStart, ServerRecord};
use oca_state::{RefRecord, RefState, RefStore, RefStorePaths};
use oca_testkit::{FailureAction, FailureHttpServer, HttpResponse};

#[test]
fn pre_transmit_connection_cut_retries_session_creation_once_and_prompts_once() {
    let (home, server) = fixture([
        FailureAction::DropBeforeRequest,
        respond(200, "application/json", br#"[{"name":"impl"}]"#),
        respond(200, "application/json", br#"{"id":"ses_recovered"}"#),
        respond(200, "text/event-stream", b""),
        respond(204, "text/plain", b""),
        FailureAction::EchoPrompt {
            session_id: "ses_recovered".to_owned(),
        },
    ]);

    let output = run(&home, ["luna:h", "-b", "--headless", "do", "the", "work"]);
    let requests = server.join().expect("failure server thread");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert_eq!(
        routes(&requests),
        [
            "agents",
            "agents",
            "session",
            "event",
            "prompt_async",
            "messages"
        ]
    );
    assert_eq!(
        routes(&requests)
            .iter()
            .filter(|route| **route == "prompt_async")
            .count(),
        1
    );
    let records = stored_refs(&home);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].session_id, "ses_recovered");
    assert_eq!(records[0].last_state, Some(RefState::Running));
}

#[test]
fn headless_background_missing_prompt_is_uncertain_instead_of_running() {
    let (home, server) = missing_prompt_fixture("ses_headless_missing");

    let output = run(
        &home,
        [
            "--json",
            "luna:h",
            "-b",
            "--headless",
            "do",
            "not",
            "strand",
            "running",
        ],
    );
    let requests = server.join().expect("failure server thread");

    assert_uncertain_missing_prompt(&home, &output, &requests, "ses_headless_missing");
}

#[test]
fn foreground_missing_prompt_is_uncertain_before_terminal_wait() {
    let (home, server) = missing_prompt_fixture("ses_foreground_missing");

    let output = run(
        &home,
        [
            "--json",
            "luna:h",
            "--headless",
            "do",
            "not",
            "wait",
            "for",
            "a",
            "missing",
            "prompt",
        ],
    );
    let requests = server.join().expect("failure server thread");

    assert_uncertain_missing_prompt(&home, &output, &requests, "ses_foreground_missing");
}

#[test]
fn post_transmit_pre_response_cut_never_replays_and_persists_unknown_ref() {
    let (home, server) = fixture([
        respond(200, "application/json", br#"{"id":"ses_uncertain"}"#),
        respond(200, "text/event-stream", b""),
        FailureAction::DropAfterRequest,
    ]);

    let output = run(
        &home,
        ["--json", "luna:h", "-b", "--headless", "one", "shot"],
    );
    let requests = server.join().expect("failure server thread");

    assert_eq!(output.status.code(), Some(exit::FAILURE));
    assert!(
        output.stdout.is_empty(),
        "an uncertain prompt is never acknowledged"
    );
    let error = parsed_error(&output);
    assert_eq!(error.code(), ErrorCode::PromptUncertain.as_str());
    assert!(error.reference().is_some());
    assert_eq!(
        routes(&requests),
        ["agents", "session", "event", "prompt_async"]
    );
    assert_eq!(
        routes(&requests)
            .iter()
            .filter(|route| **route == "prompt_async")
            .count(),
        1
    );
    let records = stored_refs(&home);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].last_state, Some(RefState::Unknown));
    assert_eq!(records[0].id, error.reference().unwrap());

    let reference = error.reference().unwrap().to_owned();
    let recovery = FailureHttpServer::bind(
        "127.0.0.1:0",
        [
            respond(200, "text/event-stream", b""),
            respond(200, "application/json", b"[]"),
        ],
    )
    .expect("recovery server binds");
    replace_server_record(&home, recovery.local_addr().unwrap().port());
    let recovery = thread::spawn(move || recovery.serve().expect("recovery script completes"));
    let follow = run(&home, ["--json", "f", &reference]);
    let recovery_requests = recovery.join().expect("recovery server thread");

    let recovered_error = parsed_error(&follow);
    assert_eq!(recovered_error.code(), ErrorCode::PromptUncertain.as_str());
    assert!(
        recovered_error
            .help()
            .contains(&format!("oca m {reference}")),
        "the explicit resend command must be named"
    );
    assert_eq!(routes(&recovery_requests), ["event", "messages"]);
    assert_eq!(request_directory(&recovery_requests[0].path), home.path());
    assert!(
        !routes(&recovery_requests).contains(&"prompt_async"),
        "fresh reconciliation must never resend the uncertain prompt"
    );
}

#[test]
fn fake_500_after_worktree_prepare_leaves_no_ref_intent_branch_or_worktree() {
    let (home, server) = fixture([respond(500, "application/json", br#"{"error":"injected"}"#)]);
    init_repository(home.path());

    let output = run(
        &home,
        [
            "luna:h",
            "-w",
            "-b",
            "--headless",
            "prepare",
            "then",
            "fail",
        ],
    );
    let requests = server.join().expect("failure server thread");

    assert!(!output.status.success());
    assert_eq!(routes(&requests), ["agents", "session"]);
    assert!(stored_refs(&home).is_empty());
    assert!(intent_json_files(home.path()).is_empty());
    assert!(!git_branches(home.path()).contains("oca/"));
    assert!(
        !home.path().join(".oca/wt").exists()
            || std::fs::read_dir(home.path().join(".oca/wt"))
                .unwrap()
                .next()
                .is_none()
    );
}

#[test]
fn deterministic_pre_prompt_failures_clean_worktree_refs_and_list_state() {
    for (actions, expected_routes) in [
        (
            vec![
                respond(200, "application/json", br#"{"id":"ses_subscribe_500"}"#),
                respond(
                    500,
                    "application/json",
                    br#"{"error":"subscribe rejected"}"#,
                ),
            ],
            vec!["agents", "session", "event"],
        ),
        (
            vec![
                respond(200, "application/json", br#"{"id":"ses_prompt_429"}"#),
                respond(200, "text/event-stream", b""),
                FailureAction::Respond(HttpResponse::new(
                    429,
                    [("content-type", "application/json"), ("retry-after", "1")],
                    [br#"{"error":"prompt rejected"}"#.to_vec()],
                )),
            ],
            vec!["agents", "session", "event", "prompt_async"],
        ),
    ] {
        let (home, server) = fixture(actions);
        init_repository(home.path());

        let output = run(
            &home,
            [
                "--json",
                "luna:h",
                "-w",
                "-b",
                "--headless",
                "fail",
                "before",
                "prompt",
            ],
        );
        let requests = server.join().expect("failure server thread");

        assert!(!output.status.success());
        assert_eq!(routes(&requests), expected_routes);
        assert!(stored_refs(&home).is_empty());
        assert!(intent_json_files(home.path()).is_empty());
        assert!(!git_branches(home.path()).contains("oca/"));

        let listed = run(&home, ["ls", "--all", "--json"]);
        assert!(listed.status.success());
        let list: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
        assert_eq!(list["total"], 0);
    }
}

#[test]
fn subscribe_recovery_rechecks_agent_before_replacement_session() {
    let (home, server) = fixture([
        respond(200, "application/json", br#"{"id":"ses_before_cut"}"#),
        FailureAction::DropAfterRequest,
        respond(200, "application/json", br#"[{"name":"impl"}]"#),
        respond(200, "application/json", br#"{"id":"ses_after_cut"}"#),
        respond(200, "text/event-stream", b""),
        respond(204, "text/plain", b""),
        FailureAction::EchoPrompt {
            session_id: "ses_after_cut".to_owned(),
        },
    ]);

    let output = run(
        &home,
        [
            "luna:h",
            "-b",
            "--headless",
            "recover",
            "subscription",
            "safely",
        ],
    );
    let requests = server.join().expect("failure server thread");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        routes(&requests),
        [
            "agents",
            "session",
            "event",
            "agents",
            "session",
            "event",
            "prompt_async",
            "messages"
        ]
    );
    assert_eq!(stored_refs(&home)[0].session_id, "ses_after_cut");
}

#[test]
fn killed_worktree_ready_phase_is_cleaned_by_a_fresh_ls() {
    let home = tempfile::tempdir().expect("temporary home");
    std::fs::create_dir(home.path().join(".oca")).unwrap();
    std::fs::write(home.path().join(".oca/config.toml"), "").unwrap();
    init_repository(home.path());

    let killed = Command::new(env!("CARGO_BIN_EXE_oca"))
        .args([
            "luna:h",
            "-w",
            "-b",
            "--headless",
            "crash",
            "after",
            "worktree",
        ])
        .env("HOME", home.path())
        .env("OCA_FAILPOINT", "worktree_ready")
        .current_dir(home.path())
        .output()
        .unwrap();
    assert_eq!(killed.status.code(), Some(86));
    assert_eq!(intent_json_files(home.path()).len(), 1);
    assert!(git_branches(home.path()).contains("oca/"));

    let listed = run(&home, ["ls", "--all", "--json"]);
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let list: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid list output ({error}): {:?}",
            String::from_utf8_lossy(&listed.stdout)
        )
    });
    assert_eq!(list["total"], 0);
    assert!(intent_json_files(home.path()).is_empty());
    assert!(!git_branches(home.path()).contains("oca/"));
}

#[test]
fn killed_session_created_phase_is_queried_without_a_prompt() {
    let (home, server) = fixture([
        respond(
            200,
            "application/json",
            br#"{"id":"ses_created_before_crash"}"#,
        ),
        respond(200, "application/json", b"[]"),
    ]);

    let killed = Command::new(env!("CARGO_BIN_EXE_oca"))
        .args(["luna:h", "-b", "--headless", "stop", "before", "prompt"])
        .env("HOME", home.path())
        .env("OCA_FAILPOINT", "session_created")
        .current_dir(home.path())
        .output()
        .unwrap();
    assert_eq!(killed.status.code(), Some(86));
    let intent_files = intent_json_files(home.path());
    assert_eq!(intent_files.len(), 1);
    let durable_intent: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&intent_files[0]).unwrap()).unwrap();
    assert_eq!(durable_intent["phase"], "session_created");
    assert_eq!(durable_intent["session_id"], "ses_created_before_crash");
    assert!(stored_refs(&home).is_empty());

    let listed = run(&home, ["ls", "--all", "--json"]);
    let requests = server.join().expect("failure server thread");
    assert!(listed.status.success());
    let list: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(list["items"][0]["state"], "session_created");
    assert_eq!(routes(&requests), ["agents", "session", "messages"]);
    assert!(!routes(&requests).contains(&"prompt_async"));
}

#[test]
fn killed_running_phase_persists_production_intent_before_ref_materialization() {
    let (home, server) = fixture([
        respond(
            200,
            "application/json",
            br#"{"id":"ses_running_before_crash"}"#,
        ),
        respond(200, "text/event-stream", b""),
        respond(204, "text/plain", b""),
        FailureAction::EchoPrompt {
            session_id: "ses_running_before_crash".to_owned(),
        },
    ]);

    let killed = Command::new(env!("CARGO_BIN_EXE_oca"))
        .args(["luna:h", "-b", "--headless", "stop", "after", "prompt"])
        .env("HOME", home.path())
        .env("OCA_FAILPOINT", "running")
        .current_dir(home.path())
        .output()
        .unwrap();
    let requests = server.join().expect("failure server thread");
    assert_eq!(killed.status.code(), Some(86));
    assert_eq!(
        routes(&requests),
        ["agents", "session", "event", "prompt_async", "messages"]
    );

    let intent_files = intent_json_files(home.path());
    assert_eq!(intent_files.len(), 1);
    let durable_intent: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&intent_files[0]).unwrap()).unwrap();
    assert_eq!(durable_intent["phase"], "running");
    assert_eq!(durable_intent["session_id"], "ses_running_before_crash");
    assert!(durable_intent["message_id"].as_str().is_some());
    assert!(durable_intent["prompt_sha256"].as_str().is_some());
    assert!(stored_refs(&home).is_empty());

    let listed = run(&home, ["ls", "--all", "--json"]);
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let records = stored_refs(&home);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].session_id, "ses_running_before_crash");
    assert_eq!(records[0].last_state, Some(RefState::Running));
}

#[test]
fn mid_sse_cut_keeps_the_admitted_prompt_exactly_once() {
    let (home, server) = fixture([
        respond(200, "application/json", br#"{"id":"ses_stream_cut"}"#),
        FailureAction::RespondThenGarble(HttpResponse::new(
            200,
            [("content-type", "text/event-stream")],
            [b": connected\n\n".to_vec()],
        )),
        respond(204, "text/plain", b""),
        FailureAction::EchoPrompt {
            session_id: "ses_stream_cut".to_owned(),
        },
        respond(200, "application/json", b"[]"),
        respond(200, "application/json", b"[]"),
    ]);

    let output = run(&home, ["--json", "luna:h", "--headless", "stream", "once"]);
    let requests = server.join().expect("failure server thread");

    assert_eq!(output.status.code(), Some(exit::FAILURE));
    let error = parsed_error(&output);
    assert_eq!(error.code(), ErrorCode::ProtocolMismatch.as_str());
    assert_eq!(
        routes(&requests),
        [
            "agents",
            "session",
            "event",
            "prompt_async",
            "messages",
            "messages",
            "messages"
        ]
    );
    assert_eq!(
        routes(&requests)
            .iter()
            .filter(|route| **route == "prompt_async")
            .count(),
        1
    );
    let records = stored_refs(&home);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].last_state, Some(RefState::Running));

    let listed = run(&home, ["ls", "--all", "--json"]);
    assert!(listed.status.success());
    let list: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(list["total"], 1);
    assert_eq!(list["items"][0]["ref"], records[0].id);
    assert_eq!(list["items"][0]["state"], "running");
}

#[test]
fn every_429_is_terminal_rate_limited_with_retry_metadata_and_no_replay() {
    let (home, server) = fixture([FailureAction::Respond(HttpResponse::new(
        429,
        [("content-type", "application/json"), ("retry-after", "2")],
        [br#"{"error":"slow down"}"#.to_vec()],
    ))]);
    let output = run(&home, ["--json", "luna:h", "-b", "--headless", "limited"]);
    let requests = server.join().expect("failure server thread");
    assert_rate_limit(&output, Some(2_000));
    assert_eq!(routes(&requests), ["agents", "session"]);
    assert!(stored_refs(&home).is_empty());

    let (home, server) = fixture([
        respond(200, "application/json", br#"{"id":"ses_limited"}"#),
        respond(200, "text/event-stream", b""),
        FailureAction::Respond(HttpResponse::new(
            429,
            [
                ("content-type", "application/json"),
                ("x-ratelimit-reset-requests", "1750ms"),
            ],
            [br#"{"error":"provider limit"}"#.to_vec()],
        )),
    ]);
    let output = run(&home, ["--json", "luna:h", "-b", "--headless", "limited"]);
    let requests = server.join().expect("failure server thread");
    assert_rate_limit(&output, Some(1_750));
    assert_eq!(
        routes(&requests),
        ["agents", "session", "event", "prompt_async"]
    );
    assert_eq!(
        routes(&requests)
            .iter()
            .filter(|route| **route == "prompt_async")
            .count(),
        1
    );
    assert!(stored_refs(&home).is_empty());

    let (home, server) = fixture_without_agents([FailureAction::Respond(HttpResponse::new(
        429,
        [("content-type", "application/json"), ("retry-after", "4")],
        [br#"{"error":"follow limited"}"#.to_vec()],
    ))]);
    RefStore::with_paths(RefStorePaths::in_directory(home.path().join(".oca")))
        .insert(RefRecord {
            id: "w4f2a1".to_owned(),
            session_id: "ses_follow_limited".to_owned(),
            message_id: Some("msg_dispatch".to_owned()),
            alias: Some("luna".to_owned()),
            effort: Some("high".to_owned()),
            role: Some("impl".to_owned()),
            cwd: Some(home.path().display().to_string()),
            last_state: Some(RefState::Running),
            repo: Some(home.path().display().to_string()),
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
        .expect("follow ref");
    let output = run(&home, ["--json", "f", "w4f2a1"]);
    let requests = server.join().expect("failure server thread");
    assert_rate_limit(&output, Some(4_000));
    assert_eq!(routes(&requests), ["event"]);
}

#[test]
fn unregistered_dispatch_agent_fails_before_prompt_and_leaves_list_in_agreement() {
    let (home, server) = fixture_with_agents("build", []);
    init_repository(home.path());

    let output = run(
        &home,
        [
            "--json",
            "luna:h",
            "-w",
            "-b",
            "--headless",
            "agent",
            "must",
            "exist",
        ],
    );
    let requests = server.join().expect("failure server thread");

    assert_eq!(output.status.code(), Some(exit::FAILURE));
    assert!(output.stdout.is_empty());
    let error = parsed_error(&output);
    assert_eq!(error.code(), ErrorCode::ProtocolMismatch.as_str());
    assert!(error.error().contains("`impl`"));
    assert!(error.error().contains("dispatch directory"));
    assert!(error.help().contains("OpenCode configuration"));
    assert_eq!(routes(&requests), ["agents"]);
    assert!(!routes(&requests).contains(&"prompt_async"));
    assert!(stored_refs(&home).is_empty());
    assert!(intent_json_files(home.path()).is_empty());
    assert!(!git_branches(home.path()).contains("oca/"));
    assert!(
        !home.path().join(".config/opencode/opencode.jsonc").exists(),
        "oca must never register agents by mutating OpenCode configuration"
    );

    let listed = run(&home, ["ls", "--all", "--json"]);
    assert!(listed.status.success());
    let list: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(list["total"], 0);
}

fn fixture(
    actions: impl IntoIterator<Item = FailureAction>,
) -> (
    tempfile::TempDir,
    thread::JoinHandle<Vec<oca_testkit::HttpRequest>>,
) {
    fixture_with_agents("impl", actions)
}

fn fixture_with_agents(
    agent: &str,
    actions: impl IntoIterator<Item = FailureAction>,
) -> (
    tempfile::TempDir,
    thread::JoinHandle<Vec<oca_testkit::HttpRequest>>,
) {
    let agent_body = format!(r#"[{{"name":"{agent}"}}]"#);
    let actions = std::iter::once(respond(200, "application/json", agent_body.as_bytes()))
        .chain(actions)
        .collect::<Vec<_>>();
    let server = FailureHttpServer::bind("127.0.0.1:0", actions).expect("failure server binds");
    let port = server.local_addr().expect("failure server address").port();
    let home = tempfile::tempdir().expect("temporary home");
    let state = home.path().join(".oca");
    std::fs::create_dir(&state).expect("state directory");
    std::fs::write(state.join("config.toml"), "").expect("default config");
    ConnectOrStart::new(&state, port, [], std::time::Duration::from_secs(1))
        .write_record(&ServerRecord::new(
            port,
            installed_opencode_version(),
            environment_hash_for(home.path()),
        ))
        .expect("server record");
    let server = thread::spawn(move || server.serve().expect("failure script completes"));
    (home, server)
}

fn fixture_without_agents(
    actions: impl IntoIterator<Item = FailureAction>,
) -> (
    tempfile::TempDir,
    thread::JoinHandle<Vec<oca_testkit::HttpRequest>>,
) {
    let server = FailureHttpServer::bind("127.0.0.1:0", actions).expect("failure server binds");
    let port = server.local_addr().expect("failure server address").port();
    let home = tempfile::tempdir().expect("temporary home");
    let state = home.path().join(".oca");
    std::fs::create_dir(&state).expect("state directory");
    std::fs::write(state.join("config.toml"), "").expect("default config");
    ConnectOrStart::new(&state, port, [], std::time::Duration::from_secs(1))
        .write_record(&ServerRecord::new(
            port,
            installed_opencode_version(),
            environment_hash_for(home.path()),
        ))
        .expect("server record");
    let server = thread::spawn(move || server.serve().expect("failure script completes"));
    (home, server)
}

fn missing_prompt_fixture(
    session_id: &str,
) -> (
    tempfile::TempDir,
    thread::JoinHandle<Vec<oca_testkit::HttpRequest>>,
) {
    let session = format!(r#"{{"id":"{session_id}"}}"#);
    let actions = [
        respond(200, "application/json", session.as_bytes()),
        respond(200, "text/event-stream", b""),
        respond(204, "text/plain", b""),
    ]
    .into_iter()
    .chain(std::iter::repeat_with(|| respond(200, "application/json", b"[]")).take(13));
    fixture(actions)
}

fn assert_uncertain_missing_prompt(
    home: &tempfile::TempDir,
    output: &std::process::Output,
    requests: &[oca_testkit::HttpRequest],
    session_id: &str,
) {
    assert_eq!(output.status.code(), Some(exit::FAILURE));
    assert!(output.stdout.is_empty());
    let error = parsed_error(output);
    assert_eq!(error.code(), ErrorCode::PromptUncertain.as_str());
    let reference = error.reference().expect("uncertain ref is surfaced");
    assert_eq!(
        routes(requests)[..4],
        ["agents", "session", "event", "prompt_async"]
    );
    assert!(
        routes(requests)[4..]
            .iter()
            .all(|route| *route == "messages")
    );
    assert_eq!(
        routes(requests)
            .iter()
            .filter(|route| **route == "prompt_async")
            .count(),
        1,
        "confirmation must never replay a missing prompt"
    );

    let records = stored_refs(home);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, reference);
    assert_eq!(records[0].session_id, session_id);
    assert_eq!(records[0].last_state, Some(RefState::Unknown));

    let intents = intent_json_files(home.path());
    assert_eq!(intents.len(), 1);
    let intent: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&intents[0]).unwrap()).unwrap();
    assert_eq!(intent["phase"], "prompt_uncertain");
    assert_eq!(intent["ref"], reference);
    assert_eq!(intent["session_id"], session_id);

    let listed = run(home, ["ls", "--json"]);
    assert!(listed.status.success());
    let list: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(list["total"], 1);
    assert_eq!(list["items"][0]["ref"], reference);
    assert_eq!(list["items"][0]["state"], "prompt_uncertain");
}

fn respond(status: u16, content_type: &str, body: &[u8]) -> FailureAction {
    FailureAction::Respond(HttpResponse::new(
        status,
        [("content-type", content_type)],
        [body.to_vec()],
    ))
}

fn replace_server_record(home: &tempfile::TempDir, port: u16) {
    ConnectOrStart::new(
        home.path().join(".oca"),
        port,
        [],
        std::time::Duration::from_secs(1),
    )
    .write_record(&ServerRecord::new(
        port,
        installed_opencode_version(),
        environment_hash_for(home.path()),
    ))
    .unwrap();
}

fn init_repository(path: &std::path::Path) {
    for arguments in [
        vec!["init", "--quiet"],
        vec!["config", "user.name", "oca test"],
        vec!["config", "user.email", "oca@example.test"],
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
    std::fs::write(path.join("README.md"), "base\n").unwrap();
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

fn git_branches(path: &std::path::Path) -> String {
    String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["branch", "--list"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
}

fn intent_json_files(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    match std::fs::read_dir(home.join(".oca/intents")) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("could not list intents: {error}"),
    }
}

fn run<const N: usize>(home: &tempfile::TempDir, arguments: [&str; N]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_oca"))
        .args(arguments)
        .env("HOME", home.path())
        .current_dir(home.path())
        .output()
        .expect("oca process runs")
}

fn routes(requests: &[oca_testkit::HttpRequest]) -> Vec<&'static str> {
    requests
        .iter()
        .map(|request| {
            let path = request
                .path
                .split_once('?')
                .map_or(request.path.as_str(), |pair| pair.0);
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
        })
        .collect()
}

fn request_directory(path: &str) -> std::path::PathBuf {
    url::Url::parse(&format!("http://localhost{path}"))
        .unwrap()
        .query_pairs()
        .find_map(|(key, value)| {
            (key == "directory").then(|| std::path::PathBuf::from(value.as_ref()))
        })
        .expect("directory query")
}

fn stored_refs(home: &tempfile::TempDir) -> Vec<RefRecord> {
    match std::fs::read(home.path().join(".oca/refs.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).expect("valid refs JSON"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("could not read refs: {error}"),
    }
}

fn assert_rate_limit(output: &std::process::Output, retry_after_ms: Option<u64>) {
    assert_eq!(output.status.code(), Some(exit::FAILURE));
    assert!(output.stdout.is_empty());
    let error = parsed_error(output);
    assert_eq!(error.code(), ErrorCode::RateLimited.as_str());
    assert_eq!(error.retry_after_ms(), retry_after_ms);
}

fn parsed_error(output: &std::process::Output) -> oca_core::ErrorEnvelope {
    let stderr = String::from_utf8_lossy(&output.stderr);
    parse_error_envelope(stderr.trim())
        .unwrap_or_else(|error| panic!("invalid error envelope ({error}): {stderr:?}"))
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
