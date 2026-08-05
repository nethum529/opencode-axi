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
    type Error = OpenCodeError;

    fn send(
        &mut self,
        client: &OpenCodeClient,
    ) -> impl Future<Output = Result<Self::Output, RequestFailure<Self::Error>>> + Send {
        let request = self.request.clone();
        async move {
            client.create_session(request).await.map_err(|error| {
                // Even a lost create-session response precedes prompt admission. A
                // duplicate empty session is harmless, so T14 may recover once.
                if matches!(error, OpenCodeError::Transport { .. }) {
                    RequestFailure::Connection(error)
                } else {
                    RequestFailure::Application(error)
                }
            })
        }
    }
}

pub(crate) fn connect_error(error: ConnectError<OpenCodeError>) -> OcaError {
    match error {
        ConnectError::Request(error) | ConnectError::RequestMayHaveBeenTransmitted(error) => {
            open_code_error(error)
        }
        ConnectError::Startup(diagnostics) => {
            let detail = diagnostics
                .into_iter()
                .map(|diagnostic| diagnostic.to_string())
                .collect::<Vec<_>>()
                .join("; ");
            OcaError::new(ErrorCode::ServerStartTimeout)
                .with_error(format!("OpenCode could not be started: {detail}"))
        }
        ConnectError::State(error) => OcaError::new(ErrorCode::ServerUnavailable)
            .with_error(format!("OpenCode discovery failed: {error}")),
    }
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
