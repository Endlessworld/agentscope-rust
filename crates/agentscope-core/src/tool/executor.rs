//! Tool execution pipeline: lookup, authorization, validation, timeout, retry.
//! Mirrors `io.agentscope.core.tool.ToolExecutor`.
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::message::{JsonMap, ToolResultBlock, ToolUseBlock};
use crate::model::ExecutionConfig;

use super::tool::{Tool, ToolExecutionContext};
use super::toolkit::Toolkit;
use super::validator::ToolValidator;

/// Executes tool calls through the full governance pipeline.
#[derive(Clone, Default)]
pub struct ToolExecutor;

impl ToolExecutor {
    pub fn new() -> Self { Self }

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
        let tool = toolkit.get_tool_with(&call.name, view).ok_or_else(|| {
            Error::Tool(format!("Tool not found: {}", call.name))
        })?;
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
        // 6. Timeout + retry around the raw call.
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
    let cfg = config.cloned().unwrap_or_else(ExecutionConfig::tool_defaults);
    let timeout = cfg.timeout;
    let max_attempts = cfg.max_attempts.unwrap_or(1);
    let mut attempt: u32 = 0;
    loop {
        let fut = tool.call(input.clone(), ctx.clone());
        let result = match timeout {
            Some(d) => tokio::time::timeout(d, fut).await.map_err(|_| {
                Error::Tool(format!("Tool execution timeout after {:?} for '{}'", d, call.name))
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
