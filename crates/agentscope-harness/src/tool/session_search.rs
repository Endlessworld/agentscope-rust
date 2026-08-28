//! `SessionSearchTool` — searches past session transcripts and views session
//! history, mirroring `io.agentscope.harness.agent.tool.SessionSearchTool`.
//!
//! Operates exclusively on the local session cache (`agents/<agentId>/sessions/`);
//! remote synchronisation happens at write time, keeping these tools lightweight.
use std::sync::Arc;

use async_trait::async_trait;
use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext};

use crate::agent::memory::session::{SessionEntry, SessionTree};
use crate::agent::workspace::manager::WorkspaceManager;
use crate::tool::{int_arg, opt_str_arg};

const DEFAULT_SEARCH_LIMIT: usize = 10;
const DEFAULT_HISTORY_LIMIT: usize = 20;

/// Extracts lowercase-searchable text from a session entry.
fn searchable_text(entry: &SessionEntry) -> Option<String> {
    match entry {
        SessionEntry::Message { content, .. } => Some(content.clone()),
        SessionEntry::Summary { summary, .. } => Some(summary.clone()),
        _ => None,
    }
}

/// Human-readable role label for an entry.
fn role_label(entry: &SessionEntry) -> String {
    match entry {
        SessionEntry::Message { role, .. } => role.clone(),
        SessionEntry::Compaction { .. } => "COMPACTION".into(),
        SessionEntry::Summary { .. } => "SUMMARY".into(),
    }
}

/// Loads a session tree from a JSONL file on local disk.
fn load_tree(path: &std::path::Path) -> Option<SessionTree> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut tree = SessionTree::new();
    tree.load_jsonl(&content);
    Some(tree)
}

/// Searches past session transcripts for a keyword or phrase.
pub struct SessionSearchTool {
    workspace: Arc<WorkspaceManager>,
}

impl SessionSearchTool {
    pub fn new(workspace: Arc<WorkspaceManager>) -> Self {
        Self { workspace }
    }

    /// Registers all three session tools into a toolkit.
    pub fn register_all(toolkit: &agentscope_core::tool::Toolkit, workspace: Arc<WorkspaceManager>) {
        toolkit.register_tool(Arc::new(SessionSearchTool::new(workspace.clone())));
        toolkit.register_tool(Arc::new(SessionListTool::new(workspace.clone())));
        toolkit.register_tool(Arc::new(SessionHistoryTool::new(workspace)));
    }
}

#[async_trait]
impl Tool for SessionSearchTool {
    fn name(&self) -> &str {
        "session_search"
    }

    fn description(&self) -> &str {
        "Search past session transcripts for a keyword or phrase. Returns matching \
         entries with session context."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search query (keyword or phrase)"},
                "agentId": {"type": "string", "description": "Agent ID to search sessions for"},
                "maxResults": {"type": "integer", "description": "Maximum results (default 10)"}
            },
            "required": ["query"]
        })
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let Some(query) = opt_str_arg(&args, "query").filter(|q| !q.trim().is_empty()) else {
            return Ok(ToolResultBlock::error("Error: query is required"));
        };
        let limit = int_arg(&args, "maxResults").filter(|&n| n > 0).unwrap_or(DEFAULT_SEARCH_LIMIT);
        let agent_id = opt_str_arg(&args, "agentId").filter(|a| !a.trim().is_empty());
        let lower_query = query.to_lowercase();

        let rc = RuntimeContext::default();
        let mut results: Vec<String> = Vec::new();

        for rel_log in self.workspace.list_session_log_files(&rc).await {
            if results.len() >= limit {
                break;
            }
            // Filter by agent id when provided (`agents/<agentId>/sessions/...`).
            if let Some(agent) = &agent_id {
                if !rel_log.starts_with(&format!("agents/{agent}/")) {
                    continue;
                }
            }
            let log_path = self.workspace.workspace().join(&rel_log);
            let ctx_path = log_path.with_file_name(
                log_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .replace(".log.jsonl", ".jsonl"),
            );
            let Some(tree) = load_tree(&ctx_path) else {
                continue;
            };
            for entry in tree.get_all_entries() {
                if results.len() >= limit {
                    break;
                }
                let Some(text) = searchable_text(entry) else {
                    continue;
                };
                if text.to_lowercase().contains(&lower_query) {
                    let mut preview: String = text.chars().take(200).collect();
                    if text.chars().count() > 200 {
                        preview.push_str("...");
                    }
                    results.push(format!(
                        "  [{rel_log}] {} \u{2014} [{}]: {}",
                        entry.id(),
                        role_label(entry),
                        preview
                    ));
                }
            }
        }

        if results.is_empty() {
            return Ok(ToolResultBlock::text(format!("No matches found for: {query}")));
        }
        let mut sb = format!("Found {} matches for \"{query}\":\n\n", results.len());
        for r in &results {
            sb.push_str(r);
            sb.push('\n');
        }
        Ok(ToolResultBlock::text(sb))
    }
}

