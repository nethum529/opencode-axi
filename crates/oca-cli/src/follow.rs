//! Executable `oca f` wiring kept separate from parser-owned CLI grammar.

use std::{fmt::Write as _, path::Path, time::Duration};

use oca_core::{
    ErrorCode, EventJournalWriter, FollowError, FollowExit, FollowOutcome, FollowTarget,
    FollowTerminal, OcaError, OcaEvent, RefId, ReplyContract, WorkerState, decode_role_reply,
    follow_until_terminal_from_cursor, validate_reply_floor,
};
use oca_opencode::OpenCodeClient;
use oca_server::ConnectOrStart;
use oca_state::{
    EventJournal, Intent, IntentDurability, IntentPhase, IntentStore, OcaConfig, RefStore,
    RefStorePaths,
};
use serde_json::{Map, Value};
use url::Url;

use crate::{
    FollowCommand,
    crash_recovery::{ReconcileCommand, event_cursor_failpoint, persist_intent, reconcile_ref},
    worktree_dispatch::finalize_turn,
};

/// Successful command execution, including non-zero frozen follow outcomes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FollowCommandOutput {
    pub exit: FollowExit,
    pub stdout: String,
}

/// Resolves one ref and follows its current dispatch against the discovered server.
///
/// Timeout and server-unreachable branches never call a ref mutation or abort endpoint.
///
/// # Errors
///
/// Returns one stable CLI error for missing state, protocol mismatches, and journal failures.
pub async fn execute_follow(
    command: &FollowCommand,
    home: impl AsRef<Path>,
) -> Result<FollowCommandOutput, OcaError> {
    let home = home.as_ref();
    let state_directory = home.join(".oca");
    reconcile_ref(home, &command.reference, ReconcileCommand::Follow).await?;
    let store = RefStore::with_paths(RefStorePaths::in_directory(&state_directory));
    let record = store
        .resolve(&command.reference)
        .map_err(|error| state_error(&command.reference, &error.to_string()))?
        .ok_or_else(|| {
            OcaError::new(ErrorCode::UnknownRef)
                .with_ref(&command.reference)
                .with_help("Run `oca ls --all` to list refs")
        })?;
    if record.tombstoned {
        return Err(OcaError::new(ErrorCode::SessionNotFound).with_ref(&command.reference));
    }
    let message_id = record.message_id.clone().ok_or_else(|| {
        OcaError::new(ErrorCode::PromptUncertain)
            .with_ref(&command.reference)
            .with_error("The ref has no dispatch message id to attribute")
            .with_help(format!(
                "Inspect `oca events {}` before retrying",
                command.reference
            ))
    })?;
    let config = OcaConfig::load_from_home(home).map_err(|error| {
        OcaError::new(ErrorCode::Usage)
            .with_error(format!("failed to load configuration: {error}"))
            .with_help("fix ~/.oca/config.toml and retry")
    })?;
    let server = ConnectOrStart::from_home(home, &config.server)
        .read_record()
        .map_err(|error| server_unreachable(&command.reference, &error.to_string()))?
        .ok_or_else(|| server_unreachable(&command.reference, "no server discovery record"))?;
    let base_url = Url::parse(&format!("http://127.0.0.1:{}", server.port)).map_err(|error| {
        OcaError::new(ErrorCode::ProtocolMismatch)
            .with_ref(&command.reference)
            .with_error(format!("invalid discovered server URL: {error}"))
    })?;
    let client = OpenCodeClient::new(base_url);
    let reference = RefId::new(&command.reference).map_err(|error| {
        OcaError::new(ErrorCode::Usage)
            .with_error(error.to_string())
            .with_ref(&command.reference)
    })?;
    let journal = EventJournal::create(&state_directory, &reference, &message_id)
        .map_err(|error| journal_error(&command.reference, &error.to_string()))?;
    let intent_store = IntentStore::in_directory(&state_directory);
    let intent = intent_store
        .read(&command.reference)
        .map_err(|error| journal_error(&command.reference, &error.to_string()))?;
    let initial_cursor = intent
        .as_ref()
        .and_then(|intent| intent.event_cursor.clone());
    let mut journal = RecoveryJournal {
        journal,
        intents: intent_store.clone(),
        intent,
    };
    let target = FollowTarget {
        session_id: record.session_id.clone(),
        message_id,
    };
    let timeout = command.timeout_seconds.map(Duration::from_secs);
    let outcome = follow_until_terminal_from_cursor(
        &client,
        &target,
        timeout,
        Some(&mut journal),
        initial_cursor.as_deref(),
    )
    .await
    .map_err(|error| follow_error(&command.reference, error))?;
    journal
        .finish()
        .map_err(|error| journal_error(&command.reference, &error.to_string()))?;

    match outcome {
        FollowOutcome::Terminal(terminal) => {
            if let Some(intent) = journal.intent.as_mut() {
                intent.set_phase(IntentPhase::TerminalObserved);
                persist_intent(&journal.intents, intent)?;
            }
            let reply = validate_terminal_reply(&record, &terminal)?;
            finalize_turn(&store, &command.reference, &reply)?;
            Ok(FollowCommandOutput {
                exit: if terminal.state == WorkerState::Blocked {
                    FollowExit::Blocked
                } else {
                    FollowExit::Success
                },
                stdout: render_terminal(&command.reference, &terminal, command.json),
            })
        }
        FollowOutcome::Timeout => Ok(FollowCommandOutput {
            exit: FollowExit::Timeout,
            stdout: format!("oca wake ref={} state=timeout\n", command.reference),
        }),
        FollowOutcome::ServerUnreachable => Err(server_unreachable(
            &command.reference,
            "the OpenCode server could not be reached",
        )),
    }
}

