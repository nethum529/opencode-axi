//! Typed client for herdr's newline-delimited Unix-socket API.

use std::{
    env,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use socket2::{Domain, SockAddr, Socket, Type};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const DISCOVERY_CONNECT_TIMEOUT: Duration = Duration::from_millis(100);
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn socket_accepts_connections(path: &Path) -> bool {
    let Ok(address) = SockAddr::unix(path) else {
        return false;
    };
    let Ok(socket) = Socket::new(Domain::UNIX, Type::STREAM, None) else {
        return false;
    };
    socket
        .connect_timeout(&address, DISCOVERY_CONNECT_TIMEOUT)
        .is_ok()
}

/// A herdr workspace identifier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceId(String);

impl WorkspaceId {
    /// Returns the protocol identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A herdr tab identifier together with its shell pane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TabId {
    id: String,
    root_pane_id: String,
}

impl TabId {
    /// Returns the protocol identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.id
    }
}

/// A herdr agent terminal identifier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentId(String);

impl AgentId {
    /// Returns the terminal identifier assigned by herdr.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Typed failures from herdr discovery and socket requests.
#[derive(Debug, Error)]
pub enum HerdrError {
    #[error("herdr request timed out after {timeout_ms} ms")]
    Timeout { timeout_ms: u128 },
    #[error("herdr socket transport failed: {0}")]
    Transport(#[from] std::io::Error),
    #[error("malformed herdr response envelope: {message}")]
    MalformedResponse { message: String },
    #[error("herdr returned {code}: {message}")]
    Server { code: String, message: String },
    #[error("herdr agent argv must start with a non-empty executable name")]
    InvalidAgentArgv,
}

/// A short-lived client for herdr's local Unix socket.
#[derive(Clone, Debug)]
pub struct HerdrClient {
    socket_path: PathBuf,
    timeout: Duration,
}

impl HerdrClient {
    /// Default deadline for every individual socket request.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(750);

    /// Discovers the socket from the process environment and home directory.
    ///
    /// `HERDR_SOCKET_PATH` wins, followed by `XDG_CONFIG_HOME`, then
    /// `$HOME/.config/herdr/herdr.sock`. A candidate is returned only when it
    /// accepts a bounded connection probe, so discovery never starts herdr.
    #[must_use]
    pub fn discover() -> Option<Self> {
        let home = env::var_os("HOME").map(PathBuf::from)?;
        Self::discover_from(&home, None, Self::DEFAULT_TIMEOUT)
    }

    /// Discovers a configured or conventional socket for an explicit home.
    ///
    /// The configured path has highest priority. This form keeps `oca` tests
    /// and alternate state roots independent of the ambient home directory.
    #[must_use]
    pub fn discover_from(
        home: &Path,
        configured_socket: Option<&Path>,
        timeout: Duration,
    ) -> Option<Self> {
        let socket_path = configured_socket
            .filter(|path| !path.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .or_else(|| env::var_os("HERDR_SOCKET_PATH").map(PathBuf::from))
            .or_else(|| {
                env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .map(|root| root.join("herdr/herdr.sock"))
            })
            .unwrap_or_else(|| home.join(".config/herdr/herdr.sock"));
        socket_accepts_connections(&socket_path).then_some(Self {
            socket_path,
            timeout,
        })
    }

    /// Builds a client for a known socket, primarily for injected adapters.
    #[must_use]
    pub fn new(socket_path: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            socket_path: socket_path.into(),
            timeout,
        }
    }

    /// Creates or reuses the workspace with `name`.
    ///
    /// # Errors
    ///
    /// Returns a typed transport, timeout, server, or envelope error.
    pub async fn workspace(&self, name: &str) -> Result<WorkspaceId, HerdrError> {
        let listed: WorkspaceListResult = self.call("workspace.list", json!({})).await?;
        if let Some(workspace) = listed
            .workspaces
            .into_iter()
            .find(|workspace| workspace.label == name)
        {
            return Ok(WorkspaceId(workspace.workspace_id));
        }

        let created: WorkspaceCreatedResult = self
            .call("workspace.create", json!({ "label": name, "focus": false }))
            .await?;
        Ok(WorkspaceId(created.workspace.workspace_id))
    }

    /// Creates a tab in `workspace`, rooted at `cwd`.
    ///
    /// # Errors
    ///
    /// Returns a typed transport, timeout, server, or envelope error.
    pub async fn tab(
        &self,
        workspace: &WorkspaceId,
        title: &str,
        no_focus: bool,
        cwd: &Path,
    ) -> Result<TabId, HerdrError> {
        let created: TabCreatedResult = self
            .call(
                "tab.create",
                json!({
                    "workspace_id": workspace.as_str(),
                    "label": title,
                    "cwd": cwd,
                    "focus": !no_focus,
                }),
            )
            .await?;
        Ok(TabId {
            id: created.tab.tab_id,
            root_pane_id: created.root_pane.pane_id,
        })
    }

    /// Starts the interactive agent described by `argv` in a tab's root pane.
    ///
    /// Herdr owns the canonical executable for the named agent kind. Therefore
    /// the first argv item supplies both `name` and `kind`, while the remaining
    /// items are passed as agent arguments.
    ///
    /// # Errors
    ///
    /// Returns [`HerdrError::InvalidAgentArgv`] for an empty command, or a typed
    /// transport, timeout, server, or envelope error.
    pub async fn agent_start(&self, tab: &TabId, argv: Vec<String>) -> Result<AgentId, HerdrError> {
        let Some((program, args)) = argv.split_first() else {
            return Err(HerdrError::InvalidAgentArgv);
        };
        if program.is_empty() {
            return Err(HerdrError::InvalidAgentArgv);
        }
        let started: AgentStartedResult = self
            .call(
                "agent.start",
                json!({
                    "name": program,
                    "kind": program,
                    "pane_id": tab.root_pane_id,
                    "args": args,
                }),
            )
            .await?;
        Ok(AgentId(started.agent.terminal_id))
    }

