use std::{future, sync::Mutex};

use oca_core::{
    EventSubscription, FollowError, FollowMessage, FollowOutcome, FollowTarget, FollowTransport,
    FollowTransportError, WorkerState, follow_until_terminal,
};
use oca_opencode::{MessageWithParts, adapt_message};

struct HistoryTransport {
    messages: Mutex<Option<Vec<FollowMessage>>>,
}

struct PendingSubscription;

impl EventSubscription for PendingSubscription {
    async fn next(&mut self) -> Result<Option<oca_core::OcaEvent>, FollowTransportError> {
        future::pending().await
    }
}

impl FollowTransport for HistoryTransport {
    type Subscription = PendingSubscription;

    async fn subscribe(
        &self,
        _directory: &str,
        _last_event_id: Option<&str>,
    ) -> Result<Self::Subscription, FollowTransportError> {
        Ok(PendingSubscription)
    }

    async fn messages(
        &self,
        _session_id: &str,
    ) -> Result<Vec<FollowMessage>, FollowTransportError> {
        Ok(self.messages.lock().unwrap().take().unwrap())
    }
}

#[tokio::test]
async fn completed_three_message_turn_uses_reply_from_an_earlier_tool_step() {
    let outcome = follow_fixture(include_str!(
        "fixtures/follow_three_message_reply_earlier.json"
    ))
    .await
    .unwrap();

    let FollowOutcome::Terminal(terminal) = outcome else {
        panic!("completed fixture must classify as terminal");
    };
    assert_eq!(terminal.state, WorkerState::Done);
    assert_eq!(terminal.message.id, "msg_assistant_step_3");
    assert_eq!(
        terminal.message.reply().unwrap()["files"],
        serde_json::json!(["src/follow.rs"]),
        "the earlier JSON text reply is normalized onto the terminal projection"
    );
}

#[tokio::test]
async fn completed_three_message_turn_without_any_reply_is_a_protocol_mismatch() {
    let error = follow_fixture(include_str!("fixtures/follow_three_message_no_reply.json"))
        .await
        .unwrap_err();

    assert_eq!(
        error,
        FollowError::Protocol {
            message:
                "completed attributed assistant message chain has no valid worker status reply"
                    .to_owned(),
        }
    );
}

#[tokio::test]
async fn completed_single_message_turn_keeps_its_existing_classification() {
    let message = FollowMessage {
        id: "msg_assistant".to_owned(),
        session_id: "ses_target".to_owned(),
        parent_id: Some("msg_this_dispatch".to_owned()),
        role: "assistant".to_owned(),
        completed: true,
        structured: Some(serde_json::json!({
            "status": "blocked",
            "files": [],
            "note": "Waiting for required user input before continuing the assigned worker task."
        })),
        parts: Vec::new(),
        error: None,
    };
    let outcome = follow_messages(vec![message]).await.unwrap();

    let FollowOutcome::Terminal(terminal) = outcome else {
        panic!("single-message fixture must classify as terminal");
    };
    assert_eq!(terminal.state, WorkerState::Blocked);
    assert_eq!(terminal.message.id, "msg_assistant");
}

async fn follow_fixture(fixture: &str) -> Result<FollowOutcome, FollowError> {
    let messages = serde_json::from_str::<Vec<MessageWithParts>>(fixture)
        .unwrap()
        .into_iter()
        .map(adapt_message)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    follow_messages(messages).await
}

async fn follow_messages(messages: Vec<FollowMessage>) -> Result<FollowOutcome, FollowError> {
    let transport = HistoryTransport {
        messages: Mutex::new(Some(messages)),
    };
    follow_until_terminal::<_, NoJournal>(&transport, &target(), None, None).await
}

fn target() -> FollowTarget {
    FollowTarget {
        session_id: "ses_target".to_owned(),
        message_id: "msg_this_dispatch".to_owned(),
        directory: "/repo".to_owned(),
    }
}

struct NoJournal;

impl oca_core::EventJournalWriter for NoJournal {
    fn append(&mut self, _event: &oca_core::OcaEvent) -> Result<(), String> {
        Ok(())
    }
}
