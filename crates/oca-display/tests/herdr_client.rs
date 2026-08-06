use std::{
    future::Future,
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use oca_display::{HerdrClient, HerdrError};
use serde_json::{Value, json};

const TEST_TIMEOUT: Duration = Duration::from_millis(20);

enum FakeResponse {
    Result(Value),
    MalformedEnvelope,
    WrongResultType,
    MismatchedId,
    Never,
}

#[test]
fn discover_finds_a_configured_socket_and_keeps_the_default_deadline() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("herdr.sock");
    let _listener = UnixListener::bind(&socket).unwrap();

    assert!(
        HerdrClient::discover_from(temp.path(), Some(&socket), HerdrClient::DEFAULT_TIMEOUT)
            .is_some()
    );
    assert_eq!(HerdrClient::DEFAULT_TIMEOUT, Duration::from_millis(750));
}

#[test]
fn discover_skips_a_missing_socket() {
    let temp = tempfile::tempdir().unwrap();
    assert!(
        HerdrClient::discover_from(
            temp.path(),
            Some(&temp.path().join("missing.sock")),
            TEST_TIMEOUT,
        )
        .is_none()
    );
}

#[test]
fn workspace_succeeds_against_a_fake_socket() {
    let fixture = Fixture::new(2, |index, request| match index {
        0 => {
            assert_request(request, "workspace.list");
            FakeResponse::Result(json!({"type":"workspace_list","workspaces":[]}))
        }
        1 => {
            assert_request(request, "workspace.create");
            assert_eq!(request["params"]["label"], "oca");
            assert_eq!(request["params"]["focus"], false);
            FakeResponse::Result(json!({
                "type":"workspace_created",
                "workspace":{"workspace_id":"w1","label":"oca"}
            }))
        }
        _ => unreachable!(),
    });

    let workspace = run(fixture.client().workspace("oca")).unwrap();
    assert_eq!(workspace.as_str(), "w1");
    fixture.finish();
}

#[test]
fn workspace_times_out_against_a_fake_socket() {
    let fixture = Fixture::new(1, |_, request| {
        assert_request(request, "workspace.list");
        FakeResponse::Never
    });

    assert_timeout(run(fixture.client().workspace("oca")).unwrap_err());
    fixture.finish();
}

#[test]
fn workspace_rejects_a_malformed_response_envelope() {
    let fixture = Fixture::new(1, |_, request| {
        assert_request(request, "workspace.list");
        FakeResponse::MalformedEnvelope
    });

    assert_malformed(run(fixture.client().workspace("oca")).unwrap_err());
    fixture.finish();
}

#[test]
fn a_well_formed_envelope_carrying_another_call_s_result_type_is_rejected() {
    let fixture = Fixture::new(1, |_, request| {
        assert_request(request, "workspace.list");
        FakeResponse::WrongResultType
    });

    assert_malformed(run(fixture.client().workspace("oca")).unwrap_err());
    fixture.finish();
}

#[test]
fn a_response_correlated_to_another_request_id_is_rejected() {
    let fixture = Fixture::new(1, |_, request| {
        assert_request(request, "workspace.list");
        FakeResponse::MismatchedId
    });

    assert_malformed(run(fixture.client().workspace("oca")).unwrap_err());
    fixture.finish();
}

#[test]
fn tab_succeeds_with_no_focus_and_worker_cwd() {
    let fixture = Fixture::new(2, |index, request| match index {
        0 => existing_workspace(request),
        1 => {
            assert_request(request, "tab.create");
            assert_eq!(request["params"]["workspace_id"], "w1");
            assert_eq!(request["params"]["label"], "wabc12");
            assert_eq!(request["params"]["cwd"], "/worker");
            assert_eq!(request["params"]["focus"], false);
            FakeResponse::Result(json!({
                "type":"tab_created",
                "tab":{"tab_id":"t1"},
                "root_pane":{"pane_id":"p1"}
            }))
        }
        _ => unreachable!(),
    });
    let client = fixture.client();

    let workspace = run(client.workspace("oca")).unwrap();
    let tab = run(client.tab(&workspace, "wabc12", true, Path::new("/worker"))).unwrap();
    assert_eq!(tab.as_str(), "t1");
    fixture.finish();
}

