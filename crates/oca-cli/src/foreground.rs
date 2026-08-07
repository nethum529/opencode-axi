//! Production adapters for the core foreground dispatch state machine.

use std::{
    collections::VecDeque,
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
    CreateSessionRequest, MessageWithParts, OpenCodeClient, OpenCodeError, PromptRequest, SseError,
    SseEvent, SseSourceErrorKind, Subscription, TextPart, attributed_streamed_reply,
    attributed_structured_reply, is_target_message_event, is_target_session_idle,
};
use oca_server::{ConnectOrStart, SystemRuntime};
use oca_state::{
    DispatchTransport, Intent, IntentOperation, IntentPhase, IntentRequest, IntentStore, NewRef,
    OcaConfig, PendingRefAllocation, RefState, RefStore, RefStorePaths,
};

use crate::{
    DispatchCommand,
    attach_diagnostics::binding_identity,
    crash_recovery::{RESERVED_SESSION_ID, intent_failpoint, persist_intent, prompt_sha256},
    scope::Scope,
    transport::{CreateSessionOperation, create_session_error, open_code_error, prompt_error},
    worktree_dispatch::{WorktreeDispatch, finalize_turn},
};

const PROMPT_CONFIRM_TIMEOUT: Duration = Duration::from_secs(2);
const PROMPT_CONFIRM_INITIAL_BACKOFF: Duration = Duration::from_millis(20);
const PROMPT_CONFIRM_MAX_BACKOFF: Duration = Duration::from_millis(200);

enum PromptConfirmationCheck {
    Stream(Result<Option<SseEvent>, SseError>),
    History(Result<Vec<MessageWithParts>, OpenCodeError>),
}

enum PromptHistoryEvidence {
    Missing,
    Rejected(String),
    Failed(String),
    TimedOut,
}

pub(crate) struct DispatchSubscription {
    inner: Subscription,
    pending: VecDeque<Result<Option<SseEvent>, SseError>>,
}

impl DispatchSubscription {
    pub(crate) fn new(inner: Subscription) -> Self {
        Self {
            inner,
            pending: VecDeque::new(),
        }
    }

    async fn next(&mut self) -> Result<Option<SseEvent>, SseError> {
        match self.pending.pop_front() {
            Some(result) => result,
            None => self.inner.next().await,
        }
    }

    /// Reads past the buffer deliberately: confirmation only ever runs before
    /// any downstream read, so `pending` holds exactly the events this loop has
    /// already inspected and replaying them here would re-inspect them.
    async fn next_confirmation(&mut self) -> Result<Option<SseEvent>, SseError> {
        self.inner.next().await
    }

    /// Queues one inspected event for the downstream consumer. Appending keeps
    /// `pending` in arrival order, so `next` replays the stream exactly as it
    /// arrived. The confirming event itself is deliberately not preserved: it
    /// identifies our own user message, which no downstream consumer acts on.
    fn preserve(&mut self, result: Result<Option<SseEvent>, SseError>) {
        self.pending.push_back(result);
    }
}

