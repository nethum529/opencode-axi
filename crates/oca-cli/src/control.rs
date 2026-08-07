//! Production execution for ref-scoped message, queue, and abort commands.

use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use oca_core::{
    ErrorCode, MessageIdGenerator, OcaError, RANDOM_SUFFIX_WIDTH, ReplyContract, ResolvedModel,
    WorkerPolicy, resolve_model,
};
use oca_display::{Acknowledgement, HerdrClient};
use oca_opencode::{OpenCodeClient, PromptRequest, TextPart};
use oca_server::ConnectOrStart;
use oca_state::{
    Intent, IntentOperation, IntentPhase, IntentStore, OcaConfig, RefPatch, RefRecord, RefState,
    RefStore, RefStorePaths, SessionTurnLock,
};
use url::Url;

use crate::{
    AbortCommand, MessageCommand,
    crash_recovery::{
        ReconcileCommand, persist_intent, prompt_sha256, reconcile_ref, remove_intent,
    },
    foreground::{DispatchSubscription, confirm_prompt_landed},
    transport::{open_code_error, prompt_error},
};

/// The complete stdout emitted after a control request is accepted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlCommandOutput {
    pub stdout: String,
}

/// Starts one new turn on a terminal persistent session.
///
/// # Errors
///
/// Returns `worker_busy` before opening a connection when the stored session
/// state is running. Other local state, resolution, transport, and
/// persistence failures retain their stable oca error codes.
pub async fn execute_message(
    command: &MessageCommand,
    home: impl AsRef<Path>,
) -> Result<ControlCommandOutput, OcaError> {
    let home = home.as_ref();
    let state_directory = home.join(".oca");
    let _turn_lock = acquire_turn_lock(&state_directory, &command.reference)?;
    reconcile_ref(home, &command.reference, ReconcileCommand::Message).await?;
    let store = RefStore::with_paths(RefStorePaths::in_directory(&state_directory));
    let record = resolve_active_ref(&store, &command.reference)?;
    let state = required_state(&record, &command.reference)?;
    if state == RefState::Running {
        return Err(OcaError::new(ErrorCode::WorkerBusy)
            .with_ref(&command.reference)
            .with_help(format!(
                "Use `oca q {} \"<message>\"` to queue it after the running turn",
                command.reference
            )));
    }
    if state == RefState::Unknown {
        return Err(OcaError::new(ErrorCode::PromptUncertain)
            .with_ref(&command.reference)
            .with_help(format!("Reconcile with `oca f {}`", command.reference)));
    }
    if !state.accepts_new_turn() {
        return Err(OcaError::new(ErrorCode::SessionNotFound)
            .with_ref(&command.reference)
            .with_error("the worker session was aborted"));
    }

    let config = load_config(home)?;
    let context = ControlContext::from_record(&record, &config, command.effort.as_deref())?;
    let client = discovered_client(home, &config, &command.reference)?;
    let directory = record.session_directory().ok_or_else(|| {
        OcaError::new(ErrorCode::ProtocolMismatch)
            .with_ref(&command.reference)
            .with_error("the ref has no stored session directory")
    })?;
    let mut subscription = client
        .subscribe(directory, None)
        .await
        .map(DispatchSubscription::new)
        .map_err(|error| open_code_error(error).with_ref(&command.reference))?;
    let message_id = mint_message_id()?;
    let mut intent = Intent::new(&command.reference, IntentOperation::Message);
    intent.session_id = Some(record.session_id.clone());
    intent.message_id = Some(message_id.clone());
    intent.prompt_sha256 = Some(prompt_sha256(&command.message));
    intent.set_phase(IntentPhase::PromptUncertain);
    let intents = IntentStore::in_directory(&state_directory);
    persist_intent(&intents, &intent)?;
    let result = client
        .prompt_async(
            &record.session_id,
            context.prompt_request(message_id.clone(), command.message.clone()),
        )
        .await;
    if let Err(error) = result {
        let error = prompt_error(error).with_ref(&command.reference);
        if error.code() != ErrorCode::PromptUncertain.as_str() {
            remove_intent(&intents, &command.reference)?;
        }
        return Err(error);
    }
    confirm_prompt_landed(
        &client,
        &mut subscription,
        &record.session_id,
        &message_id,
        &command.message,
    )
    .await
    .map_err(|error| error.with_ref(&command.reference))?;
    intent.set_phase(IntentPhase::Running);
    persist_intent(&intents, &intent)?;

    let mut patch = RefPatch::default()
        .with_message_id(message_id)
        .with_last_state(RefState::Running);
    if command.effort.is_some() {
        patch = patch.with_effort(&context.model.effort);
    }
    store
        .patch(&command.reference, patch)
        .map_err(|error| state_error(&command.reference, "could not update ref", error))?;

    Ok(accepted_output(
        &command.reference,
        RefState::Running.as_str(),
        &context.model,
        command.json,
    ))
}

