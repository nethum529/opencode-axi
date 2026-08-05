//! Production adapters for the core foreground dispatch state machine.

use std::{
    io::{self, Write},
    path::Path,
    process::{Command as ProcessCommand, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use oca_core::{
    DispatchPrompt, ErrorCode, ForegroundBackend, ForegroundRequest, MessageIdGenerator, OcaError,
    RANDOM_SUFFIX_WIDTH, ReplyContract, ResolvedModel, RoleReply, TerminalReply, WorkerPolicy,
    WorkerState, run_foreground,
};
use oca_display::{Acknowledgement, CompletionRecord};
use oca_opencode::{
    CreateSessionRequest, OpenCodeClient, OpenCodeError, PromptRequest, Subscription, TextPart,
    attributed_structured_reply, is_target_session_idle,
};
use oca_server::ConnectOrStart;
use oca_state::{
    NewRef, OcaConfig, PendingRefAllocation, RefPatch, RefState, RefStore, RefStorePaths,
};

use crate::DispatchCommand;

/// Executes a parsed foreground dispatch using the user's local state root.
///
/// # Errors
///
/// Returns a stable oca error when role resolution, server discovery, transport,
/// persistence, decoding, or output fails.
pub async fn execute_foreground(
    command: DispatchCommand,
    home: impl AsRef<Path>,
) -> Result<(), OcaError> {
    if command.worktree {
        return Err(OcaError::new(ErrorCode::Usage)
            .with_error("worktree dispatch is not available in the foreground path yet")
            .with_help("retry without `-w`; worktree dispatch is owned by T21/T22"));
    }

    let home = home.as_ref();
    let config = OcaConfig::load_from_home(home).map_err(|error| {
        OcaError::new(ErrorCode::Usage)
            .with_error(format!("failed to load configuration: {error}"))
            .with_help("fix ~/.oca/config.toml and retry")
    })?;
    if !config.roles.contains_key(&command.role) {
        return Err(OcaError::new(ErrorCode::Usage)
            .with_error(format!("unknown worker role `{}`", command.role))
            .with_help("configure the role under [roles] and retry"));
    }
    let contract = ReplyContract::resolve(&command.role)?;
    let cwd = std::env::current_dir().map_err(io_error)?;
    let policy = WorkerPolicy::restricted([cwd.clone()]);

    // Server discovery happens only after every local parse/resolve failure.
    let manager = ConnectOrStart::from_home(home, &config.server);
    let record = manager
        .read_record()
        .map_err(|error| server_state_error("could not read server record", error))?
        .ok_or_else(|| {
            OcaError::new(ErrorCode::ServerUnavailable)
                .with_error("no OpenCode server record is available")
                .with_help("start `opencode serve` and retry")
        })?;
    let base_url = format!("http://127.0.0.1:{}", record.port)
        .parse()
        .map_err(|error| {
            OcaError::new(ErrorCode::ProtocolMismatch)
                .with_error(format!("invalid recorded OpenCode URL: {error}"))
        })?;
    let refs = RefStore::with_paths(RefStorePaths::in_directory(home.join(".oca")));
    let mut backend = ProductionBackend::new(OpenCodeClient::new(base_url), refs);
    let request = ForegroundRequest {
        model: command.model,
        prompt: command.prompt,
        role: command.role,
        contract,
        policy,
        cwd,
        headless: command.headless,
        json: command.json,
    };

    run_foreground(&mut backend, request).await.map(|_| ())
}

struct ProductionBackend {
    client: OpenCodeClient,
    refs: RefStore,
    message_ids: MessageIdGenerator,
}

impl ProductionBackend {
    fn new(client: OpenCodeClient, refs: RefStore) -> Self {
        Self {
            client,
            refs,
            message_ids: MessageIdGenerator::new(),
        }
    }

    async fn read_attributed_reply(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<Option<TerminalReply>, OcaError> {
        let messages = self
            .client
            .messages(session_id)
            .await
            .map_err(open_code_error)?;
        Ok(
            attributed_structured_reply(&messages, session_id, message_id)
                .map(|structured| TerminalReply { structured }),
        )
    }
}

impl ForegroundBackend for ProductionBackend {
    type Subscription = Subscription;
    type PendingRef = PendingRefAllocation;

    async fn create_session(&mut self, request: &ForegroundRequest) -> Result<String, OcaError> {
        let permission =
            serde_json::to_value(request.policy.permission_profile()).map_err(|error| {
                OcaError::new(ErrorCode::ProtocolMismatch)
                    .with_error(format!("permission profile could not be encoded: {error}"))
            })?;
        self.client
            .create_session(CreateSessionRequest {
                directory: Some(request.cwd.display().to_string()),
                agent: Some(request.role.clone()),
                model: Some(request.model.clone()),
                permission: Some(permission),
                ..CreateSessionRequest::default()
            })
            .await
            .map(|session| session.id)
            .map_err(open_code_error)
    }

    async fn subscribe(&mut self) -> Result<Self::Subscription, OcaError> {
        self.client.subscribe(None).await.map_err(open_code_error)
    }

    fn mint_message_id(&mut self) -> Result<String, OcaError> {
        let timestamp_ms = u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        )
        .unwrap_or(u64::MAX);
        let mut random = [0_u8; RANDOM_SUFFIX_WIDTH];
        getrandom::fill(&mut random).map_err(|error| {
            OcaError::new(ErrorCode::ServerUnavailable)
                .with_error(format!("could not mint an OpenCode message id: {error}"))
        })?;
        Ok(self.message_ids.mint(timestamp_ms, random))
    }

    async fn prompt_async(
        &mut self,
        session_id: &str,
        prompt: &DispatchPrompt,
    ) -> Result<(), OcaError> {
        self.client
            .prompt_async(
                session_id,
                PromptRequest {
                    message_id: prompt.message_id.clone(),
                    model: prompt.model.clone(),
                    variant: prompt.variant.clone(),
                    role: prompt.role.clone(),
                    parts: vec![TextPart {
                        text: prompt.text.clone(),
                    }],
                    output_schema: Some(prompt.output_schema.clone()),
                    permission: prompt.permission.clone(),
                },
            )
            .await
            .map(|_| ())
            .map_err(open_code_error)
    }

    fn write_ref(
        &mut self,
        session_id: &str,
        message_id: &str,
        request: &ForegroundRequest,
    ) -> Result<Self::PendingRef, OcaError> {
        self.refs
            .allocate(
                NewRef::for_session(session_id)
                    .with_message_id(message_id)
                    .with_control_metadata(
                        &request.model.alias,
                        &request.model.effort,
                        &request.role,
                        request.cwd.display().to_string(),
                        RefState::Running,
                    ),
            )
            .map_err(|error| state_error("could not write ref record", error))
    }

    fn acknowledge(
        &mut self,
        pending: Self::PendingRef,
        model: &ResolvedModel,
        json: bool,
    ) -> Result<String, OcaError> {
        let completion = pending
            .acknowledge_with(|record| {
                let document = Acknowledgement::from_resolved(&record.id, "running", model);
                let rendered = if json {
                    document.render_json()
                } else {
                    document.render_toon()
                };
                let mut stdout = io::stdout().lock();
                stdout.write_all(rendered.as_bytes())?;
                stdout.flush()
            })
            .map_err(io_error)?;
        if let Some(warning) = completion.durability_warning() {
            eprintln!("warning: {warning}");
        }
        Ok(completion.record().id.clone())
    }

    fn spawn_attach(
        &mut self,
        reference: &str,
        session_id: &str,
        cwd: &Path,
        headless: bool,
    ) -> Result<(), OcaError> {
        if headless {
            return Ok(());
        }
        let executable = std::env::current_exe().map_err(io_error)?;
        let mut command = ProcessCommand::new(executable);
        command
            .arg("__attach")
            .arg(reference)
            .arg(session_id)
            .arg(cwd)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        if let Err(error) = command.spawn() {
            // Display degradation is explicitly non-fatal to dispatch.
            eprintln!("warning: detached display attach failed: {error}");
        }
        Ok(())
    }

    async fn reconcile_once(
        &mut self,
        session_id: &str,
        message_id: &str,
    ) -> Result<Option<TerminalReply>, OcaError> {
        self.read_attributed_reply(session_id, message_id).await
    }

    async fn wait_terminal(
        &mut self,
        subscription: &mut Self::Subscription,
        session_id: &str,
        message_id: &str,
    ) -> Result<TerminalReply, OcaError> {
        loop {
            let event = subscription
                .next()
                .await
                .map_err(|error| {
                    OcaError::new(ErrorCode::ServerUnreachable)
                        .with_error(format!("OpenCode event stream failed: {error}"))
                })?
                .ok_or_else(|| {
                    OcaError::new(ErrorCode::ServerUnreachable)
                        .with_error("OpenCode event stream closed before a terminal event")
                })?;
            if is_target_session_idle(&event, session_id)
                && let Some(reply) = self.read_attributed_reply(session_id, message_id).await?
            {
                return Ok(reply);
            }
        }
    }

    fn finalize(&mut self, reference: &str, reply: &RoleReply) -> Result<(), OcaError> {
        // Non-worktree dispatch has no git finalization side effect.
        let state = match reply {
            RoleReply::Impl(reply) => reply.status,
            RoleReply::Review(reply) => reply.status,
        };
        let state = match state {
            WorkerState::Done => RefState::Done,
            WorkerState::Blocked => RefState::Blocked,
            WorkerState::Partial => RefState::Partial,
        };
        self.refs
            .patch(reference, RefPatch::default().with_last_state(state))
            .map(|_| ())
            .map_err(|error| state_error("could not update terminal ref state", error))
    }

    fn print_final(
        &mut self,
        reference: &str,
        reply: &RoleReply,
        json: bool,
    ) -> Result<(), OcaError> {
        let state = match reply {
            RoleReply::Impl(reply) => reply.status,
            RoleReply::Review(reply) => reply.status,
        };
        let (state, outcome) = match state {
            WorkerState::Done => ("completed", "success"),
            WorkerState::Blocked => ("blocked", "blocked"),
            WorkerState::Partial => ("partial", "partial"),
        };
        let document = CompletionRecord::new(reference, state, outcome);
        let rendered = if json {
            document.render_json()
        } else {
            document.render_toon()
        };
        let mut stdout = io::stdout().lock();
        stdout.write_all(rendered.as_bytes()).map_err(io_error)?;
        stdout.flush().map_err(io_error)
    }
}

fn open_code_error(error: OpenCodeError) -> OcaError {
    match error {
        OpenCodeError::ProtocolMismatch { message } => {
            OcaError::new(ErrorCode::ProtocolMismatch).with_error(message)
        }
        OpenCodeError::Server { status: 429, body } => {
            OcaError::new(ErrorCode::RateLimited).with_error(body)
        }
        OpenCodeError::Server { status, body } => OcaError::new(ErrorCode::ServerUnavailable)
            .with_error(format!("OpenCode returned HTTP {status}: {body}")),
        OpenCodeError::Transport { message } => {
            OcaError::new(ErrorCode::ServerUnreachable).with_error(message)
        }
    }
}

fn io_error(error: io::Error) -> OcaError {
    OcaError::new(ErrorCode::ServerUnavailable).with_error(error.to_string())
}

fn state_error(context: &str, error: impl std::fmt::Display) -> OcaError {
    OcaError::new(ErrorCode::ServerUnavailable).with_error(format!("{context}: {error}"))
}

fn server_state_error(context: &str, error: io::Error) -> OcaError {
    state_error(context, error)
}
