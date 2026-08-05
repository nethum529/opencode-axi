//! Intent phase transitions and the only lazy reconciliation entry points.

use std::{path::Path, process::Command};

use oca_core::{ErrorCode, OcaError, RoleReply};
use oca_git::{RefId, cleanup_orphaned_worktree};
use oca_opencode::{MessageWithParts, OpenCodeClient};
use oca_server::ConnectOrStart;
use oca_state::{
    Intent, IntentPhase, IntentStore, IntentStoreError, OcaConfig, RefPatch, RefState, RefStore,
    RefStorePaths,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;

pub(crate) const RESERVED_SESSION_ID: &str = "oca_worktree_session_pending";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReconcileCommand {
    List,
    Follow,
    Message,
}

/// The phase which should override the ordinary ref state in `oca ls`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReconciledState {
    PromptUncertain,
    PublishedUncertain,
    SessionCreated,
    TerminalObserved,
}

pub(crate) fn prompt_sha256(prompt: &str) -> String {
    format!("{:x}", Sha256::digest(prompt.as_bytes()))
}

pub(crate) fn persist_intent(store: &IntentStore, intent: &Intent) -> Result<(), OcaError> {
    store.write(intent).map_err(intent_error)?;
    failpoint(intent.phase);
    Ok(())
}

pub(crate) fn remove_intent(store: &IntentStore, reference: &str) -> Result<(), OcaError> {
    store.remove(reference).map(|_| ()).map_err(intent_error)
}

/// Reconciles every visible intent for `oca ls` and returns state overrides.
pub(crate) async fn reconcile_for_list(
    home: &Path,
) -> Result<Vec<(String, ReconciledState)>, OcaError> {
    let intents = IntentStore::in_directory(home.join(".oca"))
        .list()
        .map_err(intent_error)?;
    let mut states = Vec::new();
    for intent in intents {
        if let Some(state) = reconcile_intent(home, intent, ReconcileCommand::List).await? {
            states.push(state);
        }
    }
    Ok(states)
}

/// Reconciles one ref before `oca f` or `oca m` resolves normal ref state.
pub(crate) async fn reconcile_ref(
    home: &Path,
    reference: &str,
    command: ReconcileCommand,
) -> Result<Option<ReconciledState>, OcaError> {
    let store = IntentStore::in_directory(home.join(".oca"));
    let Some(intent) = store.read(reference).map_err(intent_error)? else {
        return Ok(None);
    };
    reconcile_intent(home, intent, command)
        .await
        .map(|result| result.map(|(_, state)| state))
}

