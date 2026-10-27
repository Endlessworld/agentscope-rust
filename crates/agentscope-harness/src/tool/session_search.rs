//! Session transcript tools, mirroring `io.agentscope.harness.agent.tool.SessionSearchTool`.
//!
//! All three tools read the **native session journal** through [`SessionLogStore`], exactly
//! like Java after the session redesign: discovery scans journal headers and history is a
//! read-time projection of committed events. There is no `sessions.json` index and no
//! `.log.jsonl` mirror — those belonged to the retired design.
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;
use agentscope_core::message::{JsonMap, Msg, ToolResultBlock};
use agentscope_core::session::{SessionKey, SessionLogStore, Transcript};
use agentscope_core::tool::{Tool, ToolExecutionContext};
use async_trait::async_trait;

use crate::tool::{int_arg, opt_str_arg};

const DEFAULT_SEARCH_LIMIT: usize = 10;
const DEFAULT_HISTORY_LIMIT: usize = 20;

/// Hard ceiling for model-supplied result limits. Both session tools are excluded from
/// tool-result eviction on the bounded-results rationale, so nothing downstream trims an
/// oversized result — the clamp has to happen here.
const MAX_RESULTS_CEILING: usize = 100;

/// Characters kept from a matched message before the preview is elided.
const PREVIEW_CHARS: usize = 200;

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
    Terms {
        terms: Vec<String>,
        require_all: bool,
    },
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
                Ok(KeywordMatcher::Terms {
                    terms,
                    require_all: mode == MatchMode::All,
                })
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

/// Case-insensitive searchable text of one committed message.
fn searchable_text(message: &Msg) -> Option<String> {
    let text = message.text_content();
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Human-readable role label for a message.
fn role_label(message: &Msg) -> String {
    format!("{:?}", message.role).to_uppercase()
}

/// Truncates `text` to a single-line preview.
fn preview(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat.chars().take(PREVIEW_CHARS).collect();
    if flat.chars().count() > PREVIEW_CHARS {
        out.push_str("...");
    }
    out
}

/// Opens the journal for `key` and rebuilds its committed transcript.
async fn read_transcript(
    store: &dyn SessionLogStore,
    key: &SessionKey,
    context: &RuntimeContext,
) -> Result<Transcript> {
    let log = store.open(key, context).await?;
    agentscope_core::session::transcript(&*log).await
}

/// Discovers the caller's sessions for `agent_id`, sorted by session id.
async fn sessions_of(
    store: &dyn SessionLogStore,
    context: &RuntimeContext,
    agent_id: &str,
) -> Result<Vec<SessionKey>> {
    let mut keys: Vec<SessionKey> = store
        .list(context)
        .await?
        .into_iter()
        .filter(|key| key.agent_id == agent_id)
        .collect();
    keys.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    Ok(keys)
}

/// Searches past session journals for a keyword or phrase.
pub struct SessionSearchTool {
    store: Arc<dyn SessionLogStore>,
    context: RuntimeContext,
}

impl SessionSearchTool {
    pub fn new(store: Arc<dyn SessionLogStore>) -> Self {
        Self {
            store,
            context: RuntimeContext::default(),
        }
    }

    /// Binds the caller identity used for journal discovery.
    pub fn with_context(mut self, context: RuntimeContext) -> Self {
        self.context = context;
        self
    }

    /// Registers all three session tools into a toolkit.
    pub fn register_all(toolkit: &agentscope_core::tool::Toolkit, store: Arc<dyn SessionLogStore>) {
        toolkit.register_tool(Arc::new(SessionSearchTool::new(Arc::clone(&store))));
        toolkit.register_tool(Arc::new(SessionListTool::new(Arc::clone(&store))));
        toolkit.register_tool(Arc::new(SessionHistoryTool::new(store)));
    }
}

#[async_trait]
impl Tool for SessionSearchTool {
    fn name(&self) -> &str {
        "session_search"
    }

    fn description(&self) -> &str {
        "Search past session transcripts for a keyword or phrase. Returns matching entries with session context."
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

        // Without an explicit agent filter, every session the caller can see is searched.
        let keys = match &agent_id {
            Some(agent) => sessions_of(&*self.store, &self.context, agent).await?,
            None => {
                let mut keys = self.store.list(&self.context).await?;
                keys.sort_by(|a, b| {
                    a.agent_id
                        .cmp(&b.agent_id)
                        .then_with(|| a.session_id.cmp(&b.session_id))
                });
                keys
            }
        };

        let mut results: Vec<String> = Vec::new();
        'outer: for key in keys {
            // A journal that cannot be read must not abort the whole search; report the rest.
            let Ok(transcript) = read_transcript(&*self.store, &key, &self.context).await else {
                continue;
            };
            for message in &transcript.messages {
                if results.len() >= limit {
                    break 'outer;
                }
                let Some(text) = searchable_text(message) else {
                    continue;
                };
                if matcher.matches(&text.to_lowercase()) {
                    results.push(format!(
                        "  [{}] {} - [{}]: {}",
                        key.session_id,
                        message.id,
                        role_label(message),
                        preview(&text)
                    ));
                }
            }
        }

        if results.is_empty() {
            return Ok(ToolResultBlock::text(format!(
                "No matches found for: {query}"
            )));
        }
        let mut sb = format!("Found {} matches for '{query}':\n\n", results.len());
        for r in &results {
            sb.push_str(r);
            sb.push('\n');
        }
        Ok(ToolResultBlock::text(sb))
    }
}

