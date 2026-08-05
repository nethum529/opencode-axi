//! Explicitly-invoked acceptance tests against real OpenCode and herdr.
//!
//! These tests are deliberately ignored by the hermetic workspace gate. Run them with:
//!
//! ```text
//! OCA_LIVE=1 cargo test -p oca-cli --test live_acceptance -- --ignored --nocapture
//! ```
//!
//! The fixture starts and kills its own `opencode serve`, copies only the OpenCode credentials
//! needed by that server into a temporary HOME, and keeps all oca state below that HOME. It never
//! reads or writes the invoking user's `~/.oca`.

#![cfg(unix)]

use std::{
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{Mutex, MutexGuard, OnceLock},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use oca_core::{
    MessageIdGenerator, ModelCatalog, RANDOM_SUFFIX_WIDTH, ResolvedModel, WorkerPolicy,
    resolve_model,
};
use oca_display::HerdrClient;
use oca_opencode::{
    CreateSessionRequest, MessageWithParts, OpenCodeClient, PromptRequest, SseEvent, Subscription,
    TextPart, is_target_session_idle,
};
use oca_server::{ConnectOrStart, ServerRecord};
use oca_state::{RefRecord, RefState, RefStore, RefStorePaths};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::runtime::{Builder, Runtime};
use url::Url;

const OPENAI_PROVIDER: &str = "openai";
const OPENAI_MODEL: &str = "gpt-5.6-terra";
const DEEPSEEK_PROVIDER: &str = "opencode";
const DEEPSEEK_MODEL: &str = "deepseek-v4-flash-free";
const LIVE_TIMEOUT: Duration = Duration::from_secs(45);

static LIVE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[test]
#[ignore = "requires OCA_LIVE=1 and a real authenticated OpenCode installation"]
fn live_session_creation_http_prompt_terminal_sse_and_message_id_echo() {
    let _guard = live_guard();
    let server = LiveServer::start(false);
    let runtime = runtime();
    let client = server.client();
    let model = deepseek_model("high");

    let (session_id, message_id, events, messages) = runtime.block_on(async {
        let session = create_session(&client, &server, &model, None).await;
        let mut subscription = client.subscribe(None).await.expect("live SSE subscription");
        let message_id = mint_message_id();
        client
            .prompt_async(
                &session.id,
                prompt(
                    &server,
                    &model,
                    "high",
                    &message_id,
                    "Reply with the exact token LIVE-ASYNC-OK and do not use tools.",
                    None,
                ),
            )
            .await
            .expect("live asynchronous prompt accepted");
        let events = events_through_idle(&mut subscription, &session.id, LIVE_TIMEOUT).await;
        let messages = client
            .messages(&session.id)
            .await
            .expect("live message history");
        (session.id, message_id, events, messages)
    });

    assert_eq!(idle_count(&events, &session_id), 1, "one idle per turn");
    assert_message_id_echo(&messages, &session_id, &message_id);
    assert!(
        assistant_text_for(&messages, &message_id).contains("LIVE-ASYNC-OK"),
        "the terminal assistant reply must be visible in message history"
    );
    eprintln!(
        "LIVE evidence: session={session_id} caller_message_id={message_id} idle_count=1 parent_echo=true"
    );
}

#[test]
#[ignore = "requires OCA_LIVE=1 and both real providers"]
fn live_variant_is_accepted_on_both_prompt_endpoints_for_both_providers() {
    let _guard = live_guard();
    let server = LiveServer::start(false);
    let runtime = runtime();

    runtime.block_on(async {
        for (model, variant) in [
            (deepseek_model("high"), "high"),
            (openai_model("high"), "high"),
        ] {
            let client = server.client();
            let session = create_session(&client, &server, &model, None).await;
            let mut subscription = client.subscribe(None).await.expect("live SSE subscription");
            let message_id = mint_message_id();
            client
                .prompt_async(
                    &session.id,
                    prompt(
                        &server,
                        &model,
                        variant,
                        &message_id,
                        "Reply with ASYNC-VARIANT-OK only; do not use tools.",
                        None,
                    ),
                )
                .await
                .unwrap_or_else(|error| {
                    panic!(
                        "{}/{} rejected variant {variant} on prompt_async: {error}",
                        model.provider, model.model
                    )
                });
            events_through_idle(&mut subscription, &session.id, LIVE_TIMEOUT).await;
            let messages = client.messages(&session.id).await.expect("message history");
            assert_eq!(assistant_variant_for(&messages, &message_id), Some(variant));

            let sync_session = create_session(&client, &server, &model, None).await;
            let response = sync_prompt(
                &server,
                &sync_session.id,
                &model,
                variant,
                "Reply with SYNC-VARIANT-OK only; do not use tools.",
                None,
                None,
            )
            .await;
            assert_eq!(response["info"]["variant"].as_str(), Some(variant));
            eprintln!(
                "LIVE variant accepted: provider={} model={} variant={} endpoints=prompt_async,message",
                model.provider, model.model, variant
            );
        }
    });
}

#[test]
#[ignore = "requires OCA_LIVE=1 and performs the DeepSeek behavioral measurement"]
fn live_deepseek_variant_behavioral_effect_is_measured() {
    let _guard = live_guard();
    let server = LiveServer::start(false);
    let runtime = runtime();
    const TRIALS: usize = 8;

    let measurements = runtime.block_on(async {
        let mut measurements = Vec::new();
        for variant in ["high", "max"] {
            for _ in 0..TRIALS {
                let model = deepseek_model(variant);
                let client = server.client();
                let session = create_session(&client, &server, &model, None).await;
                let started = Instant::now();
                let response = sync_prompt(
                    &server,
                    &session.id,
                    &model,
                    variant,
                    "Solve independently: Two fair dice are rolled. Given that at least one die is a six, what is the probability that the sum is at least ten? Give the reduced fraction and a brief derivation.",
                    None,
                    None,
                )
                .await;
                let text = response_parts_text(&response);
                measurements.push(VariantMeasurement {
                    variant,
                    reasoning: response["info"]["tokens"]["reasoning"]
                        .as_u64()
                        .unwrap_or(0),
                    output: response["info"]["tokens"]["output"].as_u64().unwrap_or(0),
                    elapsed_ms: started.elapsed().as_millis(),
                    correct: text.contains("5/11"),
                    echoed: response["info"]["variant"].as_str() == Some(variant),
                });
            }
        }
        measurements
    });

    assert!(measurements.iter().all(|sample| sample.echoed));
    let high = measurements
        .iter()
        .filter(|sample| sample.variant == "high")
        .map(|sample| sample.reasoning as f64)
        .collect::<Vec<_>>();
    let max = measurements
        .iter()
        .filter(|sample| sample.variant == "max")
        .map(|sample| sample.reasoning as f64)
        .collect::<Vec<_>>();
    let p = exact_permutation_p(&high, &max);
    let effect = p < 0.05;
    eprintln!(
        "LIVE DeepSeek variant effect: answer={} high_reasoning={:?} max_reasoning={:?} high_mean={:.1} max_mean={:.1} p={:.4} correct={}/{}",
        if effect { "YES" } else { "NO" },
        high,
        max,
        mean(&high),
        mean(&max),
        p,
        measurements.iter().filter(|sample| sample.correct).count(),
        measurements.len()
    );
    eprintln!(
        "LIVE DeepSeek raw samples: {:?}",
        measurements
            .iter()
            .map(|sample| json!({
                "variant": sample.variant,
                "reasoning": sample.reasoning,
                "output": sample.output,
                "elapsed_ms": sample.elapsed_ms,
                "correct": sample.correct,
            }))
            .collect::<Vec<_>>()
    );
}

#[test]
#[ignore = "requires OCA_LIVE=1 and a real provider"]
fn live_json_schema_output_enforcement_returns_conforming_reply() {
    let _guard = live_guard();
    let server = LiveServer::start(false);
    let runtime = runtime();
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["code", "label"],
        "properties": {
            "code": { "type": "integer" },
            "label": { "type": "string" }
        }
    });

    let response = runtime.block_on(async {
        let model = openai_model("high");
        let client = server.client();
        let session = create_session(&client, &server, &model, None).await;
        sync_prompt(
            &server,
            &session.id,
            &model,
            "high",
            "Return a structured answer whose integer code is 37 and whose label is exactly LIVE-SCHEMA-OK.",
            Some(schema),
            None,
        )
        .await
    });
    assert_eq!(response["info"]["structured"]["code"], 37);
    assert_eq!(response["info"]["structured"]["label"], "LIVE-SCHEMA-OK");
    eprintln!(
        "LIVE schema enforcement: structured={}",
        response["info"]["structured"]
    );
}

