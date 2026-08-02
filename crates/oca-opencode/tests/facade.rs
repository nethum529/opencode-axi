use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    thread,
};

use oca_core::ResolvedModel;
use oca_opencode::{CreateSessionRequest, OpenCodeClient, OpenCodeError, PromptRequest, TextPart};

struct FakeServer {
    base_url: String,
    handle: thread::JoinHandle<String>,
}

impl FakeServer {
    fn once(response: impl Into<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fake server binds");
        let base_url = format!("http://{}", listener.local_addr().expect("server address"));
        let response = response.into();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("fake server accepts request");
            let request = read_request(&mut stream);
            stream
                .write_all(response.as_bytes())
                .expect("fake server writes response");
            request
        });

        Self { base_url, handle }
    }

    fn finish(self) -> String {
        self.handle.join().expect("fake server completes")
    }
}

fn response(status: &str, content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
}

fn prompt_request() -> PromptRequest {
    PromptRequest {
        message_id: "msg_oca_01".to_owned(),
        model: ResolvedModel {
            alias: "opus".to_owned(),
            provider: "openai".to_owned(),
            model: "gpt-5.6".to_owned(),
            effort: "high".to_owned(),
            variant: "high".to_owned(),
        },
        variant: "high".to_owned(),
        role: "worker".to_owned(),
        parts: vec![TextPart {
            text: "Implement the facade".to_owned(),
        }],
        output_schema: None,
        permission: None,
    }
}

fn assert_prompt_body(request: &str) {
    let body = request
        .split_once("\r\n\r\n")
        .expect("request has a body")
        .1;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body).expect("prompt body is JSON"),
        serde_json::json!({
            "agent": "worker",
            "messageID": "msg_oca_01",
            "model": { "providerID": "openai", "modelID": "gpt-5.6" },
            "parts": [{ "type": "text", "text": "Implement the facade" }],
            "variant": "high",
        })
    );
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream
            .read(&mut buffer)
            .expect("request headers are readable");
        assert_ne!(read, 0, "request closed before headers arrived");
        request.extend_from_slice(&buffer[..read]);
    }

    let headers_end = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("request has complete headers")
        + 4;
    let headers = String::from_utf8_lossy(&request[..headers_end]);
    let content_length = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length: "))
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_default();
    while request.len() - headers_end < content_length {
        let read = stream.read(&mut buffer).expect("request body is readable");
        assert_ne!(read, 0, "request closed before body arrived");
        request.extend_from_slice(&buffer[..read]);
    }
    String::from_utf8(request).expect("request is utf-8")
}

#[tokio::test]
async fn prompt_async_sends_the_native_variant_to_the_pinned_operation() {
    let server = FakeServer::once(response("204 No Content", "text/plain", ""));
    let client = OpenCodeClient::new(server.base_url.parse().expect("valid URL"));

    client
        .prompt_async("ses_target", prompt_request())
        .await
        .expect("prompt is accepted");

    let request = server.finish();
    assert!(request.starts_with("POST /session/ses_target/prompt_async HTTP/1.1\r\n"));
    assert_prompt_body(&request);
}

#[tokio::test]
async fn queue_uses_the_legacy_prompt_async_operation_with_the_native_variant() {
    let server = FakeServer::once(response("204 No Content", "text/plain", ""));
    let client = OpenCodeClient::new(server.base_url.parse().expect("valid URL"));

    client
        .queue("ses_target", prompt_request())
        .await
        .expect("queued prompt is accepted");

    let request = server.finish();
    assert!(request.starts_with("POST /session/ses_target/prompt_async HTTP/1.1\r\n"));
    assert_prompt_body(&request);
}

#[tokio::test]
async fn abort_sends_the_pinned_operation_shape() {
    let server = FakeServer::once(response("200 OK", "application/json", "true"));
    let client = OpenCodeClient::new(server.base_url.parse().expect("valid URL"));

    let accepted = client.abort("ses_target").await.expect("abort is accepted");

    assert!(accepted.aborted);
    let request = server.finish();
    assert!(request.starts_with("POST /session/ses_target/abort HTTP/1.1\r\n"));
    assert!(!request.contains("\r\n\r\n{"));
}

#[tokio::test]
async fn messages_sends_the_pinned_operation_shape() {
    let server = FakeServer::once(response(
        "200 OK",
        "application/json",
        "[{\"info\":{\"id\":\"msg_01\"},\"parts\":[]}]",
    ));
    let client = OpenCodeClient::new(server.base_url.parse().expect("valid URL"));

    let messages = client
        .messages("ses_target")
        .await
        .expect("messages are returned");

    assert_eq!(messages.len(), 1);
    let request = server.finish();
    assert!(request.starts_with("GET /session/ses_target/message HTTP/1.1\r\n"));
}

#[tokio::test]
async fn subscribe_forwards_last_event_id_and_uses_the_sse_parser() {
    let server = FakeServer::once(response(
        "200 OK",
        "text/event-stream",
        "id: evt_42\nevent: session.idle\ndata: done\n\n",
    ));
    let client = OpenCodeClient::new(server.base_url.parse().expect("valid URL"));

    let mut subscription = client
        .subscribe(Some("evt_41"))
        .await
        .expect("subscription opens");
    let event = subscription
        .next()
        .await
        .expect("SSE event parses")
        .expect("SSE event exists");

    assert_eq!(event.id.as_deref(), Some("evt_42"));
    assert_eq!(event.event.as_deref(), Some("session.idle"));
    let request = server.finish();
    assert!(request.starts_with("GET /event HTTP/1.1\r\n"));
    assert!(request.contains("last-event-id: evt_41\r\n"));
}

