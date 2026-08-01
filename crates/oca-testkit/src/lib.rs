//! Fixtures for recording and replaying the supported `OpenCode` HTTP surface.
//!
//! The recorder deliberately owns sanitization.  A [`Recording`] is therefore safe to
//! commit as a test fixture: callers cannot opt out of redacting credentials, local paths,
//! prompts, identifiers, or timestamps.

use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    path::Path,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A transport-neutral HTTP request captured by the recorder or accepted by replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    #[must_use]
    pub fn new<H, K, V>(
        method: impl Into<String>,
        path: impl Into<String>,
        headers: H,
        body: impl AsRef<[u8]>,
    ) -> Self
    where
        H: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self {
            method: method.into(),
            path: path.into(),
            headers: headers
                .into_iter()
                .map(|(name, value)| (name.into(), value.into()))
                .collect(),
            body: body.as_ref().to_vec(),
        }
    }
}

/// A transport-neutral HTTP response.
///
/// `body_chunks` is intentionally not flattened. Each item is a captured HTTP body chunk,
/// allowing replay to retain deliberately split SSE frames.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body_chunks: Vec<Vec<u8>>,
}

impl HttpResponse {
    #[must_use]
    pub fn new<H, K, V, C>(status: u16, headers: H, body_chunks: C) -> Self
    where
        H: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
        C: IntoIterator<Item = Vec<u8>>,
    {
        Self {
            status,
            headers: headers
                .into_iter()
                .map(|(name, value)| (name.into(), value.into()))
                .collect(),
            body_chunks: body_chunks.into_iter().collect(),
        }
    }
}

/// One request/response pair in a recording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedExchange {
    pub request: HttpRequest,
    pub response: HttpResponse,
}

/// A sanitized, serializable request/response transcript.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recording {
    pub exchanges: Vec<RecordedExchange>,
}

/// Captures exchanges after mandatory sanitization.
#[derive(Debug, Default)]
pub struct Recorder {
    recording: Recording,
    sanitizer: Sanitizer,
}

impl Recorder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a request and its response, removing sensitive and unstable values first.
    pub fn record(&mut self, request: HttpRequest, response: HttpResponse) {
        let request = self.sanitizer.request(request);
        let response = self.sanitizer.response(response);
        self.recording
            .exchanges
            .push(RecordedExchange { request, response });
    }

    #[must_use]
    pub fn finish(self) -> Recording {
        self.recording
    }
}

/// An operation explicitly supported by the pinned `OpenCode` compatibility surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    pub operation_id: String,
    pub method: String,
    pub path: String,
}

/// The small, pinned subset of `OpenCode` routes that the fake server may serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Surface {
    operations: Vec<Operation>,
}

impl Surface {
    /// Parses a `surface.json` compatibility artifact.
    ///
    /// # Errors
    ///
    /// Returns [`SurfaceError`] when the artifact is not valid surface JSON.
    pub fn from_json(json: &[u8]) -> Result<Self, SurfaceError> {
        #[derive(Deserialize)]
        struct Artifact {
            operations: Vec<ArtifactOperation>,
        }
        #[derive(Deserialize)]
        struct ArtifactOperation {
            operation_id: String,
            method: String,
            path: String,
        }

        let artifact: Artifact = serde_json::from_slice(json).map_err(SurfaceError::InvalidJson)?;
        if artifact.operations.is_empty() {
            return Err(SurfaceError::Empty);
        }
        Ok(Self {
            operations: artifact
                .operations
                .into_iter()
                .map(|operation| Operation {
                    operation_id: operation.operation_id,
                    method: operation.method,
                    path: operation.path,
                })
                .collect(),
        })
    }

    /// Loads `surface.json` from a compatibility snapshot directory.
    ///
    /// # Errors
    ///
    /// Returns [`SurfaceError`] when the file cannot be read or parsed.
    pub fn from_snapshot(directory: impl AsRef<Path>) -> Result<Self, SurfaceError> {
        let file = directory.as_ref().join("surface.json");
        let json = std::fs::read(&file).map_err(|source| SurfaceError::Read {
            path: file.display().to_string(),
            source,
        })?;
        Self::from_json(&json)
    }

