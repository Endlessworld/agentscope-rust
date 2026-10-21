//! Memory flush management, mirroring `MemoryFlushManager`.
//!
//! Extracts long-term memories from a conversation window via an LLM and appends them to
//! today's daily memory ledger (`memory/YYYY-MM-DD.md`). This is the first layer of the
//! two-layer memory model; `MEMORY.md` is owned by [`crate::agent::memory::consolidator`]
//! and is treated here as read-only context.

use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;
use agentscope_core::message::{ContentBlock, Msg, MsgRole, TextBlock, ToolResultBlock, ToolUseBlock};
use agentscope_core::model::Model;

use crate::agent::context::auxiliary_text;
use crate::agent::memory::compaction::SUMMARY_MSG_NAME;
use crate::filesystem::Filesystem;

/// Default prompt for the memory extraction step. Exposed publicly so callers can
/// extend it when constructing a [`MemoryConfig`](crate::agent::memory::MemoryConfig).
pub const DEFAULT_FLUSH_PROMPT: &str = "\
You are a memory extraction assistant. Analyze the conversation below and extract \
important facts, decisions, preferences, and contextual information that should be \
remembered for future conversations.

Output ONLY the extracted memories as a markdown bullet list. Each item should be \
a concise, self-contained fact. Include dates, names, and specifics when available.

If there is nothing worth remembering, respond with exactly: NO_REPLY

Guidelines:
- Extract user preferences, personal information, project decisions
- Capture important technical decisions and their rationale
- Note any commitments, deadlines, or action items
- Record relationship context (who works on what, team structure)
- Ignore routine greetings, tool invocations, and ephemeral status updates

IMPORTANT — write target and append-only rules:
- You are writing to TODAY'S daily memory ledger (memory/YYYY-MM-DD.md), NOT to MEMORY.md.
- MEMORY.md is the curated long-term memory and is shown ONLY as read-only context. Do NOT \
restate facts already covered by MEMORY.md or by today's earlier entries.
- Keep each bullet point independent and self-contained so entries can be searched individually.";

const SESSION_CONTEXT_TAG: &str = "<session_context>";


/// Shared filesystem helpers used by both the flush and consolidation managers.
pub(crate) mod fileio {
    use std::sync::Arc;

    use agentscope_core::agent::RuntimeContext;
    use agentscope_core::error::Result;

    use crate::filesystem::Filesystem;

    /// Reads a UTF-8 file, treating a missing/erroring file as empty.
    pub(crate) async fn read_text(fs: &Arc<dyn Filesystem>, ctx: &RuntimeContext, path: &str) -> String {
        match fs.read(ctx, path, 0, 0).await {
            Ok(d) => d.content,
            Err(_) => String::new(),
        }
    }

    /// Overwrites a file, deleting any existing copy first (the local backend refuses
    /// to write over an existing file).
    pub(crate) async fn write_overwrite(
        fs: &Arc<dyn Filesystem>,
        ctx: &RuntimeContext,
        path: &str,
        content: &str,
    ) -> Result<()> {
        let _ = fs.delete(ctx, path).await;
        fs.write(ctx, path, content).await
    }

    /// Appends text to a file, creating it if absent.
    pub(crate) async fn append_text(
        fs: &Arc<dyn Filesystem>,
        ctx: &RuntimeContext,
        path: &str,
        addition: &str,
    ) -> Result<()> {
        let existing = read_text(fs, ctx, path).await;
        let combined = format!("{existing}{addition}");
        write_overwrite(fs, ctx, path, &combined).await
    }
}

/// Extracts long-term memories from a conversation window and appends them to today's
/// daily memory ledger.
pub struct MemoryFlushManager {
    fs: Arc<dyn Filesystem>,
    model: Arc<dyn Model>,
    flush_prompt: String,
}

impl MemoryFlushManager {
    /// Creates a flush manager using the default flush prompt.
    pub fn new(fs: Arc<dyn Filesystem>, model: Arc<dyn Model>) -> Self {
        Self::new_with_prompt(fs, model, None)
    }

    /// Creates a flush manager with an optional prompt override (`None` uses the default).
    pub fn new_with_prompt(
        fs: Arc<dyn Filesystem>,
        model: Arc<dyn Model>,
        flush_prompt: Option<String>,
    ) -> Self {
        Self {
            fs,
            model,
            flush_prompt: flush_prompt.unwrap_or_else(|| DEFAULT_FLUSH_PROMPT.to_string()),
        }
    }

