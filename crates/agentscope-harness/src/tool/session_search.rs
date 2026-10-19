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

/// Hard ceiling for model-supplied result limits. Both session tools are excluded from
/// tool-result eviction on the "bounded results" rationale, so nothing downstream trims an
/// oversized result — the clamp has to happen here.
const MAX_RESULTS_CEILING: usize = 100;

/// Clamps a model-supplied limit: non-positive values fall back to `fallback`, otherwise the
/// value is capped at [`MAX_RESULTS_CEILING`].
fn capped_limit(value: Option<usize>, fallback: usize) -> usize {
    match value.filter(|&n| n > 0) {
        Some(n) => n.min(MAX_RESULTS_CEILING),
        None => fallback,
    }
}

/// How a query's terms are combined against a single record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchMode {
    /// Exact substring of the whole query (default).
    Phrase,
    /// Every whitespace-separated keyword must appear in the same record.
    All,
    /// At least one keyword must appear in the record.
    Any,
}

impl MatchMode {
    /// Parses the model-supplied `matchMode`; an absent value means `phrase`.
    fn parse(raw: Option<&str>) -> std::result::Result<Self, String> {
        match raw.unwrap_or("phrase") {
            "phrase" => Ok(MatchMode::Phrase),
            "all" => Ok(MatchMode::All),
            "any" => Ok(MatchMode::Any),
            _ => Err("matchMode must be one of: phrase, all, any".to_string()),
        }
    }
}

/// A case-insensitive literal matcher over one record's text.
///
/// Mirrors Java `KeywordMatcher`: `phrase` keeps the query's own whitespace verbatim, while
/// `all`/`any` split it on Unicode whitespace, drop empties and dedupe the terms. No regex, no
/// automatic CJK segmentation — every term is a literal substring.
#[derive(Debug, Clone, PartialEq, Eq)]
enum KeywordMatcher {
    Phrase(String),
    Terms { terms: Vec<String>, require_all: bool },
}

impl KeywordMatcher {
    /// Compiles `query` under `mode` against lowercased text.
    fn compile(query: &str, mode: MatchMode) -> std::result::Result<Self, String> {
        match mode {
            MatchMode::Phrase => Ok(KeywordMatcher::Phrase(query.to_lowercase())),
            MatchMode::All | MatchMode::Any => {
                let mut terms: Vec<String> = Vec::new();
                for term in query.split_whitespace() {
                    let term = term.to_lowercase();
                    if !terms.contains(&term) {
                        terms.push(term);
                    }
                }
                // Do not let an empty ALL query match every record.
                if terms.is_empty() {
                    return Err("query must contain at least one keyword".to_string());
                }
                Ok(KeywordMatcher::Terms { terms, require_all: mode == MatchMode::All })
            }
        }
    }