    /// Loads the `OpenCode` 1.18.10 surface committed with this crate.
    ///
    /// Embedding the pinned artifact makes the test kit independent of the caller's current
    /// directory while retaining the snapshot as the sole supported route authority.
    ///
    /// # Errors
    ///
    /// Returns [`SurfaceError`] if the committed artifact cannot be parsed.
    pub fn pinned() -> Result<Self, SurfaceError> {
        Self::from_json(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../compatibility/opencode/1.18.10/surface.json"
        )))
    }

    #[must_use]
    pub fn operations(&self) -> &[Operation] {
        &self.operations
    }

    fn operation(&self, method: &str, path: &str) -> Option<&Operation> {
        self.operations.iter().find(|operation| {
            operation.method.eq_ignore_ascii_case(method) && path_matches(&operation.path, path)
        })
    }
}

/// An error while loading the route surface.
#[derive(Debug)]
pub enum SurfaceError {
    Read {
        path: String,
        source: std::io::Error,
    },
    InvalidJson(serde_json::Error),
    Empty,
}

impl fmt::Display for SurfaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => write!(formatter, "could not read {path}: {source}"),
            Self::InvalidJson(source) => write!(formatter, "invalid surface.json: {source}"),
            Self::Empty => formatter.write_str("surface.json contains no operations"),
        }
    }
}

impl std::error::Error for SurfaceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::InvalidJson(source) => Some(source),
            Self::Empty => None,
        }
    }
}

/// A deterministic in-memory fake server backed by a sanitized recording.
#[derive(Debug)]
pub struct ReplayServer {
    surface: Surface,
    recording: Recording,
    request_index: usize,
}

impl ReplayServer {
    #[must_use]
    pub fn new(surface: Surface, recording: Recording) -> Self {
        Self {
            surface,
            recording,
            request_index: 0,
        }
    }

    /// Creates a replay server using the committed `OpenCode` 1.18.10 surface.
    ///
    /// # Errors
    ///
    /// Returns [`SurfaceError`] if the committed compatibility artifact is invalid.
    pub fn pinned(recording: Recording) -> Result<Self, SurfaceError> {
        Ok(Self::new(Surface::pinned()?, recording))
    }

    /// Handles one request, strictly matching the next recorded request.
    ///
    /// Matching uses a surface operation identifier rather than a literal path, so captures
    /// with different session or message identifiers replay correctly.
    ///
    /// # Errors
    ///
    /// Returns [`ReplayError`] for a route outside `surface.json`, a mismatch, or an exhausted
    /// recording.
    pub fn handle(&mut self, request: &HttpRequest) -> Result<HttpResponse, ReplayError> {
        let index = self.request_index;
        let actual_operation = self
            .surface
            .operation(&request.method, &request.path)
            .ok_or_else(|| ReplayError::UndocumentedRoute {
                request_index: index,
                method: request.method.clone(),
                path: request.path.clone(),
                expected_operation: self.expected_operation(),
            })?;

        let Some(expected) = self.recording.exchanges.get(index) else {
            return Err(ReplayError::UnexpectedRequest {
                request_index: index,
                operation: actual_operation.operation_id.clone(),
            });
        };
        let expected_operation = self
            .surface
            .operation(&expected.request.method, &expected.request.path)
            .ok_or_else(|| ReplayError::InvalidRecording {
                request_index: index,
                method: expected.request.method.clone(),
                path: expected.request.path.clone(),
            })?;

        if expected_operation.operation_id != actual_operation.operation_id {
            return Err(ReplayError::Mismatch {
                request_index: index,
                field: "operation",
                expected: expected_operation.operation_id.clone(),
                actual: actual_operation.operation_id.clone(),
            });
        }

        let actual_body = sanitize_for_matching(request).body;
        if canonical_json(&expected.request.body) != canonical_json(&actual_body) {
            return Err(ReplayError::Mismatch {
                request_index: index,
                field: "body",
                expected: printable_body(&expected.request.body),
                actual: printable_body(&actual_body),
            });
        }

        self.request_index += 1;
        Ok(expected.response.clone())
    }

    /// Verifies that replay consumed exactly every captured request.
    ///
    /// # Errors
    ///
    /// Returns [`ReplayError::UnconsumedRequests`] when the client made too few requests.
    pub fn assert_finished(&self) -> Result<(), ReplayError> {
        let remaining = self
            .recording
            .exchanges
            .len()
            .saturating_sub(self.request_index);
        if remaining == 0 {
            Ok(())
        } else {
            Err(ReplayError::UnconsumedRequests {
                request_index: self.request_index,
                remaining,
                expected_operation: self.expected_operation(),
            })
        }
    }

