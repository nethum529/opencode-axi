//! Scripted socket cuts layered onto the pinned fake-server request shapes.

use std::{collections::VecDeque, io::Write, time::Duration};

use crate::{
    HttpRequest, HttpResponse, ReplayHttpServerError, read_http_request, status_reason,
    write_http_response,
};

/// One connection-level action consumed by [`FailureHttpServer`].
#[derive(Clone, Debug)]
pub enum FailureAction {
    /// Accept and immediately close a connection without reading request bytes.
    DropBeforeRequest,
    /// Read and retain the full request, then close without writing a response.
    DropAfterRequest,
    /// Return a complete chunked HTTP response.
    Respond(HttpResponse),
    /// Return headers and the configured chunks, then close before the terminal chunk.
    RespondThenDrop(HttpResponse),
    /// Return valid configured chunks followed by an invalid chunk-size line.
    RespondThenGarble(HttpResponse),
    /// Return a session-history user message derived from the most recently
    /// captured asynchronous prompt. This preserves caller-minted message IDs.
    EchoPrompt { session_id: String },
}

/// A bounded fake HTTP server that consumes an explicit failure script.
#[derive(Debug)]
pub struct FailureHttpServer {
    listener: std::net::TcpListener,
    actions: VecDeque<FailureAction>,
    deadline: Duration,
}

impl FailureHttpServer {
    /// Binds a loopback listener with the supplied connection actions.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the listener cannot bind.
    pub fn bind(
        address: impl std::net::ToSocketAddrs,
        actions: impl IntoIterator<Item = FailureAction>,
    ) -> Result<Self, ReplayHttpServerError> {
        let listener = std::net::TcpListener::bind(address).map_err(ReplayHttpServerError::Io)?;
        listener
            .set_nonblocking(true)
            .map_err(ReplayHttpServerError::Io)?;
        Ok(Self {
            listener,
            actions: actions.into_iter().collect(),
            deadline: Duration::from_secs(10),
        })
    }

    /// Returns the selected listener address.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the address cannot be queried.
    pub fn local_addr(&self) -> Result<std::net::SocketAddr, ReplayHttpServerError> {
        self.listener
            .local_addr()
            .map_err(ReplayHttpServerError::Io)
    }

    /// Serves the complete script and returns every fully transmitted request.
    ///
    /// Empty TCP readiness probes do not consume an action. The bounded accept
    /// deadline turns missing requests into a deterministic fixture failure.
    ///
    /// # Errors
    ///
    /// Returns an I/O or malformed-request error, including an expired script.
    pub fn serve(mut self) -> Result<Vec<HttpRequest>, ReplayHttpServerError> {
        let deadline = std::time::Instant::now() + self.deadline;
        let mut requests = Vec::new();
        while !self.actions.is_empty() {
            let (mut stream, _) = match self.listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return Err(ReplayHttpServerError::InvalidRequest(format!(
                            "failure script timed out with {} action(s) remaining",
                            self.actions.len()
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                Err(error) => return Err(ReplayHttpServerError::Io(error)),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .map_err(ReplayHttpServerError::Io)?;
            if matches!(self.actions.front(), Some(FailureAction::DropBeforeRequest)) {
                self.actions.pop_front();
                continue;
            }
            let request = match read_http_request(&mut stream) {
                Ok(request) => request,
                Err(ReplayHttpServerError::InvalidRequest(message))
                    if message == "request line has no method" =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };
            requests.push(request);
            match self.actions.pop_front().expect("the script is non-empty") {
                FailureAction::DropBeforeRequest => unreachable!(),
                FailureAction::DropAfterRequest => {}
                FailureAction::Respond(response) => {
                    if let Err(error) = write_http_response(&mut stream, &response)
                        && !is_peer_disconnect(&error)
                    {
                        return Err(error);
                    }
                }
                FailureAction::RespondThenDrop(response) => {
                    if let Err(error) = write_incomplete_response(&mut stream, &response)
                        && !is_peer_disconnect(&error)
                    {
                        return Err(error);
                    }
                }
                FailureAction::RespondThenGarble(response) => {
                    if let Err(error) = write_garbled_response(&mut stream, &response)
                        && !is_peer_disconnect(&error)
                    {
                        return Err(error);
                    }
                }
                FailureAction::EchoPrompt { session_id } => {
                    let response = prompt_echo_response(&requests, &session_id)?;
                    if let Err(error) = write_http_response(&mut stream, &response)
                        && !is_peer_disconnect(&error)
                    {
                        return Err(error);
                    }
                }
            }
        }
        Ok(requests)
    }
}

fn prompt_echo_response(
    requests: &[HttpRequest],
    session_id: &str,
) -> Result<HttpResponse, ReplayHttpServerError> {
    let prompt = requests
        .iter()
        .rev()
        .find(|request| request.path.ends_with("/prompt_async"))
        .ok_or_else(|| {
            ReplayHttpServerError::InvalidRequest(
                "prompt echo requested before a prompt was captured".to_owned(),
            )
        })?;
    let prompt: serde_json::Value = serde_json::from_slice(&prompt.body).map_err(|error| {
        ReplayHttpServerError::InvalidRequest(format!("captured prompt body is not JSON: {error}"))
    })?;
    let message_id = prompt["messageID"].as_str().ok_or_else(|| {
        ReplayHttpServerError::InvalidRequest(
            "captured prompt body has no string messageID".to_owned(),
        )
    })?;
    let parts = prompt["parts"].clone();
    let body = serde_json::json!([{
        "info": {
            "id": message_id,
            "sessionID": session_id,
            "role": "user"
        },
        "parts": parts
    }])
    .to_string();
    Ok(HttpResponse::new(
        200,
        [("content-type", "application/json")],
        [body.into_bytes()],
    ))
}

fn is_peer_disconnect(error: &ReplayHttpServerError) -> bool {
    matches!(
        error,
        ReplayHttpServerError::Io(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
            )
    )
}

fn write_incomplete_response(
    stream: &mut std::net::TcpStream,
    response: &HttpResponse,
) -> Result<(), ReplayHttpServerError> {
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
        stream
            .write_all(chunk)
            .and_then(|()| stream.write_all(b"\r\n"))
            .map_err(ReplayHttpServerError::Io)?;
    }
    stream.flush().map_err(ReplayHttpServerError::Io)
}

fn write_garbled_response(
    stream: &mut std::net::TcpStream,
    response: &HttpResponse,
) -> Result<(), ReplayHttpServerError> {
    write_incomplete_response(stream, response)?;
    stream
        .write_all(b"not-a-chunk\r\n")
        .and_then(|()| stream.flush())
        .map_err(ReplayHttpServerError::Io)
}