    /// Tests `lower_text` (already lowercased) against this matcher.
    fn matches(&self, lower_text: &str) -> bool {
        match self {
            KeywordMatcher::Phrase(phrase) => lower_text.contains(phrase.as_str()),
            KeywordMatcher::Terms { terms, require_all } => {
                if *require_all {
                    terms.iter().all(|t| lower_text.contains(t.as_str()))
                } else {
                    terms.iter().any(|t| lower_text.contains(t.as_str()))
                }
            }
        }
    }
}

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
                "query": {"type": "string", "description": "Literal phrase, or whitespace-separated keywords when matchMode is all/any; no automatic Chinese word segmentation"},
                "agentId": {"type": "string", "description": "Agent ID to search sessions for"},
                "maxResults": {"type": "integer", "description": "Maximum results (default 10, capped at 100)"},
                "matchMode": {"type": "string", "description": "phrase (default): exact substring; all: every keyword in the same session entry; any: at least one keyword in that entry. Case-insensitive literal matching."}
            },
            "required": ["query"]
        })
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let Some(query) = opt_str_arg(&args, "query").filter(|q| !q.trim().is_empty()) else {
            return Ok(ToolResultBlock::error("Error: query is required"));
        };
        let limit = capped_limit(int_arg(&args, "maxResults"), DEFAULT_SEARCH_LIMIT);
        let agent_id = opt_str_arg(&args, "agentId").filter(|a| !a.trim().is_empty());
        let mode = match MatchMode::parse(opt_str_arg(&args, "matchMode").as_deref()) {
            Ok(m) => m,
            Err(e) => return Ok(ToolResultBlock::error(e)),
        };
        let matcher = match KeywordMatcher::compile(&query, mode) {
            Ok(m) => m,
            Err(e) => return Ok(ToolResultBlock::error(e)),
        };

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
                if matcher.matches(&text.to_lowercase()) {
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
                "lastN": {"type": "integer", "description": "Number of recent messages to return (default 20, capped at 100)"}
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
        let limit = capped_limit(int_arg(&args, "lastN"), DEFAULT_HISTORY_LIMIT);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_ws(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("asr_sess_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// Writes a session context file (`<session>.jsonl`) plus the `.log.jsonl` marker the
    /// searcher lists sessions by.
    fn write_session(
        root: &std::path::Path,
        agent: &str,
        session: &str,
        entries: &[(&str, String)],
    ) {
        let dir = root.join("agents").join(agent).join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{session}.log.jsonl")), "").unwrap();
        let mut jsonl = String::new();
        for (i, (role, content)) in entries.iter().enumerate() {
            let v = serde_json::json!({
                "type": "message",
                "id": format!("m{i}"),
                "timestamp": "2026-01-01T00:00:00Z",
                "role": role,
                "content": content,
            });
            jsonl.push_str(&v.to_string());
            jsonl.push('\n');
        }
        std::fs::write(dir.join(format!("{session}.jsonl")), jsonl).unwrap();
    }

    fn args(v: serde_json::Value) -> JsonMap {
        v.as_object().unwrap().clone()
    }

    /// Counts needle occurrences the way the Java ceiling test does.
    fn count(text: &str, needle: &str) -> usize {
        (text.len() - text.replace(needle, "").len()) / needle.len()
    }

    async fn search_out(
        tool: &SessionSearchTool,
        query: &str,
        max: Option<usize>,
        mode: Option<&str>,
    ) -> String {
        let mut a = serde_json::json!({"query": query});
        if let Some(m) = max {
            a["maxResults"] = serde_json::json!(m);
        }
        if let Some(m) = mode {
            a["matchMode"] = serde_json::json!(m);
        }
        tool.call(args(a), ToolExecutionContext::default())
            .await
            .unwrap()
            .text_content()
    }

    #[tokio::test]
    async fn session_search_clamps_model_supplied_max_results() {
        let root = temp_ws("search_clamp");
        let entries: Vec<(&str, String)> =
            (1..=150).map(|i| ("user", format!("needle item {i}"))).collect();
        write_session(&root, "agent-a", "s1", &entries);
        let tool = SessionSearchTool::new(Arc::new(WorkspaceManager::new(root.clone())));

        let out = search_out(&tool, "needle", Some(100_000), None).await;
        assert!(out.starts_with("Found 100 matches"), "{out}");
        assert_eq!(count(&out, "needle item"), 100);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn session_history_clamps_model_supplied_last_n() {
        let root = temp_ws("history_clamp");
        let entries: Vec<(&str, String)> =
            (1..=150).map(|i| ("user", format!("needle item {i}"))).collect();
        write_session(&root, "agent-a", "s1", &entries);
        let tool = SessionHistoryTool::new(Arc::new(WorkspaceManager::new(root.clone())));

        let out = tool
            .call(
                args(serde_json::json!({"agentId": "agent-a", "sessionId": "s1", "lastN": 100_000})),
                ToolExecutionContext::default(),
            )
            .await
            .unwrap()
            .text_content();
        assert!(out.contains("showing last 100"), "{out}");
        assert_eq!(count(&out, "needle item"), 100);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn session_search_default_limit_still_applies() {
        let root = temp_ws("search_default");
        let entries: Vec<(&str, String)> =
            (1..=15).map(|i| ("user", format!("needle item {i}"))).collect();
        write_session(&root, "agent-a", "s1", &entries);
        let tool = SessionSearchTool::new(Arc::new(WorkspaceManager::new(root.clone())));

        let out = search_out(&tool, "needle", None, None).await;
        assert!(out.starts_with("Found 10 matches"), "{out}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn session_search_match_mode_all_and_any() {
        let root = temp_ws("search_mode");
        write_session(
            &root,
            "agent-a",
            "s1",
            &[
                ("user", "only-first".to_string()),
                ("assistant", "only-second".to_string()),
                ("user", "部署决定：使用蓝鲸方案".to_string()),
            ],
        );
        let tool = SessionSearchTool::new(Arc::new(WorkspaceManager::new(root.clone())));

        // `all` requires every keyword in the same entry: they live in different entries.
        let all = search_out(&tool, "only-first only-second", None, Some("all")).await;
        assert!(all.starts_with("No matches"), "{all}");
        // `any` matches each entry separately.
        let any = search_out(&tool, "only-first only-second", None, Some("any")).await;
        assert!(any.starts_with("Found 2 matches"), "{any}");
        // Default phrase keeps the query verbatim, so the space-joined query matches nothing.
        let phrase = search_out(&tool, "only-first only-second", None, None).await;
        assert!(phrase.starts_with("No matches"), "{phrase}");
        // Single keyword still matches in `all`.
        let single = search_out(&tool, "蓝鲸", None, Some("all")).await;
        assert!(single.starts_with("Found 1 matches"), "{single}");
        // Duplicates and unicode whitespace collapse to one distinct term.
        let dup = search_out(&tool, " \t蓝鲸\n蓝鲸\u{3000}", None, Some("all")).await;
        assert!(dup.starts_with("Found 1 matches"), "{dup}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn session_search_rejects_invalid_match_mode() {
        let root = temp_ws("search_badmode");
        let tool = SessionSearchTool::new(Arc::new(WorkspaceManager::new(root.clone())));

        for bad in ["", "ALL", "unknown"] {
            let out = search_out(&tool, "蓝鲸", None, Some(bad)).await;
            assert!(out.starts_with("Error: matchMode"), "mode {bad:?}: {out}");
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn keyword_matcher_rejects_empty_all_query() {
        assert!(KeywordMatcher::compile("   \t\n", MatchMode::All).is_err());
        assert!(KeywordMatcher::compile("", MatchMode::Any).is_err());
        // Phrase mode accepts an empty query (caller guards on blank query first).
        assert!(KeywordMatcher::compile("  ", MatchMode::Phrase).is_ok());
    }

    #[test]
    fn capped_limit_bounds_and_falls_back() {
        assert_eq!(capped_limit(None, 10), 10);
        assert_eq!(capped_limit(Some(0), 10), 10);
        assert_eq!(capped_limit(Some(5), 10), 5);
        assert_eq!(capped_limit(Some(100), 10), 100);
        assert_eq!(capped_limit(Some(100_000), 10), 100);
    }
}