    fn expected_operation(&self) -> Option<String> {
        let exchange = self.recording.exchanges.get(self.request_index)?;
        self.surface
            .operation(&exchange.request.method, &exchange.request.path)
            .map(|operation| operation.operation_id.clone())
    }
}

/// A route or request-order mismatch produced by [`ReplayServer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayError {
    UndocumentedRoute {
        request_index: usize,
        method: String,
        path: String,
        expected_operation: Option<String>,
    },
    UnexpectedRequest {
        request_index: usize,
        operation: String,
    },
    InvalidRecording {
        request_index: usize,
        method: String,
        path: String,
    },
    Mismatch {
        request_index: usize,
        field: &'static str,
        expected: String,
        actual: String,
    },
    UnconsumedRequests {
        request_index: usize,
        remaining: usize,
        expected_operation: Option<String>,
    },
}

impl fmt::Display for ReplayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UndocumentedRoute {
                request_index,
                method,
                path,
                expected_operation,
            } => write!(
                formatter,
                "replay request-index {request_index}: undocumented route {method} {path}; expected operation {}",
                expected_operation
                    .as_deref()
                    .unwrap_or("<end-of-recording>")
            ),
            Self::UnexpectedRequest {
                request_index,
                operation,
            } => write!(
                formatter,
                "replay request-index {request_index}: unexpected operation {operation}; recording is exhausted"
            ),
            Self::InvalidRecording {
                request_index,
                method,
                path,
            } => write!(
                formatter,
                "replay request-index {request_index}: recording contains route outside surface.json: {method} {path}"
            ),
            Self::Mismatch {
                request_index,
                field,
                expected,
                actual,
            } => write!(
                formatter,
                "replay request-index {request_index}: first differing {field}; expected {expected}, got {actual}"
            ),
            Self::UnconsumedRequests {
                request_index,
                remaining,
                expected_operation,
            } => write!(
                formatter,
                "replay request-index {request_index}: {remaining} recorded request(s) remain; next operation {}",
                expected_operation.as_deref().unwrap_or("<unknown>")
            ),
        }
    }
}

impl std::error::Error for ReplayError {}

/// The upstream HTTP seam used by [`RecordingTransport`].
pub trait HttpTransport {
    type Error;

    /// Sends one request to the real server.
    ///
    /// # Errors
    ///
    /// Returns the transport-specific error if the upstream cannot serve the request.
    fn execute(&mut self, request: &HttpRequest) -> Result<HttpResponse, Self::Error>;
}

/// A forwarding transport that records each successful upstream exchange.
///
/// This is the recorder's boundary with a real local `OpenCode` server: it forwards the original
/// request unchanged, returns the original response unchanged, and stores only a sanitized copy.
#[derive(Debug)]
pub struct RecordingTransport<T> {
    upstream: T,
    recorder: Recorder,
}

impl<T> RecordingTransport<T> {
    #[must_use]
    pub fn new(upstream: T) -> Self {
        Self {
            upstream,
            recorder: Recorder::new(),
        }
    }

    #[must_use]
    pub fn into_recording(self) -> Recording {
        self.recorder.finish()
    }

    #[must_use]
    pub fn into_upstream(self) -> T {
        self.upstream
    }
}

impl<T> HttpTransport for RecordingTransport<T>
where
    T: HttpTransport,
{
    type Error = T::Error;

    fn execute(&mut self, request: &HttpRequest) -> Result<HttpResponse, Self::Error> {
        let response = self.upstream.execute(request)?;
        self.recorder.record(request.clone(), response.clone());
        Ok(response)
    }
}

/// A loopback HTTP replay server backed by [`ReplayServer`].
///
/// It uses HTTP chunked transfer encoding for every response. Each recorded `body_chunks` item
/// becomes exactly one transfer chunk, preserving intentionally split SSE frames for a real HTTP
/// client as well as for the in-memory replay seam.
#[derive(Debug)]
pub struct ReplayHttpServer {
    listener: std::net::TcpListener,
    replay: ReplayServer,
}

