//! Foreground-specific projections over facade responses and SSE frames.

use serde_json::Value;

use crate::{MessageWithParts, SseEvent};

/// Returns the structured assistant reply attributed to `message_id`, if the
/// projected message read model already contains one.
#[must_use]
pub fn attributed_structured_reply(
    messages: &[MessageWithParts],
    session_id: &str,
    message_id: &str,
) -> Option<Value> {
    messages.iter().rev().find_map(|message| {
        let info = message.info.as_object()?;
        (info.get("role")?.as_str()? == "assistant"
            && info.get("sessionID")?.as_str()? == session_id
            && info.get("parentID")?.as_str()? == message_id)
            .then(|| info.get("structured").cloned())
            .flatten()
            .filter(|structured| !structured.is_null())
    })
}

/// Recognizes a target session's idle event in both the legacy event envelope
/// and direct named-event recordings.
#[must_use]
pub fn is_target_session_idle(event: &SseEvent, session_id: &str) -> bool {
    let Ok(payload) = serde_json::from_str::<Value>(&event.data) else {
        return false;
    };
    let payload_type = payload.get("type").and_then(Value::as_str);
    let named_idle =
        event.event.as_deref() == Some("session.idle") || payload_type == Some("session.idle");
    if !named_idle {
        return false;
    }

    payload
        .get("properties")
        .or_else(|| payload.get("data"))
        .unwrap_or(&payload)
        .get("sessionID")
        .and_then(Value::as_str)
        == Some(session_id)
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, json};

    use super::{attributed_structured_reply, is_target_session_idle};
    use crate::{MessageWithParts, SseEvent};

    #[test]
    fn reconciliation_selects_only_the_attributed_structured_assistant() {
        let messages = vec![
            message(json!({
                "role":"assistant", "sessionID":"ses_target",
                "parentID":"msg_other", "structured":{"status":"done"}
            })),
            message(json!({
                "role":"assistant", "sessionID":"ses_target",
                "parentID":"msg_target", "structured":{"status":"partial"}
            })),
        ];

        assert_eq!(
            attributed_structured_reply(&messages, "ses_target", "msg_target"),
            Some(json!({"status":"partial"}))
        );
        assert_eq!(
            attributed_structured_reply(&messages, "ses_other", "msg_target"),
            None
        );
    }

    #[test]
    fn idle_filter_accepts_legacy_and_direct_envelopes_for_only_the_target() {
        for event in [
            SseEvent {
                id: Some("evt_1".to_owned()),
                event: None,
                data: json!({
                    "type":"session.idle",
                    "properties":{"sessionID":"ses_target"}
                })
                .to_string(),
            },
            SseEvent {
                id: Some("evt_2".to_owned()),
                event: Some("session.idle".to_owned()),
                data: json!({"sessionID":"ses_target"}).to_string(),
            },
        ] {
            assert!(is_target_session_idle(&event, "ses_target"));
            assert!(!is_target_session_idle(&event, "ses_other"));
        }
    }

    fn message(info: serde_json::Value) -> MessageWithParts {
        MessageWithParts {
            info,
            parts: Vec::new(),
            extra: Map::new(),
        }
    }
}