/// Queues a message after the current turn and returns on server acceptance.
///
/// # Errors
///
/// Returns stable state, resolution, transport, or persistence failures.
pub async fn execute_queue(
    command: &MessageCommand,
    home: impl AsRef<Path>,
) -> Result<ControlCommandOutput, OcaError> {
    let home = home.as_ref();
    let state_directory = home.join(".oca");
    let _turn_lock = acquire_turn_lock(&state_directory, &command.reference)?;
    let store = RefStore::with_paths(RefStorePaths::in_directory(&state_directory));
    let record = resolve_active_ref(&store, &command.reference)?;
    match required_state(&record, &command.reference)? {
        RefState::Aborted => {
            return Err(OcaError::new(ErrorCode::SessionNotFound).with_ref(&command.reference));
        }
        RefState::Unknown => {
            return Err(OcaError::new(ErrorCode::PromptUncertain)
                .with_ref(&command.reference)
                .with_help(format!("Reconcile with `oca f {}`", command.reference)));
        }
        _ => {}
    }

    let config = load_config(home)?;
    let context = ControlContext::from_record(&record, &config, None)?;
    let client = discovered_client(home, &config, &command.reference)?;
    let message_id = mint_message_id()?;
    client
        .queue(
            &record.session_id,
            context.prompt_request(message_id.clone(), command.message.clone()),
        )
        .await
        .map_err(|error| prompt_error(error).with_ref(&command.reference))?;
    store
        .patch(
            &command.reference,
            RefPatch::default().with_message_id(message_id),
        )
        .map_err(|error| state_error(&command.reference, "could not update ref", error))?;

    Ok(accepted_output(
        &command.reference,
        "queued",
        &context.model,
        command.json,
    ))
}

/// Aborts a worker session and tears down its persisted herdr attachment.
///
/// # Errors
///
/// Returns stable ref, transport, protocol, or persistence failures.
pub async fn execute_abort(
    command: &AbortCommand,
    home: impl AsRef<Path>,
) -> Result<ControlCommandOutput, OcaError> {
    let home = home.as_ref();
    let state_directory = home.join(".oca");
    let _turn_lock = acquire_turn_lock(&state_directory, &command.reference)?;
    let store = RefStore::with_paths(RefStorePaths::in_directory(&state_directory));
    let record = resolve_active_ref(&store, &command.reference)?;
    let config = load_config(home)?;
    let context = ControlContext::from_record(&record, &config, None)?;
    let client = discovered_client(home, &config, &command.reference)?;
    let accepted = client
        .abort(&record.session_id)
        .await
        .map_err(|error| open_code_error(error).with_ref(&command.reference))?;
    if !accepted.aborted {
        return Err(OcaError::new(ErrorCode::ProtocolMismatch)
            .with_ref(&command.reference)
            .with_error("OpenCode did not acknowledge the abort"));
    }
    store
        .patch(
            &command.reference,
            RefPatch::default().with_last_state(RefState::Aborted),
        )
        .map_err(|error| state_error(&command.reference, "could not update ref", error))?;
    close_herdr_tab(home, &config, &record).await;

    Ok(accepted_output(
        &command.reference,
        RefState::Aborted.as_str(),
        &context.model,
        command.json,
    ))
}

async fn close_herdr_tab(home: &Path, config: &OcaConfig, record: &RefRecord) {
    if record.display.as_deref() != Some("herdr") {
        return;
    }
    let Some(tab_id) = record.herdr_tab.as_deref() else {
        return;
    };
    let configured_socket =
        (!config.herdr.socket.is_empty()).then(|| Path::new(config.herdr.socket.as_str()));
    let Some(herdr) = HerdrClient::discover_from(
        home,
        configured_socket,
        std::time::Duration::from_millis(config.herdr.timeout_ms),
    ) else {
        return;
    };

    // Display teardown is best effort: abort already succeeded, and a stale
    // tab ID or a stopped herdr means the desired end state already holds.
    let _ = herdr.close_tab_id(tab_id).await;
}

struct ControlContext {
    model: ResolvedModel,
    role: String,
    contract: ReplyContract,
    policy: WorkerPolicy,
}