impl ReplayHttpServer {
    /// Binds a local HTTP listener.
    ///
    /// # Errors
    ///
    /// Returns [`ReplayHttpServerError`] when binding fails.
    pub fn bind(
        address: impl std::net::ToSocketAddrs,
        replay: ReplayServer,
    ) -> Result<Self, ReplayHttpServerError> {
        Ok(Self {
            listener: std::net::TcpListener::bind(address).map_err(ReplayHttpServerError::Io)?,
            replay,
        })
    }

    /// Returns the address selected by [`Self::bind`].
    ///
    /// # Errors
    ///
    /// Returns [`ReplayHttpServerError`] if the listener cannot report its address.
    pub fn local_addr(&self) -> Result<std::net::SocketAddr, ReplayHttpServerError> {
        self.listener
            .local_addr()
            .map_err(ReplayHttpServerError::Io)
    }

    /// Serves exactly one connection, then returns.
    ///
    /// A request outside `surface.json` is rejected as a 404 response whose body contains the
    /// replay diagnostic; other request mismatches use 409. I/O and malformed HTTP are returned
    /// to the caller as errors.
    ///
    /// # Errors
    ///
    /// Returns [`ReplayHttpServerError`] for listener, socket, and request-parsing failures.
    pub fn serve_one(&mut self) -> Result<(), ReplayHttpServerError> {
        let (mut stream, _) = self.listener.accept().map_err(ReplayHttpServerError::Io)?;
        let request = read_http_request(&mut stream)?;
        match self.replay.handle(&request) {
            Ok(response) => write_http_response(&mut stream, &response),
            Err(error) => {
                let status = if matches!(error, ReplayError::UndocumentedRoute { .. }) {
                    404
                } else {
                    409
                };
                write_http_error(&mut stream, status, &error.to_string())
            }
        }
    }
}

/// An error produced by the loopback HTTP replay server.
#[derive(Debug)]
pub enum ReplayHttpServerError {
    Io(std::io::Error),
    InvalidRequest(String),
}

impl fmt::Display for ReplayHttpServerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "replay HTTP I/O error: {error}"),
            Self::InvalidRequest(message) => {
                write!(formatter, "invalid replay HTTP request: {message}")
            }
        }
    }
}

impl std::error::Error for ReplayHttpServerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::InvalidRequest(_) => None,
        }
    }
}

fn read_http_request(
    stream: &mut std::net::TcpStream,
) -> Result<HttpRequest, ReplayHttpServerError> {
    use std::io::{BufRead, Read};

    let mut reader =
        std::io::BufReader::new(stream.try_clone().map_err(ReplayHttpServerError::Io)?);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(ReplayHttpServerError::Io)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or_else(|| {
        ReplayHttpServerError::InvalidRequest("request line has no method".to_owned())
    })?;
    let path = parts.next().ok_or_else(|| {
        ReplayHttpServerError::InvalidRequest("request line has no path".to_owned())
    })?;
    if parts.next().is_none() {
        return Err(ReplayHttpServerError::InvalidRequest(
            "request line has no HTTP version".to_owned(),
        ));
    }

    let mut headers = BTreeMap::new();
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(ReplayHttpServerError::Io)?;
        if line.is_empty() {
            return Err(ReplayHttpServerError::InvalidRequest(
                "headers ended before a blank line".to_owned(),
            ));
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| ReplayHttpServerError::InvalidRequest("malformed header".to_owned()))?;
        headers.insert(
            name.trim().to_owned(),
            value.trim_end_matches(['\r', '\n']).trim().to_owned(),
        );
    }
    let content_length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| {
            value.parse::<usize>().map_err(|_| {
                ReplayHttpServerError::InvalidRequest("invalid Content-Length".to_owned())
            })
        })
        .transpose()?
        .unwrap_or(0);
    let mut body = vec![0; content_length];
    reader
        .read_exact(&mut body)
        .map_err(ReplayHttpServerError::Io)?;
    Ok(HttpRequest {
        method: method.to_owned(),
        path: path.to_owned(),
        headers,
        body,
    })
}

