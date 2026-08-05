//! Foreground dispatch ordering over injected side-effect seams.

use std::path::PathBuf;

use serde_json::Value;

use crate::{
    DisplayMode, OcaError, PermissionProfile, ReplyContract, ResolvedModel, RoleReply,
    WorkerPolicy, decode_role_reply, validate_reply_floor,
};

/// A fully locally-resolved foreground dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForegroundRequest {
    pub model: ResolvedModel,
    pub prompt: String,
    pub role: String,
    pub contract: ReplyContract,
    pub policy: WorkerPolicy,
    pub cwd: PathBuf,
    pub display: DisplayMode,
    pub json: bool,
}

/// The prompt admitted by the foreground state machine.
#[derive(Clone, Debug, PartialEq)]
pub struct DispatchPrompt {
    pub message_id: String,
    pub model: ResolvedModel,
    pub variant: String,
    pub role: String,
    pub text: String,
    pub output_schema: Value,
    pub permission: PermissionProfile,
}

/// A terminal structured payload attributed to the caller-minted message ID.
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalReply {
    pub structured: Value,
}

/// Successful foreground completion after structural decoding and output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForegroundOutcome {
    pub reference: String,
    pub session_id: String,
    pub message_id: String,
    pub reply: RoleReply,
}

/// The common dispatch prefix retained by the process that owns completion.
///
/// Background dispatch deliberately drops `subscription` after acknowledgement;
/// foreground dispatch carries it into terminal waiting.
pub(crate) struct StartedDispatch<S> {
    pub(crate) request: ForegroundRequest,
    pub(crate) subscription: S,
    pub(crate) reference: String,
    pub(crate) session_id: String,
    pub(crate) message_id: String,
}

/// Injected transport, state, output, launcher, and finalization seams.
///
/// Keeping each phase separate makes the two load-bearing request-order and
/// acknowledgement-order edges directly observable in tests.
#[allow(async_fn_in_trait)]
pub trait ForegroundBackend {
    type Subscription;
    type PendingRef;

    /// Performs dispatch-local preparation before session creation.
    ///
    /// The production worktree backend uses this boundary to reserve the ref,
    /// create the worktree, and replace the request cwd/write scope. The
    /// default keeps non-worktree backends entirely free of git work.
    fn prepare(&mut self, _request: &mut ForegroundRequest) -> Result<(), OcaError> {
        Ok(())
    }

    async fn create_session(&mut self, request: &ForegroundRequest) -> Result<String, OcaError>;

    async fn subscribe(&mut self) -> Result<Self::Subscription, OcaError>;

    fn mint_message_id(&mut self) -> Result<String, OcaError>;

    async fn prompt_async(
        &mut self,
        session_id: &str,
        prompt: &DispatchPrompt,
    ) -> Result<(), OcaError>;

    fn write_ref(
        &mut self,
        session_id: &str,
        message_id: &str,
        request: &ForegroundRequest,
    ) -> Result<Self::PendingRef, OcaError>;

    fn acknowledge(
        &mut self,
        pending: Self::PendingRef,
        model: &ResolvedModel,
        json: bool,
    ) -> Result<String, OcaError>;

    fn spawn_attach(
        &mut self,
        reference: &str,
        session_id: &str,
        cwd: &std::path::Path,
        display: DisplayMode,
    ) -> Result<(), OcaError>;

    /// Exactly one reconciliation pass after subscription and prompt
    /// admission. It is not called on a timer.
    async fn reconcile_once(
        &mut self,
        session_id: &str,
        message_id: &str,
    ) -> Result<Option<TerminalReply>, OcaError>;

    async fn wait_terminal(
        &mut self,
        subscription: &mut Self::Subscription,
        session_id: &str,
        message_id: &str,
    ) -> Result<TerminalReply, OcaError>;

    /// Tool-owned finalization. This is reached only after structural decode.
    fn finalize(&mut self, reference: &str, reply: &RoleReply) -> Result<(), OcaError>;