    /// Extracts long-term memories from `messages` via the model and appends them to the
    /// daily ledger. No-op when the serialized conversation is empty or the model emits
    /// `NO_REPLY`.
    pub async fn flush_memories(&self, ctx: &RuntimeContext, messages: &[Msg]) -> Result<()> {
        let conversation = serialize_messages(messages);
        if conversation.trim().is_empty() {
            return Ok(());
        }

        let existing_memory = fileio::read_text(&self.fs, ctx, "MEMORY.md").await;
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let daily_rel = format!("memory/{today}.md");
        let existing_daily = fileio::read_text(&self.fs, ctx, &daily_rel).await;

        let mut user_prompt = String::new();
        if !existing_memory.trim().is_empty() {
            user_prompt.push_str(
                &format!("MEMORY.md (read-only curated long-term memory — do NOT restate):\n{existing_memory}\n\n"),
            );
        }
        if !existing_daily.trim().is_empty() {
            user_prompt.push_str(
                &format!("Today's daily ledger so far (your output will be appended after):\n{existing_daily}\n\n"),
            );
        }
        user_prompt.push_str(
            &format!(
                "Extract NEW memories from this conversation window (skip anything already \
                 covered above):\n\n{conversation}"
            ),
        );

        let input = vec![
            Msg::builder().role(MsgRole::System).content(vec![ContentBlock::Text(TextBlock::new(self.flush_prompt.clone()))]).build(),
            Msg::builder().role(MsgRole::User).content(vec![ContentBlock::Text(TextBlock::new(user_prompt))]).build(),
        ];
        let extracted = self.collect_text(ctx, input).await;
        let extracted = extracted.trim();
        if extracted.is_empty() || extracted == "NO_REPLY" {
            return Ok(());
        }
        let timestamp = chrono::Utc::now().to_rfc3339();
        let daily_entry = format!("\n## Memory Flush — {timestamp}\n{extracted}\n");
        fileio::append_text(&self.fs, ctx, &daily_rel, &daily_entry).await?;
        Ok(())
    }

    /// Resolves the workspace-relative path where a session's full history is offloaded.
    pub fn resolve_offload_path(&self, agent_id: &str, session_id: &str) -> String {
        format!("agents/{agent_id}/sessions/{session_id}.jsonl")
    }

    /// Runs the memory-extraction auxiliary call and returns its concatenated text.
    ///
    /// Extraction goes through the budget-only auxiliary path (System material only, no task
    /// state, no context middleware), so a flush can never recursively compact or inject task
    /// state. A failing model must not fail the flush: the extraction is best-effort and the
    /// daily ledger simply gains no entry, matching the previous ignore-errors behaviour.
    async fn collect_text(&self, ctx: &RuntimeContext, messages: Vec<Msg>) -> String {
        auxiliary_text(Arc::clone(&self.model), messages, Some(ctx))
            .await
            .unwrap_or_default()
    }
}


// ---------------------------------------------------------------------------
// Message serialization
// ---------------------------------------------------------------------------

fn role_name(role: MsgRole) -> &'static str {
    match role {
        MsgRole::User => "USER",
        MsgRole::Assistant => "ASSISTANT",
        MsgRole::System => "SYSTEM",
        MsgRole::Tool => "TOOL",
    }
}