fn write_http_response(
    stream: &mut std::net::TcpStream,
    response: &HttpResponse,
) -> Result<(), ReplayHttpServerError> {
    use std::io::Write;

    write!(
        stream,
        "HTTP/1.1 {} {}\r\n",
        response.status,
        status_reason(response.status)
    )
    .map_err(ReplayHttpServerError::Io)?;
    for (name, value) in &response.headers {
        if name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("transfer-encoding")
            || name.eq_ignore_ascii_case("connection")
        {
            continue;
        }
        write!(stream, "{name}: {value}\r\n").map_err(ReplayHttpServerError::Io)?;
    }
    stream
        .write_all(b"transfer-encoding: chunked\r\nconnection: close\r\n\r\n")
        .map_err(ReplayHttpServerError::Io)?;
    for chunk in &response.body_chunks {
        write!(stream, "{:x}\r\n", chunk.len()).map_err(ReplayHttpServerError::Io)?;
        stream.write_all(chunk).map_err(ReplayHttpServerError::Io)?;
        stream
            .write_all(b"\r\n")
            .map_err(ReplayHttpServerError::Io)?;
    }
    stream
        .write_all(b"0\r\n\r\n")
        .map_err(ReplayHttpServerError::Io)
}

fn write_http_error(
    stream: &mut std::net::TcpStream,
    status: u16,
    message: &str,
) -> Result<(), ReplayHttpServerError> {
    use std::io::Write;

    write!(
        stream,
        "HTTP/1.1 {status} {}\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{message}",
        status_reason(status),
        message.len()
    )
    .map_err(ReplayHttpServerError::Io)
}

const fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Replay Response",
    }
}

fn path_matches(pattern: &str, path: &str) -> bool {
    let path = path.split_once('?').map_or(path, |(path, _)| path);
    let pattern_parts: Vec<_> = pattern.split('/').collect();
    let path_parts: Vec<_> = path.split('/').collect();
    pattern_parts.len() == path_parts.len()
        && pattern_parts
            .iter()
            .zip(path_parts)
            .all(|(pattern, value)| {
                (pattern.starts_with('{') && pattern.ends_with('}'))
                    || value.starts_with('$')
                    || *pattern == value
            })
}

fn sanitize_for_matching(request: &HttpRequest) -> HttpRequest {
    let mut sanitizer = Sanitizer::default();
    sanitizer.request(request.clone())
}

fn canonical_json(body: &[u8]) -> Vec<u8> {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return body.to_vec();
    };
    let value = canonical_value(value);
    serde_json::to_vec(&value).expect("JSON values serialize")
}

fn canonical_value(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(canonical_value).collect()),
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, canonical_value(value)))
                .collect(),
        ),
        value => value,
    }
}

fn printable_body(body: &[u8]) -> String {
    String::from_utf8_lossy(body).into_owned()
}

#[derive(Debug, Default)]
struct Sanitizer {
    replacements: HashMap<String, &'static str>,
}

impl Sanitizer {
    fn request(&mut self, request: HttpRequest) -> HttpRequest {
        let path = self.path(&request.path);
        let headers = self.headers(request.headers);
        let body = self.body(request.body);
        HttpRequest {
            method: request.method,
            path,
            headers,
            body,
        }
    }

    fn response(&mut self, response: HttpResponse) -> HttpResponse {
        let headers = self.headers(response.headers);
        let body_chunks = response
            .body_chunks
            .into_iter()
            .map(|chunk| self.body(chunk))
            .collect();
        HttpResponse {
            status: response.status,
            headers,
            body_chunks,
        }
    }

    fn headers(&mut self, headers: BTreeMap<String, String>) -> BTreeMap<String, String> {
        headers
            .into_iter()
            .map(|(name, value)| {
                let value = if is_auth_header(&name) {
                    "$REDACTED".to_owned()
                } else {
                    self.value_for_key(&name, &value)
                };
                (name, value)
            })
            .collect()
    }

    fn path(&mut self, path: &str) -> String {
        let mut previous = "";
        let mut sanitized = Vec::new();
        for part in path.split('/') {
            let replacement = match previous {
                "session" => Some("$SESSION"),
                "message" => Some("$MSG"),
                _ => None,
            };
            let value = if let Some(kind) = replacement {
                self.replacements.insert(part.to_owned(), kind);
                kind.to_owned()
            } else {
                self.replace_known(part)
            };
            previous = part;
            sanitized.push(value);
        }
        sanitized.join("/")
    }