struct RecoveryJournal {
    journal: EventJournal,
    intents: IntentStore,
    intent: Option<Intent>,
}

impl RecoveryJournal {
    fn finish(&self) -> Result<(), oca_state::JournalError> {
        self.journal.finish()
    }
}

impl EventJournalWriter for RecoveryJournal {
    fn append(&mut self, event: &OcaEvent) -> Result<(), String> {
        self.journal
            .append(event)
            .map_err(|error| error.to_string())?;
        if let Some(cursor) = event.cursor.as_ref()
            && let Some(intent) = self.intent.as_mut()
        {
            intent.event_cursor = Some(cursor.clone());
            intent.set_phase(IntentPhase::Running);
            self.intents
                .write(intent, IntentDurability::PreAck)
                .map_err(|error| error.to_string())?;
            event_cursor_failpoint();
        }
        Ok(())
    }
}

fn validate_terminal_reply(
    record: &oca_state::RefRecord,
    terminal: &FollowTerminal,
) -> Result<oca_core::RoleReply, OcaError> {
    let role = record.role.as_deref().ok_or_else(|| {
        OcaError::new(ErrorCode::ProtocolMismatch)
            .with_ref(&record.id)
            .with_error("the ref has no stored role for reply validation")
    })?;
    let structured = terminal.message.reply().cloned().ok_or_else(|| {
        OcaError::new(ErrorCode::ContractInvalid)
            .with_ref(&record.id)
            .with_error("terminal worker message has no structured reply")
    })?;
    let reply = decode_role_reply(ReplyContract::resolve(role)?, structured)?;
    validate_reply_floor(&reply)?;
    Ok(reply)
}

fn render_terminal(reference: &str, terminal: &FollowTerminal, json: bool) -> String {
    let state = state_name(terminal.state);
    let mut output = format!("oca wake ref={reference} state={state}\n");
    let mut reply = terminal
        .message
        .reply()
        .cloned()
        .unwrap_or_else(|| serde_json::json!({ "status": state }));
    if json {
        if !reply.is_object() {
            reply = Value::Object(Map::from_iter([("reply".to_owned(), reply)]));
        }
        let object = reply
            .as_object_mut()
            .expect("reply was normalized to an object");
        object.insert("ref".to_owned(), Value::String(reference.to_owned()));
        writeln!(
            output,
            "{}",
            serde_json::to_string(&reply).expect("reply JSON is already serializable")
        )
        .expect("writing to a String cannot fail");
        return output;
    }

    if let Some(object) = reply.as_object() {
        if let Some(status) = object.get("status").and_then(Value::as_str) {
            writeln!(output, "status: {status}").expect("writing to a String cannot fail");
        }
        if let Some(files) = object.get("files").and_then(Value::as_array) {
            let count = files.len();
            let files = files
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(output, "files[{count}]: {files}").expect("writing to a String cannot fail");
        }
        if let Some(note) = object.get("note").and_then(Value::as_str) {
            writeln!(
                output,
                "note: {}",
                serde_json::to_string(note).expect("a string is serializable")
            )
            .expect("writing to a String cannot fail");
        }
        if object.get("status").is_none() {
            writeln!(output, "reply: {reply}").expect("writing to a String cannot fail");
        }
    } else {
        writeln!(output, "reply: {reply}").expect("writing to a String cannot fail");
    }
    writeln!(output, "ref: {reference}").expect("writing to a String cannot fail");
    output
}

const fn state_name(state: WorkerState) -> &'static str {
    match state {
        WorkerState::Done => "done",
        WorkerState::Blocked => "blocked",
        WorkerState::Partial => "partial",
    }
}

fn state_error(reference: &str, detail: &str) -> OcaError {
    OcaError::new(ErrorCode::UnknownRef)
        .with_ref(reference)
        .with_error(format!("could not read ref state: {detail}"))
}

fn server_unreachable(reference: &str, detail: &str) -> OcaError {
    OcaError::new(ErrorCode::ServerUnreachable)
        .with_ref(reference)
        .with_error(format!("OpenCode server unreachable: {detail}"))
        .with_help(format!(
            "Retry `oca f {reference}`; sessions persist on disk"
        ))
}

fn journal_error(reference: &str, detail: &str) -> OcaError {
    OcaError::new(ErrorCode::EventsCorrupt)
        .with_ref(reference)
        .with_error(format!("event journal failed: {detail}"))
        .with_help(format!("Inspect `oca events {reference}`"))
}

fn follow_error(reference: &str, error: FollowError) -> OcaError {
    match error {
        FollowError::Protocol { message } => OcaError::new(ErrorCode::ProtocolMismatch)
            .with_ref(reference)
            .with_error(format!("OpenCode follow protocol mismatch: {message}")),
        FollowError::Journal { message } => journal_error(reference, &message),
        FollowError::RateLimited {
            message,
            retry_after_ms,
        } => {
            let error = OcaError::new(ErrorCode::RateLimited)
                .with_ref(reference)
                .with_error(message);
            match retry_after_ms {
                Some(delay) => error.with_retry_after_ms(delay),
                None => error,
            }
        }
    }
}
