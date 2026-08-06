//! Stable CLI errors and safe session-creation recovery over the OpenCode facade.

use std::future::Future;

use oca_core::{ErrorCode, OcaError};
use oca_opencode::{
    CreateSessionRequest, OpenCodeClient, OpenCodeError, Session, TransmissionStage,
};
use oca_server::{ConnectError, OpenCodeRequest, RequestFailure};

pub(crate) struct CreateSessionOperation {
    request: CreateSessionRequest,
}

impl CreateSessionOperation {
    pub(crate) const fn new(request: CreateSessionRequest) -> Self {
        Self { request }
    }
}

impl OpenCodeRequest for CreateSessionOperation {
    type Output = Session;
    type Error = CreateSessionError;

    fn send(
        &mut self,
        client: &OpenCodeClient,
    ) -> impl Future<Output = Result<Self::Output, RequestFailure<Self::Error>>> + Send {
        let request = self.request.clone();
        async move {
            let agent = request
                .agent
                .as_deref()
                .ok_or_else(|| RequestFailure::Application(CreateSessionError::MissingAgentName))?;
            let agents = client
                .agents(request.directory.as_deref(), request.workspace.as_deref())
                .await
                .map_err(create_session_request_failure)?;
            if !agents.iter().any(|available| available.name == agent) {
                return Err(RequestFailure::Application(
                    CreateSessionError::UnregisteredAgent(agent.to_owned()),
                ));
            }

            client.create_session(request).await.map_err(|error| {
                // Even a lost create-session response precedes prompt admission. A
                // duplicate empty session is harmless, so T14 may recover once.
                if matches!(error, OpenCodeError::Transport { .. }) {
                    RequestFailure::Connection(CreateSessionError::OpenCode(error))
                } else {
                    RequestFailure::Application(CreateSessionError::OpenCode(error))
                }
            })
        }
    }
}

#[derive(Debug)]
pub(crate) enum CreateSessionError {
    OpenCode(OpenCodeError),
    MissingAgentName,
    UnregisteredAgent(String),
}

impl std::fmt::Display for CreateSessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OpenCode(error) => error.fmt(formatter),
            Self::MissingAgentName => formatter.write_str("dispatch has no OpenCode agent name"),
            Self::UnregisteredAgent(agent) => {
                write!(formatter, "OpenCode agent `{agent}` is not registered")
            }
        }
    }
}

impl std::error::Error for CreateSessionError {}

fn create_session_request_failure(error: OpenCodeError) -> RequestFailure<CreateSessionError> {
    if matches!(error, OpenCodeError::Transport { .. }) {
        RequestFailure::Connection(CreateSessionError::OpenCode(error))
    } else {
        RequestFailure::Application(CreateSessionError::OpenCode(error))
    }
}

pub(crate) fn create_session_error(error: ConnectError<CreateSessionError>) -> OcaError {
    match error {
        ConnectError::Request(CreateSessionError::UnregisteredAgent(agent))
        | ConnectError::RequestMayHaveBeenTransmitted(CreateSessionError::UnregisteredAgent(
            agent,
        )) => OcaError::new(ErrorCode::ProtocolMismatch)
            .with_error(format!(
                "OpenCode agent `{agent}` is not registered for this dispatch directory"
            ))
            .with_help(format!(
                "Register agent `{agent}` in OpenCode configuration and retry"
            )),
        ConnectError::Request(CreateSessionError::MissingAgentName)
        | ConnectError::RequestMayHaveBeenTransmitted(CreateSessionError::MissingAgentName) => {
            OcaError::new(ErrorCode::ProtocolMismatch)
                .with_error("dispatch has no OpenCode agent name")
        }
        ConnectError::Request(CreateSessionError::OpenCode(error))
        | ConnectError::RequestMayHaveBeenTransmitted(CreateSessionError::OpenCode(error)) => {
            open_code_error(error)
        }
        ConnectError::Startup(diagnostics) => startup_error(diagnostics),
        ConnectError::State(error) => discovery_error(error),
    }
}

fn startup_error(diagnostics: Vec<oca_server::StartupDiagnostic>) -> OcaError {
    let detail = diagnostics
        .into_iter()
        .map(|diagnostic| diagnostic.to_string())
        .collect::<Vec<_>>()
        .join("; ");
    OcaError::new(ErrorCode::ServerStartTimeout)
        .with_error(format!("OpenCode could not be started: {detail}"))
}

fn discovery_error(error: std::io::Error) -> OcaError {
    OcaError::new(ErrorCode::ServerUnavailable)
        .with_error(format!("OpenCode discovery failed: {error}"))
}

pub(crate) fn open_code_error(error: OpenCodeError) -> OcaError {
    match error {
        OpenCodeError::ProtocolMismatch { message } => {
            OcaError::new(ErrorCode::ProtocolMismatch).with_error(message)
        }
        OpenCodeError::RateLimited { body, limit } => {
            let error = OcaError::new(ErrorCode::RateLimited).with_error(body);
            match limit.retry_after_ms() {
                Some(delay) => error.with_retry_after_ms(delay),
                None => error,
            }
        }
        OpenCodeError::Server { status, body } => OcaError::new(ErrorCode::ServerUnavailable)
            .with_error(format!("OpenCode returned HTTP {status}: {body}")),
        OpenCodeError::Transport { message, .. } => {
            OcaError::new(ErrorCode::ServerUnreachable).with_error(message)
        }
    }
}

pub(crate) fn prompt_error(error: OpenCodeError) -> OcaError {
    match error {
        OpenCodeError::Transport {
            stage: TransmissionStage::PossiblyTransmitted,
            message,
        } => OcaError::new(ErrorCode::PromptUncertain)
            .with_error(format!("prompt response was lost: {message}"))
            .with_help("reconcile the stored message id; never replay this prompt automatically"),
        error => open_code_error(error),
    }
}