    fn body(&mut self, body: Vec<u8>) -> Vec<u8> {
        let Ok(text) = std::str::from_utf8(&body) else {
            return body;
        };
        let Ok(mut json) = serde_json::from_str::<Value>(text) else {
            return self.replace_known(text).into_bytes();
        };
        self.json_value(None, &mut json);
        serde_json::to_vec(&json).expect("JSON values serialize")
    }

    fn json_value(&mut self, key: Option<&str>, value: &mut Value) {
        match value {
            Value::Object(object) => {
                for (child_key, child_value) in object {
                    self.json_value(Some(child_key), child_value);
                }
            }
            Value::Array(values) => {
                for item in values {
                    self.json_value(key, item);
                }
            }
            Value::String(text) => {
                *text = self.value_for_key(key.unwrap_or_default(), text);
            }
            Value::Number(number) => {
                if let Some(kind) = placeholder_for_key(key.unwrap_or_default()) {
                    self.replacements.insert(number.to_string(), kind);
                    *value = Value::String(kind.to_owned());
                }
            }
            _ => {}
        }
    }

    fn value_for_key(&mut self, key: &str, value: &str) -> String {
        if let Some(kind) = placeholder_for_key(key) {
            self.replacements.insert(value.to_owned(), kind);
            return kind.to_owned();
        }
        self.replace_known(value)
    }

    fn replace_known(&self, value: &str) -> String {
        if looks_like_filesystem_path(value) {
            return "$PATH".to_owned();
        }
        let mut sanitized = value.to_owned();
        for (original, replacement) in &self.replacements {
            sanitized = sanitized.replace(original, replacement);
        }
        sanitized
    }
}

fn looks_like_filesystem_path(value: &str) -> bool {
    [
        "/home/",
        "/Users/",
        "/tmp/",
        "/var/",
        "/private/",
        "/opt/",
        "/etc/",
        "/mnt/",
    ]
    .iter()
    .any(|marker| value.contains(marker))
        || value.contains("C:\\Users\\")
}

fn placeholder_for_key(key: &str) -> Option<&'static str> {
    let key = key.to_ascii_lowercase();
    if key.contains("prompt") || key == "text" || key == "content" || key == "instructions" {
        Some("$REDACTED")
    } else if key.contains("session") && key.contains("id") {
        Some("$SESSION")
    } else if key.contains("message") && key.contains("id") {
        Some("$MSG")
    } else if key.contains("turn") || key.contains("request") {
        Some("$TURN")
    } else if key == "id" || key.ends_with("_id") || key.ends_with("id") {
        Some("$ID")
    } else if key.contains("timestamp")
        || key.ends_with("_at")
        || key.ends_with("at")
        || key == "time"
    {
        Some("$TIMESTAMP")
    } else if key.contains("path") || key.contains("file") || key.contains("directory") {
        Some("$PATH")
    } else {
        None
    }
}