#[test]
#[ignore = "requires OCA_LIVE=1 and drives a denied real tool call"]
fn live_permission_deny_returns_without_asking_or_hanging() {
    let _guard = live_guard();
    let server = LiveServer::start(false);
    let runtime = runtime();
    let denied_path = server.home.path().join("permission-denied-sentinel");
    let permission = json!([
        { "permission": "bash", "pattern": "*", "action": "allow" },
        { "permission": "bash", "pattern": "echo *", "action": "deny" }
    ]);

    let (session_id, events, messages, elapsed) = runtime.block_on(async {
        let model = deepseek_model("high");
        let client = server.client();
        let session = create_session(&client, &server, &model, Some(permission)).await;
        let mut subscription = client.subscribe(None).await.expect("live SSE subscription");
        let message_id = mint_message_id();
        let started = Instant::now();
        client
            .prompt_async(
                &session.id,
                prompt(
                    &server,
                    &model,
                    "high",
                    &message_id,
                    &format!(
                        "You must use the bash tool once to run exactly: echo LIVE-DENIED > {}. After the attempt, report whether it was denied.",
                        denied_path.display()
                    ),
                    None,
                ),
            )
            .await
            .expect("denial prompt accepted");
        let events = events_through_idle(&mut subscription, &session.id, LIVE_TIMEOUT).await;
        let elapsed = started.elapsed();
        let messages = client.messages(&session.id).await.expect("message history");
        (session.id, events, messages, elapsed)
    });

    assert!(!denied_path.exists(), "the denied command must not execute");
    assert_eq!(idle_count(&events, &session_id), 1, "denied turn returned");
    assert_eq!(
        permission_ask_count(&events, &session_id),
        0,
        "deny is not ask"
    );
    assert!(
        messages
            .iter()
            .flat_map(|message| &message.parts)
            .any(|part| {
                part["tool"].as_str() == Some("bash")
                    && (part["state"]["status"].as_str() == Some("error")
                        || part["state"]["error"]
                            .as_str()
                            .is_some_and(|error| error.to_ascii_lowercase().contains("den")))
            }),
        "the model must reach the call-time deny path"
    );
    eprintln!(
        "LIVE permission denial: returned_ms={} ask_events=0 sentinel_created=false",
        elapsed.as_millis()
    );
}

