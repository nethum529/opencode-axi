//! OpenCode wire-shape adapter for the transport-independent follow loop.

use oca_core::{EventSubscription, FollowMessage, FollowTransport, FollowTransportError, OcaEvent};
use serde_json::Value;

use crate::{
    MessageWithParts, OpenCodeClient, OpenCodeError, SseError, SseEvent, SseSourceErrorKind,
    Subscription,
};

/// Adapts one projected REST message into the follow state machine's stable shape.
///
/// # Errors
///
/// Returns a protocol error when required message identity fields are absent.
pub fn adapt_message(message: MessageWithParts) -> Result<FollowMessage, FollowTransportError> {
    message_from_values(message.info, message.parts)
}

/// Adapts an SSE frame into a small event shape while retaining only known public payloads.
///
/// # Errors
///
/// Returns a protocol error when the frame data is not JSON or a known event is malformed.
pub fn adapt_sse_event(event: SseEvent) -> Result<OcaEvent, FollowTransportError> {
    let envelope: Value = serde_json::from_str(&event.data).map_err(|error| {
        FollowTransportError::protocol(format!("event data is not JSON: {error}"))
    })?;
    let kind = envelope
        .get("type")
        .and_then(Value::as_str)
        .or(event.event.as_deref())
        .unwrap_or("unknown")
        .to_owned();
    let envelope_id = envelope
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let event_id = envelope_id.clone().or_else(|| event.id.clone());
    let cursor = event.id.or(envelope_id);
    let properties = envelope.get("properties").unwrap_or(&envelope);
    let session_id = session_id(properties);
    let known = is_known_event(&kind);
    let message = if kind == "message.updated" {
        let info = properties
            .get("info")
            .cloned()
            .ok_or_else(|| FollowTransportError::protocol("message.updated has no info object"))?;
        Some(message_from_values(info, Vec::new())?)
    } else {
        None
    };

    Ok(OcaEvent {
        id: event_id,
        cursor,
        kind,
        session_id,
        payload: known.then_some(envelope),
        message,
        known,
    })
}

fn message_from_values(
    info: Value,
    parts: Vec<Value>,
) -> Result<FollowMessage, FollowTransportError> {
    let id = required_string(&info, "id")?;
    let session_id = required_string(&info, "sessionID")?;
    let role = required_string(&info, "role")?;
    let parent_id = info
        .get("parentID")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let completed = info
        .pointer("/time/completed")
        .is_some_and(Value::is_number)
        || info.get("finish").is_some_and(|value| !value.is_null())
        || info.get("error").is_some_and(|value| !value.is_null());
    let structured = info
        .get("structured")
        .filter(|value| !value.is_null())
        .cloned();
    let error = info.get("error").filter(|value| !value.is_null()).cloned();

    Ok(FollowMessage {
        id,
        session_id,
        parent_id,
        role,
        completed,
        structured,
        parts,
        error,
    })
}

fn required_string(value: &Value, field: &str) -> Result<String, FollowTransportError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| FollowTransportError::protocol(format!("message info has no `{field}`")))
}

fn session_id(properties: &Value) -> Option<String> {
    properties
        .get("sessionID")
        .and_then(Value::as_str)
        .or_else(|| {
            properties
                .pointer("/info/sessionID")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            properties
                .pointer("/part/sessionID")
                .and_then(Value::as_str)
        })
        .map(str::to_owned)
}

fn is_known_event(kind: &str) -> bool {
    matches!(
        kind,
        "session.status"
            | "session.busy"
            | "session.idle"
            | "message.updated"
            | "message.part.updated"
            | "permission.asked"
            | "permission.v2.asked"
            | "session.error"
    )
}

impl EventSubscription for Subscription {
    async fn next(&mut self) -> Result<Option<OcaEvent>, FollowTransportError> {
        Subscription::next(self)
            .await
            .map_err(map_sse_error)?
            .map(adapt_sse_event)
            .transpose()
    }
}

impl FollowTransport for OpenCodeClient {
    type Subscription = Subscription;