/// Lists available sessions for an agent.
pub struct SessionListTool {
    store: Arc<dyn SessionLogStore>,
    context: RuntimeContext,
}

impl SessionListTool {
    pub fn new(store: Arc<dyn SessionLogStore>) -> Self {
        Self {
            store,
            context: RuntimeContext::default(),
        }
    }

    /// Binds the caller identity used for journal discovery.
    pub fn with_context(mut self, context: RuntimeContext) -> Self {
        self.context = context;
        self
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
        let keys = sessions_of(&*self.store, &self.context, &agent_id).await?;
        if keys.is_empty() {
            return Ok(ToolResultBlock::text(format!(
                "No sessions found for agent: {agent_id}"
            )));
        }
        let mut sb = format!("Sessions for agent {agent_id}:\n");
        for key in keys {
            // The head reports how much committed history exists without reading it.
            let head = self.store.open(&key, &self.context).await?.head().await?;
            sb.push_str(&format!(
                "  - {} (events: {}, lease_until: {})\n",
                key.session_id, head.seq, head.lease_until
            ));
        }
        Ok(ToolResultBlock::text(sb))
    }
}

/// Gets the conversation history for a specific session.
pub struct SessionHistoryTool {
    store: Arc<dyn SessionLogStore>,
    context: RuntimeContext,
}

impl SessionHistoryTool {
    pub fn new(store: Arc<dyn SessionLogStore>) -> Self {
        Self {
            store,
            context: RuntimeContext::default(),
        }
    }

    /// Binds the caller identity used for journal discovery.
    pub fn with_context(mut self, context: RuntimeContext) -> Self {
        self.context = context;
        self
    }
}

#[async_trait]
impl Tool for SessionHistoryTool {
    fn name(&self) -> &str {
        "session_history"
    }