#[test]
#[ignore = "requires OCA_LIVE=1 and a running herdr 0.7.5 server"]
fn live_herdr_workspace_tab_agent_start_and_tui_boot_has_no_idle() {
    let _guard = live_guard();
    let server = LiveServer::start(true);
    let runtime = runtime();
    let herdr = server.herdr_client();

    let (tab, session_id, events, agent_id) = runtime.block_on(async {
        let model = deepseek_model("high");
        let client = server.client();
        let session = create_session(&client, &server, &model, None).await;
        let mut subscription = client.subscribe(None).await.expect("live SSE subscription");
        let workspace = herdr.workspace("oca").await.expect("herdr workspace");
        let tab = herdr
            .tab(&workspace, "oca-live-idle-boot", true, server.repo.path())
            .await
            .expect("herdr tab");
        let launch = herdr
            .agent_start(
                &tab,
                vec![
                    "opencode".to_owned(),
                    "attach".to_owned(),
                    server.base_url().to_string(),
                    "--session".to_owned(),
                    session.id.clone(),
                    "--mini".to_owned(),
                ],
            )
            .await;
        let agent_id = match launch {
            Ok(agent) => agent.as_str().to_owned(),
            Err(error) => {
                let _ = herdr.close_tab(&tab).await;
                panic!("herdr agent.start failed: {error}");
            }
        };
        let events = collect_events_for(&mut subscription, Duration::from_secs(5)).await;
        (tab, session.id, events, agent_id)
    });

    runtime
        .block_on(herdr.close_tab(&tab))
        .expect("explicit herdr tab close");
    assert_eq!(idle_count(&events, &session_id), 0);
    eprintln!(
        "LIVE herdr lifecycle: version=0.7.5 agent_id={agent_id} tui_boot_observation_s=5 idle_count=0 tab_closed=true"
    );
}

#[test]
#[ignore = "requires OCA_LIVE=1, a real provider, and a running herdr server"]
fn live_queue_runs_after_turn_and_abort_reaches_terminal_for_tab_close() {
    let _guard = live_guard();
    let server = LiveServer::start(true);
    let runtime = runtime();

    let (queue_session, first_message, queued_message, queue_events, queue_messages) =
        runtime.block_on(async {
            let model = deepseek_model("high");
            let client = server.client();
            let session = create_session(&client, &server, &model, None).await;
            let mut events = client.subscribe(None).await.expect("queue SSE subscription");
            let first_message = mint_message_id();
            client
                .prompt_async(
                    &session.id,
                    prompt(
                        &server,
                        &model,
                        "high",
                        &first_message,
                        "Solve 987654321 divided by 12345 carefully, then finish with marker FIRST-TURN. Use no tools.",
                        None,
                    ),
                )
                .await
                .expect("first queue turn accepted");
            tokio::time::sleep(Duration::from_millis(750)).await;
            let queued_message = mint_message_id();
            client
                .queue(
                    &session.id,
                    prompt(
                        &server,
                        &model,
                        "high",
                        &queued_message,
                        "Reply with marker SECOND-QUEUED-TURN only. Use no tools.",
                        None,
                    ),
                )
                .await
                .expect("queued turn accepted");
            let events = events_through_idle(&mut events, &session.id, LIVE_TIMEOUT).await;
            let messages = client.messages(&session.id).await.expect("queued history");
            (session.id, first_message, queued_message, events, messages)
        });
    assert_eq!(idle_count(&queue_events, &queue_session), 1);
    assert!(assistant_text_for(&queue_messages, &first_message).contains("FIRST-TURN"));
    assert!(assistant_text_for(&queue_messages, &queued_message).contains("SECOND-QUEUED-TURN"));

    // Exercise the public q subprocess against a real in-flight worker too. Terminal observation
    // for schema-bearing oca turns remains a separately recorded OpenCode/T24 item.
    let dispatched = server.oca(&[
        "terra:h",
        "-b",
        "--headless",
        "Produce a long analysis before returning a structured done reply.",
    ]);
    assert_success(&dispatched, "oca queue admission dispatch");
    let reference = acknowledgement_ref(&dispatched);
    let before_queue = server.record(&reference).message_id;
    let queued = server.oca(&["q", &reference, "Return a second structured done reply."]);
    assert_success(&queued, "oca q subprocess");
    assert_ne!(before_queue, server.record(&reference).message_id);
    let _ = server.oca(&["k", &reference]);

    let (headed_record, mut abort_events) = runtime.block_on(async {
        let model = deepseek_model("high");
        let client = server.client();
        let session = create_session(&client, &server, &model, None).await;
        let events = client.subscribe(None).await.expect("abort SSE subscription");
        let message_id = mint_message_id();
        client
            .prompt_async(
                &session.id,
                prompt(
                    &server,
                    &model,
                    "high",
                    &message_id,
                    "Produce a long analysis of the integers from one through ten thousand. Use no tools.",
                    None,
                ),
            )
            .await
            .expect("abort target accepted");
        let record = server.insert_ref("wab0rt", &session.id, &message_id, RefState::Running);
        (record, events)
    });
    let herdr = server.herdr_client();
    let tab = runtime.block_on(async {
        let workspace = herdr.workspace("oca").await.expect("abort workspace");
        let tab = herdr
            .tab(&workspace, "oca-live-abort", true, server.repo.path())
            .await
            .expect("abort tab");
        if let Err(error) = herdr
            .agent_start(
                &tab,
                vec![
                    "opencode".to_owned(),
                    "attach".to_owned(),
                    server.base_url().to_string(),
                    "--session".to_owned(),
                    headed_record.session_id.clone(),
                    "--mini".to_owned(),
                ],
            )
            .await
        {
            let _ = herdr.close_tab(&tab).await;
            panic!("abort tab agent failed: {error}");
        }
        tab
    });
    let tab_id = tab.as_str().to_owned();
    assert!(
        server.herdr_tab_exists(&tab_id),
        "headed tab exists before abort"
    );
    let aborted = server.oca(&["k", "wab0rt"]);
    assert_success(&aborted, "live abort");
    let abort_events = runtime.block_on(events_through_idle_count(
        &mut abort_events,
        &headed_record.session_id,
        1,
        LIVE_TIMEOUT,
    ));
    assert!(idle_count(&abort_events, &headed_record.session_id) >= 1);
    runtime
        .block_on(herdr.close_tab(&tab))
        .expect("close abort tab at terminal state");
    assert!(
        !server.herdr_tab_exists(&tab_id),
        "abort terminal closes tab"
    );
    eprintln!(
        "LIVE controls: raw_queue_after_boundary=true oca_q_admitted=true abort_ack=true headed_tab_closed_at_terminal=true tab_id={tab_id}"
    );
}

