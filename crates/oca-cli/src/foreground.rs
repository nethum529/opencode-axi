//! Production adapters for the core foreground dispatch state machine.

use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use oca_core::{
    DispatchPrompt, DisplayMode, ErrorCode, ForegroundBackend, ForegroundRequest,
    MessageIdGenerator, OcaError, RANDOM_SUFFIX_WIDTH, ReplyContract, ResolvedModel, RoleReply,
    TerminalReply, WorkerPolicy, WorkerState, run_foreground,
};
use oca_display::{Acknowledgement, CompletionRecord, HerdrClient};
use oca_opencode::{
    CreateSessionRequest, OpenCodeClient, PromptRequest, Subscription, TextPart,
    attributed_structured_reply, is_target_session_idle,
};
use oca_server::{ConnectOrStart, SystemRuntime};
use oca_state::{NewRef, OcaConfig, PendingRefAllocation, RefState, RefStore, RefStorePaths};

use crate::{
    DispatchCommand,
    scope::Scope,
    transport::{CreateSessionOperation, connect_error, open_code_error, prompt_error},
    worktree_dispatch::{WorktreeDispatch, finalize_turn},
};

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
    let mut prepared = prepare_dispatch(command, home, PostAckDurability::Complete)?;
    run_foreground(&mut prepared.backend, prepared.request)
        .await
        .map(|_| ())
}

/// Who owns the potentially blocking parent-directory durability attempt after
/// acknowledgement has been flushed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PostAckDurability {
    Complete,
    Transfer,
}

pub(crate) struct PreparedDispatch {
    pub(crate) backend: ProductionBackend,
    pub(crate) request: ForegroundRequest,
}

/// Resolves production state shared by foreground and background dispatch.
pub(crate) fn prepare_dispatch(
    command: DispatchCommand,
    home: impl AsRef<Path>,
    post_ack_durability: PostAckDurability,
) -> Result<PreparedDispatch, OcaError> {
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
    let scope = crate::scope::current(home, &cwd).map_err(io_error)?;
    let policy = WorkerPolicy::restricted([cwd.clone()]);
    if command.worktree && !Path::new(&scope.repo).join(".git").exists() {
        return Err(OcaError::new(ErrorCode::Usage)
            .with_error("`-w` requires a Git repository")
            .with_help("run the dispatch from inside a Git repository"));
    }
    let worktree = command
        .worktree
        .then(|| WorktreeDispatch::new(PathBuf::from(&scope.repo), &command.prompt));
    let configured_socket =
        (!config.herdr.socket.is_empty()).then(|| Path::new(config.herdr.socket.as_str()));
    let display = DisplayMode::select(
        command.headless,
        || {
            HerdrClient::discover_from(
                home,
                configured_socket,
                Duration::from_millis(config.herdr.timeout_ms),
            )
            .is_some()
        },
        std::env::var_os("TMUX").is_some(),
    );

    // Server discovery happens only after every local parse/resolve failure.
    let manager = ConnectOrStart::from_home(home, &config.server);
    let refs = RefStore::with_paths(RefStorePaths::in_directory(home.join(".oca")));
    let backend = ProductionBackend::new(manager, refs, post_ack_durability, scope, worktree);
    let request = ForegroundRequest {
        model: command.model,
        prompt: command.prompt,
        role: command.role,
        contract,
        policy,
        cwd,
        display,
        json: command.json,
    };

    Ok(PreparedDispatch { backend, request })
}

pub(crate) struct ProductionBackend {
    manager: ConnectOrStart,
    runtime: SystemRuntime,
    client: Option<OpenCodeClient>,
    refs: RefStore,
    message_ids: MessageIdGenerator,
    post_ack_durability: PostAckDurability,
    scope: Scope,
    worktree: Option<WorktreeDispatch>,
    dispatch_cwd: Option<PathBuf>,
}

pub(crate) enum PendingProductionRef {
    Allocated(Box<PendingRefAllocation>),
    Reserved(String),
}

impl ProductionBackend {
    fn new(
        manager: ConnectOrStart,
        refs: RefStore,
        post_ack_durability: PostAckDurability,
        scope: Scope,
        worktree: Option<WorktreeDispatch>,
    ) -> Self {
        Self {
            manager,
            runtime: SystemRuntime::default(),
            client: None,
            refs,
            message_ids: MessageIdGenerator::new(),
            post_ack_durability,
            scope,
            worktree,
            dispatch_cwd: None,
        }
    }

    fn client(&self) -> Result<&OpenCodeClient, OcaError> {
        self.client.as_ref().ok_or_else(|| {
            OcaError::new(ErrorCode::ProtocolMismatch)
                .with_error("OpenCode client used before session creation")
        })
    }

    fn persist_uncertain_ref(
        &self,
        session_id: &str,
        prompt: &DispatchPrompt,
    ) -> Result<String, OcaError> {
        let record = NewRef::for_session(session_id)
            .with_message_id(prompt.message_id.clone())
            .with_control_metadata(
                &prompt.model.alias,
                &prompt.model.effort,
                &prompt.role,
                self.dispatch_cwd
                    .as_deref()
                    .unwrap_or_else(|| Path::new("."))
                    .display()
                    .to_string(),
                RefState::Unknown,
            )
            .with_repo(&self.scope.repo)
            .with_spawner_tag(&self.scope.spawner_tag);
        let pending = self
            .refs
            .allocate(record)
            .map_err(|error| state_error("could not preserve uncertain prompt ref", error))?;
        let reference = pending.record().id.clone();
        // There is deliberately no success acknowledgement for an uncertain
        // prompt. Dropping transfers the pending directory sync while leaving
        // the content-synced, atomically renamed ref visible for reconciliation.
        drop(pending);
        Ok(reference)
    }