    fn description(&self) -> &str {
        "Get the conversation history for a specific session. Returns the messages in the session."
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

        let key = SessionKey::new(
            self.context.user_id().map(str::to_string),
            agent_id.clone(),
            session_id.clone(),
        )?;
        let transcript = read_transcript(&*self.store, &key, &self.context).await?;
        if transcript.messages.is_empty() {
            return Ok(ToolResultBlock::text(format!(
                "No history found for session: {session_id}"
            )));
        }

        let start = transcript.messages.len().saturating_sub(limit);
        let mut sb = format!(
            "History for agent {agent_id}, session {session_id} ({} of {} messages):\n",
            transcript.messages.len() - start,
            transcript.messages.len()
        );
        for message in &transcript.messages[start..] {
            sb.push_str(&format!(
                "  [{}] {}: {}\n",
                message.id,
                role_label(message),
                preview(&message.text_content())
            ));
        }
        Ok(ToolResultBlock::text(sb))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::session::{InMemorySessionLogStore, SessionRecorder};
    use agentscope_core::state::AgentState;

    fn ctx(user: Option<&str>) -> RuntimeContext {
        match user {
            Some(user) => RuntimeContext::new().with_user_id(user),
            None => RuntimeContext::new(),
        }
    }

    /// Commits a journal for `session` with `messages` as its persisted context.
    async fn seed(
        store: &InMemorySessionLogStore,
        user: Option<&str>,
        agent: &str,
        session: &str,
        messages: Vec<Msg>,
    ) {
        let context = ctx(user);
        let key = SessionKey::new(user.map(str::to_string), agent, session).unwrap();
        let log = store.open(&key, &context).await.unwrap();
        let recorder = SessionRecorder::open(log, None).await.unwrap();
        let mut state = AgentState::builder().session_id(session).build();
        if let Some(user) = user {
            state.user_id = Some(user.to_string());
        }
        for message in messages {
            state.context.push(message);
        }
        recorder.start(&[], &state).await.unwrap();
        recorder.finish(&state, "completed").await.unwrap();
    }

    fn args(pairs: &[(&str, serde_json::Value)]) -> JsonMap {
        let mut map = JsonMap::new();
        for (k, v) in pairs {
            map.insert((*k).to_string(), v.clone());
        }
        map
    }

    #[tokio::test]
    async fn search_reads_committed_journal_events() {
        let store = Arc::new(InMemorySessionLogStore::new());
        seed(
            &store,
            Some("alice"),
            "a1",
            "s1",
            vec![Msg::user("How do quantum computers work?")],
        )
        .await;
        let tool = SessionSearchTool::new(Arc::clone(&store) as Arc<dyn SessionLogStore>)
            .with_context(ctx(Some("alice")));
        let out = tool
            .call(
                args(&[("query", serde_json::json!("quantum"))]),
                Default::default(),
            )
            .await
            .unwrap();
        let text = out.text_content();
        assert!(text.contains("Found 1 matches"), "{text}");
        assert!(text.contains("s1"), "{text}");
    }

    #[tokio::test]
    async fn search_does_not_leak_other_users_sessions() {
        let store = Arc::new(InMemorySessionLogStore::new());
        seed(
            &store,
            Some("bob"),
            "a1",
            "b1",
            vec![Msg::user("quantum notes")],
        )
        .await;
        let tool = SessionSearchTool::new(Arc::clone(&store) as Arc<dyn SessionLogStore>)
            .with_context(ctx(Some("alice")));
        let out = tool
            .call(
                args(&[("query", serde_json::json!("quantum"))]),
                Default::default(),
            )
            .await
            .unwrap();
        assert!(out.text_content().contains("No matches found"));
    }

    #[tokio::test]
    async fn list_reports_journal_heads() {
        let store = Arc::new(InMemorySessionLogStore::new());
        seed(&store, Some("alice"), "a1", "s1", vec![Msg::user("hi")]).await;
        let tool = SessionListTool::new(Arc::clone(&store) as Arc<dyn SessionLogStore>)
            .with_context(ctx(Some("alice")));
        let out = tool
            .call(
                args(&[("agentId", serde_json::json!("a1"))]),
                Default::default(),
            )
            .await
            .unwrap();
        let text = out.text_content();
        assert!(text.contains("- s1"), "{text}");
        assert!(text.contains("events:"), "{text}");
    }

    #[tokio::test]
    async fn history_returns_the_last_n_messages() {
        let store = Arc::new(InMemorySessionLogStore::new());
        seed(
            &store,
            Some("alice"),
            "a1",
            "s1",
            vec![Msg::user("one"), Msg::assistant("two"), Msg::user("three")],
        )
        .await;
        let tool = SessionHistoryTool::new(Arc::clone(&store) as Arc<dyn SessionLogStore>)
            .with_context(ctx(Some("alice")));
        let out = tool
            .call(
                args(&[
                    ("agentId", serde_json::json!("a1")),
                    ("sessionId", serde_json::json!("s1")),
                    ("lastN", serde_json::json!(2)),
                ]),
                Default::default(),
            )
            .await
            .unwrap();
        let text = out.text_content();
        assert!(text.contains("2 of 3"), "{text}");
        assert!(text.contains("three"), "{text}");
        assert!(!text.contains("one"), "{text}");
    }

    #[tokio::test]
    async fn limits_are_capped_at_the_ceiling() {
        let store = Arc::new(InMemorySessionLogStore::new());
        seed(
            &store,
            Some("alice"),
            "a1",
            "s1",
            (0..150).map(|i| Msg::user(format!("needle {i}"))).collect(),
        )
        .await;
        let tool = SessionSearchTool::new(Arc::clone(&store) as Arc<dyn SessionLogStore>)
            .with_context(ctx(Some("alice")));
        let out = tool
            .call(
                args(&[
                    ("query", serde_json::json!("needle")),
                    ("maxResults", serde_json::json!(100_000)),
                ]),
                Default::default(),
            )
            .await
            .unwrap();
        assert!(out.text_content().contains("Found 100 matches"));
    }

    #[test]
    fn match_mode_defaults_to_phrase_and_rejects_unknown_values() {
        assert_eq!(MatchMode::parse(None).unwrap(), MatchMode::Phrase);
        assert!(MatchMode::parse(Some("bogus")).is_err());
        // An ALL query with no keywords must not match everything.
        assert!(KeywordMatcher::compile("   ", MatchMode::All).is_err());
    }
}