#[test]
#[ignore = "records the foreman-approved T18 steer retirement against a live fixture"]
fn live_steer_criterion_is_superseded_by_the_t18_scope_decision() {
    let _guard = live_guard();
    let server = LiveServer::start(false);
    let output = server.oca(&["s", "wabc12", "steer", "now"]);
    assert_eq!(output.status.code(), Some(2));
    eprintln!(
        "LIVE steer result: OPEN/SUPERSEDED — issue #18 dropped `oca s` because OpenCode 1.18.10 accepts but silently discards cross-pipeline steer; retired_cli_error={}",
        String::from_utf8_lossy(&output.stderr)
            .trim()
            .replace('\n', " | ")
    );
}

#[test]
#[ignore = "requires OCA_LIVE=1 and real end-to-end subprocess turns"]
fn live_oca_follow_exit_gate_records_pre_t24_blocker_and_server_loss() {
    let _guard = live_guard();
    let mut server = LiveServer::start(false);

    let dispatch = server.oca(&[
        "terra:h",
        "-b",
        "--headless",
        "Return structured status done with a valid detailed note and no tools.",
    ]);
    assert_success(&dispatch, "background dispatch for live follow probe");
    let reference = acknowledgement_ref(&dispatch);
    let followed = server.oca(&["f", &reference, "-t", "2"]);
    assert_eq!(followed.status.code(), Some(1));
    let follow_error = String::from_utf8_lossy(&followed.stderr);
    assert!(follow_error.contains("protocol_mismatch"));
    assert!(follow_error.contains("Expected OutputFormatJsonSchema"));

    let loss_dispatch = server.oca(&[
        "terra:h",
        "-b",
        "--headless",
        "Return structured status done with a sufficiently detailed two-sentence note.",
    ]);
    assert_success(&loss_dispatch, "server-loss dispatch");
    let loss_ref = acknowledgement_ref(&loss_dispatch);
    server.kill();
    let unreachable = server.oca(&["--json", "f", &loss_ref, "-t", "2"]);
    assert_eq!(unreachable.status.code(), Some(5), "live server-loss exit");
    assert!(String::from_utf8_lossy(&unreachable.stderr).contains("server_unreachable"));
    eprintln!(
        "LIVE oca f exits: done/blocked/timeout=OPEN (OpenCode 1.18.10 rejects schema-bearing message history before T24 reconciliation); server_unreachable=5 PASS"
    );
}

#[test]
#[ignore = "requires OCA_LIVE=1 and measures a real OpenCode server"]
fn live_warm_headless_ack_p95_is_under_backstop_and_recorded() {
    let _guard = live_guard();
    let server = LiveServer::start(false);
    const WARMUP: usize = 3;
    const SAMPLES: usize = 20;
    const BACKSTOP: Duration = Duration::from_millis(250);

    for _ in 0..WARMUP {
        let sample = server.timed_background_ack();
        server.abort_ref(&sample.reference);
    }
    let mut elapsed = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let sample = server.timed_background_ack();
        elapsed.push(sample.elapsed);
        server.abort_ref(&sample.reference);
    }
    elapsed.sort_unstable();
    let p50 = elapsed[elapsed.len() / 2];
    let p95 = elapsed[(elapsed.len() * 95).div_ceil(100) - 1];
    eprintln!(
        "LIVE warm headless timing: p50={:.3}ms p95={:.3}ms samples_ms={:?}",
        milliseconds(p50),
        milliseconds(p95),
        elapsed
            .iter()
            .copied()
            .map(milliseconds)
            .collect::<Vec<_>>()
    );
    eprintln!(
        "LIVE warm backstop result: {} (p95={:.3}ms; limit <{}ms)",
        if p95 < BACKSTOP { "PASS" } else { "FAIL" },
        milliseconds(p95),
        BACKSTOP.as_millis()
    );
    assert!(
        p95 < BACKSTOP,
        "live warm headless acknowledgement p95 {:.3}ms must remain under the {}ms backstop",
        milliseconds(p95),
        BACKSTOP.as_millis()
    );
}

