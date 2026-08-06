//! Transport-independent follow state machine and turn attribution.

use std::{
    collections::HashSet,
    fmt,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serde_json::Value;

use crate::{FollowExit, WorkerState};

/// The exact session and caller-minted message that identify one dispatched turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FollowTarget {
    pub session_id: String,
    pub message_id: String,
}

/// A projected assistant message used for terminal attribution and reply decoding.
#[derive(Clone, Debug, PartialEq)]
pub struct FollowMessage {
    pub id: String,
    pub session_id: String,
    pub parent_id: Option<String>,
    pub role: String,
    pub completed: bool,
    pub structured: Option<Value>,
    pub parts: Vec<Value>,
    pub error: Option<Value>,
}

impl FollowMessage {
    fn worker_state(&self) -> Option<WorkerState> {
        self.structured
            .as_ref()
            .and_then(|value| value.get("status"))
            .and_then(Value::as_str)
            .and_then(parse_worker_state)
            .or_else(|| {
                self.parts.iter().find_map(|part| {
                    let text = part.get("text")?.as_str()?;
                    let value: Value = serde_json::from_str(text).ok()?;
                    value
                        .get("status")
                        .and_then(Value::as_str)
                        .and_then(parse_worker_state)
                })
            })
    }

    #[must_use]
    pub fn reply(&self) -> Option<&Value> {
        self.structured.as_ref()
    }
}

fn parse_worker_state(value: &str) -> Option<WorkerState> {
    match value {
        "done" => Some(WorkerState::Done),
        "blocked" => Some(WorkerState::Blocked),
        "partial" => Some(WorkerState::Partial),
        _ => None,
    }
}

/// One adapted OpenCode event. Unknown events deliberately carry no payload.
#[derive(Clone, Debug, PartialEq)]
pub struct OcaEvent {
    /// Stable event identity used for deduplication.
    pub id: Option<String>,
    /// Resumable SSE cursor, which may intentionally repeat across events.
    pub cursor: Option<String>,
    pub kind: String,
    pub session_id: Option<String>,
    pub payload: Option<Value>,
    pub message: Option<FollowMessage>,
    pub known: bool,
}

impl OcaEvent {
    #[must_use]
    pub fn is_session_idle(&self) -> bool {
        self.kind == "session.idle"
    }

    #[must_use]
    pub fn is_reasoning(&self) -> bool {
        self.kind.contains("reasoning")
            || self.payload.as_ref().is_some_and(|payload| {
                payload
                    .pointer("/properties/part/type")
                    .and_then(Value::as_str)
                    == Some("reasoning")
            })
    }
}

/// A terminal result that has been attributed to this dispatch.
#[derive(Clone, Debug, PartialEq)]
pub struct FollowTerminal {
    pub state: WorkerState,
    pub message: FollowMessage,
}

/// Frozen process-level outcomes of `oca f`.
#[derive(Clone, Debug, PartialEq)]
pub enum FollowOutcome {
    Terminal(FollowTerminal),
    Timeout,
    ServerUnreachable,
}

impl FollowOutcome {
    #[must_use]
    pub const fn exit(&self) -> FollowExit {
        match self {
            Self::Terminal(terminal) if matches!(terminal.state, WorkerState::Blocked) => {
                FollowExit::Blocked
            }
            Self::Terminal(_) => FollowExit::Success,
            Self::Timeout => FollowExit::Timeout,
            Self::ServerUnreachable => FollowExit::ServerUnreachable,
        }
    }
}

/// A transport failure classified at the facade boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FollowTransportError {
    Unreachable {
        message: String,
    },
    Protocol {
        message: String,
    },
    RateLimited {
        message: String,
        retry_after_ms: Option<u64>,
    },
}

impl FollowTransportError {
    #[must_use]
    pub fn unreachable(message: impl Into<String>) -> Self {
        Self::Unreachable {
            message: message.into(),
        }
    }

    #[must_use]
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol {
            message: message.into(),
        }
    }

    #[must_use]
    pub fn rate_limited(message: impl Into<String>, retry_after_ms: Option<u64>) -> Self {
        Self::RateLimited {
            message: message.into(),
            retry_after_ms,
        }
    }
}

/// An open event stream supplied by the transport adapter.
pub trait EventSubscription {
    fn next(
        &mut self,
    ) -> impl Future<Output = Result<Option<OcaEvent>, FollowTransportError>> + Send;
}

/// The two OpenCode reads permitted to the follow loop.
pub trait FollowTransport: Sync {
    type Subscription: EventSubscription + Send;

    fn subscribe(
        &self,
        last_event_id: Option<&str>,
    ) -> impl Future<Output = Result<Self::Subscription, FollowTransportError>> + Send;