async fn reconcile_intent(
    home: &Path,
    mut intent: Intent,
    command: ReconcileCommand,
) -> Result<Option<(String, ReconciledState)>, OcaError> {
    let state_directory = home.join(".oca");
    let intents = IntentStore::in_directory(&state_directory);
    let refs = RefStore::with_paths(RefStorePaths::in_directory(&state_directory));
    match intent.phase {
        IntentPhase::Planned | IntentPhase::WorktreeReady => {
            let record = refs
                .resolve(&intent.reference)
                .map_err(|error| recovery_error(&intent.reference, error))?;
            if let Some(record) = record
                && record.session_id != RESERVED_SESSION_ID
            {
                intent.session_id = Some(record.session_id);
                intent.set_phase(IntentPhase::SessionCreated);
                persist_intent(&intents, &intent)?;
                return Ok(Some((intent.reference, ReconciledState::SessionCreated)));
            }
            cleanup_pre_session_strand(&refs, &intents, &intent)?;
            Ok(None)
        }
        IntentPhase::SessionCreated => {
            // Querying the session before any new prompt is the recovery action.
            // A failed list-side query leaves the durable phase visible.
            let result = query_messages(home, &intent).await;
            match result {
                Ok(()) => {
                    let session_id =
                        required_intent(&intent, intent.session_id.as_deref(), "session id")?;
                    let state = if command == ReconcileCommand::Message {
                        RefState::Idle
                    } else {
                        RefState::Running
                    };
                    refs.patch(
                        &intent.reference,
                        RefPatch::default()
                            .with_session_id(session_id)
                            .with_last_state(state),
                    )
                    .map_err(|error| recovery_error(&intent.reference, error))?;
                    if command == ReconcileCommand::Message {
                        remove_intent(&intents, &intent.reference)?;
                        return Ok(None);
                    }
                }
                Err(error) if command != ReconcileCommand::List => return Err(error),
                Err(_) => {}
            }
            Ok(Some((intent.reference, ReconciledState::SessionCreated)))
        }
        IntentPhase::PromptUncertain => {
            let landed = match prompt_landed(home, &intent).await {
                Ok(landed) => landed,
                Err(_) if command == ReconcileCommand::List => {
                    return Ok(Some((intent.reference, ReconciledState::PromptUncertain)));
                }
                Err(error) => return Err(error),
            };
            if landed {
                let session_id =
                    required_intent(&intent, intent.session_id.as_deref(), "session id")?;
                let message_id =
                    required_intent(&intent, intent.message_id.as_deref(), "message id")?;
                refs.patch(
                    &intent.reference,
                    RefPatch::default()
                        .with_session_id(session_id)
                        .with_message_id(message_id)
                        .with_last_state(RefState::Running),
                )
                .map_err(|error| recovery_error(&intent.reference, error))?;
                intent.set_phase(IntentPhase::Running);
                persist_intent(&intents, &intent)?;
                return Ok(None);
            }

            match command {
                ReconcileCommand::Message => {
                    // This invocation is the explicit operator-authorized resend.
                    refs.patch(
                        &intent.reference,
                        RefPatch::default().with_last_state(RefState::Idle),
                    )
                    .map_err(|error| recovery_error(&intent.reference, error))?;
                    remove_intent(&intents, &intent.reference)?;
                    Ok(None)
                }
                ReconcileCommand::List => {
                    Ok(Some((intent.reference, ReconciledState::PromptUncertain)))
                }
                ReconcileCommand::Follow => Err(prompt_uncertain(&intent.reference)),
            }
        }
        IntentPhase::Committed => {
            settle_committed(&refs, &intents, &intent)?;
            Ok(None)
        }
        IntentPhase::TerminalObserved | IntentPhase::Validated => {
            if adopt_existing_commit(&refs, &intents, &mut intent)? {
                Ok(None)
            } else if let Some(reply) = intent.terminal_reply.as_ref() {
                crate::worktree_dispatch::finalize_turn(&refs, &intent.reference, reply)?;
                Ok(None)
            } else {
                Ok(Some((intent.reference, ReconciledState::TerminalObserved)))
            }
        }
        IntentPhase::PublishedUncertain => Ok(Some((
            intent.reference,
            ReconciledState::PublishedUncertain,
        ))),
        IntentPhase::Running => {
            if refs
                .resolve(&intent.reference)
                .map_err(|error| recovery_error(&intent.reference, error))?
                .and_then(|record| record.last_state)
                .is_some_and(|state| !matches!(state, RefState::Running | RefState::Unknown))
            {
                remove_intent(&intents, &intent.reference)?;
                return Ok(None);
            }
            if let (Some(session_id), Some(message_id)) =
                (intent.session_id.as_deref(), intent.message_id.as_deref())
            {
                refs.patch(
                    &intent.reference,
                    RefPatch::default()
                        .with_session_id(session_id)
                        .with_message_id(message_id)
                        .with_last_state(RefState::Running),
                )
                .map_err(|error| recovery_error(&intent.reference, error))?;
            }
            Ok(None)
        }
    }
}

fn cleanup_pre_session_strand(
    refs: &RefStore,
    intents: &IntentStore,
    intent: &Intent,
) -> Result<(), OcaError> {
    if intent
        .requested
        .as_ref()
        .is_some_and(|request| request.worktree)
    {
        let request = intent.requested.as_ref().expect("checked above");
        let reference = RefId::new(&intent.reference).map_err(|error| {
            OcaError::new(ErrorCode::WorktreeConflict)
                .with_ref(&intent.reference)
                .with_error(error.to_string())
        })?;
        cleanup_orphaned_worktree(Path::new(&request.repo), &reference).map_err(|error| {
            OcaError::new(ErrorCode::WorktreeDirty)
                .with_ref(&intent.reference)
                .with_error(format!("could not clean stranded worktree: {error}"))
        })?;
    }
    if refs
        .resolve(&intent.reference)
        .map_err(|error| recovery_error(&intent.reference, error))?
        .is_some()
    {
        refs.tombstone(&intent.reference)
            .map_err(|error| recovery_error(&intent.reference, error))?;
    }
    remove_intent(intents, &intent.reference)
}