#[test]
#[ignore = "requires OCA_LIVE=1 and measures replay behavior on real SSE"]
fn live_last_event_id_replay_support_is_measured() {
    let _guard = live_guard();
    let server = LiveServer::start(false);
    let runtime = runtime();

    let (old_session, ids, replayed_old, fresh_seen) = runtime.block_on(async {
        let model = deepseek_model("high");
        let client = server.client();
        let session = create_session(&client, &server, &model, None).await;
        let mut first = client
            .subscribe(None)
            .await
            .expect("first SSE subscription");
        let message_id = mint_message_id();
        client
            .prompt_async(
                &session.id,
                prompt(
                    &server,
                    &model,
                    "high",
                    &message_id,
                    "Reply with REPLAY-PROBE-ONE only; do not use tools.",
                    None,
                ),
            )
            .await
            .expect("first replay probe prompt");
        let first_events = events_through_idle(&mut first, &session.id, LIVE_TIMEOUT).await;
        let ids = first_events
            .iter()
            .filter_map(|event| event.id.clone())
            .collect::<Vec<_>>();
        drop(first);

        let cursor = ids
            .last()
            .map(String::as_str)
            .unwrap_or("oca-live-cursor-probe");
        let mut resumed = client
            .subscribe(Some(cursor))
            .await
            .expect("Last-Event-ID subscription accepted");
        let reconnect_events = collect_events_for(&mut resumed, Duration::from_secs(1)).await;
        let replayed_old = reconnect_events
            .iter()
            .any(|event| event_session(event) == Some(session.id.as_str()));

        let fresh = create_session(&client, &server, &model, None).await;
        let fresh_message = mint_message_id();
        client
            .prompt_async(
                &fresh.id,
                prompt(
                    &server,
                    &model,
                    "high",
                    &fresh_message,
                    "Reply with REPLAY-PROBE-TWO only; do not use tools.",
                    None,
                ),
            )
            .await
            .expect("fresh replay probe prompt");
        let fresh_events = events_through_idle(&mut resumed, &fresh.id, LIVE_TIMEOUT).await;
        let fresh_seen = fresh_events
            .iter()
            .any(|event| event_session(event) == Some(fresh.id.as_str()));
        (session.id, ids, replayed_old, fresh_seen)
    });

    assert!(!replayed_old, "old session events unexpectedly replayed");
    assert!(
        fresh_seen,
        "reconnected stream must still deliver fresh events"
    );
    eprintln!(
        "LIVE Last-Event-ID: replay_support=NO old_session={old_session} sse_ids={ids:?} replayed_old=false fresh_events=true"
    );
}

#[test]
#[ignore = "requires OCA_LIVE=1; run this ignored suite on each release platform"]
fn live_platform_gate_runs_on_linux_or_macos() {
    let _guard = live_guard();
    assert!(["linux", "macos"].contains(&env::consts::OS));
    eprintln!("LIVE platform: {}", env::consts::OS);
}

struct LiveServer {
    home: TempDir,
    repo: TempDir,
    port: u16,
    version: String,
    child: Option<Child>,
    herdr_socket: Option<PathBuf>,
}

