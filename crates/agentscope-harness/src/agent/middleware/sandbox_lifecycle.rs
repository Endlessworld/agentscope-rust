//! `SandboxLifecycleMiddleware` — orchestrates sandbox session lifecycle around each
//! agent call, mirroring `io.agentscope.harness.agent.middleware.SandboxLifecycleMiddleware`.
//!
//! Pre-call: acquires (or resumes) a sandbox via `SandboxManager` — but **only when the call
//! can actually reach a sandbox**. The acquisition takes an execution-guard lease (fail-closed)
//! and resumes or creates the backend, so paying it for a pure-chat turn is pure cost.
//! Post-call: persists the sandbox state and releases the lease.
//!
//! Filesystem injection (`SandboxBackedFilesystem`) happens at agent build time; this
//! middleware owns the acquire/persist/release orchestration point.
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::middleware::{AgentInput, EventStream, Middleware, Next};
use futures::stream::{self, StreamExt};

use crate::agent::sandbox::{SandboxContext, SandboxManager};
use crate::tool::ShellExecuteTool;

/// Tool names that can only run against an acquired sandbox.
///
/// `execute` is the shell tool bound to the sandbox filesystem; `deliver_artifact` reads the
/// sandbox workspace to export deliverables. A turn with neither of them (pure chat, or only
/// hosted tools) never touches the sandbox and must not pay for one.
const SANDBOX_BOUND_TOOLS: [&str; 2] = [ShellExecuteTool::NAME, "deliver_artifact"];

/// Middleware that acquires before, and persists/releases after, each agent call.
pub struct SandboxLifecycleMiddleware {
    manager: Arc<SandboxManager>,
    context: Arc<SandboxContext>,
    /// Acquire unconditionally, even when no sandbox-bound capability is visible.
    always_acquire: bool,
}

impl SandboxLifecycleMiddleware {
    pub fn new(manager: Arc<SandboxManager>, context: SandboxContext) -> Self {
        Self {
            manager,
            context: Arc::new(context),
            always_acquire: false,
        }
    }

    /// Replaces the sandbox context used for acquisition decisions.
    pub fn with_context(mut self, context: SandboxContext) -> Self {
        self.context = Arc::new(context);
        self
    }

    /// Acquires the sandbox on every call, including turns that expose no sandbox-bound tool.
    ///
    /// For hosts whose sandbox is reached through a channel this middleware cannot see (for
    /// example a host-registered tool that runs commands itself).
    pub fn with_always_acquire(mut self, always: bool) -> Self {
        self.always_acquire = always;
        self
    }

    /// Whether this call can reach a sandbox, i.e. whether acquiring one is worth a guard
    /// lease + resume/create.
    ///
    /// Signals: an explicit `always_acquire`, a sandbox-bound tool registered on the call's
    /// toolkit, or a capability surface that grants one.
    fn needs_sandbox(&self, ctx: &RuntimeContext) -> bool {
        if self.always_acquire {
            return true;
        }
        if let Some(toolkit) = ctx.toolkit.as_ref() {
            if SANDBOX_BOUND_TOOLS
                .iter()
                .any(|name| toolkit.get_tool(name).is_some())
            {
                return true;
            }
        }
        if let Some(surface) = agentscope_core::agent::capability_surface_from_context(ctx) {
            if let Some(tools) = surface.allowed_tools() {
                if SANDBOX_BOUND_TOOLS.iter().any(|name| tools.contains(*name)) {
                    return true;
                }
            }
        }
        false
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
        let acquire = self.needs_sandbox(ctx);
        let fut = async move {
            let acquired = if acquire {
                match manager.acquire(&sctx, Some(&rt)).await {
                    Ok(res) => Some(res),
                    Err(e) => {
                        tracing::warn!("[sandbox-lifecycle] acquire failed: {e}");
                        None
                    }
                }
            } else {
                None
            };
            let stream = next(input);
            let events: Vec<_> = stream.collect().await;
            if let Some(res) = acquired {
                manager.persist_state(&res, &sctx, Some(&rt)).await;
                // By value: `release` owns the result so it can close a self-managed
                // sandbox (stop + shutdown) instead of only releasing the lease.
                manager.release(res).await;
            }
            events
        };
        stream::once(fut).map(stream::iter).flatten().boxed()
    }
}
