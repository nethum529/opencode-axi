use std::fmt;

use oca_core::ResolvedModel;
use reqwest::{Method, StatusCode, header};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};
use url::Url;

use crate::{SseChunkSource, SseError, SseEvent, SseStream, generated};

/// A session identifier assigned by `OpenCode`.
pub type SessionId = String;

/// A caller-generated message identifier.
pub type MessageId = String;

/// A plain-text prompt part.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextPart {
    pub text: String,
}

/// The data used to create an `OpenCode` session.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CreateSessionRequest {
    pub directory: Option<String>,
    pub workspace: Option<String>,
    pub title: Option<String>,
    pub agent: Option<String>,
    pub model: Option<ResolvedModel>,
    pub permission: Option<Value>,
    pub metadata: Map<String, Value>,
}

/// The data sent with an asynchronous prompt or queued prompt.
#[derive(Clone, Debug, PartialEq)]
pub struct PromptRequest {
    pub message_id: MessageId,
    pub model: ResolvedModel,
    /// The native `OpenCode` per-request effort field.
    pub variant: String,
    /// The `OpenCode` agent role receiving the prompt.
    pub role: String,
    pub parts: Vec<TextPart>,
    pub output_schema: Option<Value>,
    pub permission: Option<Value>,
}

/// A minimally typed `OpenCode` session, retaining additive server fields.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Session {
    pub id: SessionId,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A projected message together with its parts, retaining additive server fields.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct MessageWithParts {
    pub info: Value,
    pub parts: Vec<Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The acknowledgement returned by an accepted asynchronous prompt.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PromptAccepted;

/// The acknowledgement returned by an accepted queued prompt.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ControlAccepted;

/// The acknowledgement returned by a successful abort.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AbortAccepted {
    pub aborted: bool,
}

/// Failures produced while adapting the `OpenCode` HTTP API.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OpenCodeError {
    ProtocolMismatch { message: String },
    Server { status: u16, body: String },
    Transport { message: String },
}

impl fmt::Display for OpenCodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProtocolMismatch { message } => write!(formatter, "protocol_mismatch: {message}"),
            Self::Server { status, body } => {
                write!(formatter, "OpenCode returned HTTP {status}: {body}")
            }
            Self::Transport { message } => {
                write!(formatter, "OpenCode transport failed: {message}")
            }
        }
    }
}

impl std::error::Error for OpenCodeError {}

/// A parsed global `OpenCode` event stream.
pub struct Subscription {
    stream: SseStream<ResponseChunks>,
}

impl Subscription {
    /// Returns the next parsed SSE event from `OpenCode`.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying SSE response cannot be read or parsed.
    pub async fn next(&mut self) -> Result<Option<SseEvent>, SseError> {
        self.stream.next().await
    }
}

struct ResponseChunks {
    response: reqwest::Response,
}

impl SseChunkSource for ResponseChunks {
    type Error = reqwest::Error;

    #[allow(clippy::manual_async_fn)]
    fn next_chunk(&mut self) -> impl Future<Output = Result<Option<Vec<u8>>, Self::Error>> + Send {
        async move {
            self.response
                .chunk()
                .await
                .map(|chunk| chunk.map(|bytes| bytes.to_vec()))
        }
    }
}

use std::future::Future;

/// The sole hand-written boundary around `OpenCode`'s generated HTTP client.
pub struct OpenCodeClient {
    generated: generated::Client,
    base_url: Url,
}

impl OpenCodeClient {
    #[must_use]
    pub fn new(base_url: Url) -> Self {
        let normalized = base_url.as_str().trim_end_matches('/');
        Self {
            generated: generated::Client::new(normalized),
            base_url,
        }
    }

    /// Creates a session through the pinned `session.create` operation.
    ///
    /// # Errors
    ///
    /// Returns an error when `OpenCode` rejects the request or its response is malformed.
    pub async fn create_session(
        &self,
        request: CreateSessionRequest,
    ) -> Result<Session, OpenCodeError> {
        let mut body = Map::new();
        insert_optional_string(&mut body, "title", request.title);
        insert_optional_string(&mut body, "agent", request.agent);
        if let Some(model) = request.model {
            body.insert("model".to_owned(), create_model_value(&model));
        }
        if let Some(permission) = request.permission {
            body.insert("permission".to_owned(), permission);
        }
        if !request.metadata.is_empty() {
            body.insert("metadata".to_owned(), Value::Object(request.metadata));
        }

        self.json(
            Method::POST,
            "session",
            request.directory.as_deref(),
            request.workspace.as_deref(),
            Some(Value::Object(body)),
            StatusCode::OK,
        )
        .await
    }

    /// Submits an asynchronous prompt and returns immediately after server acceptance.
    ///
    /// # Errors
    ///
    /// Returns an error when `OpenCode` rejects the request or its response is malformed.
    pub async fn prompt_async(
        &self,
        session: &SessionId,
        request: PromptRequest,
    ) -> Result<PromptAccepted, OpenCodeError> {
        self.send_prompt(session, request).await?;
        Ok(PromptAccepted)
    }

    /// Queues a prompt with the legacy `prompt_async` endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when `OpenCode` rejects the request or its response is malformed.
    pub async fn queue(
        &self,
        session: &SessionId,
        request: PromptRequest,
    ) -> Result<ControlAccepted, OpenCodeError> {
        self.send_prompt(session, request).await?;
        Ok(ControlAccepted)
    }

    /// Aborts an active `OpenCode` session.
    ///
    /// # Errors
    ///
    /// Returns an error when `OpenCode` rejects the request or its response is malformed.
    pub async fn abort(&self, session: &SessionId) -> Result<AbortAccepted, OpenCodeError> {
        let path = format!("session/{session}/abort");
        let aborted: bool = self
            .json(Method::POST, &path, None, None, None, StatusCode::OK)
            .await?;
        Ok(AbortAccepted { aborted })
    }