    fn print_final(
        &mut self,
        reference: &str,
        reply: &RoleReply,
        json: bool,
    ) -> Result<(), OcaError>;
}

/// Runs the complete non-worktree foreground pipeline.
///
/// # Errors
///
/// Stops at the first failed phase. In particular, structural decode failure
/// occurs before `finalize` and is returned as `contract_invalid`.
pub async fn run_foreground<B>(
    backend: &mut B,
    request: ForegroundRequest,
) -> Result<ForegroundOutcome, OcaError>
where
    B: ForegroundBackend,
{
    let mut started = start_dispatch(backend, request).await?;

    let terminal = match backend
        .reconcile_once(&started.session_id, &started.message_id)
        .await?
    {
        Some(terminal) => terminal,
        None => {
            backend
                .wait_terminal(
                    &mut started.subscription,
                    &started.session_id,
                    &started.message_id,
                )
                .await?
        }
    };

    let reply = decode_role_reply(started.request.contract, terminal.structured)?;
    validate_reply_floor(&reply)?;
    backend.finalize(&started.reference, &reply)?;
    backend.print_final(&started.reference, &reply, started.request.json)?;

    Ok(ForegroundOutcome {
        reference: started.reference,
        session_id: started.session_id,
        message_id: started.message_id,
        reply,
    })
}