    async fn subscribe(
        &self,
        directory: &str,
        last_event_id: Option<&str>,
    ) -> Result<Self::Subscription, FollowTransportError> {
        OpenCodeClient::subscribe(self, directory, last_event_id)
            .await
            .map_err(map_client_error)
    }

    async fn messages(&self, session_id: &str) -> Result<Vec<FollowMessage>, FollowTransportError> {
        OpenCodeClient::messages(self, session_id)
            .await
            .map_err(map_history_error)?
            .into_iter()
            .map(adapt_message)
            .collect()
    }
}

fn map_history_error(error: OpenCodeError) -> FollowTransportError {
    match error {
        OpenCodeError::Server { status: 400, body } => FollowTransportError::history_rejected(
            format!("OpenCode rejected session history with HTTP 400: {body}"),
        ),
        error => map_client_error(error),
    }
}

fn map_client_error(error: OpenCodeError) -> FollowTransportError {
    match error {
        OpenCodeError::Transport { message, .. } => FollowTransportError::unreachable(message),
        OpenCodeError::ProtocolMismatch { message } => FollowTransportError::protocol(message),
        OpenCodeError::RateLimited { body, limit } => {
            FollowTransportError::rate_limited(body, limit.retry_after_ms())
        }
        OpenCodeError::Server { status, body } => FollowTransportError::protocol(format!(
            "OpenCode returned HTTP {status} while following: {body}"
        )),
    }
}

fn map_sse_error(error: SseError) -> FollowTransportError {
    match error {
        SseError::Source {
            message,
            kind: SseSourceErrorKind::Transport,
        } => FollowTransportError::unreachable(message),
        SseError::Source {
            message,
            kind: SseSourceErrorKind::Protocol,
        } => FollowTransportError::protocol(message),
        SseError::InvalidUtf8 { .. } => {
            FollowTransportError::protocol("event stream contains invalid UTF-8")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_parent_and_completed_state_survive_event_adaptation() {
        let event = adapt_sse_event(SseEvent {
            id: Some("evt-sse".to_owned()),
            event: None,
            data: serde_json::json!({
                "id": "evt-envelope",
                "type": "message.updated",
                "properties": {
                    "sessionID": "ses_target",
                    "info": {
                        "id": "msg_assistant",
                        "sessionID": "ses_target",
                        "role": "assistant",
                        "parentID": "msg_dispatch",
                        "time": { "created": 1, "completed": 2 },
                        "structured": { "status": "done" }
                    }
                }
            })
            .to_string(),
        })
        .unwrap();

        assert_eq!(event.id.as_deref(), Some("evt-envelope"));
        assert_eq!(event.cursor.as_deref(), Some("evt-sse"));
        let message = event.message.unwrap();
        assert_eq!(message.parent_id.as_deref(), Some("msg_dispatch"));
        assert!(message.completed);
    }

    #[test]
    fn unknown_event_omits_payload_but_retains_type_and_session() {
        let event = adapt_sse_event(SseEvent {
            id: None,
            event: None,
            data: serde_json::json!({
                "id": "evt-new",
                "type": "future.event",
                "properties": { "sessionID": "ses_target", "secret": "not journaled" }
            })
            .to_string(),
        })
        .unwrap();

        assert_eq!(event.kind, "future.event");
        assert_eq!(event.session_id.as_deref(), Some("ses_target"));
        assert!(event.payload.is_none());
        assert!(!event.known);
    }

    #[test]
    fn history_400_is_distinct_from_stream_protocol_failures() {
        assert!(matches!(
            map_history_error(OpenCodeError::Server {
                status: 400,
                body: "stored format contains retryCount".to_owned(),
            }),
            FollowTransportError::HistoryRejected { .. }
        ));
        assert!(matches!(
            map_client_error(OpenCodeError::Server {
                status: 400,
                body: "event endpoint rejected request".to_owned(),
            }),
            FollowTransportError::Protocol { .. }
        ));
    }
}
