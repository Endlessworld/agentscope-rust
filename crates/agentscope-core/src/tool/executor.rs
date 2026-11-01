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
        // 3. Group activation check. Only backend tools are gated: a tool injected through
        //    the request config is ungrouped and always callable, so a same-named but
        //    inactive backend tool must not reject the call the model was shown.
        let from_request_config = view.map(|c| c.overrides(&call.name)).unwrap_or(false);
        if !from_request_config && !toolkit.is_tool_active(&call.name) {
            return Err(Error::Tool(format!(
                "Unauthorized tool call: {} is not available",
                call.name
            )));
        }
        // 4. Schema validation.
        let params = tool.parameters_schema();
        let schema = params.as_object().cloned().unwrap_or_default();
        let input_val = serde_json::Value::Object(call.input.clone());
        if let Some(err) = ToolValidator::validate_input(&schema, &input_val) {
            return Err(Error::Tool(format!("Parameter validation failed: {}", err)));
        }
        // 5. Preset parameters override (preset wins over LLM input).
        let input = toolkit.merge_presets(&call.name, call.input.clone());
        // 6. Per-call interception (Java `ToolCallInterceptor`): after schema validation
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
        // 7. Timeout + retry around the raw call.
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
        let result = match timeout {
            Some(d) => tokio::time::timeout(d, fut).await.map_err(|_| {
                Error::Tool(format!(
                    "Tool execution timeout after {:?} for '{}'",
                    d, call.name
                ))
            })?,
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
                if attempt >= max_attempts || !e.is_retryable() {
                    return Err(e);
                }
                tokio::time::sleep(cfg.backoff_for(attempt - 1)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{JsonMap, ToolResultBlock, ToolUseBlock};
    use crate::tool::{Tool, ToolExecutionContext, Toolkit};
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
        let out = run(true).await.expect("long-running tool must not be cut off");
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
}