    /// Retrieves projected messages for one reconciliation pass.
    ///
    /// # Errors
    ///
    /// Returns an error when `OpenCode` rejects the request or its response is malformed.
    pub async fn messages(
        &self,
        session: &SessionId,
    ) -> Result<Vec<MessageWithParts>, OpenCodeError> {
        let path = format!("session/{session}/message");
        self.json(Method::GET, &path, None, None, None, StatusCode::OK)
            .await
    }

    /// Opens the global SSE stream, optionally resuming after an event id.
    ///
    /// # Errors
    ///
    /// Returns an error when `OpenCode` rejects the subscription or does not return SSE.
    pub async fn subscribe(
        &self,
        last_event_id: Option<&str>,
    ) -> Result<Subscription, OpenCodeError> {
        let url = self.endpoint("event");
        let mut request = self
            .generated
            .client()
            .get(url)
            .header("api-version", self.generated.api_version())
            .header(header::ACCEPT, "text/event-stream");
        if let Some(last_event_id) = last_event_id {
            request = request.header("last-event-id", last_event_id);
        }
        let response = request
            .send()
            .await
            .map_err(|error| transport_error(&error))?;
        if response.status() != StatusCode::OK {
            return Err(server_error(response).await);
        }
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok());
        if !content_type.is_some_and(|value| value.starts_with("text/event-stream")) {
            return Err(protocol_mismatch(
                "event.subscribe response is not an SSE stream",
            ));
        }
        Ok(Subscription {
            stream: SseStream::new(ResponseChunks { response }),
        })
    }

    async fn send_prompt(
        &self,
        session: &SessionId,
        request: PromptRequest,
    ) -> Result<(), OpenCodeError> {
        let mut body = Map::new();
        body.insert("messageID".to_owned(), Value::String(request.message_id));
        body.insert("model".to_owned(), model_value(&request.model, None));
        body.insert("variant".to_owned(), Value::String(request.variant));
        body.insert("agent".to_owned(), Value::String(request.role));
        body.insert(
            "parts".to_owned(),
            Value::Array(
                request
                    .parts
                    .into_iter()
                    .map(|part| json!({ "type": "text", "text": part.text }))
                    .collect(),
            ),
        );
        let path = format!("session/{session}/prompt_async");
        self.empty(
            Method::POST,
            &path,
            Some(Value::Object(body)),
            StatusCode::NO_CONTENT,
        )
        .await
    }

    async fn json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        directory: Option<&str>,
        workspace: Option<&str>,
        body: Option<Value>,
        expected_status: StatusCode,
    ) -> Result<T, OpenCodeError> {
        let mut request = self
            .generated
            .client()
            .request(method, self.endpoint(path))
            .header("api-version", self.generated.api_version())
            .header(header::ACCEPT, "application/json");
        if let Some(directory) = directory {
            request = request.query(&[("directory", directory)]);
        }
        if let Some(workspace) = workspace {
            request = request.query(&[("workspace", workspace)]);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|error| transport_error(&error))?;
        if response.status() != expected_status {
            return Err(status_error(response, expected_status).await);
        }
        response.json().await.map_err(|error| {
            protocol_mismatch(format!(
                "{path} response envelope could not be decoded: {error}"
            ))
        })
    }

    async fn empty(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        expected_status: StatusCode,
    ) -> Result<(), OpenCodeError> {
        let mut request = self
            .generated
            .client()
            .request(method, self.endpoint(path))
            .header("api-version", self.generated.api_version());
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|error| transport_error(&error))?;
        if response.status() != expected_status {
            return Err(status_error(response, expected_status).await);
        }
        Ok(())
    }

    fn endpoint(&self, path: &str) -> Url {
        let prefix = self.base_url.path().trim_end_matches('/');
        let mut endpoint = self.base_url.clone();
        endpoint.set_path(&format!("{prefix}/{path}"));
        endpoint.set_query(None);
        endpoint.set_fragment(None);
        endpoint
    }
}

fn model_value(model: &ResolvedModel, variant: Option<&str>) -> Value {
    let mut value = Map::from_iter([
        (
            "providerID".to_owned(),
            Value::String(model.provider.clone()),
        ),
        ("modelID".to_owned(), Value::String(model.model.clone())),
    ]);
    if let Some(variant) = variant {
        value.insert("variant".to_owned(), Value::String(variant.to_owned()));
    }
    Value::Object(value)
}

fn create_model_value(model: &ResolvedModel) -> Value {
    json!({
        "id": model.model,
        "providerID": model.provider,
        "variant": model.variant,
    })
}

fn insert_optional_string(body: &mut Map<String, Value>, name: &str, value: Option<String>) {
    if let Some(value) = value {
        body.insert(name.to_owned(), Value::String(value));
    }
}

fn protocol_mismatch(message: impl Into<String>) -> OpenCodeError {
    OpenCodeError::ProtocolMismatch {
        message: message.into(),
    }
}

fn transport_error(error: &reqwest::Error) -> OpenCodeError {
    OpenCodeError::Transport {
        message: error.to_string(),
    }
}

async fn status_error(response: reqwest::Response, expected_status: StatusCode) -> OpenCodeError {
    if response.status().is_success() {
        protocol_mismatch(format!(
            "expected HTTP {}, received HTTP {}",
            expected_status,
            response.status()
        ))
    } else {
        server_error(response).await
    }
}

async fn server_error(response: reqwest::Response) -> OpenCodeError {
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    OpenCodeError::Server { status, body }
}
