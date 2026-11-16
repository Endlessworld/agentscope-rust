//! Tool execution pipeline: lookup, authorization, validation, timeout, retry.
//! Mirrors `io.agentscope.core.tool.ToolExecutor`.
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::message::{JsonMap, ToolResultBlock, ToolUseBlock};
use crate::model::ExecutionConfig;

use super::interceptor::{run_interceptor_chain, ToolInvocation};
use super::tool::{Tool, ToolExecutionContext};
use super::toolkit::Toolkit;
use super::validator::ToolValidator;

/// Executes tool calls through the full governance pipeline.
#[derive(Clone, Default)]
pub struct ToolExecutor;

impl ToolExecutor {
    pub fn new() -> Self {
        Self
    }

    /// Executes a single tool call with the given config.
    ///
    /// Pipeline: lookup -> external short-circuit -> group activation check ->
    /// schema validation -> preset-parameter merge -> raw call with timeout/retry.
    pub async fn execute(
        &self,
        toolkit: &Toolkit,
        call: &ToolUseBlock,
        ctx: ToolExecutionContext,
        config: Option<&ExecutionConfig>,
    ) -> Result<ToolResultBlock> {
        // A malformed-arguments marker means the raw streamed arguments were truncated or
        // corrupt. The agent loop refuses these calls before execution; this is the single
        // funnel for direct Toolkit/executor paths too, and the merged input beside the marker
        // can coincidentally look complete, so refuse here instead of executing it.
        if call.has_malformed_arguments() {
            return Err(Error::Tool(
                "Tool call not executed: its arguments were marked malformed (truncated or \
                 corrupt). Reissue the call with corrected JSON object arguments."
                    .into(),
            ));
        }
        // 1. Lookup (composed with the per-call request config: external tools take
        //    priority; EXTERNAL_ONLY hides the backend).
        let request_config = ctx.tool_request_config.clone();
        let view = request_config.as_deref();
        let tool = toolkit
            .get_tool_with(&call.name, view)
            .ok_or_else(|| Error::Tool(format!("Tool not found: {}", call.name)))?;
        // 2. External short-circuit (suspended semantics).
        if tool.is_external() {
            return Err(Error::Tool(format!(
                "Tool '{}' is external and cannot be executed by the framework",
                call.name
            )));
        }
        // 3. Capability grant gate. Runs *before* the activation check so an ungranted probe
        //    never reaches activation wording, and reports the same text a missing tool does:
        //    the model channel must not distinguish "exists but ungranted" from "missing".
        //    Only backend tools are gated — a tool injected through the request config was
        //    supplied by the caller for this call.
        let from_request_config = view.map(|c| c.overrides(&call.name)).unwrap_or(false);
        if !from_request_config {
            if let Some(surface) = ctx.capability_surface() {
                let groups = toolkit.tool_groups_for(&call.name);
                if !surface.grants_tool(&call.name, &groups, tool.is_system()) {
                    tracing::info!(
                        "tool call denied by capability grant: tool='{}', userId='{}', \
                         sessionId='{}'",
                        call.name,
                        ctx.runtime
                            .get("userId")
                            .and_then(|v| v.as_str())
                            .unwrap_or(""),
                        ctx.runtime
                            .get("sessionId")
                            .and_then(|v| v.as_str())
                            .unwrap_or(""),
                    );
                    return Err(Error::Tool(format!("Tool not found: {}", call.name)));
                }
            }
        }
        // 4. Group activation check. Only backend tools are gated: a tool injected through
        //    the request config is ungrouped and always callable, so a same-named but
        //    inactive backend tool must not reject the call the model was shown.
        if !from_request_config && !toolkit.is_tool_active(&call.name) {
            return Err(Error::Tool(format!(
                "Unauthorized tool call: {} is not available",
                call.name
            )));
        }
        // 5. Schema validation.
        // strict 模式用「注入了 additionalProperties:false 的同一份 schema」校验，
        // 因此 schema 未声明的入参会被拒绝（与发给模型的 schema 保持一致）。
        let schema = if tool.is_strict() {
            super::schema::effective_parameters(&tool.parameters_schema(), true)
        } else {
            tool.parameters_schema()
                .as_object()
                .cloned()
                .unwrap_or_default()
        };
        let input_val = serde_json::Value::Object(call.input.clone());
        if let Some(err) = ToolValidator::validate_input(&schema, &input_val) {
            return Err(Error::Tool(format!("Parameter validation failed: {}", err)));
        }
        // 6. Preset parameters override (preset wins over LLM input).
        let input = toolkit.merge_presets(&call.name, call.input.clone());
        // 7. Per-call interception (Java `ToolCallInterceptor`): after schema validation
        //    and preset merging, before real execution. External tools never reach it
        //    (they short-circuited above).
        let interceptors = toolkit.config().call_interceptors.clone();
        if !interceptors.is_empty() {
            let invocation = ToolInvocation {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                tool: Arc::clone(&tool),
                input,
                runtime: ctx.runtime.clone(),
            };
            let owned_call = call.clone();
            let owned_ctx = ctx.clone();
            let owned_config = config.cloned();
            let execution: super::interceptor::InterceptorNext =
                Box::new(move |invocation: ToolInvocation| {
                    let call = owned_call.clone();
                    let ctx = owned_ctx.clone();
                    let config = owned_config.clone();
                    Box::pin(async move {
                        let tool = Arc::clone(&invocation.tool);
                        execute_with_policy(&tool, &call, ctx, invocation.input, config.as_ref())
                            .await
                    })
                });
            let call_id = call.id.clone();
            let call_name = call.name.clone();
            return match run_interceptor_chain(&interceptors, invocation, execution).await {
                // A suspend raised by an interceptor has the same semantics as one raised
                // by the tool itself: the call halts for external execution.
                Err(Error::ToolSuspended(reason)) => {
                    Ok(ToolResultBlock::suspended(reason, &call_id, &call_name))
                }
                other => other,
            };
        }
        // 8. Timeout + retry around the raw call.
        execute_with_policy(&tool, call, ctx, input, config).await
    }

