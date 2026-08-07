//! Background dispatch ownership over the shared foreground admission prefix.

use crate::{ForegroundBackend, ForegroundRequest, OcaError, foreground::start_dispatch};

/// A background dispatch has the same fully resolved local inputs as a
/// foreground dispatch; only terminal ownership differs.
pub type BackgroundRequest = ForegroundRequest;

/// Successful background admission. Completion is intentionally absent: a
/// separate follow process owns reconciliation, waiting, decoding, and
/// finalization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackgroundOutcome {
    pub reference: String,
    pub session_id: String,
    pub message_id: String,
}

/// Runs the shared subscribe-before-prompt prefix and returns immediately after
/// the post-ack detached attach spawn.
///
/// The opened subscription is deliberately owned only through prompt admission
/// and acknowledgement. It is never consumed here. A separately invoked
/// `oca f` opens its own stream, performs the one-shot messages reconciliation,
/// and owns all terminal work.
///
/// # Errors
///
/// Returns the first session, subscription, prompt, ref, acknowledgement, or
/// detached-attach admission error.
pub async fn run_background<B>(
    backend: &mut B,
    request: BackgroundRequest,
) -> Result<BackgroundOutcome, OcaError>
where
    B: ForegroundBackend,
{
    let started = start_dispatch(backend, request).await?;
    Ok(BackgroundOutcome {
        reference: started.reference,
        session_id: started.session_id,
        message_id: started.message_id,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        path::{Path, PathBuf},
        pin::pin,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll, Waker},
    };

    use super::{BackgroundRequest, run_background};
    use crate::{
        DispatchPrompt, DisplayMode, ForegroundBackend, ModelCatalog, OcaError, ReplyContract,
        ResolvedModel, RoleReply, TerminalReply, WorkerPolicy, resolve_model,
    };

    struct Subscription(Arc<AtomicUsize>);

    impl Drop for Subscription {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct FakeBackend {
        calls: Vec<&'static str>,
        subscription_drops: Arc<AtomicUsize>,
    }

    impl FakeBackend {
        fn new(subscription_drops: Arc<AtomicUsize>) -> Self {
            Self {
                calls: Vec::new(),
                subscription_drops,
            }
        }
    }

    impl ForegroundBackend for FakeBackend {
        type Subscription = Subscription;
        type PendingRef = String;

        async fn create_session(
            &mut self,
            _request: &BackgroundRequest,
        ) -> Result<String, OcaError> {
            self.calls.push("create");
            Ok("ses_background".to_owned())
        }

        async fn subscribe(&mut self) -> Result<Self::Subscription, OcaError> {
            self.calls.push("subscribe");
            Ok(Subscription(Arc::clone(&self.subscription_drops)))
        }

        fn mint_message_id(&mut self) -> Result<String, OcaError> {
            self.calls.push("mint");
            Ok("msg_f9a4a7a00001AAAAAAAAAAAAAA".to_owned())
        }

        async fn prompt_async(
            &mut self,
            _session_id: &str,
            _prompt: &DispatchPrompt,
        ) -> Result<(), OcaError> {
            self.calls.push("prompt");
            assert_eq!(self.calls[..2], ["create", "subscribe"]);
            Ok(())
        }

        async fn confirm_prompt_landed(
            &mut self,
            _subscription: &mut Self::Subscription,
            _session_id: &str,
            _prompt: &DispatchPrompt,
        ) -> Result<(), OcaError> {
            self.calls.push("confirm");
            Ok(())
        }

        fn mark_prompt_running(&mut self) -> Result<(), OcaError> {
            self.calls.push("running");
            Ok(())
        }

        fn write_ref(
            &mut self,
            _session_id: &str,
            _message_id: &str,
            _request: &crate::ForegroundRequest,
        ) -> Result<Self::PendingRef, OcaError> {
            self.calls.push("write_ref");
            Ok("wback1".to_owned())
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
            _cwd: &Path,
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
            panic!("background dispatch must not reconcile messages")
        }

        async fn wait_terminal(
            &mut self,
            _subscription: &mut Self::Subscription,
            _session_id: &str,
            _message_id: &str,
        ) -> Result<TerminalReply, OcaError> {
            panic!("background dispatch must not wait on SSE")
        }

        fn finalize(&mut self, _reference: &str, _reply: &RoleReply) -> Result<(), OcaError> {
            panic!("background dispatch must not finalize")
        }

        fn print_final(
            &mut self,
            _reference: &str,
            _reply: &RoleReply,
            _json: bool,
        ) -> Result<(), OcaError> {
            panic!("background dispatch must not print a final reply")
        }
    }

    #[test]
    fn background_returns_after_the_post_ack_attach_without_terminal_work() {
        let subscription_drops = Arc::new(AtomicUsize::new(0));
        let mut backend = FakeBackend::new(Arc::clone(&subscription_drops));

        let outcome = block_on(run_background(&mut backend, request())).unwrap();

        assert_eq!(
            backend.calls,
            [
                "create",
                "subscribe",
                "mint",
                "prompt",
                "confirm",
                "running",
                "write_ref",
                "ack",
                "spawn"
            ]
        );
        assert_eq!(outcome.reference, "wback1");
        assert_eq!(outcome.session_id, "ses_background");
        assert_eq!(outcome.message_id, "msg_f9a4a7a00001AAAAAAAAAAAAAA");
        assert_eq!(subscription_drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn tooled_incompatible_background_model_fails_before_provider_access() {
        let subscription_drops = Arc::new(AtomicUsize::new(0));
        let mut backend = FakeBackend::new(Arc::clone(&subscription_drops));
        let mut request = request();
        request.model = resolve_model("flash", "high", ModelCatalog::default()).unwrap();

        let error = block_on(run_background(&mut backend, request))
            .expect_err("the default flash model must be rejected locally");

        assert_eq!(error.code_kind(), crate::ErrorCode::ModelUnsupportedTooled);
        assert!(
            backend.calls.is_empty(),
            "blocked dispatch must be side-effect free"
        );
        assert_eq!(subscription_drops.load(Ordering::SeqCst), 0);
    }

    fn request() -> BackgroundRequest {
        let cwd = PathBuf::from("/work");
        BackgroundRequest {
            model: resolve_model("luna", "h", ModelCatalog::default()).unwrap(),
            prompt: "do the work".to_owned(),
            role: "impl".to_owned(),
            contract: ReplyContract::Impl,
            policy: WorkerPolicy::restricted([cwd.clone()]),
            cwd,
            display: DisplayMode::Headless,
            json: false,
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