impl ControlContext {
    fn from_record(
        record: &RefRecord,
        config: &OcaConfig,
        effort_override: Option<&str>,
    ) -> Result<Self, OcaError> {
        let alias = required_metadata(record, "alias")?;
        let stored_effort = required_metadata(record, "effort")?;
        let role = required_metadata(record, "role")?.to_owned();
        let cwd = PathBuf::from(required_metadata(record, "cwd")?);
        if !config.roles.contains_key(&role) {
            return Err(OcaError::new(ErrorCode::Usage)
                .with_ref(&record.id)
                .with_error(format!("unknown stored worker role `{role}`"))
                .with_help("restore the role in ~/.oca/config.toml and retry"));
        }
        let model = resolve_model(
            alias,
            effort_override.unwrap_or(stored_effort),
            config.model_catalog(),
        )?;
        Ok(Self {
            model,
            contract: ReplyContract::resolve(&role)?,
            policy: WorkerPolicy::restricted([cwd]),
            role,
        })
    }

    fn prompt_request(&self, message_id: String, text: String) -> PromptRequest {
        PromptRequest {
            message_id,
            model: self.model.clone(),
            variant: self.model.variant.clone(),
            role: self.role.clone(),
            parts: vec![TextPart { text }],
            output_schema: Some(self.contract.schema()),
            permission: self.policy.permission_profile(),
        }
    }
}

fn acquire_turn_lock(state_directory: &Path, reference: &str) -> Result<SessionTurnLock, OcaError> {
    SessionTurnLock::acquire(state_directory, reference).map_err(|error| {
        OcaError::new(ErrorCode::ServerUnavailable)
            .with_ref(reference)
            .with_error(format!("could not lock worker turn: {error}"))
    })
}

fn resolve_active_ref(store: &RefStore, reference: &str) -> Result<RefRecord, OcaError> {
    let record = store
        .resolve(reference)
        .map_err(|error| state_error(reference, "could not read ref", error))?
        .ok_or_else(|| OcaError::new(ErrorCode::UnknownRef).with_ref(reference))?;
    if record.tombstoned {
        return Err(OcaError::new(ErrorCode::SessionNotFound).with_ref(reference));
    }
    Ok(record)
}

fn required_state(record: &RefRecord, reference: &str) -> Result<RefState, OcaError> {
    record.last_state.ok_or_else(|| {
        OcaError::new(ErrorCode::ProtocolMismatch)
            .with_ref(reference)
            .with_error("the ref has no stored worker state")
            .with_help("start a new dispatch to create complete continuation metadata")
    })
}

fn required_metadata<'a>(record: &'a RefRecord, name: &str) -> Result<&'a str, OcaError> {
    let value = match name {
        "alias" => record.alias.as_deref(),
        "effort" => record.effort.as_deref(),
        "role" => record.role.as_deref(),
        "cwd" => record.cwd.as_deref(),
        _ => None,
    };
    value.ok_or_else(|| {
        OcaError::new(ErrorCode::ProtocolMismatch)
            .with_ref(&record.id)
            .with_error(format!("the ref has no stored {name}"))
            .with_help("start a new dispatch to create complete continuation metadata")
    })
}

fn load_config(home: &Path) -> Result<OcaConfig, OcaError> {
    OcaConfig::load_from_home(home).map_err(|error| {
        OcaError::new(ErrorCode::Usage)
            .with_error(format!("failed to load configuration: {error}"))
            .with_help("fix ~/.oca/config.toml and retry")
    })
}

fn discovered_client(
    home: &Path,
    config: &OcaConfig,
    reference: &str,
) -> Result<OpenCodeClient, OcaError> {
    let server = ConnectOrStart::from_home(home, &config.server)
        .read_record()
        .map_err(|error| {
            OcaError::new(ErrorCode::ServerUnreachable)
                .with_ref(reference)
                .with_error(format!("could not read server record: {error}"))
        })?
        .ok_or_else(|| {
            OcaError::new(ErrorCode::ServerUnavailable)
                .with_ref(reference)
                .with_error("no OpenCode server record is available")
        })?;
    let url = Url::parse(&format!("http://127.0.0.1:{}", server.port)).map_err(|error| {
        OcaError::new(ErrorCode::ProtocolMismatch)
            .with_ref(reference)
            .with_error(format!("invalid discovered server URL: {error}"))
    })?;
    Ok(OpenCodeClient::new(url))
}

fn mint_message_id() -> Result<String, OcaError> {
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
    Ok(MessageIdGenerator::new().mint(timestamp_ms, random))
}

fn accepted_output(
    reference: &str,
    state: &str,
    model: &ResolvedModel,
    json: bool,
) -> ControlCommandOutput {
    let acknowledgement = Acknowledgement::from_resolved(reference, state, model);
    ControlCommandOutput {
        stdout: if json {
            acknowledgement.render_json()
        } else {
            acknowledgement.render_toon()
        },
    }
}

fn state_error(reference: &str, context: &str, error: impl std::fmt::Display) -> OcaError {
    OcaError::new(ErrorCode::ServerUnavailable)
        .with_ref(reference)
        .with_error(format!("{context}: {error}"))
}