    /// Executes a batch of tool calls, sequentially or in parallel (order kept).
    ///
    /// In parallel mode, contiguous runs of concurrency-safe tools are executed
    /// concurrently while preserving output order; tools that are not
    /// concurrency-safe (or unknown) form their own serial slots so two
    /// invocations that share state never overlap.
    pub async fn execute_all(
        &self,
        toolkit: &Toolkit,
        calls: &[ToolUseBlock],
        ctx: ToolExecutionContext,
        config: Option<&ExecutionConfig>,
        parallel: bool,
    ) -> Vec<Result<ToolResultBlock>> {
        if calls.is_empty() {
            return Vec::new();
        }
        if parallel {
            // Partition into contiguous safe batches and serial (unsafe) slots,
            // then run the chunks in sequence; within a safe batch the calls run
            // concurrently but are collected in declared order.
            let mut out = Vec::with_capacity(calls.len());
            let mut safe_batch: Vec<&ToolUseBlock> = Vec::new();
            for call in calls {
                if toolkit.is_concurrency_safe_tool(&call.name) {
                    safe_batch.push(call);
                } else {
                    run_safe_batch(self, toolkit, &safe_batch, &ctx, config, &mut out).await;
                    safe_batch.clear();
                    out.push(self.execute(toolkit, call, ctx.clone(), config).await);
                }
            }
            run_safe_batch(self, toolkit, &safe_batch, &ctx, config, &mut out).await;
            out
        } else {
            let mut out = Vec::with_capacity(calls.len());
            for call in calls {
                out.push(self.execute(toolkit, call, ctx.clone(), config).await);
            }
            out
        }
    }
}

