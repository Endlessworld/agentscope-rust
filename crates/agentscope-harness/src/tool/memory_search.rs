//! `MemorySearchTool` — searches long-term memory files, mirroring
//! `io.agentscope.harness.agent.tool.MemorySearchTool`.
//!
//! Uses case-insensitive literal substring search across every memory file visible
//! through the configured filesystem (`MEMORY.md` and `memory/*.md`).
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext};
use async_trait::async_trait;

use crate::agent::workspace::WorkspaceManager;

/// Default number of matches returned when the caller passes no `maxResults`.
const DEFAULT_MAX_RESULTS: usize = 30;

/// Hard cap on `maxResults`: a single call must never dump an unbounded slice of memory.
const MAX_RESULTS_LIMIT: usize = 200;

/// Characters of a matching line kept in the preview.
const MAX_LINE_PREVIEW_CHARS: usize = 200;

/// Reads `maxResults` from the arguments, applying the default and the hard cap.
fn max_results(args: &JsonMap) -> usize {
    match args.get("maxResults").and_then(|v| v.as_u64()) {
        Some(n) => (n as usize).clamp(1, MAX_RESULTS_LIMIT),
        None => DEFAULT_MAX_RESULTS,
    }
}

/// Truncates a matching line to [`MAX_LINE_PREVIEW_CHARS`] characters.
fn preview_line(line: &str) -> String {
    let line = line.trim_end_matches('\r');
    if line.chars().count() <= MAX_LINE_PREVIEW_CHARS {
        return line.to_string();
    }
    let mut out: String = line.chars().take(MAX_LINE_PREVIEW_CHARS).collect();
    out.push_str("...");
    out
}

/// Searches the agent's long-term memory files (`MEMORY.md` + `memory/*.md`).
pub struct MemorySearchTool {
    workspace: Arc<WorkspaceManager>,
}

impl MemorySearchTool {
    pub fn new(workspace: Arc<WorkspaceManager>) -> Self {
        Self { workspace }
    }

    /// Registers the tool into a toolkit.
    pub fn register(toolkit: &agentscope_core::tool::Toolkit, workspace: Arc<WorkspaceManager>) {
        toolkit.register_tool(Arc::new(Self::new(workspace)));
    }
}

#[async_trait]
impl Tool for MemorySearchTool {
    fn name(&self) -> &str {
        "memory_search"
    }

    fn description(&self) -> &str {
        "Search through long-term memory files (MEMORY.md and memory/*.md) for relevant \
         information. Use before answering questions about prior work, decisions, dates, \
         people, preferences, or todos."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Keywords to search for in memory files"},
                "maxResults": {
                    "type": "integer",
                    "description": "Maximum number of matching lines to return (default 30, at most 200)",
                    "default": 30
                }
            },
            "required": ["query"]
        })
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let query = match args.get("query").and_then(|v| v.as_str()) {
            Some(q) if !q.trim().is_empty() => q.to_string(),
            _ => return Ok(ToolResultBlock::text("No query provided")),
        };
        let max_results = max_results(&args);
        let rc = RuntimeContext::default();

        // Case-insensitive literal substring match (Java `Pattern.quote` + CASE_INSENSITIVE).
        let needle = query.to_lowercase();
        let mut results: Vec<String> = Vec::new();
        let mut match_count = 0usize;

        for rel in self.workspace.list_memory_file_paths(&rc).await {
            let content = self
                .workspace
                .read_managed_workspace_file_utf8(&rc, &rel)
                .await;
            if content.is_empty() {
                continue;
            }
            for (i, line) in content.split('\n').enumerate() {
                if line.to_lowercase().contains(&needle) {
                    match_count += 1;
                    // Bounded output: every match is counted, but only the first
                    // `maxResults` are materialised, each with a truncated preview.
                    if results.len() < max_results {
                        results.push(format!("Source: {rel}#{}: {}", i + 1, preview_line(line)));
                    }
                }
            }
        }

        if match_count == 0 {
            return Ok(ToolResultBlock::text(format!(
                "No matching memories found for: {query}"
            )));
        }
        let mut text = format!("Found {match_count} matches:\n\n{}", results.join("\n"));
        if match_count > results.len() {
            text.push_str(&format!(
                "\n\n(Showing the first {} matches; narrow the query or raise maxResults \
                 (at most {MAX_RESULTS_LIMIT}) to see the rest.)",
                results.len()
            ));
        }
        Ok(ToolResultBlock::text(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(value: serde_json::Value) -> JsonMap {
        value.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn max_results_defaults_and_clamps_to_the_hard_cap() {
        assert_eq!(max_results(&JsonMap::new()), DEFAULT_MAX_RESULTS);
        assert_eq!(max_results(&args(serde_json::json!({"maxResults": 5}))), 5);
        assert_eq!(max_results(&args(serde_json::json!({"maxResults": 0}))), 1);
        assert_eq!(
            max_results(&args(serde_json::json!({"maxResults": 10_000}))),
            MAX_RESULTS_LIMIT
        );
        assert_eq!(
            max_results(&args(serde_json::json!({"maxResults": "many"}))),
            DEFAULT_MAX_RESULTS
        );
    }

    #[test]
    fn previews_truncate_at_two_hundred_chars() {
        assert_eq!(preview_line("- short"), "- short");
        let exact = "y".repeat(MAX_LINE_PREVIEW_CHARS);
        assert_eq!(preview_line(&exact), exact);
        let long = "é".repeat(MAX_LINE_PREVIEW_CHARS + 50);
        let preview = preview_line(&long);
        assert_eq!(preview.chars().count(), MAX_LINE_PREVIEW_CHARS + 3);
        assert!(preview.ends_with("..."));
    }
}