    fn messages(
        &self,
        session_id: &str,
    ) -> impl Future<Output = Result<Vec<FollowMessage>, FollowTransportError>> + Send;
}

/// Append-only journal seam; the concrete writer remains owned by `oca-state`.
pub trait EventJournalWriter {
    fn append(&mut self, event: &OcaEvent) -> Result<(), String>;
}

/// Reconnection limits for a stream that is not making progress.
///
/// Receiving an event resets both limits. A successful subscription alone does
/// not reset them unless an explicit follow timeout is governing the operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FollowPolicy {
    pub max_reconnect_attempts: usize,
    pub max_reconnect_elapsed: Duration,
    pub initial_backoff: Duration,
}

impl Default for FollowPolicy {
    fn default() -> Self {
        Self {
            max_reconnect_attempts: 5,
            max_reconnect_elapsed: Duration::from_secs(30),
            initial_backoff: Duration::from_secs(1),
        }
    }
}

/// Non-outcome failures produced while following.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FollowError {
    Protocol {
        message: String,
    },
    Journal {
        message: String,
    },
    RateLimited {
        message: String,
        retry_after_ms: Option<u64>,
    },
}

impl fmt::Display for FollowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol { message } => write!(formatter, "protocol mismatch: {message}"),
            Self::Journal { message } => write!(formatter, "event journal failed: {message}"),
            Self::RateLimited { message, .. } => write!(formatter, "rate limited: {message}"),
        }
    }
}

impl std::error::Error for FollowError {}

/// Follows one dispatch until its attributed assistant message reaches a terminal boundary.
///
/// The optional timeout covers subscription, reconciliation, event consumption, and reconnects.
/// No branch in this function aborts a worker or mutates ref state.
pub async fn follow_until_terminal<T, J>(
    transport: &T,
    target: &FollowTarget,
    timeout: Option<Duration>,
    journal: Option<&mut J>,
) -> Result<FollowOutcome, FollowError>
where
    T: FollowTransport,
    J: EventJournalWriter,
{
    follow_until_terminal_from_cursor(transport, target, timeout, journal, None).await
}

/// Follows one dispatch while resuming the event stream after a durable cursor.
pub async fn follow_until_terminal_from_cursor<T, J>(
    transport: &T,
    target: &FollowTarget,
    timeout: Option<Duration>,
    journal: Option<&mut J>,
    initial_cursor: Option<&str>,
) -> Result<FollowOutcome, FollowError>
where
    T: FollowTransport,
    J: EventJournalWriter,
{
    follow_with_policy_and_cursor(
        transport,
        target,
        timeout,
        journal,
        FollowPolicy::default(),
        initial_cursor,
    )
    .await
}

/// Policy-injected form used for deterministic reconnect tests.
pub async fn follow_until_terminal_with_policy<T, J>(
    transport: &T,
    target: &FollowTarget,
    timeout: Option<Duration>,
    journal: Option<&mut J>,
    policy: FollowPolicy,
) -> Result<FollowOutcome, FollowError>
where
    T: FollowTransport,
    J: EventJournalWriter,
{
    follow_with_policy_and_cursor(transport, target, timeout, journal, policy, None).await
}

async fn follow_with_policy_and_cursor<T, J>(
    transport: &T,
    target: &FollowTarget,
    timeout: Option<Duration>,
    journal: Option<&mut J>,
    policy: FollowPolicy,
    initial_cursor: Option<&str>,
) -> Result<FollowOutcome, FollowError>
where
    T: FollowTransport,
    J: EventJournalWriter,
{
    let connection_failed = Arc::new(AtomicBool::new(false));
    let has_explicit_timeout = timeout.is_some();
    let follow = follow_inner(
        transport,
        target,
        journal,
        policy,
        Arc::clone(&connection_failed),
        initial_cursor.map(str::to_owned),
        has_explicit_timeout,
    );
    match timeout {
        Some(timeout) => match tokio::time::timeout(timeout, follow).await {
            Ok(result) => result,
            Err(_) if connection_failed.load(Ordering::Relaxed) => {
                Ok(FollowOutcome::ServerUnreachable)
            }
            Err(_) => Ok(FollowOutcome::Timeout),
        },
        None => follow.await,
    }
}