async fn prompt_landed(home: &Path, intent: &Intent) -> Result<bool, OcaError> {
    let client = discovered_client(home, &intent.reference)?;
    // Subscribe first. A terminal event observed later is accepted only through
    // the follow tracker that matches the assistant's event-stream parentID.
    let _subscription = client
        .subscribe(None)
        .await
        .map_err(|error| transport_error(&intent.reference, error))?;
    let messages = client
        .messages(required_intent(
            intent,
            intent.session_id.as_deref(),
            "session id",
        )?)
        .await
        .map_err(|error| transport_error(&intent.reference, error))?;
    Ok(user_prompt_matches(&messages, intent))
}

async fn query_messages(home: &Path, intent: &Intent) -> Result<(), OcaError> {
    discovered_client(home, &intent.reference)?
        .messages(required_intent(
            intent,
            intent.session_id.as_deref(),
            "session id",
        )?)
        .await
        .map(|_| ())
        .map_err(|error| transport_error(&intent.reference, error))
}

fn user_prompt_matches(messages: &[MessageWithParts], intent: &Intent) -> bool {
    let Some(session_id) = intent.session_id.as_deref() else {
        return false;
    };
    let Some(message_id) = intent.message_id.as_deref() else {
        return false;
    };
    let Some(expected_hash) = intent.prompt_sha256.as_deref() else {
        return false;
    };
    messages.iter().any(|message| {
        message.info.get("role").and_then(Value::as_str) == Some("user")
            && message.info.get("sessionID").and_then(Value::as_str) == Some(session_id)
            && message.info.get("id").and_then(Value::as_str) == Some(message_id)
            && prompt_sha256(&message_text(message)) == expected_hash
    })
}

fn message_text(message: &MessageWithParts) -> String {
    message
        .parts
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
}

fn settle_committed(
    refs: &RefStore,
    intents: &IntentStore,
    intent: &Intent,
) -> Result<(), OcaError> {
    let commit = required_intent(intent, intent.commit_id.as_deref(), "commit id")?;
    let reply = intent.terminal_reply.clone().ok_or_else(|| {
        recovery_protocol(&intent.reference, "committed intent has no terminal reply")
    })?;
    refs.patch(
        &intent.reference,
        RefPatch::default()
            .with_commit(commit)
            .with_last_state(reply_state(&reply))
            .with_completion(reply),
    )
    .map_err(|error| recovery_error(&intent.reference, error))?;
    remove_intent(intents, &intent.reference)
}

fn adopt_existing_commit(
    refs: &RefStore,
    intents: &IntentStore,
    intent: &mut Intent,
) -> Result<bool, OcaError> {
    let record = refs
        .resolve(&intent.reference)
        .map_err(|error| recovery_error(&intent.reference, error))?
        .ok_or_else(|| OcaError::new(ErrorCode::UnknownRef).with_ref(&intent.reference))?;
    let Some(worktree) = record.worktree.as_deref() else {
        return Ok(false);
    };
    let Some(summary) = record.commit_subject.as_deref() else {
        return Ok(false);
    };
    let expected_subject = format!("oca {}: {summary}", intent.reference);
    let output = Command::new("git")
        .args(["-C", worktree, "log", "-1", "--format=%H%x00%s"])
        .output()
        .map_err(|error| recovery_protocol(&intent.reference, &error.to_string()))?;
    if !output.status.success() {
        return Ok(false);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let Some((commit, subject)) = text.trim_end().split_once('\0') else {
        return Ok(false);
    };
    if subject != expected_subject {
        return Ok(false);
    }
    intent.commit_id = Some(commit.to_owned());
    intent.set_phase(IntentPhase::Committed);
    persist_intent(intents, intent)?;
    settle_committed(refs, intents, intent)?;
    Ok(true)
}

fn discovered_client(home: &Path, reference: &str) -> Result<OpenCodeClient, OcaError> {
    let config = OcaConfig::load_from_home(home).map_err(|error| {
        OcaError::new(ErrorCode::Usage)
            .with_error(format!("failed to load configuration: {error}"))
            .with_help("fix ~/.oca/config.toml and retry")
    })?;
    let server = ConnectOrStart::from_home(home, &config.server)
        .read_record()
        .map_err(|error| {
            OcaError::new(ErrorCode::ServerUnreachable)
                .with_ref(reference)
                .with_error(format!("could not read server record: {error}"))
        })?
        .ok_or_else(|| {
            OcaError::new(ErrorCode::ServerUnreachable)
                .with_ref(reference)
                .with_error("no OpenCode server record is available")
        })?;
    let url = Url::parse(&format!("http://127.0.0.1:{}", server.port)).map_err(|error| {
        recovery_protocol(
            reference,
            &format!("invalid discovered server URL: {error}"),
        )
    })?;
    Ok(OpenCodeClient::new(url))
}

fn required_intent<'a>(
    intent: &Intent,
    value: Option<&'a str>,
    field: &str,
) -> Result<&'a str, OcaError> {
    value.ok_or_else(|| {
        recovery_protocol(
            &intent.reference,
            &format!("{} intent has no {field}", intent.phase.as_str()),
        )
    })
}

