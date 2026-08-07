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
    let (model, variant) = resolved_model(alias, effort, config);
    let mut identity = binding_identity(reference, agent, &model, &variant);
    if let Some(status) = probe.status() {
        identity.push_str(" | ");
        identity.push_str(HISTORY_IDENTITY_WARNING);
        if status == RETRY_POISONING_STATUS {
            identity.push_str(RETRY_POISONING_IDENTITY_SUFFIX);
        }
    }
    identity
}

/// Builds the full headed binding shown on oca-owned user-visible surfaces.
pub(crate) fn binding_identity(reference: &str, agent: &str, model: &str, variant: &str) -> String {
    format!("{reference} | {agent} | {model} | {variant} | {COMPOSER_GUARD}")
}

/// Builds a full binding for a persisted headed ref, if it has a headed display.
pub(crate) fn headed_binding(record: &RefRecord, config: &OcaConfig) -> Option<String> {
    matches!(record.display.as_deref(), Some("herdr" | "tmux")).then(|| {
        let agent = record.role.as_deref().unwrap_or("unknown-agent");
        let alias = record.alias.as_deref().unwrap_or("unknown-model");
        let effort = record.effort.as_deref().unwrap_or("unknown-variant");
        let (model, variant) = resolved_model(alias, effort, config);
        binding_identity(&record.id, agent, &model, &variant)
    })
}

/// Builds herdr's compact agent name from the dispatch binding.
pub(crate) fn herdr_agent_name(reference: &str, record: &RefRecord, config: &OcaConfig) -> String {
    let agent = record.role.as_deref().unwrap_or("unknown-agent");
    let model_short = record.alias.as_deref().unwrap_or("unknown-model");
    let effort = record.effort.as_deref().unwrap_or("unknown-variant");
    let (_, variant) = resolved_model(model_short, effort, config);
    agent_name_slug(reference, agent, model_short, &variant)
}

fn resolved_model(alias: &str, effort: &str, config: &OcaConfig) -> (String, String) {
    resolve_model(alias, effort, config.model_catalog()).map_or_else(
        |_| (alias.to_owned(), effort.to_owned()),
        |resolved| {
            (
                format!("{}/{}", resolved.provider, resolved.model),
                resolved.variant,
            )
        },
    )
}

fn agent_name_slug(reference: &str, agent: &str, model_short: &str, variant: &str) -> String {
    let raw = format!("{reference}-{agent}-{model_short}-{variant}");
    let mut slug = raw
        .bytes()
        .map(|byte| match byte.to_ascii_lowercase() {
            byte @ (b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_') => char::from(byte),
            _ => '-',
        })
        .collect::<String>();
    if !slug.starts_with(|character: char| character.is_ascii_lowercase()) {
        slug.insert_str(0, "w-");
    }
    slug.truncate(32);
    slug
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
    fn herdr_agent_slug_lowercases_and_maps_configured_punctuation() {
        assert_eq!(
            agent_name_slug("W6H6MN", "Impl/Review", "LUNA@Preview", "LOW!"),
            "w6h6mn-impl-review-luna-preview-"
        );
    }

    #[test]
    fn herdr_agent_slug_truncates_long_configured_components_to_32_characters() {
        let slug = agent_name_slug(
            "w6h6mn",
            "impl/review",
            "an-extremely-long-user-model-alias",
            "extra-high",
        );

        assert_eq!(slug, "w6h6mn-impl-review-an-extremely-");
        assert_eq!(slug.len(), 32);
        assert!(slug.starts_with(char::is_lowercase));
        assert!(slug.bytes().all(|byte| byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || matches!(byte, b'-' | b'_')));
    }

    #[test]
    fn herdr_agent_slug_is_deterministic_and_guarantees_a_lowercase_prefix() {
        let first = agent_name_slug("7REF", "Impl", "Luna", "Low");
        let second = agent_name_slug("7REF", "Impl", "Luna", "Low");

        assert_eq!(first, second);
        assert_eq!(first, "w-7ref-impl-luna-low");
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