async fn follow_inner<T, J>(
    transport: &T,
    target: &FollowTarget,
    mut journal: Option<&mut J>,
    policy: FollowPolicy,
    connection_failed: Arc<AtomicBool>,
    initial_cursor: Option<String>,
    has_explicit_timeout: bool,
) -> Result<FollowOutcome, FollowError>
where
    T: FollowTransport,
    J: EventJournalWriter,
{
    let mut subscription = match transport.subscribe(initial_cursor.as_deref()).await {
        Ok(subscription) => {
            connection_failed.store(false, Ordering::Relaxed);
            subscription
        }
        Err(FollowTransportError::Unreachable { .. }) => {
            connection_failed.store(true, Ordering::Relaxed);
            return Ok(FollowOutcome::ServerUnreachable);
        }
        Err(error) => return Err(protocol_error(error)),
    };
    let mut tracker = TurnTracker::new(target);
    let messages = match transport.messages(&target.session_id).await {
        Ok(messages) => {
            connection_failed.store(false, Ordering::Relaxed);
            messages
        }
        Err(FollowTransportError::Unreachable { .. }) => {
            connection_failed.store(true, Ordering::Relaxed);
            return Ok(FollowOutcome::ServerUnreachable);
        }
        Err(error) => return Err(protocol_error(error)),
    };
    if let Some(terminal) = tracker.reconcile(messages)? {
        return Ok(FollowOutcome::Terminal(terminal));
    }

    let mut reconnect_started = None;
    let mut reconnect_attempts = 0_usize;
    let mut last_event_id = initial_cursor;
    let mut no_cursor_reconciled = false;

    loop {
        tokio::task::yield_now().await;
        match subscription.next().await {
            Ok(Some(event)) => {
                reconnect_started = None;
                reconnect_attempts = 0;
                connection_failed.store(false, Ordering::Relaxed);
                if let Some(cursor) = event.cursor.as_ref() {
                    last_event_id = Some(cursor.clone());
                }
                if event.session_id.as_deref() != Some(target.session_id.as_str()) {
                    continue;
                }
                if !tracker.observe_event_id(event.id.as_deref()) {
                    continue;
                }
                if !event.is_reasoning()
                    && let Some(writer) = journal.as_deref_mut()
                {
                    writer
                        .append(&event)
                        .map_err(|message| FollowError::Journal { message })?;
                }
                if let Some(terminal) = tracker.observe(&event)? {
                    return Ok(FollowOutcome::Terminal(terminal));
                }
            }
            stream_end @ (Ok(None) | Err(FollowTransportError::Unreachable { .. })) => {
                if stream_end.is_err() {
                    connection_failed.store(true, Ordering::Relaxed);
                }
                let reconnect_since =
                    *reconnect_started.get_or_insert_with(tokio::time::Instant::now);
                if last_event_id.is_none() && !no_cursor_reconciled {
                    no_cursor_reconciled = true;
                    match transport.messages(&target.session_id).await {
                        Ok(messages) => {
                            connection_failed.store(false, Ordering::Relaxed);
                            if let Some(terminal) = tracker.reconcile(messages)? {
                                return Ok(FollowOutcome::Terminal(terminal));
                            }
                        }
                        Err(FollowTransportError::Unreachable { .. }) => {
                            connection_failed.store(true, Ordering::Relaxed);
                        }
                        Err(error) => return Err(protocol_error(error)),
                    }
                }

                if reconnect_attempts >= policy.max_reconnect_attempts
                    || reconnect_since.elapsed() >= policy.max_reconnect_elapsed
                {
                    return Ok(FollowOutcome::ServerUnreachable);
                }

                let delay = reconnect_delay(policy, reconnect_attempts, reconnect_since.elapsed());
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                reconnect_attempts += 1;
                match transport.subscribe(last_event_id.as_deref()).await {
                    Ok(reconnected) => {
                        subscription = reconnected;
                        if has_explicit_timeout {
                            reconnect_started = None;
                            reconnect_attempts = 0;
                        }
                        connection_failed.store(false, Ordering::Relaxed);
                    }
                    Err(FollowTransportError::Unreachable { .. }) => {
                        connection_failed.store(true, Ordering::Relaxed);
                    }
                    Err(error) => return Err(protocol_error(error)),
                }
            }
            Err(error) => return Err(protocol_error(error)),
        }
    }
}

fn reconnect_delay(policy: FollowPolicy, attempt: usize, elapsed: Duration) -> Duration {
    let exponent = u32::try_from(attempt).unwrap_or(u32::MAX).min(31);
    let multiplier = 1_u32.checked_shl(exponent).unwrap_or(u32::MAX);
    let delay = policy.initial_backoff.saturating_mul(multiplier);
    delay.min(policy.max_reconnect_elapsed.saturating_sub(elapsed))
}

fn protocol_error(error: FollowTransportError) -> FollowError {
    let message = match error {
        FollowTransportError::Protocol { message }
        | FollowTransportError::Unreachable { message } => message,
        FollowTransportError::RateLimited {
            message,
            retry_after_ms,
        } => {
            return FollowError::RateLimited {
                message,
                retry_after_ms,
            };
        }
    };
    FollowError::Protocol { message }
}