/// Runs the single subscribe-before-prompt dispatch prefix shared by foreground
/// and background ownership.
pub(crate) async fn start_dispatch<B>(
    backend: &mut B,
    mut request: ForegroundRequest,
) -> Result<StartedDispatch<B::Subscription>, OcaError>
where
    B: ForegroundBackend,
{
    backend.prepare(&mut request)?;
    let session_id = backend.create_session(&request).await?;

    // This ordering is binding: the stream must be established before the
    // server is allowed to admit the prompt.
    let subscription = backend.subscribe().await?;
    let message_id = backend.mint_message_id()?;
    let prompt = DispatchPrompt {
        message_id: message_id.clone(),
        model: request.model.clone(),
        variant: request.model.variant.clone(),
        role: request.role.clone(),
        text: request.prompt.clone(),
        output_schema: request.contract.schema(),
        permission: request.policy.permission_profile(),
    };
    backend.prompt_async(&session_id, &prompt).await?;

    let pending = backend.write_ref(&session_id, &message_id, &request)?;
    let reference = backend.acknowledge(pending, &request.model, request.json)?;

    // The helper cannot be spawned until the acknowledgement is emitted and
    // flushed by the backend's acknowledgement boundary.
    backend.spawn_attach(&reference, &session_id, &request.cwd, request.display)?;

    Ok(StartedDispatch {
        request,
        subscription,
        reference,
        session_id,
        message_id,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        path::PathBuf,
        pin::pin,
        task::{Context, Poll, Waker},
    };

    use serde_json::json;

    use super::{
        DispatchPrompt, ForegroundBackend, ForegroundRequest, TerminalReply, run_foreground,
    };
    use crate::{
        DisplayMode, ErrorCode, MessageIdGenerator, ModelCatalog, OcaError, RANDOM_SUFFIX_WIDTH,
        ReplyContract, ResolvedModel, RoleReply, WorkerPolicy, is_opencode_message_id,
        resolve_model,
    };

    const REPLAY_PREVIOUS_ID: &str = "msg_f9a4a7900001AAAAAAAAAAAAAA";
    const REPLAY_SERVER_REPLY_ID: &str = "msg_f9a4a7b00001BBBBBBBBBBBBBB";

    enum ReplayMessageId {
        ProductionAt(u64),
        Injected(&'static str),
    }

    struct Gate0ServerReplay {
        source: ReplayMessageId,
        generator: MessageIdGenerator,
        submitted_id: Option<String>,
        prompts: usize,
        assistant_messages: usize,
        terminal_events: usize,
        aborted: bool,
        finalized: usize,
    }

    impl Gate0ServerReplay {
        fn production_at(timestamp_ms: u64) -> Self {
            Self::new(ReplayMessageId::ProductionAt(timestamp_ms))
        }

        fn injected(message_id: &'static str) -> Self {
            Self::new(ReplayMessageId::Injected(message_id))
        }

        fn new(source: ReplayMessageId) -> Self {
            Self {
                source,
                generator: MessageIdGenerator::new(),
                submitted_id: None,
                prompts: 0,
                assistant_messages: 0,
                terminal_events: 0,
                aborted: false,
                finalized: 0,
            }
        }
    }

    impl ForegroundBackend for Gate0ServerReplay {
        type Subscription = ();
        type PendingRef = String;

        async fn create_session(
            &mut self,
            _request: &ForegroundRequest,
        ) -> Result<String, OcaError> {
            Ok("ses_gate0_replay".to_owned())
        }

        async fn subscribe(&mut self) -> Result<Self::Subscription, OcaError> {
            Ok(())
        }

        fn mint_message_id(&mut self) -> Result<String, OcaError> {
            Ok(match self.source {
                ReplayMessageId::ProductionAt(timestamp_ms) => {
                    self.generator.mint(timestamp_ms, [0; RANDOM_SUFFIX_WIDTH])
                }
                ReplayMessageId::Injected(message_id) => message_id.to_owned(),
            })
        }

        async fn prompt_async(
            &mut self,
            _session_id: &str,
            prompt: &DispatchPrompt,
        ) -> Result<(), OcaError> {
            self.submitted_id = Some(prompt.message_id.clone());
            Ok(())
        }

        fn write_ref(
            &mut self,
            _session_id: &str,
            _message_id: &str,
            _request: &ForegroundRequest,
        ) -> Result<Self::PendingRef, OcaError> {
            Ok("wgate0".to_owned())
        }

        fn acknowledge(
            &mut self,
            pending: Self::PendingRef,
            _model: &ResolvedModel,
            _json: bool,
        ) -> Result<String, OcaError> {
            Ok(pending)
        }

        fn spawn_attach(
            &mut self,
            _reference: &str,
            _session_id: &str,
            _cwd: &std::path::Path,
            _display: DisplayMode,
        ) -> Result<(), OcaError> {
            Ok(())
        }

        async fn reconcile_once(
            &mut self,
            _session_id: &str,
            _message_id: &str,
        ) -> Result<Option<TerminalReply>, OcaError> {
            Ok(None)
        }

        async fn wait_terminal(
            &mut self,
            _subscription: &mut Self::Subscription,
            _session_id: &str,
            _message_id: &str,
        ) -> Result<TerminalReply, OcaError> {
            let submitted_id = self.submitted_id.as_deref().expect("prompt was admitted");
            if submitted_id <= REPLAY_PREVIOUS_ID {
                // Recorded gate-0 case 3: all six prompts survived, but two
                // adjacent pairs were merged into four assistant turns.
                self.prompts = 6;
                self.assistant_messages = 4;
                self.terminal_events = 4;
                return Ok(valid_terminal());
            }
            if submitted_id >= REPLAY_SERVER_REPLY_ID {
                // Recorded gate-0 high-id run: the server stayed busy and
                // regenerated until the experiment aborted after message 32.
                self.prompts = 1;
                self.assistant_messages = 32;
                self.aborted = true;
                return Err(OcaError::new(ErrorCode::Interrupted)
                    .with_error("gate-0 replay aborted unterminated generation"));
            }

            self.prompts = 1;
            self.assistant_messages = 1;
            self.terminal_events = 1;
            Ok(valid_terminal())
        }

        fn finalize(&mut self, _reference: &str, _reply: &RoleReply) -> Result<(), OcaError> {
            self.finalized += 1;
            Ok(())
        }

        fn print_final(
            &mut self,
            _reference: &str,
            _reply: &RoleReply,
            _json: bool,
        ) -> Result<(), OcaError> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeBackend {
        calls: Vec<&'static str>,
        prompt: Option<DispatchPrompt>,
        reconciled: Option<TerminalReply>,
        waited: Option<TerminalReply>,
        finalized: usize,
    }

    impl FakeBackend {
        fn normal() -> Self {
            Self {
                waited: Some(valid_terminal()),
                ..Self::default()
            }
        }

        fn fast() -> Self {
            Self {
                reconciled: Some(valid_terminal()),
                ..Self::default()
            }
        }
    }

    impl ForegroundBackend for FakeBackend {
        type Subscription = ();
        type PendingRef = String;

        async fn create_session(
            &mut self,
            request: &ForegroundRequest,
        ) -> Result<String, OcaError> {
            self.calls.push("create");
            assert!(
                request
                    .policy
                    .permission_profile()
                    .0
                    .iter()
                    .all(|rule| rule.action == crate::PermissionAction::Deny)
            );
            Ok("ses_target".to_owned())
        }

        async fn subscribe(&mut self) -> Result<Self::Subscription, OcaError> {
            self.calls.push("subscribe");
            Ok(())
        }

        fn mint_message_id(&mut self) -> Result<String, OcaError> {
            self.calls.push("mint");
            Ok("msg_f9a4a7a00001AAAAAAAAAAAAAA".to_owned())
        }

        async fn prompt_async(
            &mut self,
            _session_id: &str,
            prompt: &DispatchPrompt,
        ) -> Result<(), OcaError> {
            self.calls.push("prompt");
            assert_eq!(self.calls[..2], ["create", "subscribe"]);
            assert!(is_opencode_message_id(&prompt.message_id));
            assert!(prompt.permission.0.iter().all(|rule| {
                rule.action == crate::PermissionAction::Deny && rule.pattern == "*"
            }));
            self.prompt = Some(prompt.clone());
            Ok(())
        }

        fn write_ref(
            &mut self,
            _session_id: &str,
            _message_id: &str,
            _request: &ForegroundRequest,
        ) -> Result<Self::PendingRef, OcaError> {
            self.calls.push("write_ref");
            Ok("w00001".to_owned())
        }

        fn acknowledge(
            &mut self,
            pending: Self::PendingRef,
            _model: &ResolvedModel,
            _json: bool,
        ) -> Result<String, OcaError> {
            self.calls.push("ack");
            Ok(pending)
        }

        fn spawn_attach(
            &mut self,
            _reference: &str,
            _session_id: &str,
            _cwd: &std::path::Path,
            _display: DisplayMode,
        ) -> Result<(), OcaError> {
            self.calls.push("spawn");
            assert_eq!(self.calls[self.calls.len() - 2], "ack");
            Ok(())
        }

        async fn reconcile_once(
            &mut self,
            _session_id: &str,
            _message_id: &str,
        ) -> Result<Option<TerminalReply>, OcaError> {
            self.calls.push("reconcile");
            Ok(self.reconciled.take())
        }

        async fn wait_terminal(
            &mut self,
            _subscription: &mut Self::Subscription,
            _session_id: &str,
            _message_id: &str,
        ) -> Result<TerminalReply, OcaError> {
            self.calls.push("wait");
            self.waited.take().ok_or_else(|| {
                OcaError::new(ErrorCode::ServerUnreachable).with_error("missing fake terminal")
            })
        }

        fn finalize(&mut self, _reference: &str, _reply: &RoleReply) -> Result<(), OcaError> {
            self.calls.push("finalize");
            self.finalized += 1;
            Ok(())
        }

        fn print_final(
            &mut self,
            _reference: &str,
            _reply: &RoleReply,
            _json: bool,
        ) -> Result<(), OcaError> {
            self.calls.push("print_final");
            Ok(())
        }
    }

    #[test]
    fn every_alias_subscribes_before_prompt_and_carries_its_role_policy() {
        for (alias, effort) in [("luna", "h"), ("sol", "h"), ("terra", "h"), ("flash", "h")] {
            let mut backend = FakeBackend::normal();
            block_on(run_foreground(&mut backend, request(alias, effort)))
                .unwrap_or_else(|error| panic!("{alias} dispatch failed: {error}"));

            assert_eq!(
                backend.calls,
                [
                    "create",
                    "subscribe",
                    "mint",
                    "prompt",
                    "write_ref",
                    "ack",
                    "spawn",
                    "reconcile",
                    "wait",
                    "finalize",
                    "print_final",
                ]
            );
            let prompt = backend.prompt.expect("prompt captured");
            assert_eq!(prompt.model.alias, alias);
            assert_eq!(prompt.permission.0.len(), 5);
        }
    }

    #[test]
    fn post_subscribe_reconciliation_catches_a_turn_that_already_completed() {
        let mut backend = FakeBackend::fast();
        block_on(run_foreground(&mut backend, request("luna", "h"))).unwrap();

        assert!(backend.calls.contains(&"reconcile"));
        assert!(!backend.calls.contains(&"wait"));
        assert_eq!(backend.finalized, 1);
    }

    #[test]
    fn structural_failure_is_contract_invalid_and_never_finalizes_the_ref() {
        let mut backend = FakeBackend {
            reconciled: Some(TerminalReply {
                structured: json!({
                    "status":"done", "files":[], "note":"ok", "unexpected":true
                }),
            }),
            ..FakeBackend::default()
        };
        let error = block_on(run_foreground(&mut backend, request("luna", "h"))).unwrap_err();

        assert_eq!(error.code_kind(), ErrorCode::ContractInvalid);
        assert_eq!(backend.finalized, 0);
        assert!(!backend.calls.contains(&"print_final"));
        assert!(backend.calls.contains(&"write_ref"));
        assert!(backend.calls.contains(&"ack"));
    }

    #[test]
    fn replay_pins_too_low_turn_merge_and_too_high_regeneration() {
        let mut valid = Gate0ServerReplay::production_at(1_785_000_000_000);
        let outcome = block_on(run_foreground(&mut valid, request("luna", "h"))).unwrap();
        assert_eq!(outcome.message_id, "msg_f9a4a7a0000100000000000000");
        assert!(is_opencode_message_id(&outcome.message_id));
        assert_eq!(valid.prompts, 1);
        assert_eq!(valid.assistant_messages, 1);
        assert_eq!(valid.terminal_events, 1);
        assert_eq!(valid.finalized, 1);

        let mut too_low = Gate0ServerReplay::injected("msg_00000000000000000000000000");
        block_on(run_foreground(&mut too_low, request("luna", "h"))).unwrap();
        assert_eq!(too_low.prompts, 6);
        assert_eq!(too_low.assistant_messages, 4);
        assert_eq!(too_low.terminal_events, 4);

        let mut too_high = Gate0ServerReplay::injected("msg_oca_t03_idem_0001");
        let error = block_on(run_foreground(&mut too_high, request("luna", "h"))).unwrap_err();
        assert_eq!(error.code_kind(), ErrorCode::Interrupted);
        assert_eq!(too_high.prompts, 1);
        assert_eq!(too_high.assistant_messages, 32);
        assert_eq!(too_high.terminal_events, 0);
        assert!(too_high.aborted);
        assert_eq!(too_high.finalized, 0);
    }

    fn request(alias: &str, effort: &str) -> ForegroundRequest {
        let cwd = PathBuf::from("/work");
        ForegroundRequest {
            model: resolve_model(alias, effort, ModelCatalog::default()).unwrap(),
            prompt: "do the work".to_owned(),
            role: "impl".to_owned(),
            contract: ReplyContract::Impl,
            policy: WorkerPolicy::restricted([cwd.clone()]),
            cwd,
            display: DisplayMode::Herdr,
            json: false,
        }
    }

    fn valid_terminal() -> TerminalReply {
        TerminalReply {
            structured: json!({
                "status":"done",
                "files":[],
                "note":"Implemented the requested dispatch behavior with deterministic validation and complete coverage for each required boundary. Verified the resulting flow remains stable across normal completion paths."
            }),
        }
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(output) => return output,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }
}