/// Confirms that an accepted asynchronous prompt became visible to OpenCode.
///
/// The already-open event stream is the primary evidence source. Session
/// history remains the authoritative fallback, including when its stored
/// records are poisoned and the endpoint rejects them with HTTP 400.
pub(crate) async fn confirm_prompt_landed(
    client: &OpenCodeClient,
    subscription: &mut DispatchSubscription,
    session_id: &str,
    message_id: &str,
    prompt_text: &str,
) -> Result<(), OcaError> {
    let started = tokio::time::Instant::now();
    let mut backoff = PROMPT_CONFIRM_INITIAL_BACKOFF;
    let expected_hash = prompt_sha256(prompt_text);
    let mut history_evidence = PromptHistoryEvidence::Missing;
    let mut stream_open = true;

    loop {
        let elapsed = started.elapsed();
        let Some(remaining) = PROMPT_CONFIRM_TIMEOUT.checked_sub(elapsed) else {
            break;
        };
        let attempt_timeout = remaining.min(PROMPT_CONFIRM_MAX_BACKOFF);
        let check = tokio::time::timeout(attempt_timeout, async {
            if stream_open {
                tokio::select! {
                    biased;
                    event = subscription.next_confirmation() => {
                        PromptConfirmationCheck::Stream(event)
                    },
                    history = client.messages(session_id) => {
                        PromptConfirmationCheck::History(history)
                    }
                }
            } else {
                PromptConfirmationCheck::History(client.messages(session_id).await)
            }
        })
        .await;
        match check {
            Ok(PromptConfirmationCheck::Stream(Ok(Some(event)))) => {
                if is_target_message_event(&event, session_id, message_id) {
                    return Ok(());
                }
                subscription.preserve(Ok(Some(event)));
                continue;
            }
            Ok(PromptConfirmationCheck::Stream(result @ (Ok(None) | Err(_)))) => {
                stream_open = false;
                subscription.preserve(result);
                continue;
            }
            Ok(PromptConfirmationCheck::History(Ok(messages))) => {
                if user_prompt_is_visible(&messages, session_id, message_id, &expected_hash) {
                    return Ok(());
                }
                history_evidence = PromptHistoryEvidence::Missing;
            }
            Ok(PromptConfirmationCheck::History(Err(error))) => {
                let rendered = error.to_string();
                history_evidence = match error {
                    OpenCodeError::Server { status: 400, .. } => {
                        PromptHistoryEvidence::Rejected(rendered)
                    }
                    _ => PromptHistoryEvidence::Failed(rendered),
                };
            }
            Err(_) => history_evidence = PromptHistoryEvidence::TimedOut,
        }

        let elapsed = started.elapsed();
        let Some(remaining) = PROMPT_CONFIRM_TIMEOUT.checked_sub(elapsed) else {
            break;
        };
        tokio::time::sleep(backoff.min(remaining)).await;
        backoff = backoff.saturating_mul(2).min(PROMPT_CONFIRM_MAX_BACKOFF);
    }

    let detail = match history_evidence {
        PromptHistoryEvidence::Missing => {
            "the accepted user message did not appear in session history".to_owned()
        }
        PromptHistoryEvidence::Rejected(error) => {
            format!("the session history endpoint rejected its stored records: {error}")
        }
        PromptHistoryEvidence::Failed(error) => {
            format!("session history could not confirm the accepted prompt: {error}")
        }
        PromptHistoryEvidence::TimedOut => {
            "session history could not confirm the accepted prompt: message lookup timed out"
                .to_owned()
        }
    };
    Err(OcaError::new(ErrorCode::PromptUncertain)
        .with_error(format!(
            "{detail} within {} ms",
            PROMPT_CONFIRM_TIMEOUT.as_millis()
        ))
        .with_help("Run `oca m <ref> \"<resend>\"`; oca will not replay"))
}

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
    let state_directory = home.join(".oca");
    let refs = RefStore::with_paths(RefStorePaths::in_directory(&state_directory));
    let backend = ProductionBackend::new(
        manager,
        refs,
        state_directory,
        post_ack_durability,
        scope,
        worktree,
    );
    let request = ForegroundRequest {
        model: command.model,
        prompt: command.prompt,
        role: command.role,
        contract,
        schema_transport: config.dispatch.transport == DispatchTransport::Schema,
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
    intents: IntentStore,
    intent: Option<Intent>,
    reference: Option<String>,
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
        state_directory: PathBuf,
        post_ack_durability: PostAckDurability,
        scope: Scope,
        worktree: Option<WorktreeDispatch>,
    ) -> Self {
        Self {
            manager,
            runtime: SystemRuntime::default(),
            client: None,
            refs,
            intents: IntentStore::in_directory(state_directory),
            intent: None,
            reference: None,
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

    fn reference(&self) -> Result<&str, OcaError> {
        self.reference.as_deref().ok_or_else(|| {
            OcaError::new(ErrorCode::ProtocolMismatch)
                .with_error("dispatch ref used before intent preparation")
        })
    }

    fn intent_mut(&mut self) -> Result<&mut Intent, OcaError> {
        self.intent.as_mut().ok_or_else(|| {
            OcaError::new(ErrorCode::ProtocolMismatch)
                .with_error("dispatch intent used before preparation")
        })
    }

    async fn read_attributed_reply(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<Option<TerminalReply>, OcaError> {
        let messages = match self.client()?.messages(session_id).await {
            Ok(messages) => messages,
            Err(OpenCodeError::Server { status: 400, .. }) => return Ok(None),
            Err(error) => return Err(open_code_error(error)),
        };
        attributed_structured_reply(&messages, session_id, message_id)
            .map(|reply| reply.map(|structured| TerminalReply { structured }))
            .map_err(|error| {
                OcaError::new(ErrorCode::ProtocolMismatch).with_error(error.to_string())
            })
    }

    fn preserve_uncertain_prompt(
        &mut self,
        session_id: &str,
        prompt: &DispatchPrompt,
        error: OcaError,
    ) -> Result<(), OcaError> {
        {
            let intent = self.intent_mut()?;
            intent.set_phase(IntentPhase::PromptUncertain);
        }
        let intent = self.intent.as_ref().expect("intent set during prepare");
        persist_intent(&self.intents, intent)?;

        let reference = self.reference()?.to_owned();
        if self.worktree.is_some() {
            self.refs
                .patch(
                    &reference,
                    oca_state::RefPatch::default()
                        .with_session_id(session_id)
                        .with_message_id(&prompt.message_id)
                        .with_last_state(RefState::Unknown),
                )
                .map_err(|state| state_error("could not mark uncertain prompt ref", state))?;
        } else {
            let pending = self
                .refs
                .allocate_reserved(
                    &reference,
                    NewRef::for_session(session_id)
                        .with_message_id(&prompt.message_id)
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
                        .with_spawner_tag(&self.scope.spawner_tag)
                        .with_display(request_display(self.intent.as_ref())),
                )
                .map_err(|state| state_error("could not preserve uncertain prompt ref", state))?;
            drop(pending);
        }
        Err(error.with_ref(reference))
    }

    fn cleanup_pre_prompt_failure(&self, error: OcaError) -> OcaError {
        let Ok(reference) = self.reference().map(str::to_owned) else {
            return error;
        };
        if let Some(worktree) = &self.worktree {
            let _ = worktree.cleanup(&reference);
        }
        let _ = self.intents.remove(&reference);
        let _ = self.refs.discard_unacknowledged(&reference);
        error.with_ref(reference)
    }
}

impl ForegroundBackend for ProductionBackend {
    type Subscription = DispatchSubscription;
    type PendingRef = PendingProductionRef;

    fn prepare(&mut self, request: &mut ForegroundRequest) -> Result<(), OcaError> {
        let requested = IntentRequest {
            alias: request.model.alias.clone(),
            effort: request.model.effort.clone(),
            role: request.role.clone(),
            cwd: request.cwd.display().to_string(),
            repo: self.scope.repo.clone(),
            spawner_tag: Some(self.scope.spawner_tag.clone()),
            worktree: self.worktree.is_some(),
            display: Some(request.display.as_str().to_owned()),
        };
        let intent = if self.worktree.is_some() {
            let reservation = self
                .refs
                .reserve(
                    NewRef::for_session(RESERVED_SESSION_ID)
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
                .map_err(|error| state_error("could not reserve worktree ref", error))?;
            Intent::new(&reservation.id, IntentOperation::Dispatch).with_requested(requested)
        } else {
            let intent = self
                .refs
                .reserve_intent(&self.intents, IntentOperation::Dispatch, requested)
                .map_err(|error| state_error("could not reserve dispatch intent", error))?;
            intent_failpoint(&intent);
            intent
        };
        self.reference = Some(intent.reference.clone());
        self.intent = Some(intent);
        if let Some(worktree) = &mut self.worktree {
            let reference = self.reference.as_deref().expect("set above");
            let intent = self.intent.as_mut().expect("set above");
            worktree.prepare(&self.refs, request, reference, &self.intents, intent)?;
            persist_intent(&self.intents, intent)?;
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
        let session = match self
            .manager
            .connect_or_start(&self.runtime, &mut operation)
            .await
        {
            Ok(session) => session,
            Err(error) => {
                return Err(self.cleanup_pre_prompt_failure(create_session_error(error)));
            }
        };
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
        {
            let intent = self.intent_mut()?;
            intent.session_id = Some(session.id.clone());
            intent.set_phase(IntentPhase::SessionCreated);
        }
        let intent = self.intent.as_ref().expect("intent set during prepare");
        persist_intent(&self.intents, intent)?;
        Ok(session.id)
    }

    async fn subscribe(&mut self) -> Result<Self::Subscription, OcaError> {
        let directory = if self.worktree.is_some() {
            self.dispatch_cwd
                .as_deref()
                .ok_or_else(|| {
                    OcaError::new(ErrorCode::ProtocolMismatch).with_error(
                        "OpenCode subscription opened before the session directory was set",
                    )
                })?
                .display()
                .to_string()
        } else {
            self.scope.repo.clone()
        };
        self.client()?
            .subscribe(&directory, None)
            .await
            .map(DispatchSubscription::new)
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
        {
            let intent = self.intent_mut()?;
            intent.session_id = Some(session_id.to_owned());
            intent.message_id = Some(prompt.message_id.clone());
            intent.prompt_sha256 = Some(prompt_sha256(&prompt.text));
            intent.set_phase(IntentPhase::PromptUncertain);
        }
        let intent = self.intent.as_ref().expect("intent set during prepare");
        persist_intent(&self.intents, intent)?;
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
                    output_schema: prompt.output_schema.clone(),
                    permission: prompt.permission.clone(),
                },
            )
            .await;
        match result {
            Ok(_) => Ok(()),
            Err(error) => {
                let error = prompt_error(error);
                if error.code() == ErrorCode::PromptUncertain.as_str() {
                    self.preserve_uncertain_prompt(session_id, prompt, error)
                } else {
                    let reference = self.reference()?.to_owned();
                    Err(error.with_ref(reference))
                }
            }
        }
    }

    async fn confirm_prompt_landed(
        &mut self,
        subscription: &mut Self::Subscription,
        session_id: &str,
        prompt: &DispatchPrompt,
    ) -> Result<(), OcaError> {
        match confirm_prompt_landed(
            self.client()?,
            subscription,
            session_id,
            &prompt.message_id,
            &prompt.text,
        )
        .await
        {
            Ok(()) => Ok(()),
            Err(error) => self.preserve_uncertain_prompt(session_id, prompt, error),
        }
    }

    fn fail_before_prompt(&mut self, error: OcaError) -> OcaError {
        self.cleanup_pre_prompt_failure(error)
    }

    fn mark_prompt_running(&mut self) -> Result<(), OcaError> {
        let intent = self.intent_mut()?;
        intent.set_phase(IntentPhase::Running);
        let intent = self.intent.as_ref().expect("intent set during prepare");
        persist_intent(&self.intents, intent)
    }

    fn write_ref(
        &mut self,
        session_id: &str,
        message_id: &str,
        request: &ForegroundRequest,
    ) -> Result<Self::PendingRef, OcaError> {
        let reference = self.reference()?.to_owned();
        if let Some(worktree) = &self.worktree {
            worktree
                .finish_ref(&self.refs, &reference, session_id, message_id)
                .map(PendingProductionRef::Reserved)
        } else {
            self.refs
                .allocate_reserved(
                    &reference,
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
                .map_err(|error| state_error("could not complete dispatch ref", error))
        }
    }

    fn acknowledge(
        &mut self,
        pending: Self::PendingRef,
        model: &ResolvedModel,
        json: bool,
    ) -> Result<String, OcaError> {
        let headed_role = self
            .intent
            .as_ref()
            .and_then(|intent| intent.requested.as_ref())
            .filter(|request| matches!(request.display.as_deref(), Some("herdr" | "tmux")))
            .map(|request| request.role.clone());
        let pending = match pending {
            PendingProductionRef::Reserved(reference) => {
                print_ack(&reference, model, json, headed_role.as_deref()).map_err(io_error)?;
                return Ok(reference);
            }
            PendingProductionRef::Allocated(pending) => *pending,
        };
        match self.post_ack_durability {
            PostAckDurability::Complete => {
                let completion = pending
                    .acknowledge_with(|record| {
                        print_ack(&record.id, model, json, headed_role.as_deref())
                    })
                    .map_err(io_error)?;
                if let Some(warning) = completion.durability_warning() {
                    eprintln!("warning: {warning}");
                }
                Ok(completion.record().id.clone())
            }
            PostAckDurability::Transfer => {
                let reference = pending.record().id.clone();
                print_ack(&reference, model, json, headed_role.as_deref()).map_err(io_error)?;
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
        display_name: &str,
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
            .arg(display_name)
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
        let mut streamed_reply = None;
        loop {
            let event = match subscription.next().await {
                Ok(Some(event)) => event,
                Ok(None) => {
                    if let Some(reply) = self.read_attributed_reply(session_id, message_id).await? {
                        return Ok(reply);
                    }
                    return Err(OcaError::new(ErrorCode::ProtocolMismatch)
                        .with_error("OpenCode event stream closed before a terminal event"));
                }
                Err(stream_error) => {
                    if let Some(reply) = self.read_attributed_reply(session_id, message_id).await? {
                        return Ok(reply);
                    }
                    return Err(OcaError::new(sse_failure_code(&stream_error))
                        .with_error(format!("OpenCode event stream failed: {stream_error}")));
                }
            };
            if let Some(structured) = attributed_streamed_reply(&event, session_id, message_id) {
                streamed_reply = Some(TerminalReply { structured });
            }
            if is_target_session_idle(&event, session_id) {
                if let Some(reply) = streamed_reply.take() {
                    return Ok(reply);
                }
                if let Some(reply) = self.read_attributed_reply(session_id, message_id).await? {
                    return Ok(reply);
                }
                return Err(OcaError::new(ErrorCode::ProtocolMismatch).with_error(
                    "completed attributed assistant message chain has no valid worker status reply",
                ));
            }
        }
    }

    fn terminal_observed(&mut self, _reference: &str) -> Result<(), OcaError> {
        let intent = self.intent_mut()?;
        intent.set_phase(IntentPhase::TerminalObserved);
        let intent = self.intent.as_ref().expect("intent set during prepare");
        persist_intent(&self.intents, intent)
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

/// Maps one event-stream failure onto its truthful public code: a lost
/// connection is an unreachable server, while a connected server that supplies
/// an undecodable body is a protocol mismatch.
fn sse_failure_code(error: &SseError) -> ErrorCode {
    match error {
        SseError::Source {
            kind: SseSourceErrorKind::Transport,
            ..
        } => ErrorCode::ServerUnreachable,
        SseError::Source {
            kind: SseSourceErrorKind::Protocol,
            ..
        }
        | SseError::InvalidUtf8 { .. } => ErrorCode::ProtocolMismatch,
    }
}

fn user_prompt_is_visible(
    messages: &[MessageWithParts],
    session_id: &str,
    message_id: &str,
    expected_hash: &str,
) -> bool {
    messages.iter().any(|message| {
        message.info.get("role").and_then(serde_json::Value::as_str) == Some("user")
            && message
                .info
                .get("sessionID")
                .and_then(serde_json::Value::as_str)
                == Some(session_id)
            && message.info.get("id").and_then(serde_json::Value::as_str) == Some(message_id)
            && prompt_sha256(&message_text(message)) == expected_hash
    })
}

fn message_text(message: &MessageWithParts) -> String {
    message
        .parts
        .iter()
        .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>()
        .join("")
}

fn print_ack(
    reference: &str,
    model: &ResolvedModel,
    json: bool,
    headed_role: Option<&str>,
) -> io::Result<()> {
    let document = Acknowledgement::from_resolved(reference, "running", model);
    let mut rendered = if json {
        document.render_json()
    } else {
        document.render_toon()
    };
    if !json && let Some(role) = headed_role {
        let binding = binding_identity(
            reference,
            role,
            &format!("{}/{}", model.provider, model.model),
            &model.variant,
        );
        rendered.push_str(&format!("headed: {binding}\n"));
    }
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

fn request_display(intent: Option<&Intent>) -> &str {
    intent
        .and_then(|intent| intent.requested.as_ref())
        .and_then(|requested| requested.display.as_deref())
        .unwrap_or("headless")
}

fn server_state_error(context: &str, error: io::Error) -> OcaError {
    state_error(context, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lost_connection_and_an_undecodable_body_get_different_codes() {
        assert_eq!(
            sse_failure_code(&SseError::Source {
                message: "connection reset".to_owned(),
                kind: SseSourceErrorKind::Transport,
            }),
            ErrorCode::ServerUnreachable,
            "a server that went away must not be reported as a protocol mismatch"
        );
        assert_eq!(
            sse_failure_code(&SseError::Source {
                message: "error decoding response body".to_owned(),
                kind: SseSourceErrorKind::Protocol,
            }),
            ErrorCode::ProtocolMismatch,
            "a reachable server that garbles its body must not be reported unreachable"
        );
        assert_eq!(
            sse_failure_code(&SseError::InvalidUtf8 { line: Vec::new() }),
            ErrorCode::ProtocolMismatch
        );
    }
}