struct TurnTracker<'a> {
    target: &'a FollowTarget,
    event_ids: HashSet<String>,
    attributed: Option<FollowMessage>,
}

impl<'a> TurnTracker<'a> {
    fn new(target: &'a FollowTarget) -> Self {
        Self {
            target,
            event_ids: HashSet::new(),
            attributed: None,
        }
    }

    fn observe_event_id(&mut self, event_id: Option<&str>) -> bool {
        event_id.is_none_or(|event_id| self.event_ids.insert(event_id.to_owned()))
    }

    fn reconcile(
        &mut self,
        messages: Vec<FollowMessage>,
    ) -> Result<Option<FollowTerminal>, FollowError> {
        for message in messages {
            if self.is_attributed(&message) && message.completed {
                return terminal_from_message(message).map(Some);
            }
        }
        Ok(None)
    }

    fn observe(&mut self, event: &OcaEvent) -> Result<Option<FollowTerminal>, FollowError> {
        if let Some(message) = event.message.as_ref()
            && self.is_attributed(message)
            && message.completed
        {
            self.attributed = Some(message.clone());
        }
        if event.is_session_idle()
            && let Some(message) = self.attributed.take()
        {
            return terminal_from_message(message).map(Some);
        }
        Ok(None)
    }

    fn is_attributed(&self, message: &FollowMessage) -> bool {
        message.session_id == self.target.session_id
            && message.role == "assistant"
            && message.parent_id.as_deref() == Some(self.target.message_id.as_str())
    }
}

