//! `XxlJobHelper.log` bridge, mirroring the inline logging hook Java attaches in
//! `XxlJobAgentScheduler.schedule(...)`.
//!
//! Java adds an anonymous `Hook` to `RuntimeAgentConfig.getHooks()` that forwards
//! every `PostReasoningEvent` reasoning message to `XxlJobHelper.log(...)`, so the
//! XXL-Job admin console shows each intermediate agent step. Rust has no hooks;
//! the equivalent is a [`Middleware`] on the core `on_reasoning_result`
//! extension point (the Rust counterpart of `PostReasoningEvent`).
//!
//! The middleware is appended by [`XxlJobAgentScheduler::schedule_runtime`], so a
//! factory that builds its agent from `RuntimeAgentConfig::middleware()` gets the
//! logging for free — the same way Java's inline hook rides along with the config.

use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::Msg;
use agentscope_core::middleware::{ExtensionPoint, Middleware};

use super::context::XxlJobHelper;

/// Forwards every produced reasoning message to the current XXL-Job dispatch log.
#[derive(Debug, Default, Clone, Copy)]
pub struct XxlJobLogMiddleware;

impl XxlJobLogMiddleware {
    /// Creates the middleware.
    pub fn new() -> Self {
        Self
    }

    /// Creates it behind the `Arc<dyn Middleware>` a config chain expects.
    pub fn shared() -> std::sync::Arc<dyn Middleware> {
        std::sync::Arc::new(Self)
    }
}

#[async_trait::async_trait]
impl Middleware for XxlJobLogMiddleware {
    /// Only the post-reasoning notification is used: no behaviour of the run is
    /// intercepted or altered.
    fn active_points(&self) -> Vec<ExtensionPoint> {
        vec![ExtensionPoint::OnReasoningResult]
    }

    async fn on_reasoning_result(&self, _ctx: &RuntimeContext, message: &Msg) {
        // Java: `XxlJobHelper.log(postReasoningEvent.getReasoningMessage().getTextContent())`.
        // Outside a dispatch (plain unit test / local call) this is a no-op, exactly
        // like `XxlJobHelper.log` outside an executor thread in Java.
        XxlJobHelper::log(message.text_content());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agentscope_core::agent::ReActAgent;
    use agentscope_core::middleware::MiddlewareChain;

    use super::*;
    use crate::test_support::stub_model;
    use crate::xxljob::context::XxlJobContext;

    fn agent_with_logging() -> ReActAgent {
        let mut chain = MiddlewareChain::new();
        chain.add_arc(XxlJobLogMiddleware::shared());
        ReActAgent::builder()
            .name("logged-agent")
            .sys_prompt("sys")
            .model(stub_model("reasoned step"))
            .middleware_chain(chain)
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn reasoning_message_reaches_the_dispatch_log() {
        let ctx = Arc::new(XxlJobContext::new(1, "param").with_log_id(9));
        let run = ctx.clone();

        XxlJobContext::scope(run, async {
            let reply = agent_with_logging()
                .call(vec![Msg::user("hi")])
                .await
                .unwrap();
            assert_eq!(reply.text_content(), "reasoned step");
        })
        .await;

        // Java `PostReasoningEvent` -> `XxlJobHelper.log`, once per reasoning step.
        assert_eq!(ctx.logs(), vec!["reasoned step".to_string()]);
        assert!(
            ctx.handle_result().is_none(),
            "logging must not report a result"
        );
    }

    #[tokio::test]
    async fn logging_outside_a_dispatch_is_a_noop() {
        assert!(XxlJobContext::current().is_none());
        // Must not panic and must not invent a context.
        agent_with_logging()
            .call(vec![Msg::user("hi")])
            .await
            .unwrap();
        assert!(XxlJobContext::current().is_none());
    }
}
