//! Per-call tool interception.
//! Mirrors `io.agentscope.core.tool.{ToolCallInterceptor, ToolInvocation}`.
//!
//! Tool-level permission ("may this tool be called at all") stays the permission
//! engine's concern; an interceptor handles per-call, parameter-level policy that tool
//! level rules cannot express: auditing, argument normalization, sandboxing by caller
//! identity, policy denial and HITL suspension.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::Result;
use crate::message::{JsonMap, ToolResultBlock};

use super::tool::Tool;

/// A boxed future returned by an interceptor continuation.
pub type InterceptorFuture = Pin<Box<dyn Future<Output = Result<ToolResultBlock>> + Send>>;

/// The continuation handed to a [`ToolCallInterceptor`]: the next interceptor in the
/// chain, or the real tool execution once the chain is exhausted.
pub type InterceptorNext = Box<dyn FnOnce(ToolInvocation) -> InterceptorFuture + Send>;

/// Snapshot of a single upcoming tool call, captured after schema validation and preset
/// merging, right before real execution.
///
/// Interceptors observe the snapshot and decide the input the tool actually receives via
/// [`ToolInvocation::with_input`]. The copy is shallow: nested maps/lists are shared with
/// the caller, so implementations must not mutate them in place.
///
/// Unlike Java's record, the caller identity is not flattened into `agentId`/`agentName`
/// fields: the execution identity travels in [`ToolInvocation::runtime`], the same map the
/// tool itself receives.
#[derive(Clone)]
pub struct ToolInvocation {
    /// The tool call this invocation belongs to.
    pub tool_call_id: String,
    /// The tool name being invoked.
    pub tool_name: String,
    /// The resolved tool instance (metadata access, e.g. `is_read_only`).
    pub tool: Arc<dyn Tool>,
    /// The final merged input (LLM arguments plus preset parameters).
    pub input: JsonMap,
    /// The execution context's runtime map (identity, per-call services).
    pub runtime: JsonMap,
}

impl ToolInvocation {
    /// Returns a copy of this invocation with the input replaced.
    pub fn with_input(mut self, input: JsonMap) -> Self {
        self.input = input;
        self
    }
}

impl std::fmt::Debug for ToolInvocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolInvocation")
            .field("tool_call_id", &self.tool_call_id)
            .field("tool_name", &self.tool_name)
            .field("input", &self.input)
            .finish()
    }
}

/// Intercepts a single tool call at the toolkit layer, after schema validation and preset
/// merging and before real execution.
///
/// Implementations must be stateless or thread-safe (they are shared across concurrent
/// sessions) and must always call `next` or return a result block. Common actions:
///
/// - pass through: `next(invocation).await`
/// - policy deny: return `ToolResultBlock::text(msg).with_state(ToolResultState::Denied)`
///   (the observation layer records a `denied` decision)
/// - invalid call: return `ToolResultBlock::error(msg)` (no denied decision)
/// - rewrite arguments: `next(invocation.with_input(new_input)).await`
/// - suspend for HITL: return `Err(Error::ToolSuspended(reason))`
#[async_trait]
pub trait ToolCallInterceptor: Send + Sync {
    /// Intercepts one call.
    async fn intercept(
        &self,
        invocation: ToolInvocation,
        next: InterceptorNext,
    ) -> Result<ToolResultBlock>;

    /// Execution order: a larger value sits closer to the outside of the chain (intercepts
    /// first, receives the result last). Same-order interceptors keep their registration
    /// order. Mirrors Java `ToolCallInterceptor.order()` (default `1`).
    fn order(&self) -> i32 {
        1
    }
}