    /// Closes one herdr tab.
    ///
    /// # Errors
    ///
    /// Returns a typed transport, timeout, server, or envelope error.
    pub async fn close_tab(&self, tab: &TabId) -> Result<(), HerdrError> {
        let _: OkResult = self
            .call("tab.close", json!({ "tab_id": tab.as_str() }))
            .await?;
        Ok(())
    }

    async fn call<T>(&self, method: &str, params: Value) -> Result<T, HerdrError>
    where
        T: DeserializeOwned + ExpectedResult,
    {
        let request_id = format!(
            "oca:{}:{}",
            std::process::id(),
            NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
        );
        let request = WireRequest {
            id: &request_id,
            method,
            params,
        };
        let timeout = self.timeout;
        match tokio::time::timeout(timeout, self.exchange(&request_id, &request)).await {
            Ok(result) => result,
            Err(_) => Err(HerdrError::Timeout {
                timeout_ms: timeout.as_millis(),
            }),
        }
    }

    async fn exchange<T>(
        &self,
        request_id: &str,
        request: &WireRequest<'_>,
    ) -> Result<T, HerdrError>
    where
        T: DeserializeOwned + ExpectedResult,
    {
        let mut stream = UnixStream::connect(&self.socket_path).await?;
        let mut encoded =
            serde_json::to_vec(request).map_err(|error| HerdrError::MalformedResponse {
                message: format!("request could not be encoded: {error}"),
            })?;
        encoded.push(b'\n');
        stream.write_all(&encoded).await?;
        stream.flush().await?;

        let mut response = Vec::new();
        BufReader::new(stream)
            .read_until(b'\n', &mut response)
            .await?;
        if response.is_empty() {
            return Err(HerdrError::MalformedResponse {
                message: "socket closed without a response".to_owned(),
            });
        }
        if response.len() > MAX_RESPONSE_BYTES {
            return Err(HerdrError::MalformedResponse {
                message: "response exceeded one MiB".to_owned(),
            });
        }
        let envelope: WireResponse =
            serde_json::from_slice(&response).map_err(|error| HerdrError::MalformedResponse {
                message: error.to_string(),
            })?;
        if envelope.id != request_id {
            return Err(HerdrError::MalformedResponse {
                message: format!(
                    "response id `{}` did not match request id `{request_id}`",
                    envelope.id
                ),
            });
        }
        match (envelope.result, envelope.error) {
            (Some(result), None) => {
                let actual = result.get("type").and_then(Value::as_str).ok_or_else(|| {
                    HerdrError::MalformedResponse {
                        message: "success result has no string `type`".to_owned(),
                    }
                })?;
                if actual != T::RESULT_TYPE {
                    return Err(HerdrError::MalformedResponse {
                        message: format!("expected `{}`, received `{actual}`", T::RESULT_TYPE),
                    });
                }
                serde_json::from_value(result).map_err(|error| HerdrError::MalformedResponse {
                    message: error.to_string(),
                })
            }
            (None, Some(error)) => Err(HerdrError::Server {
                code: error.code,
                message: error.message,
            }),
            _ => Err(HerdrError::MalformedResponse {
                message: "response must contain exactly one of `result` or `error`".to_owned(),
            }),
        }
    }
}

#[derive(Serialize)]
struct WireRequest<'a> {
    id: &'a str,
    method: &'a str,
    params: Value,
}

#[derive(Deserialize)]
struct WireResponse {
    id: String,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<ErrorBody>,
}

#[derive(Deserialize)]
struct ErrorBody {
    code: String,
    message: String,
}

trait ExpectedResult {
    const RESULT_TYPE: &'static str;
}

#[derive(Deserialize)]
struct WorkspaceListResult {
    workspaces: Vec<WorkspaceSummary>,
}

impl ExpectedResult for WorkspaceListResult {
    const RESULT_TYPE: &'static str = "workspace_list";
}

#[derive(Deserialize)]
struct WorkspaceCreatedResult {
    workspace: WorkspaceSummary,
}

impl ExpectedResult for WorkspaceCreatedResult {
    const RESULT_TYPE: &'static str = "workspace_created";
}

#[derive(Deserialize)]
struct WorkspaceSummary {
    workspace_id: String,
    label: String,
}

#[derive(Deserialize)]
struct TabCreatedResult {
    tab: TabSummary,
    root_pane: PaneSummary,
}

impl ExpectedResult for TabCreatedResult {
    const RESULT_TYPE: &'static str = "tab_created";
}

#[derive(Deserialize)]
struct TabSummary {
    tab_id: String,
}

#[derive(Deserialize)]
struct PaneSummary {
    pane_id: String,
}

#[derive(Deserialize)]
struct AgentStartedResult {
    agent: AgentSummary,
}

impl ExpectedResult for AgentStartedResult {
    const RESULT_TYPE: &'static str = "agent_started";
}

#[derive(Deserialize)]
struct AgentSummary {
    terminal_id: String,
}

#[derive(Deserialize)]
struct OkResult {}

impl ExpectedResult for OkResult {
    const RESULT_TYPE: &'static str = "ok";
}
