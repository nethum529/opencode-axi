//! Headed-attach identity and unreadable-history diagnostics.

use oca_core::{EventJournalWriter, OcaEvent, resolve_model};
use oca_opencode::{OpenCodeClient, OpenCodeError};
use oca_state::{OcaConfig, RefRecord};
use serde_json::json;

pub(crate) const HISTORY_UNREADABLE_EVENT: &str = "oca.history.unreadable";

const COMPOSER_GUARD: &str = "DO NOT TYPE: composer unbound";
const HISTORY_IDENTITY_WARNING: &str = "HISTORY UNREADABLE";
const RETRY_POISONING_IDENTITY_SUFFIX: &str = ": retryCount poisoning";

/// Only the schema rejection behind the upstream poisoning answers with 400. Any
/// other status is a different failure and must not be attributed to it.
const RETRY_POISONING_STATUS: u16 = 400;

/// Result of the explicit history read made at a user-visible attach/follow boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum HistoryProbe {
    Readable,
    Unreadable {
        status: u16,
    },
    RateLimited {
        body: String,
        retry_after_ms: Option<u64>,
    },
    Indeterminate,
}

impl HistoryProbe {
    pub(crate) fn warning(&self) -> Option<String> {
        let status = self.status()?;
        Some(format!(
            "OpenCode session history is unreadable (HTTP {status}); {}, so the attached TUI can render an empty conversation",
            cause(status)
        ))
    }

    const fn status(&self) -> Option<u16> {
        match self {
            Self::Unreadable { status } => Some(*status),
            Self::RateLimited { .. } => Some(429),
            Self::Readable | Self::Indeterminate => None,
        }
    }
}

fn cause(status: u16) -> &'static str {
    if status == RETRY_POISONING_STATUS {
        "known upstream retryCount-in-format poisoning rejects the stored messages"
    } else {
        "the server refused the history read"
    }
}

/// Probes the endpoint that the attached OpenCode TUI needs to render history.
pub(crate) async fn probe_history(client: &OpenCodeClient, session_id: &str) -> HistoryProbe {
    match client.messages(session_id).await {
        Ok(_) => HistoryProbe::Readable,
        Err(OpenCodeError::Server { status, .. }) => HistoryProbe::Unreadable { status },
        Err(OpenCodeError::RateLimited { body, limit }) => HistoryProbe::RateLimited {
            body,
            retry_after_ms: limit.retry_after_ms(),
        },
        Err(OpenCodeError::ProtocolMismatch { .. } | OpenCodeError::Transport { .. }) => {
            HistoryProbe::Indeterminate
        }
    }
}

/// Adds one public event explaining why an attached conversation may render empty.
pub(crate) fn journal_history_diagnostic<J: EventJournalWriter>(
    journal: &mut J,
    session_id: &str,
    probe: &HistoryProbe,
) -> Result<(), String> {
    let Some(status) = probe.status() else {
        return Ok(());
    };
    journal.append(&OcaEvent {
        id: None,
        cursor: None,
        kind: HISTORY_UNREADABLE_EVENT.to_owned(),
        session_id: Some(session_id.to_owned()),
        payload: Some(json!({
            "status": status,
            "condition": "session history unreadable",
            "cause": cause(status),
            "effect": "attached TUI may render an empty conversation",
        })),
        message: None,
        known: true,
    })
}

/// Builds the worker identity shown separately from the task-derived display label.
pub(crate) fn worker_identity(
    reference: &str,
    record: &RefRecord,
    config: &OcaConfig,
    probe: &HistoryProbe,
) -> String {
    let agent = record.role.as_deref().unwrap_or("unknown-agent");
    let alias = record.alias.as_deref().unwrap_or("unknown-model");
    let effort = record.effort.as_deref().unwrap_or("unknown-variant");
    let (model, variant) = resolve_model(alias, effort, config.model_catalog()).map_or_else(
        |_| (alias.to_owned(), effort.to_owned()),
        |resolved| {
            (
                format!("{}/{}", resolved.provider, resolved.model),
                resolved.variant,
            )
        },
    );
    let mut identity = format!("{reference} | {agent} | {model} | {variant} | {COMPOSER_GUARD}");
    if let Some(status) = probe.status() {
        identity.push_str(" | ");
        identity.push_str(HISTORY_IDENTITY_WARNING);
        if status == RETRY_POISONING_STATUS {
            identity.push_str(RETRY_POISONING_IDENTITY_SUFFIX);
        }
    }
    identity
}

#[cfg(test)]
mod tests {
    use oca_state::RefState;

    use super::*;

    #[test]
    fn identity_names_the_worker_binding_and_guards_the_unbound_composer() {
        let identity = worker_identity(
            "wabc12",
            &record("flash", "high", "impl"),
            &OcaConfig::default(),
            &HistoryProbe::Readable,
        );

        assert_eq!(
            identity,
            "wabc12 | impl | opencode/deepseek-v4-flash-free | high | DO NOT TYPE: composer unbound"
        );
    }

    #[test]
    fn unreadable_history_is_visible_in_the_worker_identity() {
        let identity = worker_identity(
            "wabc12",
            &record("flash", "high", "impl"),
            &OcaConfig::default(),
            &HistoryProbe::Unreadable { status: 400 },
        );

        assert!(identity.contains("HISTORY UNREADABLE: retryCount poisoning"));
    }

    #[test]
    fn a_history_failure_other_than_the_schema_rejection_is_not_blamed_on_retry_count() {
        let probe = HistoryProbe::Unreadable { status: 503 };
        let identity = worker_identity(
            "wabc12",
            &record("flash", "high", "impl"),
            &OcaConfig::default(),
            &probe,
        );

        assert!(identity.ends_with("| HISTORY UNREADABLE"));
        let warning = probe.warning().expect("a rejected read warns");
        assert!(warning.contains("(HTTP 503)"));
        assert!(!warning.contains("retryCount"));
    }

    fn record(alias: &str, effort: &str, role: &str) -> RefRecord {
        RefRecord {
            id: "wabc12".to_owned(),
            session_id: "ses_target".to_owned(),
            message_id: Some("msg_dispatch".to_owned()),
            alias: Some(alias.to_owned()),
            effort: Some(effort.to_owned()),
            role: Some(role.to_owned()),
            cwd: Some("/worker".to_owned()),
            last_state: Some(RefState::Running),
            repo: Some("/worker".to_owned()),
            spawner_tag: None,
            worktree: None,
            branch: None,
            commit: None,
            commit_subject: None,
            display: Some("herdr".to_owned()),
            herdr_tab: None,
            completion: None,
            tombstoned: false,
        }
    }
}