#[test]
fn tab_times_out_against_a_fake_socket() {
    let fixture = Fixture::new(2, |index, request| {
        if index == 0 {
            existing_workspace(request)
        } else {
            assert_request(request, "tab.create");
            FakeResponse::Never
        }
    });
    let client = fixture.client();
    let workspace = run(client.workspace("oca")).unwrap();

    assert_timeout(run(client.tab(&workspace, "wabc12", true, Path::new("/worker"))).unwrap_err());
    fixture.finish();
}

#[test]
fn tab_rejects_a_malformed_response_envelope() {
    let fixture = Fixture::new(2, |index, request| {
        if index == 0 {
            existing_workspace(request)
        } else {
            assert_request(request, "tab.create");
            FakeResponse::MalformedEnvelope
        }
    });
    let client = fixture.client();
    let workspace = run(client.workspace("oca")).unwrap();

    assert_malformed(
        run(client.tab(&workspace, "wabc12", true, Path::new("/worker"))).unwrap_err(),
    );
    fixture.finish();
}

#[test]
fn agent_start_succeeds_with_shared_input_argv() {
    let fixture = Fixture::new(3, |index, request| match index {
        0 => existing_workspace(request),
        1 => created_tab(request),
        2 => {
            assert_request(request, "agent.start");
            assert_eq!(request["params"]["name"], "opencode");
            assert_eq!(request["params"]["kind"], "opencode");
            assert_eq!(request["params"]["pane_id"], "p1");
            assert_eq!(request["params"]["args"], json!(["--session", "ses_1"]));
            assert!(
                request["params"]["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|argument| argument != "--read-only")
            );
            FakeResponse::Result(json!({
                "type":"agent_started",
                "agent":{"terminal_id":"term1"},
                "argv":["opencode","--session","ses_1"]
            }))
        }
        _ => unreachable!(),
    });
    let client = fixture.client();
    let (workspace, tab) = workspace_and_tab(&client);
    assert_eq!(workspace.as_str(), "w1");

    let agent = run(client.agent_start(
        &tab,
        vec!["opencode".into(), "--session".into(), "ses_1".into()],
    ))
    .unwrap();
    assert_eq!(agent.as_str(), "term1");
    fixture.finish();
}

#[test]
fn agent_start_times_out_against_a_fake_socket() {
    let fixture = Fixture::new(3, |index, request| match index {
        0 => existing_workspace(request),
        1 => created_tab(request),
        2 => {
            assert_request(request, "agent.start");
            FakeResponse::Never
        }
        _ => unreachable!(),
    });
    let client = fixture.client();
    let (_, tab) = workspace_and_tab(&client);

    assert_timeout(
        run(client.agent_start(&tab, vec!["opencode".into(), "--session".into()])).unwrap_err(),
    );
    fixture.finish();
}

#[test]
fn agent_start_rejects_a_malformed_response_envelope() {
    let fixture = Fixture::new(3, |index, request| match index {
        0 => existing_workspace(request),
        1 => created_tab(request),
        2 => {
            assert_request(request, "agent.start");
            FakeResponse::MalformedEnvelope
        }
        _ => unreachable!(),
    });
    let client = fixture.client();
    let (_, tab) = workspace_and_tab(&client);

    assert_malformed(
        run(client.agent_start(&tab, vec!["opencode".into(), "--session".into()])).unwrap_err(),
    );
    fixture.finish();
}

#[test]
fn close_tab_succeeds_against_a_fake_socket() {
    let fixture = Fixture::new(3, |index, request| match index {
        0 => existing_workspace(request),
        1 => created_tab(request),
        2 => {
            assert_request(request, "tab.close");
            assert_eq!(request["params"]["tab_id"], "t1");
            FakeResponse::Result(json!({"type":"ok"}))
        }
        _ => unreachable!(),
    });
    let client = fixture.client();
    let (_, tab) = workspace_and_tab(&client);

    run(client.close_tab(&tab)).unwrap();
    fixture.finish();
}

#[test]
fn close_tab_id_succeeds_for_a_persisted_identifier() {
    let fixture = Fixture::new(1, |_, request| {
        assert_request(request, "tab.close");
        assert_eq!(request["params"]["tab_id"], "persisted-t1");
        FakeResponse::Result(json!({"type":"ok"}))
    });
    let client = fixture.client();

    run(client.close_tab_id("persisted-t1")).unwrap();
    fixture.finish();
}

#[test]
fn close_tab_times_out_against_a_fake_socket() {
    let fixture = Fixture::new(3, |index, request| match index {
        0 => existing_workspace(request),
        1 => created_tab(request),
        2 => {
            assert_request(request, "tab.close");
            FakeResponse::Never
        }
        _ => unreachable!(),
    });
    let client = fixture.client();
    let (_, tab) = workspace_and_tab(&client);

    assert_timeout(run(client.close_tab(&tab)).unwrap_err());
    fixture.finish();
}

#[test]
fn close_tab_rejects_a_malformed_response_envelope() {
    let fixture = Fixture::new(3, |index, request| match index {
        0 => existing_workspace(request),
        1 => created_tab(request),
        2 => {
            assert_request(request, "tab.close");
            FakeResponse::MalformedEnvelope
        }
        _ => unreachable!(),
    });
    let client = fixture.client();
    let (_, tab) = workspace_and_tab(&client);

    assert_malformed(run(client.close_tab(&tab)).unwrap_err());
    fixture.finish();
}

fn existing_workspace(request: &Value) -> FakeResponse {
    assert_request(request, "workspace.list");
    FakeResponse::Result(json!({
        "type":"workspace_list",
        "workspaces":[{"workspace_id":"w1","label":"oca"}]
    }))
}

fn created_tab(request: &Value) -> FakeResponse {
    assert_request(request, "tab.create");
    FakeResponse::Result(json!({
        "type":"tab_created",
        "tab":{"tab_id":"t1"},
        "root_pane":{"pane_id":"p1"}
    }))
}

fn workspace_and_tab(client: &HerdrClient) -> (oca_display::WorkspaceId, oca_display::TabId) {
    let workspace = run(client.workspace("oca")).unwrap();
    let tab = run(client.tab(&workspace, "wabc12", true, Path::new("/worker"))).unwrap();
    (workspace, tab)
}

fn assert_request(request: &Value, method: &str) {
    assert_eq!(request["method"], method);
    assert!(request["id"].as_str().unwrap().starts_with("oca:"));
}

fn assert_timeout(error: HerdrError) {
    assert!(matches!(error, HerdrError::Timeout { .. }), "{error}");
}

fn assert_malformed(error: HerdrError) {
    assert!(
        matches!(error, HerdrError::MalformedResponse { .. }),
        "{error}"
    );
}

struct Fixture {
    _temp: tempfile::TempDir,
    socket: PathBuf,
    server: thread::JoinHandle<()>,
}

impl Fixture {
    fn new(
        requests: usize,
        responder: impl Fn(usize, &Value) -> FakeResponse + Send + 'static,
    ) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            for index in 0..requests {
                let (stream, _) = listener.accept().unwrap();
                let request = read_request(&stream);
                let request_id = request["id"].as_str().unwrap().to_owned();
                serve(stream, &request_id, responder(index, &request));
            }
        });
        Self {
            _temp: temp,
            socket,
            server,
        }
    }

    fn client(&self) -> HerdrClient {
        HerdrClient::new(&self.socket, TEST_TIMEOUT)
    }

    fn finish(self) {
        self.server.join().unwrap();
    }
}

fn read_request(stream: &UnixStream) -> Value {
    let mut line = String::new();
    BufReader::new(stream.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    serde_json::from_str(&line).unwrap()
}

fn serve(mut stream: UnixStream, request_id: &str, response: FakeResponse) {
    match response {
        FakeResponse::Result(result) => {
            writeln!(stream, "{}", json!({"id":request_id,"result":result})).unwrap();
        }
        FakeResponse::MalformedEnvelope => {
            writeln!(stream, "{}", json!({"id":request_id,"result":{}})).unwrap();
        }
        // Structurally valid for `workspace_list`, so only the declared result
        // type distinguishes it from a genuine reply.
        FakeResponse::WrongResultType => {
            writeln!(
                stream,
                "{}",
                json!({"id":request_id,"result":{"type":"tab_created","workspaces":[]}})
            )
            .unwrap();
        }
        FakeResponse::MismatchedId => {
            writeln!(
                stream,
                "{}",
                json!({"id":"herdr:someone-elses-request","result":{"type":"workspace_list","workspaces":[]}})
            )
            .unwrap();
        }
        FakeResponse::Never => thread::sleep(Duration::from_millis(50)),
    }
}

fn run<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}