fn terminal_from_message(message: FollowMessage) -> Result<FollowTerminal, FollowError> {
    let state = message
        .worker_state()
        .ok_or_else(|| FollowError::Protocol {
            message: "attributed terminal assistant message has no valid worker status".to_owned(),
        })?;
    Ok(FollowTerminal { state, message })
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering as AtomicOrdering},
        },
    };

    use super::*;

    #[derive(Default)]
    struct ScriptedTransport {
        subscriptions: Mutex<VecDeque<Result<ScriptedSubscription, FollowTransportError>>>,
        reconciliations: Mutex<VecDeque<Result<Vec<FollowMessage>, FollowTransportError>>>,
        cursors: Mutex<Vec<Option<String>>>,
    }

    struct ScriptedSubscription {
        events: VecDeque<Result<Option<OcaEvent>, FollowTransportError>>,
        pending: bool,
    }

    impl EventSubscription for ScriptedSubscription {
        async fn next(&mut self) -> Result<Option<OcaEvent>, FollowTransportError> {
            if let Some(event) = self.events.pop_front() {
                return event;
            }
            if self.pending {
                return std::future::pending().await;
            }
            Ok(None)
        }
    }

    impl FollowTransport for ScriptedTransport {
        type Subscription = ScriptedSubscription;

        async fn subscribe(
            &self,
            last_event_id: Option<&str>,
        ) -> Result<Self::Subscription, FollowTransportError> {
            self.cursors
                .lock()
                .unwrap()
                .push(last_event_id.map(str::to_owned));
            self.subscriptions.lock().unwrap().pop_front().unwrap()
        }

        async fn messages(
            &self,
            _session_id: &str,
        ) -> Result<Vec<FollowMessage>, FollowTransportError> {
            self.reconciliations.lock().unwrap().pop_front().unwrap()
        }
    }

    struct AlwaysEmptyTransport {
        subscriptions: AtomicUsize,
        fail_at: Option<std::time::Instant>,
    }

    impl AlwaysEmptyTransport {
        fn new(fail_after: Option<Duration>) -> Self {
            Self {
                subscriptions: AtomicUsize::new(0),
                fail_at: fail_after.map(|duration| std::time::Instant::now() + duration),
            }
        }
    }

    struct EmptySubscription;

    impl EventSubscription for EmptySubscription {
        async fn next(&mut self) -> Result<Option<OcaEvent>, FollowTransportError> {
            Ok(None)
        }
    }

    impl FollowTransport for AlwaysEmptyTransport {
        type Subscription = EmptySubscription;

        async fn subscribe(
            &self,
            _last_event_id: Option<&str>,
        ) -> Result<Self::Subscription, FollowTransportError> {
            self.subscriptions.fetch_add(1, AtomicOrdering::Relaxed);
            if self
                .fail_at
                .is_some_and(|deadline| std::time::Instant::now() >= deadline)
            {
                return Err(FollowTransportError::protocol(
                    "test watchdog reached before the runtime deadline",
                ));
            }
            Ok(EmptySubscription)
        }

        async fn messages(
            &self,
            _session_id: &str,
        ) -> Result<Vec<FollowMessage>, FollowTransportError> {
            Ok(Vec::new())
        }
    }

    #[derive(Default)]
    struct Journal(Vec<String>);

    impl EventJournalWriter for Journal {
        fn append(&mut self, event: &OcaEvent) -> Result<(), String> {
            self.0.push(event.kind.clone());
            Ok(())
        }
    }

    fn target() -> FollowTarget {
        FollowTarget {
            session_id: "ses_target".to_owned(),
            message_id: "msg_this_dispatch".to_owned(),
        }
    }

    fn message(parent_id: &str, status: &str, completed: bool) -> FollowMessage {
        FollowMessage {
            id: format!("assistant-{parent_id}"),
            session_id: "ses_target".to_owned(),
            parent_id: Some(parent_id.to_owned()),
            role: "assistant".to_owned(),
            completed,
            structured: Some(serde_json::json!({ "status": status })),
            parts: Vec::new(),
            error: None,
        }
    }

    fn event(id: &str, kind: &str, message: Option<FollowMessage>) -> OcaEvent {
        OcaEvent {
            id: Some(id.to_owned()),
            cursor: Some(id.to_owned()),
            kind: kind.to_owned(),
            session_id: Some("ses_target".to_owned()),
            payload: Some(serde_json::json!({ "public": true })),
            message,
            known: true,
        }
    }

    fn subscription(
        events: impl IntoIterator<Item = Result<Option<OcaEvent>, FollowTransportError>>,
    ) -> Result<ScriptedSubscription, FollowTransportError> {
        Ok(ScriptedSubscription {
            events: events.into_iter().collect(),
            pending: false,
        })
    }

    fn test_policy() -> FollowPolicy {
        FollowPolicy {
            max_reconnect_attempts: 1,
            max_reconnect_elapsed: Duration::ZERO,
            initial_backoff: Duration::ZERO,
        }
    }

    #[tokio::test]
    async fn foreign_turn_and_duplicate_idle_do_not_replace_parent_attribution() {
        let transport = ScriptedTransport {
            subscriptions: Mutex::new(VecDeque::from([subscription([
                Ok(Some(event(
                    "evt-foreign",
                    "message.updated",
                    Some(message("msg_foreign", "done", true)),
                ))),
                Ok(Some(event("evt-idle-foreign", "session.idle", None))),
                Ok(Some(event(
                    "evt-own",
                    "message.updated",
                    Some(message("msg_this_dispatch", "done", true)),
                ))),
                Ok(Some(event("evt-idle-own", "session.idle", None))),
                Ok(Some(event("evt-idle-duplicate", "session.idle", None))),
            ])])),
            reconciliations: Mutex::new(VecDeque::from([Ok(Vec::new())])),
            cursors: Mutex::new(Vec::new()),
        };
        let mut journal = Journal::default();

        let outcome = follow_until_terminal_with_policy(
            &transport,
            &target(),
            None,
            Some(&mut journal),
            test_policy(),
        )
        .await
        .unwrap();

        let FollowOutcome::Terminal(terminal) = outcome else {
            panic!("the attributed turn must terminate");
        };
        assert_eq!(
            terminal.message.parent_id.as_deref(),
            Some("msg_this_dispatch")
        );
        assert_eq!(
            journal.0.len(),
            4,
            "follow returns before the duplicate idle"
        );
    }

    #[tokio::test]
    async fn foreign_idle_does_not_terminate_an_incomplete_attributed_message() {
        let transport = ScriptedTransport {
            subscriptions: Mutex::new(VecDeque::from([subscription([
                Ok(Some(event(
                    "evt-own-incomplete",
                    "message.updated",
                    Some(message("msg_this_dispatch", "partial", false)),
                ))),
                Ok(Some(event("evt-idle-foreign", "session.idle", None))),
                Ok(Some(event(
                    "evt-own-complete",
                    "message.updated",
                    Some(message("msg_this_dispatch", "done", true)),
                ))),
                Ok(Some(event("evt-idle-own", "session.idle", None))),
            ])])),
            reconciliations: Mutex::new(VecDeque::from([Ok(Vec::new())])),
            cursors: Mutex::new(Vec::new()),
        };
        let mut journal = Journal::default();

        let outcome = follow_until_terminal_with_policy(
            &transport,
            &target(),
            None,
            Some(&mut journal),
            test_policy(),
        )
        .await
        .unwrap();

        let FollowOutcome::Terminal(terminal) = outcome else {
            panic!("the completed attributed turn must terminate");
        };
        assert!(terminal.message.completed);
        assert_eq!(terminal.state, WorkerState::Done);
        assert_eq!(
            journal.0,
            [
                "message.updated",
                "session.idle",
                "message.updated",
                "session.idle"
            ],
            "the foreign idle must not end the follow on the incomplete snapshot"
        );
    }

    #[tokio::test]
    async fn a_redelivered_event_id_is_journaled_only_once() {
        let transport = ScriptedTransport {
            subscriptions: Mutex::new(VecDeque::from([subscription([
                Ok(Some(event("evt-part", "message.part.updated", None))),
                Ok(Some(event("evt-part", "message.part.updated", None))),
                Ok(Some(event(
                    "evt-own",
                    "message.updated",
                    Some(message("msg_this_dispatch", "done", true)),
                ))),
                Ok(Some(event("evt-idle-own", "session.idle", None))),
            ])])),
            reconciliations: Mutex::new(VecDeque::from([Ok(Vec::new())])),
            cursors: Mutex::new(Vec::new()),
        };
        let mut journal = Journal::default();

        let outcome = follow_until_terminal_with_policy(
            &transport,
            &target(),
            None,
            Some(&mut journal),
            test_policy(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.exit(), FollowExit::Success);
        assert_eq!(
            journal.0,
            ["message.part.updated", "message.updated", "session.idle"],
            "a redelivered event id is dropped before the journal"
        );
    }

    #[tokio::test]
    async fn reconnect_uses_last_event_id_without_a_second_reconciliation() {
        let transport = ScriptedTransport {
            subscriptions: Mutex::new(VecDeque::from([
                subscription([Ok(Some(event("evt-1", "session.busy", None))), Ok(None)]),
                subscription([
                    Ok(Some(event(
                        "evt-2",
                        "message.updated",
                        Some(message("msg_this_dispatch", "blocked", true)),
                    ))),
                    Ok(Some(event("evt-3", "session.idle", None))),
                ]),
            ])),
            reconciliations: Mutex::new(VecDeque::from([Ok(Vec::new())])),
            cursors: Mutex::new(Vec::new()),
        };
        let policy = FollowPolicy {
            max_reconnect_attempts: 5,
            max_reconnect_elapsed: Duration::from_secs(30),
            initial_backoff: Duration::ZERO,
        };

        let outcome = follow_until_terminal_with_policy(
            &transport,
            &target(),
            None,
            None::<&mut Journal>,
            policy,
        )
        .await
        .unwrap();

        assert_eq!(outcome.exit(), FollowExit::Blocked);
        assert_eq!(
            *transport.cursors.lock().unwrap(),
            [None, Some("evt-1".to_owned())]
        );
        assert!(transport.reconciliations.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn killed_follow_resumes_from_the_durable_event_cursor() {
        let transport = ScriptedTransport {
            subscriptions: Mutex::new(VecDeque::from([subscription([
                Ok(Some(event(
                    "evt-after-crash",
                    "message.updated",
                    Some(message("msg_this_dispatch", "done", true)),
                ))),
                Ok(Some(event("evt-idle", "session.idle", None))),
            ])])),
            reconciliations: Mutex::new(VecDeque::from([Ok(Vec::new())])),
            cursors: Mutex::new(Vec::new()),
        };
        let mut journal = Journal::default();

        let outcome = follow_until_terminal_from_cursor(
            &transport,
            &target(),
            None,
            Some(&mut journal),
            Some("evt-before-crash"),
        )
        .await
        .unwrap();

        assert_eq!(outcome.exit(), FollowExit::Success);
        assert_eq!(
            transport.cursors.lock().unwrap().as_slice(),
            [Some("evt-before-crash".to_owned())]
        );
    }

    #[tokio::test]
    async fn reconnect_without_cursor_performs_exactly_one_fallback_reconciliation() {
        let terminal = message("msg_this_dispatch", "done", true);
        let transport = ScriptedTransport {
            subscriptions: Mutex::new(VecDeque::from([subscription([Ok(None)])])),
            reconciliations: Mutex::new(VecDeque::from([Ok(Vec::new()), Ok(vec![terminal])])),
            cursors: Mutex::new(Vec::new()),
        };

        let outcome = follow_until_terminal_with_policy(
            &transport,
            &target(),
            None,
            None::<&mut Journal>,
            test_policy(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.exit(), FollowExit::Success);
        assert!(transport.reconciliations.lock().unwrap().is_empty());
        assert_eq!(*transport.cursors.lock().unwrap(), [None]);
    }

    #[tokio::test]
    async fn reconnect_is_capped_at_five_attempts() {
        let mut subscriptions = VecDeque::from([subscription([Ok(None)])]);
        subscriptions.extend((0..5).map(|_| {
            Err(FollowTransportError::unreachable(
                "server remains unreachable",
            ))
        }));
        let transport = ScriptedTransport {
            subscriptions: Mutex::new(subscriptions),
            reconciliations: Mutex::new(VecDeque::from([Ok(Vec::new()), Ok(Vec::new())])),
            cursors: Mutex::new(Vec::new()),
        };
        let policy = FollowPolicy {
            max_reconnect_attempts: 5,
            max_reconnect_elapsed: Duration::from_secs(30),
            initial_backoff: Duration::ZERO,
        };

        let outcome = follow_until_terminal_with_policy(
            &transport,
            &target(),
            None,
            None::<&mut Journal>,
            policy,
        )
        .await
        .unwrap();

        assert_eq!(outcome, FollowOutcome::ServerUnreachable);
        assert_eq!(transport.cursors.lock().unwrap().len(), 6);
        assert!(transport.subscriptions.lock().unwrap().is_empty());
        assert!(transport.reconciliations.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn stream_progress_resets_the_reconnect_budget_without_an_explicit_timeout() {
        let transport = ScriptedTransport {
            subscriptions: Mutex::new(VecDeque::from([
                subscription([Ok(Some(event("evt-progress-1", "session.idle", None)))]),
                subscription([Ok(Some(event("evt-progress-2", "session.idle", None)))]),
                subscription([
                    Ok(Some(event(
                        "evt-terminal",
                        "message.updated",
                        Some(message("msg_this_dispatch", "blocked", true)),
                    ))),
                    Ok(Some(event("evt-idle", "session.idle", None))),
                ]),
            ])),
            reconciliations: Mutex::new(VecDeque::from([Ok(Vec::new())])),
            cursors: Mutex::new(Vec::new()),
        };

        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            follow_until_terminal_with_policy(
                &transport,
                &target(),
                None,
                None::<&mut Journal>,
                FollowPolicy {
                    max_reconnect_attempts: 1,
                    max_reconnect_elapsed: Duration::from_secs(1),
                    initial_backoff: Duration::ZERO,
                },
            ),
        )
        .await
        .expect("a progressing stream must not stall the follow loop")
        .unwrap();

        assert_eq!(
            outcome.exit(),
            FollowExit::Blocked,
            "each received event must restore the full reconnect budget"
        );
        assert_eq!(
            transport.cursors.lock().unwrap().as_slice(),
            [
                None,
                Some("evt-progress-1".to_owned()),
                Some("evt-progress-2".to_owned())
            ],
            "every reconnect must resume from the last observed cursor"
        );
    }

    #[tokio::test]
    async fn successful_empty_resubscriptions_exhaust_budget_without_explicit_timeout() {
        let transport = AlwaysEmptyTransport::new(None);
        let started = std::time::Instant::now();

        let outcome = tokio::time::timeout(
            Duration::from_millis(500),
            follow_until_terminal_with_policy(
                &transport,
                &target(),
                None,
                None::<&mut Journal>,
                FollowPolicy {
                    max_reconnect_attempts: 3,
                    max_reconnect_elapsed: Duration::from_millis(50),
                    initial_backoff: Duration::from_millis(10),
                },
            ),
        )
        .await
        .expect("successful empty resubscriptions must have a finite budget")
        .unwrap();

        assert_eq!(outcome, FollowOutcome::ServerUnreachable);
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(
            transport.subscriptions.load(AtomicOrdering::Relaxed),
            4,
            "the initial subscription and three successful resubscriptions exhaust the budget"
        );
    }

    #[tokio::test]
    async fn terminal_event_remains_prompt_after_successful_reconnects() {
        let transport = ScriptedTransport {
            subscriptions: Mutex::new(VecDeque::from([
                subscription([Ok(None)]),
                subscription([Ok(None)]),
                subscription([
                    Ok(Some(event(
                        "evt-terminal",
                        "message.updated",
                        Some(message("msg_this_dispatch", "blocked", true)),
                    ))),
                    Ok(Some(event("evt-idle", "session.idle", None))),
                ]),
            ])),
            reconciliations: Mutex::new(VecDeque::from([Ok(Vec::new()), Ok(Vec::new())])),
            cursors: Mutex::new(Vec::new()),
        };
        let policy = FollowPolicy {
            max_reconnect_attempts: 5,
            max_reconnect_elapsed: Duration::from_secs(1),
            initial_backoff: Duration::ZERO,
        };

        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            follow_until_terminal_with_policy(
                &transport,
                &target(),
                None,
                None::<&mut Journal>,
                policy,
            ),
        )
        .await
        .expect("terminal event must remain prompt")
        .unwrap();

        assert_eq!(outcome.exit(), FollowExit::Blocked);
        assert_eq!(
            transport.cursors.lock().unwrap().as_slice(),
            [None, None, None]
        );
    }

    #[tokio::test]
    async fn successful_reconnects_wait_for_the_user_deadline() {
        let transport = ScriptedTransport {
            subscriptions: Mutex::new(VecDeque::from([
                subscription([Ok(None)]),
                subscription([Ok(None)]),
                Ok(ScriptedSubscription {
                    events: VecDeque::new(),
                    pending: true,
                }),
            ])),
            reconciliations: Mutex::new(VecDeque::from([Ok(Vec::new()), Ok(Vec::new())])),
            cursors: Mutex::new(Vec::new()),
        };
        let policy = FollowPolicy {
            max_reconnect_attempts: 1,
            max_reconnect_elapsed: Duration::from_secs(1),
            initial_backoff: Duration::ZERO,
        };
        let requested_timeout = Duration::from_millis(80);
        let started = std::time::Instant::now();

        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            follow_until_terminal_with_policy(
                &transport,
                &target(),
                Some(requested_timeout),
                None::<&mut Journal>,
                policy,
            ),
        )
        .await
        .expect("the regression test has a bounded wait")
        .unwrap();

        assert_eq!(outcome, FollowOutcome::Timeout);
        assert!(
            started.elapsed() >= requested_timeout,
            "successful reconnects must not return the timeout outcome before the user deadline"
        );
        assert_eq!(
            transport.cursors.lock().unwrap().as_slice(),
            [None, None, None],
            "the follow loop must reconnect past its old cumulative attempt cap"
        );
    }

    #[tokio::test]
    async fn zero_backoff_empty_stream_still_observes_the_explicit_deadline() {
        let transport = AlwaysEmptyTransport::new(Some(Duration::from_millis(250)));
        let requested_timeout = Duration::from_millis(20);
        let started = std::time::Instant::now();

        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            follow_until_terminal_with_policy(
                &transport,
                &target(),
                Some(requested_timeout),
                None::<&mut Journal>,
                FollowPolicy {
                    max_reconnect_attempts: 1,
                    max_reconnect_elapsed: Duration::from_secs(1),
                    initial_backoff: Duration::ZERO,
                },
            ),
        )
        .await
        .expect("zero-backoff reconnects must yield to the runtime deadline")
        .unwrap();

        assert_eq!(outcome, FollowOutcome::Timeout);
        assert!(started.elapsed() >= requested_timeout);
        assert!(started.elapsed() < Duration::from_millis(250));
        assert!(transport.subscriptions.load(AtomicOrdering::Relaxed) > 1);
    }

    #[tokio::test]
    async fn clean_stream_end_with_cursor_at_the_deadline_is_a_timeout() {
        let transport = ScriptedTransport {
            subscriptions: Mutex::new(VecDeque::from([subscription([Ok(None)])])),
            reconciliations: Mutex::new(VecDeque::from([Ok(Vec::new())])),
            cursors: Mutex::new(Vec::new()),
        };
        let requested_timeout = Duration::from_millis(40);
        let started = std::time::Instant::now();

        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            follow_with_policy_and_cursor(
                &transport,
                &target(),
                Some(requested_timeout),
                None::<&mut Journal>,
                FollowPolicy {
                    max_reconnect_attempts: 5,
                    max_reconnect_elapsed: Duration::from_secs(2),
                    initial_backoff: Duration::from_secs(1),
                },
                Some("evt-before-clean-eof"),
            ),
        )
        .await
        .expect("the clean-stream regression test has a bounded wait")
        .unwrap();

        assert_eq!(outcome, FollowOutcome::Timeout);
        assert!(started.elapsed() >= requested_timeout);
    }

    #[tokio::test]
    async fn timeout_and_unreachable_are_non_mutating_outcomes() {
        let unreachable = ScriptedTransport {
            subscriptions: Mutex::new(VecDeque::from([Err(FollowTransportError::unreachable(
                "refused",
            ))])),
            ..ScriptedTransport::default()
        };
        let outcome = follow_until_terminal(&unreachable, &target(), None, None::<&mut Journal>)
            .await
            .unwrap();
        assert_eq!(outcome, FollowOutcome::ServerUnreachable);

        let pending = ScriptedTransport {
            subscriptions: Mutex::new(VecDeque::from([Ok(ScriptedSubscription {
                events: VecDeque::new(),
                pending: true,
            })])),
            reconciliations: Mutex::new(VecDeque::from([Ok(Vec::new())])),
            cursors: Mutex::new(Vec::new()),
        };
        let outcome = follow_until_terminal(
            &pending,
            &target(),
            Some(Duration::ZERO),
            None::<&mut Journal>,
        )
        .await
        .unwrap();
        assert_eq!(outcome, FollowOutcome::Timeout);
    }
}