#[tokio::test]
async fn remaining_facade_methods_map_malformed_envelopes_to_protocol_mismatch() {
    let server = FakeServer::once(response("200 OK", "application/json", "{}"));
    let client = OpenCodeClient::new(server.base_url.parse().expect("valid URL"));
    assert!(matches!(
        client.prompt_async("ses_target", prompt_request()).await,
        Err(OpenCodeError::ProtocolMismatch { .. })
    ));
    let _ = server.finish();

    let server = FakeServer::once(response("200 OK", "application/json", "{}"));
    let client = OpenCodeClient::new(server.base_url.parse().expect("valid URL"));
    assert!(matches!(
        client.queue("ses_target", prompt_request()).await,
        Err(OpenCodeError::ProtocolMismatch { .. })
    ));
    let _ = server.finish();

    let server = FakeServer::once(response("200 OK", "application/json", "{}"));
    let client = OpenCodeClient::new(server.base_url.parse().expect("valid URL"));
    assert!(matches!(
        client.abort("ses_target").await,
        Err(OpenCodeError::ProtocolMismatch { .. })
    ));
    let _ = server.finish();

    let server = FakeServer::once(response("200 OK", "application/json", "[{}]"));
    let client = OpenCodeClient::new(server.base_url.parse().expect("valid URL"));
    assert!(matches!(
        client.messages("ses_target").await,
        Err(OpenCodeError::ProtocolMismatch { .. })
    ));
    let _ = server.finish();

    let server = FakeServer::once(response("200 OK", "application/json", "{}"));
    let client = OpenCodeClient::new(server.base_url.parse().expect("valid URL"));
    assert!(matches!(
        client.subscribe(None).await,
        Err(OpenCodeError::ProtocolMismatch { .. })
    ));
    assert!(!server.finish().contains("last-event-id:"));
}

#[tokio::test]
async fn create_session_sends_the_pinned_operation_shape() {
    let server = FakeServer::once(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 20\r\n\r\n{\"id\":\"ses_created\"}",
    );
    let client = OpenCodeClient::new(server.base_url.parse().expect("valid URL"));

    let session = client
        .create_session(CreateSessionRequest {
            directory: Some("/workspace".to_owned()),
            title: Some("New worker".to_owned()),
            ..CreateSessionRequest::default()
        })
        .await
        .expect("session is created");

    assert_eq!(session.id, "ses_created");
    let request = server.finish();
    assert!(request.starts_with("POST /session?directory=%2Fworkspace HTTP/1.1\r\n"));
    assert!(request.contains("\r\n\r\n{\"title\":\"New worker\"}"));
}

#[tokio::test]
async fn create_session_uses_the_create_model_envelope() {
    let server = FakeServer::once(response(
        "200 OK",
        "application/json",
        "{\"id\":\"ses_created\"}",
    ));
    let client = OpenCodeClient::new(server.base_url.parse().expect("valid URL"));

    client
        .create_session(CreateSessionRequest {
            model: Some(prompt_request().model),
            ..CreateSessionRequest::default()
        })
        .await
        .expect("session is created");

    let request = server.finish();
    let body = request
        .split_once("\r\n\r\n")
        .expect("request has a body")
        .1;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body).expect("create body is JSON"),
        serde_json::json!({
            "model": {
                "id": "gpt-5.6",
                "providerID": "openai",
                "variant": "high",
            }
        })
    );
}

#[tokio::test]
async fn create_session_maps_a_malformed_envelope_to_protocol_mismatch() {
    let server = FakeServer::once(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n{}",
    );
    let client = OpenCodeClient::new(server.base_url.parse().expect("valid URL"));

    let error = client
        .create_session(CreateSessionRequest::default())
        .await
        .expect_err("missing session id is a malformed envelope");

    assert!(matches!(error, OpenCodeError::ProtocolMismatch { .. }));
    let _ = server.finish();
}

#[tokio::test]
async fn create_session_preserves_additive_response_fields() {
    let server = FakeServer::once(response(
        "200 OK",
        "application/json",
        "{\"id\":\"ses_created\",\"newField\":true}",
    ));
    let client = OpenCodeClient::new(server.base_url.parse().expect("valid URL"));

    let session = client
        .create_session(CreateSessionRequest::default())
        .await
        .expect("additive response fields are accepted");

    assert_eq!(
        session.extra.get("newField"),
        Some(&serde_json::json!(true))
    );
    let _ = server.finish();
}

#[tokio::test]
async fn create_session_preserves_a_base_url_path_stem() {
    let server = FakeServer::once(response(
        "200 OK",
        "application/json",
        "{\"id\":\"ses_created\"}",
    ));
    let base_url = format!("{}/api", server.base_url);
    let client = OpenCodeClient::new(base_url.parse().expect("valid URL"));

    client
        .create_session(CreateSessionRequest::default())
        .await
        .expect("session is created");

    let request = server.finish();
    assert!(request.starts_with("POST /api/session HTTP/1.1\r\n"));
}