impl LiveServer {
    fn start(with_herdr: bool) -> Self {
        require_live();
        let source_home = source_home();
        let home = tempfile::tempdir().expect("temporary live HOME");
        let repo = tempfile::tempdir().expect("temporary live cwd");
        fs::create_dir(repo.path().join(".git")).expect("temporary repository marker");
        install_opencode_credentials(&source_home, home.path());

        let executable = opencode_bin();
        let version_output = Command::new(&executable)
            .arg("--version")
            .output()
            .expect("opencode --version");
        assert!(version_output.status.success());
        let version = String::from_utf8(version_output.stdout)
            .expect("version is utf-8")
            .trim()
            .to_owned();
        let port = unused_port();
        let log = fs::File::create(home.path().join("opencode-live.log"))
            .expect("temporary OpenCode log");
        let error_log = log.try_clone().expect("clone OpenCode log");
        let child = Command::new(&executable)
            .args([
                "serve",
                "--pure",
                "--print-logs",
                "--log-level",
                "INFO",
                "--hostname",
                "127.0.0.1",
                "--port",
                &port.to_string(),
            ])
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path().join(".config"))
            .env("XDG_DATA_HOME", home.path().join(".local/share"))
            .env_remove("OPENCODE_CONFIG")
            .env_remove("OPENCODE_CONFIG_DIR")
            .current_dir(repo.path())
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(error_log))
            .spawn()
            .expect("start live OpenCode server");

        let herdr_socket = with_herdr.then(|| herdr_socket(&source_home));
        let mut server = Self {
            home,
            repo,
            port,
            version,
            child: Some(child),
            herdr_socket,
        };
        server.wait_ready();
        server.install_oca_state();
        eprintln!(
            "LIVE fixture: opencode={} herdr={} home={}",
            server.version,
            if with_herdr { "0.7.5" } else { "disabled" },
            server.home.path().display()
        );
        server
    }

    fn base_url(&self) -> Url {
        Url::parse(&format!("http://127.0.0.1:{}", self.port)).expect("live base URL")
    }

    fn client(&self) -> OpenCodeClient {
        OpenCodeClient::new(self.base_url())
    }

    fn http(&self) -> reqwest::Client {
        reqwest::Client::new()
    }

    fn herdr_client(&self) -> HerdrClient {
        HerdrClient::new(
            self.herdr_socket
                .as_ref()
                .expect("herdr enabled for this fixture"),
            Duration::from_secs(45),
        )
    }

    fn wait_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if self
                .child
                .as_mut()
                .is_some_and(|child| child.try_wait().expect("inspect OpenCode child").is_some())
            {
                panic!(
                    "OpenCode server exited during startup; log: {}",
                    fs::read_to_string(self.home.path().join("opencode-live.log"))
                        .unwrap_or_default()
                );
            }
            if live_health_check(self.port) {
                return;
            }
            assert!(Instant::now() < deadline, "OpenCode readiness timed out");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn install_oca_state(&self) {
        let state = self.home.path().join(".oca");
        fs::create_dir(&state).expect("isolated .oca directory");
        let herdr = self
            .herdr_socket
            .as_ref()
            .map_or_else(String::new, |path| path.display().to_string());
        fs::write(
            state.join("config.toml"),
            format!(
                "[server]\nport = {}\nalt_ports = []\nstart_timeout_ms = 1000\n\n[herdr]\nsocket = {:?}\ntimeout_ms = 45000\nclose_on_done = true\nworkspace = \"oca\"\n",
                self.port, herdr
            ),
        )
        .expect("isolated oca config");
        ConnectOrStart::new(&state, self.port, [], Duration::from_secs(1))
            .write_record(&ServerRecord::new(
                self.port,
                &self.version,
                "live-acceptance",
            ))
            .expect("isolated server record");
    }

    fn oca(&self, arguments: &[&str]) -> Output {
        self.oca_command(arguments)
            .output()
            .expect("oca subprocess runs")
    }

    fn oca_command(&self, arguments: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oca"));
        command
            .args(arguments)
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join(".config"))
            .env("XDG_DATA_HOME", self.home.path().join(".local/share"))
            .env("OCA_SPAWNER", "live-acceptance")
            .current_dir(self.repo.path());
        if let Some(socket) = &self.herdr_socket {
            command.env("HERDR_SOCKET_PATH", socket);
        } else {
            command.env_remove("HERDR_SOCKET_PATH");
        }
        command
    }

    fn record(&self, reference: &str) -> RefRecord {
        RefStore::with_paths(RefStorePaths::in_directory(self.home.path().join(".oca")))
            .resolve(reference)
            .expect("read isolated ref store")
            .unwrap_or_else(|| panic!("missing ref {reference}"))
    }

    fn insert_ref(
        &self,
        reference: &str,
        session_id: &str,
        message_id: &str,
        state: RefState,
    ) -> RefRecord {
        let record = RefRecord {
            id: reference.to_owned(),
            session_id: session_id.to_owned(),
            message_id: Some(message_id.to_owned()),
            alias: Some("terra".to_owned()),
            effort: Some("high".to_owned()),
            role: Some("impl".to_owned()),
            cwd: Some(self.repo.path().display().to_string()),
            last_state: Some(state),
            repo: Some(self.repo.path().display().to_string()),
            spawner_tag: Some("live-acceptance".to_owned()),
            worktree: None,
            branch: None,
            commit: None,
            commit_subject: None,
            display: Some("herdr".to_owned()),
            herdr_tab: None,
            completion: None,
            tombstoned: false,
        };
        RefStore::with_paths(RefStorePaths::in_directory(self.home.path().join(".oca")))
            .insert(record.clone())
            .expect("insert isolated live ref");
        record
    }

    fn abort_ref(&self, reference: &str) {
        let record = self.record(reference);
        let _ = runtime().block_on(self.client().abort(&record.session_id));
    }

    fn timed_background_ack(&self) -> TimedAck {
        let started = Instant::now();
        let mut child = self
            .oca_command(&[
                "terra:h",
                "-b",
                "--headless",
                "Return structured status done immediately with a valid two-sentence note of at least thirty words and no tool calls.",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("timed oca starts");
        let mut line = String::new();
        BufReader::new(child.stdout.take().expect("timed stdout"))
            .read_line(&mut line)
            .expect("timed acknowledgement");
        let elapsed = started.elapsed();
        let status = child.wait().expect("timed oca exits");
        assert!(status.success(), "timed dispatch failed");
        let reference = line
            .split_whitespace()
            .next()
            .expect("timed acknowledgement ref")
            .to_owned();
        TimedAck { reference, elapsed }
    }

    fn herdr_tab_exists(&self, tab_id: &str) -> bool {
        let output = Command::new("herdr")
            .args(["api", "snapshot"])
            .output()
            .expect("herdr snapshot");
        assert!(
            output.status.success(),
            "herdr snapshot failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let document: Value = serde_json::from_slice(&output.stdout).expect("herdr snapshot JSON");
        document["result"]["snapshot"]["tabs"]
            .as_array()
            .expect("herdr tabs array")
            .iter()
            .any(|tab| tab["tab_id"].as_str() == Some(tab_id))
    }

    fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for LiveServer {
    fn drop(&mut self) {
        if thread::panicking() {
            eprintln!(
                "LIVE OpenCode log after failure:\n{}",
                fs::read_to_string(self.home.path().join("opencode-live.log")).unwrap_or_default()
            );
        }
        self.kill();
    }
}

struct TimedAck {
    reference: String,
    elapsed: Duration,
}

#[derive(Debug)]
struct VariantMeasurement {
    variant: &'static str,
    reasoning: u64,
    output: u64,
    elapsed_ms: u128,
    correct: bool,
    echoed: bool,
}

fn live_guard() -> MutexGuard<'static, ()> {
    require_live();
    LIVE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn require_live() {
    assert_eq!(
        env::var("OCA_LIVE").as_deref(),
        Ok("1"),
        "set OCA_LIVE=1 to acknowledge that this suite uses real providers and herdr"
    );
}

fn runtime() -> Runtime {
    Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("live Tokio runtime")
}

fn source_home() -> PathBuf {
    env::var_os("OCA_LIVE_SOURCE_HOME")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
        .expect("HOME or OCA_LIVE_SOURCE_HOME is required")
}

fn opencode_bin() -> PathBuf {
    env::var_os("OCA_LIVE_OPENCODE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("opencode"))
}

fn herdr_socket(source_home: &Path) -> PathBuf {
    let path = env::var_os("OCA_LIVE_HERDR_SOCKET")
        .or_else(|| env::var_os("HERDR_SOCKET_PATH"))
        .map(PathBuf::from)
        .unwrap_or_else(|| source_home.join(".config/herdr/herdr.sock"));
    assert!(
        path.exists(),
        "herdr socket is unavailable at {}",
        path.display()
    );
    path
}

fn install_opencode_credentials(source_home: &Path, isolated_home: &Path) {
    let target_data = isolated_home.join(".local/share/opencode");
    fs::create_dir_all(&target_data).expect("isolated OpenCode data directory");
    let auth = source_home.join(".local/share/opencode/auth.json");
    assert!(
        auth.is_file(),
        "OpenCode auth is unavailable at {}",
        auth.display()
    );
    fs::copy(auth, target_data.join("auth.json")).expect("copy OpenCode auth into isolated HOME");

    let target_config = isolated_home.join(".config/opencode");
    fs::create_dir_all(&target_config).expect("isolated OpenCode config directory");
    let source_config = source_home.join(".config/opencode/opencode.jsonc");
    if source_config.is_file() {
        fs::copy(source_config, target_config.join("opencode.jsonc"))
            .expect("copy OpenCode config into isolated HOME");
    }

    // oca's role names are submitted through OpenCode's `agent` field. A release installation
    // therefore needs matching OpenCode agents; install the two built-in roles only inside this
    // fixture HOME so subprocess tests exercise the real role names without ambient setup.
    let agents = target_config.join("agents");
    fs::create_dir(&agents).expect("isolated OpenCode agents directory");
    for (name, description) in [
        (
            "impl",
            "Implement the requested work and report a structured result.",
        ),
        (
            "review",
            "Review the requested work and report a structured result.",
        ),
    ] {
        fs::write(
            agents.join(format!("{name}.md")),
            format!(
                "---\ndescription: {description}\nmode: primary\n---\nFollow the supplied output schema exactly. Obey the session permission profile and do not ask to bypass a denial.\n"
            ),
        )
        .expect("write isolated OpenCode role agent");
    }
}

fn unused_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("reserve live loopback port")
        .local_addr()
        .expect("live loopback address")
        .port()
}

fn openai_model(variant: &str) -> ResolvedModel {
    let mut model = resolve_model("terra", variant, ModelCatalog::default()).expect("terra model");
    assert_eq!(model.provider, OPENAI_PROVIDER);
    assert_eq!(model.model, OPENAI_MODEL);
    model.variant = variant.to_owned();
    model.effort = variant.to_owned();
    model
}

fn deepseek_model(variant: &str) -> ResolvedModel {
    let mut model = resolve_model("flash", variant, ModelCatalog::default()).expect("flash model");
    assert_eq!(model.provider, DEEPSEEK_PROVIDER);
    assert_eq!(model.model, DEEPSEEK_MODEL);
    model.variant = variant.to_owned();
    model.effort = variant.to_owned();
    model
}

async fn create_session(
    client: &OpenCodeClient,
    server: &LiveServer,
    model: &ResolvedModel,
    permission: Option<Value>,
) -> oca_opencode::Session {
    match client
        .create_session(CreateSessionRequest {
            directory: Some(server.repo.path().display().to_string()),
            title: Some("oca live acceptance".to_owned()),
            agent: Some("build".to_owned()),
            model: Some(model.clone()),
            permission,
            ..CreateSessionRequest::default()
        })
        .await
    {
        Ok(session) => session,
        Err(error) => panic!(
            "create live OpenCode session: {error}; server log: {}",
            fs::read_to_string(server.home.path().join("opencode-live.log")).unwrap_or_default()
        ),
    }
}

fn live_health_check(port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
    if stream
        .write_all(b"GET /global/health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut response = Vec::new();
    stream.read_to_end(&mut response).is_ok()
        && response.starts_with(b"HTTP/1.1 200")
        && response
            .windows(12)
            .any(|window| window == b"\"healthy\":tr")
}

fn prompt(
    server: &LiveServer,
    model: &ResolvedModel,
    variant: &str,
    message_id: &str,
    text: &str,
    output_schema: Option<Value>,
) -> PromptRequest {
    PromptRequest {
        message_id: message_id.to_owned(),
        model: model.clone(),
        variant: variant.to_owned(),
        role: "build".to_owned(),
        parts: vec![TextPart {
            text: text.to_owned(),
        }],
        output_schema,
        permission: WorkerPolicy::restricted([server.repo.path().to_owned()]).permission_profile(),
    }
}

async fn sync_prompt(
    server: &LiveServer,
    session_id: &str,
    model: &ResolvedModel,
    variant: &str,
    text: &str,
    output_schema: Option<Value>,
    message_id: Option<&str>,
) -> Value {
    let mut body = json!({
        "model": { "providerID": model.provider, "modelID": model.model },
        "variant": variant,
        "agent": "build",
        "parts": [{ "type": "text", "text": text }]
    });
    if let Some(schema) = output_schema {
        body["format"] = json!({ "type": "json_schema", "schema": schema });
    }
    if let Some(message_id) = message_id {
        body["messageID"] = Value::String(message_id.to_owned());
    }
    let response = server
        .http()
        .post(
            server
                .base_url()
                .join(&format!("session/{session_id}/message"))
                .expect("sync prompt URL"),
        )
        .header("api-version", "1")
        .json(&body)
        .send()
        .await
        .expect("send live synchronous prompt");
    let status = response.status();
    let bytes = response.bytes().await.expect("read sync prompt response");
    assert_eq!(
        status,
        StatusCode::OK,
        "sync prompt rejected: {}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).expect("sync prompt JSON")
}

async fn events_through_idle(
    subscription: &mut Subscription,
    session_id: &str,
    timeout: Duration,
) -> Vec<SseEvent> {
    let deadline = Instant::now() + timeout;
    let mut events = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "turn {session_id} did not become idle"
        );
        let event = tokio::time::timeout(remaining, subscription.next())
            .await
            .unwrap_or_else(|_| panic!("turn {session_id} timed out"))
            .expect("valid live SSE")
            .expect("live SSE remains open");
        let idle = is_target_session_idle(&event, session_id);
        events.push(event);
        if idle {
            break;
        }
    }
    events.extend(collect_events_for(subscription, Duration::from_millis(500)).await);
    events
}

async fn events_through_idle_count(
    subscription: &mut Subscription,
    session_id: &str,
    expected: usize,
    timeout: Duration,
) -> Vec<SseEvent> {
    let deadline = Instant::now() + timeout;
    let mut events = Vec::new();
    while idle_count(&events, session_id) < expected {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "session {session_id} produced only {} of {expected} idle events",
            idle_count(&events, session_id)
        );
        let event = tokio::time::timeout(remaining, subscription.next())
            .await
            .unwrap_or_else(|_| panic!("session {session_id} timed out"))
            .expect("valid live SSE")
            .expect("live SSE remains open");
        events.push(event);
    }
    events.extend(collect_events_for(subscription, Duration::from_millis(500)).await);
    events
}

async fn collect_events_for(subscription: &mut Subscription, duration: Duration) -> Vec<SseEvent> {
    let deadline = Instant::now() + duration;
    let mut events = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, subscription.next()).await {
            Ok(Ok(Some(event))) => events.push(event),
            Ok(Ok(None)) | Err(_) => break,
            Ok(Err(error)) => panic!("live SSE failed: {error}"),
        }
    }
    events
}