/// Lists available sessions for an agent.
pub struct SessionListTool {
    workspace: Arc<WorkspaceManager>,
}

impl SessionListTool {
    pub fn new(workspace: Arc<WorkspaceManager>) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for SessionListTool {
    fn name(&self) -> &str {
        "session_list"
    }

    fn description(&self) -> &str {
        "List available sessions for an agent, showing session IDs and metadata."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "agentId": {"type": "string", "description": "Agent ID to list sessions for"}
            },
            "required": ["agentId"]
        })
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let Some(agent_id) = opt_str_arg(&args, "agentId").filter(|a| !a.trim().is_empty()) else {
            return Ok(ToolResultBlock::error("Error: agentId is required"));
        };
        let rc = RuntimeContext::default();

        // Prefer the structured session-store index (remote then local).
        let store_rel = format!("agents/{agent_id}/sessions/sessions.json");
        let store_content = self.workspace.read_managed_workspace_file_utf8(&rc, &store_rel).await;
        if !store_content.trim().is_empty() {
            return Ok(ToolResultBlock::text(store_content));
        }

        // List sessions from local cache only.
        let session_dir = self.workspace.get_session_dir(&rc, &agent_id);
        let mut ids: Vec<String> = Vec::new();
        if session_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&session_dir) {
                for e in entries.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    if e.path().is_file() && name.ends_with(".jsonl") && !name.ends_with(".log.jsonl")
                    {
                        ids.push(name.trim_end_matches(".jsonl").to_string());
                    }
                }
            }
        }
        if ids.is_empty() {
            return Ok(ToolResultBlock::text(format!(
                "No sessions found for agent: {agent_id}"
            )));
        }
        ids.sort();
        let mut sb = format!("Sessions for agent {agent_id}:\n");
        for id in ids {
            sb.push_str(&format!("  - {id}\n"));
        }
        Ok(ToolResultBlock::text(sb))
    }
}

/// Gets the conversation history for a specific session.
pub struct SessionHistoryTool {
    workspace: Arc<WorkspaceManager>,
}

impl SessionHistoryTool {
    pub fn new(workspace: Arc<WorkspaceManager>) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for SessionHistoryTool {
    fn name(&self) -> &str {
        "session_history"
    }

    fn description(&self) -> &str {
        "Get the conversation history for a specific session. Returns the messages \
         in the session."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "agentId": {"type": "string", "description": "Agent ID"},
                "sessionId": {"type": "string", "description": "Session ID"},
                "lastN": {"type": "integer", "description": "Number of recent messages to return (default 20)"}
            },
            "required": ["agentId", "sessionId"]
        })
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let Some(agent_id) = opt_str_arg(&args, "agentId").filter(|a| !a.trim().is_empty()) else {
            return Ok(ToolResultBlock::error("Error: agentId is required"));
        };
        let Some(session_id) = opt_str_arg(&args, "sessionId").filter(|s| !s.trim().is_empty())
        else {
            return Ok(ToolResultBlock::error("Error: sessionId is required"));
        };
        let limit = int_arg(&args, "lastN").filter(|&n| n > 0).unwrap_or(DEFAULT_HISTORY_LIMIT);
        let rc = RuntimeContext::default();

        let context_file = self
            .workspace
            .resolve_session_context_file(&rc, &agent_id, &session_id);
        let log_file = self
            .workspace
            .resolve_session_log_file(&rc, &agent_id, &session_id);

        let tree_path = if context_file.is_file() {
            context_file
        } else if log_file.is_file() {
            log_file
        } else {
            return Ok(ToolResultBlock::text(format!(
                "Session not found: {session_id}"
            )));
        };

        let Some(tree) = load_tree(&tree_path) else {
            return Ok(ToolResultBlock::text(format!(
                "Session not found: {session_id}"
            )));
        };

        let messages = tree.get_message_entries();
        let start = messages.len().saturating_sub(limit);
        let shown = messages.len() - start;

        let mut sb = format!(
            "Session {session_id} ({} total messages, showing last {shown}):\n\n",
            messages.len()
        );
        for msg in &messages[start..] {
            if let SessionEntry::Message { role, content, .. } = msg {
                let mut text = content.clone();
                if text.chars().count() > 500 {
                    text = text.chars().take(500).collect::<String>() + "... [truncated]";
                }
                sb.push_str(&format!("[{role}]: {text}\n"));
            }
        }
        Ok(ToolResultBlock::text(sb))
    }
}