fn is_auth_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "proxy-authorization" | "cookie" | "set-cookie" | "x-api-key"
    ) || name.to_ascii_lowercase().contains("token")
        || name.to_ascii_lowercase().contains("secret")
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpStream,
        thread,
    };

    use super::{HttpRequest, HttpResponse, Recorder, ReplayHttpServer, ReplayServer, Surface};

    fn no_headers() -> [(&'static str, &'static str); 0] {
        []
    }

    #[test]
    fn recorder_redacts_auth_paths_and_prompt_content() {
        let mut recorder = Recorder::new();
        recorder.record(
            HttpRequest::new(
                "POST",
                "/session/session-a/prompt_async",
                [("Authorization", "Bearer very-secret")],
                br#"{"prompt":"read /home/alice/private.txt","sessionID":"session-a","messageID":"msg-3","createdAt":1700000000,"location":"stored at /var/private/notes"}"#,
            ),
            HttpResponse::new(
                204,
                [("x-request-id", "turn-9")],
                [b"".to_vec()],
            ),
        );

        let recording = recorder.finish();
        let rendered = serde_json::to_string(&recording).expect("recording serializes");
        let body = std::str::from_utf8(&recording.exchanges[0].request.body).expect("JSON body");

        for forbidden in [
            "very-secret",
            "/home/alice/private.txt",
            "/var/private/notes",
            "read /home/alice/private.txt",
            "session-a",
            "msg-3",
            "turn-9",
            "1700000000",
        ] {
            assert!(
                !rendered.contains(forbidden),
                "recording leaked {forbidden}"
            );
            assert!(
                !body.contains(forbidden),
                "sanitized body leaked {forbidden}"
            );
        }
        assert!(rendered.contains("$SESSION"));
        assert!(rendered.contains("$REDACTED"));
        assert!(rendered.contains("$TURN"));
        assert!(body.contains("$MSG"));
        assert!(body.contains("$TIMESTAMP"));
    }

    #[test]
    fn replay_matches_operation_and_canonical_json_then_preserves_sse_chunks() {
        let mut recorder = Recorder::new();
        recorder.record(
            HttpRequest::new(
                "POST",
                "/session/captured-session/prompt_async",
                no_headers(),
                br#"{"prompt":"captured prompt","unknown":{"b":2,"a":1}}"#,
            ),
            HttpResponse::new(
                200,
                [("content-type", "text/event-stream")],
                [b"event: message\nda".to_vec(), b"ta: ready\n\n".to_vec()],
            ),
        );
        let mut replay = ReplayServer::new(
            Surface::pinned().expect("pinned surface"),
            recorder.finish(),
        );

        let response = replay
            .handle(&HttpRequest::new(
                "POST",
                "/session/live-session/prompt_async",
                no_headers(),
                br#"{"unknown":{"a":1,"b":2},"prompt":"different secret"}"#,
            ))
            .expect("same operation and canonical body should replay");

        assert_eq!(response.status, 200);
        assert_eq!(
            response.body_chunks,
            vec![b"event: message\nda".to_vec(), b"ta: ready\n\n".to_vec()]
        );
        replay.assert_finished().expect("all requests used");
    }

    #[test]
    fn replay_rejects_undocumented_routes_and_unknown_json_fields() {
        let mut recorder = Recorder::new();
        recorder.record(
            HttpRequest::new("POST", "/session", no_headers(), br#"{"prompt":"hi"}"#),
            HttpResponse::new(204, no_headers(), [Vec::new()]),
        );
        let mut replay = ReplayServer::new(
            Surface::pinned().expect("pinned surface"),
            recorder.finish(),
        );

        let error = replay
            .handle(&HttpRequest::new(
                "GET",
                "/not-in-surface",
                no_headers(),
                [],
            ))
            .expect_err("undocumented route must fail");
        assert!(error.to_string().contains("request-index 0"));
        assert!(error.to_string().contains("/not-in-surface"));

        let error = replay
            .handle(&HttpRequest::new(
                "POST",
                "/session",
                no_headers(),
                br#"{"prompt":"hi","unknown":true}"#,
            ))
            .expect_err("unknown fields must not be silently dropped");
        assert!(error.to_string().contains("request-index 0"));
        assert!(error.to_string().contains("body"));
    }

    #[test]
    fn loopback_replay_server_preserves_recorded_sse_chunk_boundaries() {
        let mut recorder = Recorder::new();
        recorder.record(
            HttpRequest::new("POST", "/session", no_headers(), br#"{"prompt":"hi"}"#),
            HttpResponse::new(
                200,
                [("content-type", "text/event-stream")],
                [b"event: update\nda".to_vec(), b"ta: ready\n\n".to_vec()],
            ),
        );
        let replay = ReplayServer::pinned(recorder.finish()).expect("pinned surface");
        let mut server = ReplayHttpServer::bind("127.0.0.1:0", replay).expect("bind loopback");
        let address = server.local_addr().expect("local address");
        let worker = thread::spawn(move || server.serve_one().expect("serve one request"));

        let mut client = TcpStream::connect(address).expect("connect");
        client
            .write_all(
                b"POST /session HTTP/1.1\r\nHost: localhost\r\nContent-Length: 15\r\n\r\n{\"prompt\":\"hi\"}",
            )
            .expect("write request");
        let mut response = Vec::new();
        client.read_to_end(&mut response).expect("read response");
        worker.join().expect("server thread");

        assert!(response.starts_with(b"HTTP/1.1 200"));
        assert!(
            response
                .windows(b"10\r\nevent: update\nda\r\n".len())
                .any(|window| window == b"10\r\nevent: update\nda\r\n")
        );
        assert!(
            response
                .windows(b"b\r\nta: ready\n\n\r\n".len())
                .any(|window| window == b"b\r\nta: ready\n\n\r\n")
        );
    }
}