/// Executes a contiguous batch of concurrency-safe calls concurrently, appending
/// the results (in declared order) to `out`.
async fn run_safe_batch(
    executor: &ToolExecutor,
    toolkit: &Toolkit,
    batch: &[&ToolUseBlock],
    ctx: &ToolExecutionContext,
    config: Option<&ExecutionConfig>,
    out: &mut Vec<Result<ToolResultBlock>>,
) {
    if batch.is_empty() {
        return;
    }
    let futures: Vec<_> = batch
        .iter()
        .map(|call| executor.execute(toolkit, call, ctx.clone(), config))
        .collect();
    let results = futures::future::join_all(futures).await;
    out.extend(results);
}
/// Runs a single tool call with optional timeout and retry.
async fn execute_with_policy(
    tool: &Arc<dyn Tool>,
    call: &ToolUseBlock,
    ctx: ToolExecutionContext,
    input: JsonMap,
    config: Option<&ExecutionConfig>,
) -> Result<ToolResultBlock> {
    let cfg = config
        .cloned()
        .unwrap_or_else(ExecutionConfig::tool_defaults);
    // Long-running tools (e.g. a blocking `task_output` barrier) opt out of the
    // timeout wrapper: cutting them would abort work the caller explicitly asked
    // to wait for.
    let timeout = if tool.is_long_running() {
        None
    } else {
        cfg.timeout
    };
    let max_attempts = cfg.max_attempts.unwrap_or(1);
    let mut attempt: u32 = 0;
    loop {
        let fut = tool.call(input.clone(), ctx.clone());
        // A timeout is an attempt failure, not an exit: `?` here used to leave the
        // retry loop, so `max_attempts` was unreachable for the one failure mode a
        // hung tool actually produces. The error is `Error::Tool`, which
        // `Error::is_retryable` classifies as transient because of its wording.
        let result = match timeout {
            Some(d) => match tokio::time::timeout(d, fut).await {
                Ok(result) => result,
                Err(_) => Err(Error::Tool(format!(
                    "Tool execution timeout after {:?} for '{}'",
                    d, call.name
                ))),
            },
            None => fut.await,
        };
        match result {
            Ok(rb) => return Ok(rb),
            Err(Error::ToolSuspended(reason)) => {
                // Suspended semantics: convert to a pending ToolResultBlock flagged as
                // suspended so the agent loop can halt with GenerateReason::ToolSuspended.
                return Ok(ToolResultBlock::suspended(reason, &call.id, &call.name));
            }
            Err(e) => {
                attempt += 1;
                // Only transient failures are retried: a business error (unknown
                // tool, invalid arguments, a refused call) would fail identically and
                // a tool that already had a side effect would apply it twice.
                let retryable = e.is_retryable();
                if attempt >= max_attempts || !retryable {
                    return Err(e);
                }
                tracing::warn!(
                    tool = %call.name,
                    attempt,
                    max_attempts,
                    error = %e,
                    "tool call failed transiently; retrying"
                );
                tokio::time::sleep(cfg.backoff_for(attempt - 1)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{JsonMap, ToolResultBlock, ToolUseBlock};
    use crate::tool::{FunctionTool, Tool, ToolExecutionContext, Toolkit};
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::time::Duration;

    /// A tool that sleeps past the test timeout before answering.
    struct SleepyTool {
        long_running: bool,
    }

    #[async_trait]
    impl Tool for SleepyTool {
        fn name(&self) -> &str {
            "sleepy"
        }
        fn description(&self) -> &str {
            "sleeps past the timeout"
        }
        fn is_long_running(&self) -> bool {
            self.long_running
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn call(
            &self,
            _args: JsonMap,
            _ctx: ToolExecutionContext,
        ) -> crate::error::Result<ToolResultBlock> {
            tokio::time::sleep(Duration::from_millis(80)).await;
            Ok(ToolResultBlock::text("done"))
        }
    }

    fn config(timeout_ms: u64) -> ExecutionConfig {
        ExecutionConfig::builder()
            .timeout(Duration::from_millis(timeout_ms))
            .build()
    }

    async fn run(long_running: bool) -> Result<ToolResultBlock> {
        let toolkit = Toolkit::new();
        toolkit.register_tool(Arc::new(SleepyTool { long_running }));
        let call = ToolUseBlock::new("c1", "sleepy", JsonMap::new());
        ToolExecutor::new()
            .execute(
                &toolkit,
                &call,
                ToolExecutionContext::default(),
                Some(&config(20)),
            )
            .await
    }

    #[tokio::test]
    async fn long_running_tool_skips_the_execution_timeout() {
        let out = run(true)
            .await
            .expect("long-running tool must not be cut off");
        assert_eq!(out.text_content(), "done");
    }

    #[tokio::test]
    async fn regular_tool_is_still_cut_by_the_execution_timeout() {
        let err = run(false).await.expect_err("regular tool hits the timeout");
        match err {
            Error::Tool(msg) => {
                assert!(msg.contains("Tool execution timeout after"), "{msg}");
                assert!(msg.contains("sleepy"), "{msg}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    /// Times out on its first `sleeps` attempts, then answers immediately.
    struct FlakySlowTool {
        sleeps: std::sync::atomic::AtomicUsize,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl Tool for FlakySlowTool {
        fn name(&self) -> &str {
            "flaky_slow"
        }
        fn description(&self) -> &str {
            "times out once, then answers"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn call(
            &self,
            _args: JsonMap,
            _ctx: ToolExecutionContext,
        ) -> crate::error::Result<ToolResultBlock> {
            use std::sync::atomic::Ordering;
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.sleeps.fetch_sub(1, Ordering::SeqCst) > 0 {
                tokio::time::sleep(Duration::from_millis(80)).await;
            }
            Ok(ToolResultBlock::text("done"))
        }
    }

    /// Always fails with a business error (never transient).
    struct BrokenTool {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl Tool for BrokenTool {
        fn name(&self) -> &str {
            "broken"
        }
        fn description(&self) -> &str {
            "fails with a business error"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn call(
            &self,
            _args: JsonMap,
            _ctx: ToolExecutionContext,
        ) -> crate::error::Result<ToolResultBlock> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(Error::Tool("Tool not found: broken".to_string()))
        }
    }

    #[tokio::test]
    async fn a_tool_timeout_is_retried_until_the_attempt_budget_runs_out() {
        // The timeout used to escape the retry loop through `?`, so `max_attempts`
        // never applied to the only failure a hung tool produces.
        let tool = Arc::new(FlakySlowTool {
            sleeps: std::sync::atomic::AtomicUsize::new(1),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let toolkit = Toolkit::new();
        toolkit.register_tool(tool.clone());
        let call = ToolUseBlock::new("c1", "flaky_slow", JsonMap::new());
        let cfg = ExecutionConfig::builder()
            .timeout(Duration::from_millis(20))
            .max_attempts(2)
            .initial_backoff(Duration::from_millis(1))
            .build();
        let out = ToolExecutor::new()
            .execute(&toolkit, &call, ToolExecutionContext::default(), Some(&cfg))
            .await
            .expect("the second attempt must run and answer");
        assert_eq!(out.text_content(), "done");
        assert_eq!(tool.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_business_tool_error_is_not_retried() {
        // Retrying it would repeat the failure (and, for a tool that already ran,
        // its side effects).
        let tool = Arc::new(BrokenTool {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let toolkit = Toolkit::new();
        toolkit.register_tool(tool.clone());
        let call = ToolUseBlock::new("c1", "broken", JsonMap::new());
        let cfg = ExecutionConfig::builder()
            .timeout(Duration::from_millis(200))
            .max_attempts(3)
            .initial_backoff(Duration::from_millis(1))
            .build();
        let err = ToolExecutor::new()
            .execute(&toolkit, &call, ToolExecutionContext::default(), Some(&cfg))
            .await
            .expect_err("a business failure is surfaced immediately");
        assert!(matches!(err, Error::Tool(ref msg) if msg.contains("Tool not found")));
        assert_eq!(tool.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// 一个声明了必填参数的 strict 工具：strict 模式下未声明的入参必须被拒绝。
    fn strict_tool() -> Arc<dyn Tool> {
        Arc::new(
            FunctionTool::new(
                "strict",
                "strict tool",
                serde_json::json!({
                    "type": "object",
                    "properties": { "a": { "type": "string" } },
                    "required": ["a"]
                }),
                Box::new(|_, _| Ok(ToolResultBlock::text("ok"))),
            )
            .strict(true),
        )
    }

    async fn call_strict(input: serde_json::Value) -> Result<ToolResultBlock> {
        let toolkit = Toolkit::new();
        toolkit.register_tool(strict_tool());
        let call = ToolUseBlock::new(
            "c1",
            "strict",
            input.as_object().cloned().unwrap_or_default(),
        );
        ToolExecutor::new()
            .execute(&toolkit, &call, ToolExecutionContext::default(), None)
            .await
    }

    #[tokio::test]
    async fn strict_tool_rejects_arguments_not_declared_in_the_schema() {
        let err = call_strict(serde_json::json!({"a": "x", "extra": 1}))
            .await
            .expect_err("strict 工具必须拒绝 schema 未声明的入参");
        match err {
            Error::Tool(msg) => {
                assert!(msg.contains("Parameter validation failed"), "{msg}");
                assert!(msg.contains("extra"), "{msg}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_malformed_call_is_refused_before_schema_validation() {
        // A truncated call carries merged input beside the malformed marker — the merged args
        // can even look complete and schema-valid. Direct Toolkit paths must refuse it like
        // the agent loop does instead of executing it from that input.
        let toolkit = Toolkit::new();
        toolkit.register_tool(strict_tool());
        let mut call = ToolUseBlock::new(
            "c-malformed",
            "strict",
            serde_json::json!({"a": "looks-complete"})
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
        call.metadata.insert(
            ToolUseBlock::METADATA_MALFORMED_ARGUMENTS.to_string(),
            serde_json::Value::Bool(true),
        );

        let err = ToolExecutor::new()
            .execute(&toolkit, &call, ToolExecutionContext::default(), None)
            .await
            .expect_err("a malformed call must not execute");
        match err {
            Error::Tool(msg) => assert!(msg.contains("malformed"), "{msg}"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn strict_tool_still_requires_the_declared_arguments() {
        let err = call_strict(serde_json::json!({}))
            .await
            .expect_err("strict 工具必须拒绝缺少必填参数的入参");
        match err {
            Error::Tool(msg) => {
                assert!(msg.contains("Parameter validation failed"), "{msg}");
                assert!(msg.contains("required"), "{msg}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    fn granted_tools(names: &[&str]) -> crate::agent::CapabilitySurface {
        crate::agent::CapabilitySurface::new(
            None,
            None,
            Some(names.iter().map(|n| n.to_string()).collect()),
        )
    }

    fn echo_tool(name: &'static str) -> Arc<dyn Tool> {
        Arc::new(FunctionTool::new(
            name,
            "test tool",
            serde_json::json!({"type": "object", "properties": {}}),
            Box::new(|_, _| Ok(ToolResultBlock::text("ok"))),
        ))
    }

    #[tokio::test]
    async fn an_ungranted_tool_is_refused_with_the_not_found_wording() {
        let toolkit = Toolkit::new();
        toolkit.register_tool(echo_tool("web_read"));
        toolkit.register_tool(echo_tool("execute"));

        let ctx =
            ToolExecutionContext::default().with_capability_surface(granted_tools(&["web_read"]));
        // The granted tool still runs.
        let ok = ToolExecutor::new()
            .execute(
                &toolkit,
                &ToolUseBlock::new("c1", "web_read", JsonMap::new()),
                ctx.clone(),
                None,
            )
            .await
            .expect("a granted tool must execute");
        let text = ok
            .output
            .iter()
            .filter_map(|b| match b {
                crate::message::ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(text, "ok");

        // The ungranted one reports exactly what a missing tool reports.
        let err = ToolExecutor::new()
            .execute(
                &toolkit,
                &ToolUseBlock::new("c2", "execute", JsonMap::new()),
                ctx,
                None,
            )
            .await
            .expect_err("an ungranted tool must not execute");
        assert_eq!(err.to_string(), "Tool error: Tool not found: execute");
    }

    #[tokio::test]
    async fn a_group_grant_admits_its_members_and_the_system_flag_bypasses() {
        let toolkit = Toolkit::new();
        toolkit.register_tool(echo_tool("web_search"));
        toolkit.create_tool_group("search", "search", true);
        toolkit.add_tool_to_group("search", "web_search");
        toolkit.register_meta_tools();

        let surface = crate::agent::CapabilitySurface::new(
            None,
            Some(["search".to_string()].into_iter().collect()),
            None,
        );
        let ctx = ToolExecutionContext::default().with_capability_surface(surface);
        let call_for = |name: &str| {
            let mut input = JsonMap::new();
            if name == "reset_equipped_tools" {
                input.insert("to_activate".to_string(), serde_json::json!(["search"]));
            }
            ToolUseBlock::new("c", name, input)
        };
        for name in ["web_search", "reset_equipped_tools"] {
            ToolExecutor::new()
                .execute(&toolkit, &call_for(name), ctx.clone(), None)
                .await
                .unwrap_or_else(|e| panic!("{name} must be admitted: {e}"));
        }
    }

    #[tokio::test]
    async fn a_missing_surface_leaves_direct_execution_unrestricted() {
        // A call that never entered an agent funnel carries no surface: trusted assembler code.
        let toolkit = Toolkit::new();
        toolkit.register_tool(echo_tool("execute"));
        ToolExecutor::new()
            .execute(
                &toolkit,
                &ToolUseBlock::new("c", "execute", JsonMap::new()),
                ToolExecutionContext::default(),
                None,
            )
            .await
            .expect("no surface = unrestricted");
    }
}