/// Runs the interception chain around `execution`, outermost first.
pub(crate) async fn run_interceptor_chain(
    interceptors: &[Arc<dyn ToolCallInterceptor>],
    invocation: ToolInvocation,
    execution: InterceptorNext,
) -> Result<ToolResultBlock> {
    let mut sorted: Vec<&Arc<dyn ToolCallInterceptor>> = interceptors.iter().collect();
    // Stable sort: equal orders keep their registration order.
    sorted.sort_by(|a, b| b.order().cmp(&a.order()));

    let mut next = execution;
    for interceptor in sorted.into_iter().rev() {
        let interceptor = Arc::clone(interceptor);
        let inner = next;
        next = Box::new(move |invocation: ToolInvocation| {
            let interceptor = Arc::clone(&interceptor);
            Box::pin(async move { interceptor.intercept(invocation, inner).await })
        });
    }
    next(invocation).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ToolResultState, ToolUseBlock};
    use crate::tool::{ToolExecutionContext, ToolExecutor, Toolkit, ToolkitConfig};
    use std::sync::Mutex;

    /// A tool that records every input it receives.
    struct RecordingTool {
        seen: Arc<Mutex<Vec<JsonMap>>>,
    }

    #[async_trait]
    impl Tool for RecordingTool {
        fn name(&self) -> &str {
            "probe"
        }
        fn description(&self) -> &str {
            "records its input"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn call(
            &self,
            args: JsonMap,
            _ctx: ToolExecutionContext,
        ) -> Result<ToolResultBlock> {
            self.seen.lock().unwrap().push(args.clone());
            Ok(ToolResultBlock::text(format!(
                "value={}",
                args.get("value")
                    .and_then(|v| v.as_str())
                    .unwrap_or("none")
            )))
        }
    }

    /// Records the interceptor entry order and delegates.
    struct DelegatingInterceptor {
        name: &'static str,
        order: i32,
        seen: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl ToolCallInterceptor for DelegatingInterceptor {
        async fn intercept(
            &self,
            invocation: ToolInvocation,
            next: InterceptorNext,
        ) -> Result<ToolResultBlock> {
            self.seen.lock().unwrap().push(self.name);
            next(invocation).await
        }
        fn order(&self) -> i32 {
            self.order
        }
    }

    fn input(value: &str) -> JsonMap {
        let mut map = JsonMap::new();
        map.insert("value".into(), serde_json::json!(value));
        map
    }

    fn toolkit(interceptors: Vec<Arc<dyn ToolCallInterceptor>>, seen: Arc<Mutex<Vec<JsonMap>>>) -> Toolkit {
        let toolkit = Toolkit::with_config(
            ToolkitConfig::builder()
                .call_interceptors(interceptors)
                .build(),
        );
        toolkit.register_tool(Arc::new(RecordingTool { seen }));
        toolkit
    }

    fn call() -> ToolUseBlock {
        ToolUseBlock::new("c1", "probe", input("original"))
    }

    async fn run(
        interceptors: Vec<Arc<dyn ToolCallInterceptor>>,
    ) -> (Result<ToolResultBlock>, Vec<JsonMap>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let toolkit = toolkit(interceptors, Arc::clone(&seen));
        let result = ToolExecutor::new()
            .execute(&toolkit, &call(), ToolExecutionContext::default(), None)
            .await;
        let inputs = seen.lock().unwrap().clone();
        (result, inputs)
    }

    #[tokio::test]
    async fn an_empty_chain_executes_the_tool_directly() {
        let (result, seen) = run(Vec::new()).await;
        assert_eq!(result.unwrap().text_content(), "value=original");
        assert_eq!(seen, vec![input("original")]);
    }

    #[tokio::test]
    async fn a_pass_through_interceptor_does_not_change_the_call() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let (result, seen) = run(vec![Arc::new(DelegatingInterceptor {
            name: "audit",
            order: 1,
            seen: Arc::clone(&order),
        })])
        .await;
        assert_eq!(result.unwrap().text_content(), "value=original");
        assert_eq!(seen, vec![input("original")]);
        assert_eq!(order.lock().unwrap().clone(), vec!["audit"]);
    }

    #[tokio::test]
    async fn an_interceptor_can_rewrite_the_input_the_tool_sees() {
        struct Rewrite;

        #[async_trait]
        impl ToolCallInterceptor for Rewrite {
            async fn intercept(
                &self,
                invocation: ToolInvocation,
                next: InterceptorNext,
            ) -> Result<ToolResultBlock> {
                next(invocation.with_input(input("rewritten"))).await
            }
        }

        let (result, seen) = run(vec![Arc::new(Rewrite)]).await;
        assert_eq!(result.unwrap().text_content(), "value=rewritten");
        assert_eq!(seen, vec![input("rewritten")]);
    }

    #[tokio::test]
    async fn a_policy_denial_returns_a_denied_result_without_running_the_tool() {
        struct Deny;

        #[async_trait]
        impl ToolCallInterceptor for Deny {
            async fn intercept(
                &self,
                _invocation: ToolInvocation,
                _next: InterceptorNext,
            ) -> Result<ToolResultBlock> {
                Ok(ToolResultBlock::text("blocked by policy").with_state(ToolResultState::Denied))
            }
        }

        let (result, seen) = run(vec![Arc::new(Deny)]).await;
        let block = result.unwrap();
        assert_eq!(block.state, ToolResultState::Denied);
        assert!(seen.is_empty(), "a denied call must never reach the tool");
    }

    #[tokio::test]
    async fn a_suspend_request_becomes_a_suspended_result() {
        struct Suspend;

        #[async_trait]
        impl ToolCallInterceptor for Suspend {
            async fn intercept(
                &self,
                _invocation: ToolInvocation,
                _next: InterceptorNext,
            ) -> Result<ToolResultBlock> {
                Err(crate::error::Error::ToolSuspended("needs approval".into()))
            }
        }

        let (result, seen) = run(vec![Arc::new(Suspend)]).await;
        let block = result.unwrap();
        assert!(block.is_suspended());
        assert_eq!(block.suspend_reason(), Some("needs approval"));
        assert_eq!(block.id.as_deref(), Some("c1"));
        assert!(seen.is_empty());
    }

    #[tokio::test]
    async fn a_higher_order_interceptor_sits_outermost() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let interceptors: Vec<Arc<dyn ToolCallInterceptor>> = vec![
            Arc::new(DelegatingInterceptor {
                name: "inner",
                order: 1,
                seen: Arc::clone(&order),
            }),
            Arc::new(DelegatingInterceptor {
                name: "outer",
                order: 5,
                seen: Arc::clone(&order),
            }),
        ];
        run(interceptors).await;
        assert_eq!(order.lock().unwrap().clone(), vec!["outer", "inner"]);
    }
}