    async fn read_attributed_reply(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<Option<TerminalReply>, OcaError> {
        let messages = self
            .client()?
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
    type PendingRef = PendingProductionRef;

    fn prepare(&mut self, request: &mut ForegroundRequest) -> Result<(), OcaError> {
        if let Some(worktree) = &mut self.worktree {
            worktree.prepare(&self.refs, request, &self.scope)?;
        }
        Ok(())
    }

    async fn create_session(&mut self, request: &ForegroundRequest) -> Result<String, OcaError> {
        let permission =
            serde_json::to_value(request.policy.permission_profile()).map_err(|error| {
                OcaError::new(ErrorCode::ProtocolMismatch)
                    .with_error(format!("permission profile could not be encoded: {error}"))
            })?;
        self.dispatch_cwd = Some(request.cwd.clone());
        let mut operation = CreateSessionOperation::new(CreateSessionRequest {
            directory: Some(request.cwd.display().to_string()),
            title: Some(format!("oca:{}", self.scope.spawner_tag)),
            agent: Some(request.role.clone()),
            model: Some(request.model.clone()),
            permission: Some(permission),
            metadata: serde_json::Map::from_iter([(
                "oca_spawner".to_owned(),
                serde_json::Value::String(self.scope.spawner_tag.clone()),
            )]),
            ..CreateSessionRequest::default()
        });
        let session = self
            .manager
            .connect_or_start(&self.runtime, &mut operation)
            .await
            .map_err(connect_error)?;
        let record = self
            .manager
            .read_record()
            .map_err(|error| server_state_error("could not read recovered server record", error))?
            .ok_or_else(|| {
                OcaError::new(ErrorCode::ServerUnavailable)
                    .with_error("session creation succeeded without a server discovery record")
            })?;
        let base_url = format!("http://127.0.0.1:{}", record.port)
            .parse()
            .map_err(|error| {
                OcaError::new(ErrorCode::ProtocolMismatch)
                    .with_error(format!("invalid recovered OpenCode URL: {error}"))
            })?;
        self.client = Some(OpenCodeClient::new(base_url));
        if let Some(worktree) = &self.worktree {
            worktree.record_session(&self.refs, &session.id)?;
        }
        Ok(session.id)
    }

    async fn subscribe(&mut self) -> Result<Self::Subscription, OcaError> {
        self.client()?
            .subscribe(None)
            .await
            .map_err(open_code_error)
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
        let result = self
            .client()?
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
            .await;
        match result {
            Ok(_) => Ok(()),
            Err(error) => {
                let error = prompt_error(error);
                if error.code() == ErrorCode::PromptUncertain.as_str() {
                    let reference = self.persist_uncertain_ref(session_id, prompt)?;
                    Err(error.with_ref(reference))
                } else {
                    Err(error)
                }
            }
        }
    }

    fn write_ref(
        &mut self,
        session_id: &str,
        message_id: &str,
        request: &ForegroundRequest,
    ) -> Result<Self::PendingRef, OcaError> {
        if let Some(worktree) = &self.worktree {
            return worktree
                .finish_ref(&self.refs, session_id, message_id)
                .map(PendingProductionRef::Reserved);
        }
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
                    )
                    .with_repo(&self.scope.repo)
                    .with_spawner_tag(&self.scope.spawner_tag)
                    .with_display(request.display.as_str()),
            )
            .map(Box::new)
            .map(PendingProductionRef::Allocated)
            .map_err(|error| state_error("could not write ref record", error))
    }

    fn acknowledge(
        &mut self,
        pending: Self::PendingRef,
        model: &ResolvedModel,
        json: bool,
    ) -> Result<String, OcaError> {
        let pending = match pending {
            PendingProductionRef::Reserved(reference) => {
                print_ack(&reference, model, json).map_err(io_error)?;
                return Ok(reference);
            }
            PendingProductionRef::Allocated(pending) => *pending,
        };
        match self.post_ack_durability {
            PostAckDurability::Complete => {
                let completion = pending
                    .acknowledge_with(|record| print_ack(&record.id, model, json))
                    .map_err(io_error)?;
                if let Some(warning) = completion.durability_warning() {
                    eprintln!("warning: {warning}");
                }
                Ok(completion.record().id.clone())
            }
            PostAckDurability::Transfer => {
                let reference = pending.record().id.clone();
                print_ack(&reference, model, json).map_err(io_error)?;
                // Dropping the retained lock leaves the durability marker for
                // the next ref-store entrant (normally the follow process).
                // Background dispatch performs no directory fsync after ack.
                drop(pending);
                Ok(reference)
            }
        }
    }

    fn spawn_attach(
        &mut self,
        reference: &str,
        session_id: &str,
        cwd: &Path,
        display: DisplayMode,
    ) -> Result<(), OcaError> {
        if display == DisplayMode::Headless {
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
        finalize_turn(&self.refs, reference, reply)
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

fn print_ack(reference: &str, model: &ResolvedModel, json: bool) -> io::Result<()> {
    let document = Acknowledgement::from_resolved(reference, "running", model);
    let rendered = if json {
        document.render_json()
    } else {
        document.render_toon()
    };
    let mut stdout = io::stdout().lock();
    stdout.write_all(rendered.as_bytes())?;
    stdout.flush()
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
