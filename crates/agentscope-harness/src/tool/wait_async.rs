//! `WaitAsyncResultsTool` — blocks until background async tool results settle,
//! mirroring `io.agentscope.harness.agent.tool.WaitAsyncResultsTool` (task
//! barrier mode; the legacy inbox mode is handled by `InboxMiddleware`).
//!
//! Polls the [`AsyncToolRegistry`] for the session's records every
//! [`POLL_INTERVAL`] until all targeted tasks reach a terminal state
//! (COMPLETED / FAILED / TIMEOUT) or `timeout_ms` elapses. Note that result
//! payloads are delivered through the inbox hint channel; this tool reports the
//! barrier outcome so the agent can continue.
use std::sync::Arc;
use std::time::Duration;

use agentscope_core::error::Result;
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext};
use async_trait::async_trait;

use crate::agent::bus::async_tool::{AsyncToolRecord, AsyncToolRegistry};

/// How often the registry is polled for status changes.
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Default maximum wait before returning a partial summary.
pub const DEFAULT_TIMEOUT_MS: u64 = 60_000;

/// Barrier over async-tool registry records.
pub struct WaitAsyncResultsTool {
    registry: Arc<dyn AsyncToolRegistry>,
}

impl WaitAsyncResultsTool {
    pub fn new(registry: Arc<dyn AsyncToolRegistry>) -> Self {
        Self { registry }
    }

    /// Registers the tool into a toolkit.
    pub fn register(
        toolkit: &agentscope_core::tool::Toolkit,
        registry: Arc<dyn AsyncToolRegistry>,
    ) {
        toolkit.register_tool(Arc::new(Self::new(registry)));
    }
}

fn is_terminal(status: &str) -> bool {
    matches!(
        status,
        AsyncToolRecord::COMPLETED | AsyncToolRecord::FAILED | AsyncToolRecord::TIMEOUT
    )
}

#[async_trait]
impl Tool for WaitAsyncResultsTool {
    fn name(&self) -> &str {
        "wait_async_results"
    }

    /// The barrier may wait up to its own `timeout_ms` (default 60s, max 600s),
    /// so the shared tool timeout must not cut it short.
    fn is_long_running(&self) -> bool {
        true
    }

    fn description(&self) -> &str {
        "Block until background async tool executions finish (or time out). Pass task_ids \
         to wait for specific tasks, or omit them to wait for all pending tasks of the \
         current session."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_ids": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Specific async task ids to wait for; empty = all session tasks"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": format!("Maximum wait in milliseconds (default {DEFAULT_TIMEOUT_MS})")
                }
            }
        })
    }

    async fn call(&self, args: JsonMap, ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let task_ids: Vec<String> = args
            .get("task_ids")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_MS);
        // `sessionId` is the key `tool_runtime` injects; `session_id` is accepted as a
        // fallback for callers that put it in metadata directly.
        let session_id = ctx
            .runtime
            .get("sessionId")
            .or_else(|| ctx.runtime.get("session_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string();

        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let records = self.registry.list_session(&session_id).await?;
            let targets: Vec<&AsyncToolRecord> = if task_ids.is_empty() {
                records.iter().collect()
            } else {
                records
                    .iter()
                    .filter(|r| task_ids.contains(&r.id))
                    .collect()
            };

            let pending: Vec<String> = targets
                .iter()
                .filter(|r| !is_terminal(&r.status))
                .map(|r| r.id.clone())
                .collect();

            if targets.is_empty() {
                return Ok(ToolResultBlock::text(
                    "No async tasks found for this session.".to_string(),
                ));
            }
            if pending.is_empty() {
                let mut lines = vec!["All awaited async tasks settled:".to_string()];
                for r in &targets {
                    lines.push(format!("- {} [{}] ({})", r.id, r.tool_name, r.status));
                }
                return Ok(ToolResultBlock::text(lines.join("\n")));
            }
            if tokio::time::Instant::now() >= deadline {
                let mut lines = vec![format!(
                    "Timed out after {timeout_ms} ms waiting for async tasks. Still running:"
                )];
                for id in &pending {
                    lines.push(format!("- {id}"));
                }
                lines.push("Call wait_async_results again later for their results.".to_string());
                return Ok(ToolResultBlock::text(lines.join("\n")));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}