fn event_value(event: &SseEvent) -> Option<Value> {
    serde_json::from_str(&event.data).ok()
}

fn event_type(event: &SseEvent) -> Option<String> {
    event
        .event
        .clone()
        .or_else(|| event_value(event)?["type"].as_str().map(str::to_owned))
}

fn event_session(event: &SseEvent) -> Option<&str> {
    let value = serde_json::from_str::<Value>(&event.data).ok()?;
    let session = value
        .get("properties")
        .or_else(|| value.get("data"))
        .unwrap_or(&value)
        .get("sessionID")?
        .as_str()?
        .to_owned();
    // The event owns the source string, but serde_json::Value does not. Use the source occurrence
    // for a borrow with the event's lifetime instead of leaking the parsed allocation.
    event
        .data
        .find(&session)
        .map(|start| &event.data[start..start + session.len()])
}

fn idle_count(events: &[SseEvent], session_id: &str) -> usize {
    events
        .iter()
        .filter(|event| is_target_session_idle(event, session_id))
        .count()
}

fn permission_ask_count(events: &[SseEvent], session_id: &str) -> usize {
    events
        .iter()
        .filter(|event| event_session(event) == Some(session_id))
        .filter(|event| {
            matches!(
                event_type(event).as_deref(),
                Some("permission.asked" | "permission.v2.asked")
            )
        })
        .count()
}

