//! Invocation-scoped cooperation with an execution host, mirroring
//! `io.agentscope.core.agent.AgentExecutionHost`.
//!
//! Core owns its loop; the host coordinates persistent input and control at native boundaries and
//! can resume finalization. State and facts still commit through the native journal, which may
//! share the host's transaction. Implementations must keep blocking storage work off the async
//! runtime's worker threads.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;

use crate::agent::RuntimeContext;
use crate::error::Result;
use crate::message::Msg;
use crate::state::AgentState;

/// Context key of the host handle ([`Arc<dyn AgentExecutionHost>`]).
pub const EXECUTION_HOST_CONTEXT_KEY: &str = "execution_host";

/// Native phase boundary at which a host may apply persistent input or stop the invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionBoundary {
    Reasoning,
    Model,
    Acting,
    Finalization,
}

/// The remainder of finalization (persist the reply, run finish middleware), executed by the
/// host. Returning [`crate::error::Error::HostPaused`] instead of awaiting it preserves a
/// recovery position.
pub type FinishContinuation<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

/// Cooperation with the execution host of one invocation.
///
/// Java's `finish(context, result, continuation)` is represented by the same three-phase shape:
/// the host persists the reply, then awaits `continuation`, which re-checks the finalization
/// boundary before running finish middleware.
#[async_trait]
#[allow(unused_variables)]
pub trait AgentExecutionHost: Send + Sync {
    /// Runs before a new phase starts; input applied at [`ExecutionBoundary::Reasoning`] enters
    /// the next model context.
    async fn before_boundary(
        &self,
        boundary: ExecutionBoundary,
        context: &RuntimeContext,
        state: &mut AgentState,
    ) -> Result<()> {
        Ok(())
    }

    /// A previously produced reply whose finalization must resume, or `None` for ordinary
    /// execution.
    fn resume_result(&self) -> Option<Msg> {
        None
    }

    /// Prepares, runs or restores finalization while the recorder and authorization are live.
    async fn finish<'a>(
        &'a self,
        context: &RuntimeContext,
        result: &Msg,
        continuation: FinishContinuation<'a>,
    ) -> Result<()> {
        continuation.await
    }
}

/// Reads the host pinned on the context, when one is present.
pub fn execution_host(context: &RuntimeContext) -> Option<Arc<dyn AgentExecutionHost>> {
    context
        .extension::<Arc<dyn AgentExecutionHost>>(EXECUTION_HOST_CONTEXT_KEY)
        .map(|host| (*host).clone())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::error::Error;

    struct RecordingHost {
        boundaries: AtomicUsize,
        finished: AtomicUsize,
    }

    #[async_trait]
    impl AgentExecutionHost for RecordingHost {
        async fn before_boundary(
            &self,
            _boundary: ExecutionBoundary,
            _context: &RuntimeContext,
            _state: &mut AgentState,
        ) -> Result<()> {
            self.boundaries.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn finish<'a>(
            &'a self,
            _context: &RuntimeContext,
            _result: &Msg,
            continuation: FinishContinuation<'a>,
        ) -> Result<()> {
            self.finished.fetch_add(1, Ordering::SeqCst);
            continuation.await
        }
    }

    #[tokio::test]
    async fn host_runs_boundaries_and_finalization() {
        let host = Arc::new(RecordingHost {
            boundaries: AtomicUsize::new(0),
            finished: AtomicUsize::new(0),
        });
        let mut context = RuntimeContext::default();
        context.set_extension(
            EXECUTION_HOST_CONTEXT_KEY,
            Arc::clone(&host) as Arc<dyn AgentExecutionHost>,
        );
        let resolved = execution_host(&context).expect("host is pinned on the context");

        resolved
            .before_boundary(
                ExecutionBoundary::Reasoning,
                &context,
                &mut AgentState::default(),
            )
            .await
            .unwrap();
        let continuation = Box::pin(async { Ok(()) });
        resolved
            .finish(&context, &Msg::user("hi"), continuation)
            .await
            .unwrap();

        assert_eq!(host.boundaries.load(Ordering::SeqCst), 1);
        assert_eq!(host.finished.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_host_can_pause_finalization_without_running_the_continuation() {
        struct PausingHost;

        #[async_trait]
        impl AgentExecutionHost for PausingHost {
            async fn finish<'a>(
                &'a self,
                _context: &RuntimeContext,
                _result: &Msg,
                _continuation: FinishContinuation<'a>,
            ) -> Result<()> {
                Err(Error::HostPaused)
            }
        }

        let host = PausingHost;
        let ran = Arc::new(AtomicUsize::new(0));
        let flag = Arc::clone(&ran);
        let continuation = Box::pin(async move {
            flag.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let error = host
            .finish(&RuntimeContext::default(), &Msg::user("hi"), continuation)
            .await
            .expect_err("pausing aborts finalization");
        assert!(matches!(error, Error::HostPaused));
        assert_eq!(ran.load(Ordering::SeqCst), 0, "continuation must not run");
    }
}
