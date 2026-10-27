//! `SandboxLifecycleMiddleware` — orchestrates sandbox session lifecycle around each
//! agent call, mirroring `io.agentscope.harness.agent.middleware.SandboxLifecycleMiddleware`.
//!
//! Pre-call: acquires (or resumes) a sandbox via `SandboxManager`.
//! Post-call: persists the sandbox state and releases the lease.
//!
//! Filesystem injection (`SandboxBackedFilesystem`) happens at agent build time; this
//! middleware owns the acquire/persist/release orchestration point.
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::middleware::{AgentInput, EventStream, Middleware, Next};
use futures::stream::{self, StreamExt};

use crate::agent::sandbox::{SandboxContext, SandboxManager};

/// Middleware that acquires before, and persists/releases after, each agent call.
pub struct SandboxLifecycleMiddleware {
    manager: Arc<SandboxManager>,
    context: Arc<SandboxContext>,
}

impl SandboxLifecycleMiddleware {
    pub fn new(manager: Arc<SandboxManager>, context: SandboxContext) -> Self {
        Self {
            manager,
            context: Arc::new(context),
        }
    }

    /// Replaces the sandbox context used for acquisition decisions.
    pub fn with_context(mut self, context: SandboxContext) -> Self {
        self.context = Arc::new(context);
        self
    }
}

impl Middleware for SandboxLifecycleMiddleware {
    fn on_agent(
        &self,
        ctx: &RuntimeContext,
        input: AgentInput,
        next: Next<AgentInput>,
    ) -> EventStream {
        let manager = Arc::clone(&self.manager);
        let sctx = Arc::clone(&self.context);
        let rt = ctx.clone();
        let fut = async move {
            let acquired = match manager.acquire(&sctx, Some(&rt)).await {
                Ok(res) => Some(res),
                Err(e) => {
                    tracing::warn!("[sandbox-lifecycle] acquire failed: {e}");
                    None
                }
            };
            let stream = next(input);
            let events: Vec<_> = stream.collect().await;
            if let Some(res) = &acquired {
                manager.persist_state(res, &sctx, Some(&rt)).await;
                manager.release(res).await;
            }
            events
        };
        stream::once(fut).map(stream::iter).flatten().boxed()
    }
}