/// Serializes messages into a textual representation for the extraction model, skipping
/// SYSTEM and internal context messages (session context / compaction summaries).
fn serialize_messages(messages: &[Msg]) -> String {
    messages
        .iter()
        .filter(|m| m.role != MsgRole::System && !is_internal_context_message(m))
        .filter_map(render_message)
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_internal_context_message(msg: &Msg) -> bool {
    is_session_context_message(msg) || is_compaction_summary_message(msg)
}

fn is_compaction_summary_message(msg: &Msg) -> bool {
    msg.name.as_deref() == Some(SUMMARY_MSG_NAME)
}

fn is_session_context_message(msg: &Msg) -> bool {
    if msg.role != MsgRole::User {
        return false;
    }
    msg.text_content().contains(SESSION_CONTEXT_TAG)
}

fn render_message(msg: &Msg) -> Option<String> {
    let body = render_content_blocks(msg)?;
    Some(format!("[{}]: {}", role_name(msg.role), body))
}

fn render_content_blocks(msg: &Msg) -> Option<String> {
    if msg.content.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    for block in &msg.content {
        match block {
            ContentBlock::Text(t) if !t.text.trim().is_empty() => parts.push(t.text.clone()),
            ContentBlock::ToolUse(tu) => parts.push(render_tool_use(tu)),
            ContentBlock::ToolResult(tr) => parts.push(render_tool_result(tr)),
            _ => {}
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

fn render_tool_use(tu: &ToolUseBlock) -> String {
    let mut s = format!("[tool_call: {}]", tu.name);
    if !tu.input.is_empty() {
        if let Ok(json) = serde_json::to_string(&tu.input) {
            let clipped: String = if json.chars().count() > 500 {
                json.chars().take(500).collect::<String>() + "..."
            } else {
                json
            };
            s.push_str(&format!("({clipped})"));
        }
    }
    s
}

fn render_tool_result(tr: &ToolResultBlock) -> String {
    let mut s = "[tool_result".to_string();
    if let Some(name) = &tr.name {
        s.push_str(&format!(": {name}"));
    }
    s.push_str("] ");
    for block in &tr.output {
        if let ContentBlock::Text(t) = block {
            let text: String = if t.text.chars().count() > 1000 {
                t.text.chars().take(1000).collect::<String>() + "...(truncated)"
            } else {
                t.text.clone()
            };
            s.push_str(&text);
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use futures::stream::{self, BoxStream};
    use futures::StreamExt;
    use agentscope_core::message::{ChatUsage, TextBlock};
    use agentscope_core::model::{ChatResponse, GenerateOptions};

    use crate::filesystem::LocalFilesystem;

    use super::*;

    /// A model that always returns a fixed text block.
    struct FakeModel {
        text: String,
    }

    #[async_trait]
    impl Model for FakeModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<agentscope_core::model::ToolSchema>,
            _options: GenerateOptions,
        ) -> BoxStream<'_, Result<ChatResponse>> {
            let text = self.text.clone();
            let resp = ChatResponse::new(
                "r1",
                vec![ContentBlock::Text(TextBlock::new(text))],
                ChatUsage::default(),
                None,
                None,
            );
            stream::once(async { Ok(resp) }).boxed()
        }

        fn model_name(&self) -> &str {
            "fake"
        }
    }

    fn temp_fs(tag: &str) -> Arc<dyn Filesystem> {
        let dir = std::env::temp_dir().join(format!(
            "agentscope_mem_flush_{tag}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(LocalFilesystem::sandboxed(dir))
    }

    #[test]
    fn serialize_skips_system_and_internal_context() {
        let msgs = vec![
            Msg::builder().role(MsgRole::System).content(vec![ContentBlock::Text(TextBlock::new("sys"))]).build(),
            Msg::user("hello"),
            Msg::builder()
                .role(MsgRole::User)
                .name(SUMMARY_MSG_NAME)
                .content(vec![ContentBlock::Text(TextBlock::new("summary"))])
                .build(),
            Msg::builder()
                .role(MsgRole::User)
                .content(vec![ContentBlock::Text(TextBlock::new("<session_context>x</session_context>"))])
                .build(),
        ];
        let out = serialize_messages(&msgs);
        assert!(out.contains("[USER]: hello"));
        assert!(!out.contains("sys"));
        assert!(!out.contains("summary"));
        assert!(!out.contains("session_context"));
    }

    #[tokio::test]
    async fn flush_writes_to_daily_ledger() {
        let fs = temp_fs("daily");
        let model: Arc<dyn Model> = Arc::new(FakeModel { text: "- user prefers Rust\n".into() });
        let manager = MemoryFlushManager::new(fs.clone(), model);
        let ctx = RuntimeContext::new();
        manager
            .flush_memories(&ctx, &[Msg::user("I like Rust")])
            .await
            .unwrap();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let rel = format!("memory/{today}.md");
        let content = fileio::read_text(&fs, &ctx, &rel).await;
        assert!(content.contains("- user prefers Rust"), "got {content}");
        assert!(content.contains("## Memory Flush"), "got {content}");
    }

    #[tokio::test]
    async fn flush_no_reply_is_noop() {
        let fs = temp_fs("noreply");
        let model: Arc<dyn Model> = Arc::new(FakeModel { text: "NO_REPLY".into() });
        let manager = MemoryFlushManager::new(fs.clone(), model);
        let ctx = RuntimeContext::new();
        manager.flush_memories(&ctx, &[Msg::user("hi")]).await.unwrap();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let rel = format!("memory/{today}.md");
        let content = fileio::read_text(&fs, &ctx, &rel).await;
        assert!(content.is_empty(), "expected no daily entry, got {content}");
    }

    #[tokio::test]
    async fn flush_empty_conversation_is_noop() {
        let fs = temp_fs("empty");
        let model: Arc<dyn Model> = Arc::new(FakeModel { text: "- memory".into() });
        let manager = MemoryFlushManager::new(fs.clone(), model);
        let ctx = RuntimeContext::new();
        // Only SYSTEM messages -> empty serialization.
        let msgs = vec![Msg::builder().role(MsgRole::System).content(vec![ContentBlock::Text(TextBlock::new("sys"))]).build()];
        manager.flush_memories(&ctx, &msgs).await.unwrap();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let rel = format!("memory/{today}.md");
        let content = fileio::read_text(&fs, &ctx, &rel).await;
        assert!(content.is_empty());
    }
}