fn assert_message_id_echo(messages: &[MessageWithParts], session_id: &str, message_id: &str) {
    assert!(messages.iter().any(|message| {
        message.info["role"].as_str() == Some("user")
            && message.info["sessionID"].as_str() == Some(session_id)
            && message.info["id"].as_str() == Some(message_id)
    }));
    assert!(messages.iter().any(|message| {
        message.info["role"].as_str() == Some("assistant")
            && message.info["sessionID"].as_str() == Some(session_id)
            && message.info["parentID"].as_str() == Some(message_id)
    }));
}

fn assistant_variant_for<'a>(
    messages: &'a [MessageWithParts],
    message_id: &str,
) -> Option<&'a str> {
    messages.iter().rev().find_map(|message| {
        (message.info["role"].as_str() == Some("assistant")
            && message.info["parentID"].as_str() == Some(message_id))
        .then(|| message.info["variant"].as_str())
        .flatten()
    })
}

fn assistant_text_for(messages: &[MessageWithParts], message_id: &str) -> String {
    messages
        .iter()
        .filter(|message| {
            message.info["role"].as_str() == Some("assistant")
                && message.info["parentID"].as_str() == Some(message_id)
        })
        .flat_map(|message| &message.parts)
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn response_parts_text(response: &Value) -> String {
    response["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn acknowledgement_ref(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .expect("acknowledgement reference")
        .to_owned()
}

fn assert_success(output: &Output, context: &str) {
    assert!(
        output.status.success(),
        "{context} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn mint_message_id() -> String {
    let timestamp_ms = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX);
    let mut random = [0_u8; RANDOM_SUFFIX_WIDTH];
    getrandom::fill(&mut random).expect("message-id randomness");
    MessageIdGenerator::new().mint(timestamp_ms, random)
}

fn mean(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / values.len() as f64
}

fn exact_permutation_p(left: &[f64], right: &[f64]) -> f64 {
    assert_eq!(left.len(), right.len());
    let mut combined = left.to_vec();
    combined.extend_from_slice(right);
    assert!(combined.len() < usize::BITS as usize);
    let observed = (mean(left) - mean(right)).abs();
    let mut extreme = 0_u64;
    let mut total = 0_u64;
    for mask in 0_usize..(1_usize << combined.len()) {
        if mask.count_ones() as usize != left.len() {
            continue;
        }
        total += 1;
        let mut selected = Vec::with_capacity(left.len());
        let mut remainder = Vec::with_capacity(right.len());
        for (index, value) in combined.iter().copied().enumerate() {
            if mask & (1 << index) == 0 {
                remainder.push(value);
            } else {
                selected.push(value);
            }
        }
        if (mean(&selected) - mean(&remainder)).abs() + f64::EPSILON >= observed {
            extreme += 1;
        }
    }
    extreme as f64 / total as f64
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}