fn reply_state(reply: &RoleReply) -> RefState {
    let state = match reply {
        RoleReply::Impl(reply) => reply.status,
        RoleReply::Review(reply) => reply.status,
    };
    match state {
        oca_core::WorkerState::Done => RefState::Done,
        oca_core::WorkerState::Blocked => RefState::Blocked,
        oca_core::WorkerState::Partial => RefState::Partial,
    }
}

fn prompt_uncertain(reference: &str) -> OcaError {
    OcaError::new(ErrorCode::PromptUncertain)
        .with_ref(reference)
        .with_error("the stored prompt demonstrably did not land")
        .with_help(format!(
            "Explicitly resend with `oca m {reference} \"<message>\"`"
        ))
}

fn transport_error(reference: &str, error: impl std::fmt::Display) -> OcaError {
    OcaError::new(ErrorCode::ServerUnreachable)
        .with_ref(reference)
        .with_error(format!(
            "intent reconciliation could not query OpenCode: {error}"
        ))
}

fn recovery_error(reference: &str, error: impl std::fmt::Display) -> OcaError {
    OcaError::new(ErrorCode::ServerUnavailable)
        .with_ref(reference)
        .with_error(format!("intent reconciliation failed: {error}"))
}

fn recovery_protocol(reference: &str, detail: &str) -> OcaError {
    OcaError::new(ErrorCode::ProtocolMismatch)
        .with_ref(reference)
        .with_error(format!("intent reconciliation failed: {detail}"))
}

fn intent_error(error: IntentStoreError) -> OcaError {
    OcaError::new(ErrorCode::ServerUnavailable)
        .with_error(format!("intent journal failed: {error}"))
}

fn failpoint(phase: IntentPhase) {
    if std::env::var("OCA_FAILPOINT").as_deref() == Ok(phase.as_str()) {
        // `exit` deliberately skips destructors, matching a killed process at
        // the durable phase boundary without generating a core file.
        std::process::exit(86);
    }
}

pub(crate) fn event_cursor_failpoint() {
    if std::env::var("OCA_FAILPOINT").as_deref() == Ok("event_cursor") {
        std::process::exit(86);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oca_state::IntentOperation;
    use serde_json::{Map, json};

    #[test]
    fn prompt_reconciliation_requires_id_session_and_sha256_together() {
        let prompt = "perform exactly one model turn";
        let mut intent = Intent::new("w4f2a1", IntentOperation::Dispatch);
        intent.session_id = Some("ses_target".to_owned());
        intent.message_id = Some("msg_target".to_owned());
        intent.prompt_sha256 = Some(prompt_sha256(prompt));
        let matching = message(
            json!({"id":"msg_target","sessionID":"ses_target","role":"user"}),
            prompt,
        );
        assert!(user_prompt_matches(
            std::slice::from_ref(&matching),
            &intent
        ));

        for info in [
            json!({"id":"msg_other","sessionID":"ses_target","role":"user"}),
            json!({"id":"msg_target","sessionID":"ses_other","role":"user"}),
            json!({"id":"msg_target","sessionID":"ses_target","role":"assistant"}),
        ] {
            assert!(!user_prompt_matches(&[message(info, prompt)], &intent));
        }
        assert!(!user_prompt_matches(
            &[message(matching.info, "different")],
            &intent
        ));
    }

    fn message(info: Value, text: &str) -> MessageWithParts {
        MessageWithParts {
            info,
            parts: vec![json!({"type":"text","text":text})],
            extra: Map::new(),
        }
    }
}
