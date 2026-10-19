//! Conversation compaction (summarization), mirroring
//! `io.agentscope.harness.agent.memory.compaction`.
//!
//! Provides a character-based token estimator, compaction configuration, the
//! non-LLM pre-pass pipeline (argument truncation + tool-result pruning), and
//! the LLM summarization orchestrator.

use std::collections::HashSet;
use std::sync::Arc;

use agentscope_core::message::{ContentBlock, Msg, MsgRole, TextBlock, ToolResultBlock, ToolUseBlock};
use agentscope_core::model::{GenerateOptions, Model};

/// ~1 token per 2.5 characters (mixed English/Chinese heuristic).
const CHARS_PER_TOKEN: f64 = 2.5;
/// Structural overhead per message (role/name/formatting).
const MESSAGE_OVERHEAD: usize = 5;
/// Structural overhead per tool call.
const TOOL_CALL_OVERHEAD: usize = 10;
/// Structural overhead per tool result.
const TOOL_RESULT_OVERHEAD: usize = 8;

/// Estimates the input tokens for a list of messages.
pub fn calculate_token(messages: &[Msg]) -> usize {
    messages.iter().map(estimate_message_tokens).sum()
}

fn estimate_text_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    (text.chars().count() as f64 / CHARS_PER_TOKEN).ceil() as usize
}

fn role_name(role: MsgRole) -> &'static str {
    match role {
        MsgRole::User => "USER",
        MsgRole::Assistant => "ASSISTANT",
        MsgRole::System => "SYSTEM",
        MsgRole::Tool => "TOOL",
    }
}

fn estimate_message_tokens(msg: &Msg) -> usize {
    let mut tokens = MESSAGE_OVERHEAD;
    tokens += estimate_text_tokens(role_name(msg.role));
    if let Some(name) = &msg.name {
        tokens += estimate_text_tokens(name);
    }
    for block in &msg.content {
        tokens += estimate_content_block_tokens(block);
    }
    tokens
}

fn estimate_content_block_tokens(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text(t) => estimate_text_tokens(&t.text),
        ContentBlock::ToolUse(tu) => estimate_tool_use_tokens(tu),
        ContentBlock::ToolResult(tr) => estimate_tool_result_tokens(tr),
        ContentBlock::Thinking(th) => estimate_text_tokens(&th.thinking),
        _ => 5,
    }
}

fn estimate_tool_use_tokens(tu: &ToolUseBlock) -> usize {
    let mut tokens = TOOL_CALL_OVERHEAD;
    tokens += estimate_text_tokens(&tu.name);
    tokens += estimate_text_tokens(&tu.id);
    if !tu.input.is_empty() {
        if let Ok(json) = serde_json::to_string(&tu.input) {
            tokens += estimate_text_tokens(&json);
        }
    }
    if let Some(content) = &tu.content {
        tokens += estimate_text_tokens(content);
    }
    tokens
}

fn estimate_tool_result_tokens(tr: &ToolResultBlock) -> usize {
    let mut tokens = TOOL_RESULT_OVERHEAD;
    if let Some(name) = &tr.name {
        tokens += estimate_text_tokens(name);
    }
    if let Some(id) = &tr.id {
        tokens += estimate_text_tokens(id);
    }
    for block in &tr.output {
        tokens += estimate_content_block_tokens(block);
    }
    tokens
}

// ---------------------------------------------------------------------------
// Config types
// ---------------------------------------------------------------------------

/// Marker name identifying injected compaction-summary messages.
pub const SUMMARY_MSG_NAME: &str = "__compaction_summary__";

/// The default summarization prompt template (contains `{messages}`).
pub const DEFAULT_SUMMARY_PROMPT: &str = "\
<role>Context Extraction Assistant</role>\n\
<primary_objective>\n\
Extract the highest quality, most relevant context from the conversation below.\n\
</primary_objective>\n\
<instructions>\n\
The history below will be replaced by the context you extract. Do not repeat completed actions.\n\
Use these sections (write \"None\" if empty):\n\
## SESSION INTENT\n\
## SUMMARY\n\
## ARTIFACTS\n\
## NEXT STEPS\n\
</instructions>\n\
<messages>\n\
{messages}\n\
</messages>";

/// Configuration for conversation compaction (summarization).
#[derive(Debug, Clone)]
pub struct CompactionConfig {
    /// Trigger compaction when the conversation has at least this many messages (0 = disabled).
    pub trigger_messages: usize,
    /// Trigger compaction above this token count (0 = dynamic mode).
    pub trigger_tokens: usize,
    /// Token buffer reserved for the compaction process itself.
    pub reserved: usize,
    /// Recent conversation messages to keep verbatim (message-count keep mode).
    pub keep_messages: usize,
    /// Token budget for the preserved tail (`-1` dynamic, `0` message-count, `>0` static).
    pub keep_tokens: i64,
    pub keep_tokens_min: usize,
    pub keep_tokens_max: usize,
    pub keep_tokens_ratio: f64,
    /// Summarization prompt template; must contain `{messages}`.
    pub summary_prompt: String,
    /// Flush long-term memories before compaction.
    pub flush_before_compact: bool,
    /// Offload raw messages to the session JSONL before compaction.
    pub offload_before_compact: bool,
    /// Lightweight pre-summarization argument truncation (None = disabled).
    pub truncate_args: Option<TruncateArgsConfig>,
    /// Aggregate tool-result pruning (None = disabled).
    pub prune: Option<PruneConfig>,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            trigger_messages: 50,
            trigger_tokens: 0,
            reserved: 20_000,
            keep_messages: 20,
            keep_tokens: -1,
            keep_tokens_min: 2_000,
            keep_tokens_max: 8_000,
            keep_tokens_ratio: 0.25,
            summary_prompt: DEFAULT_SUMMARY_PROMPT.to_string(),
            flush_before_compact: true,
            offload_before_compact: true,
            truncate_args: None,
            prune: Some(PruneConfig::defaults()),
        }
    }
}

impl CompactionConfig {
    pub fn builder() -> CompactionConfigBuilder {
        CompactionConfigBuilder::default()
    }

    /// Returns a copy with effective trigger/keep token values resolved from a model's
    /// context window (dynamic mode).
    pub fn with_effective(&self, trigger_tokens: usize, keep_tokens: usize) -> Self {
        let mut c = self.clone();
        c.trigger_tokens = trigger_tokens;
        c.keep_tokens = keep_tokens as i64;
        c
    }
}

/// Builder for [`CompactionConfig`].
#[derive(Debug, Clone)]
pub struct CompactionConfigBuilder {
    pub trigger_messages: usize,
    pub trigger_tokens: usize,
    pub reserved: usize,
    pub keep_messages: usize,
    pub keep_tokens: i64,
    pub keep_tokens_min: usize,
    pub keep_tokens_max: usize,
    pub keep_tokens_ratio: f64,
    pub summary_prompt: String,
    pub flush_before_compact: bool,
    pub offload_before_compact: bool,
    pub truncate_args: Option<TruncateArgsConfig>,
    pub prune: Option<PruneConfig>,
}

impl Default for CompactionConfigBuilder {
    fn default() -> Self {
        let d = CompactionConfig::default();
        Self {
            trigger_messages: d.trigger_messages,
            trigger_tokens: d.trigger_tokens,
            reserved: d.reserved,
            keep_messages: d.keep_messages,
            keep_tokens: d.keep_tokens,
            keep_tokens_min: d.keep_tokens_min,
            keep_tokens_max: d.keep_tokens_max,
            keep_tokens_ratio: d.keep_tokens_ratio,
            summary_prompt: d.summary_prompt,
            flush_before_compact: d.flush_before_compact,
            offload_before_compact: d.offload_before_compact,
            truncate_args: d.truncate_args,
            prune: d.prune,
        }
    }
}

impl CompactionConfigBuilder {
    pub fn trigger_messages(mut self, v: usize) -> Self { self.trigger_messages = v; self }
    pub fn trigger_tokens(mut self, v: usize) -> Self { self.trigger_tokens = v; self }
    pub fn reserved(mut self, v: usize) -> Self { self.reserved = v; self }
    pub fn keep_messages(mut self, v: usize) -> Self { self.keep_messages = v; self }
    pub fn keep_tokens(mut self, v: i64) -> Self { self.keep_tokens = v; self }
    pub fn keep_tokens_min(mut self, v: usize) -> Self { self.keep_tokens_min = v; self }
    pub fn keep_tokens_max(mut self, v: usize) -> Self { self.keep_tokens_max = v; self }
    pub fn keep_tokens_ratio(mut self, v: f64) -> Self { self.keep_tokens_ratio = v; self }
    pub fn summary_prompt(mut self, v: impl Into<String>) -> Self { self.summary_prompt = v.into(); self }
    pub fn flush_before_compact(mut self, v: bool) -> Self { self.flush_before_compact = v; self }
    pub fn offload_before_compact(mut self, v: bool) -> Self { self.offload_before_compact = v; self }
    pub fn truncate_args(mut self, v: Option<TruncateArgsConfig>) -> Self { self.truncate_args = v; self }
    pub fn prune(mut self, v: Option<PruneConfig>) -> Self { self.prune = v; self }
    pub fn build(self) -> CompactionConfig {
        CompactionConfig {
            trigger_messages: self.trigger_messages,
            trigger_tokens: self.trigger_tokens,
            reserved: self.reserved,
            keep_messages: self.keep_messages,
            keep_tokens: self.keep_tokens,
            keep_tokens_min: self.keep_tokens_min,
            keep_tokens_max: self.keep_tokens_max,
            keep_tokens_ratio: self.keep_tokens_ratio,
            summary_prompt: self.summary_prompt,
            flush_before_compact: self.flush_before_compact,
            offload_before_compact: self.offload_before_compact,
            truncate_args: self.truncate_args,
            prune: self.prune,
        }
    }
}

/// Configuration for lightweight pre-summarization argument truncation.
#[derive(Debug, Clone)]
pub struct TruncateArgsConfig {
    pub trigger_messages: usize,
    pub trigger_tokens: usize,
    pub keep_messages: usize,
    pub keep_tokens: usize,
    /// Max characters of any single tool argument value.
    pub max_arg_length: usize,
    /// Suffix appended after the first 20 characters of a truncated argument.
    pub truncation_text: String,
}

impl Default for TruncateArgsConfig {
    fn default() -> Self {
        Self {
            trigger_messages: 25,
            trigger_tokens: 40_000,
            keep_messages: 20,
            keep_tokens: 0,
            max_arg_length: 2_000,
            truncation_text: "...(argument truncated)".to_string(),
        }
    }
}

impl TruncateArgsConfig {
    pub fn builder() -> TruncateArgsConfigBuilder {
        TruncateArgsConfigBuilder::default()
    }
}

/// Builder for [`TruncateArgsConfig`].
#[derive(Debug, Clone)]
pub struct TruncateArgsConfigBuilder {
    pub trigger_messages: usize,
    pub trigger_tokens: usize,
    pub keep_messages: usize,
    pub keep_tokens: usize,
    pub max_arg_length: usize,
    pub truncation_text: String,
}

impl Default for TruncateArgsConfigBuilder {
    fn default() -> Self {
        let d = TruncateArgsConfig::default();
        Self {
            trigger_messages: d.trigger_messages,
            trigger_tokens: d.trigger_tokens,
            keep_messages: d.keep_messages,
            keep_tokens: d.keep_tokens,
            max_arg_length: d.max_arg_length,
            truncation_text: d.truncation_text,
        }
    }
}

impl TruncateArgsConfigBuilder {
    pub fn trigger_messages(mut self, v: usize) -> Self { self.trigger_messages = v; self }
    pub fn trigger_tokens(mut self, v: usize) -> Self { self.trigger_tokens = v; self }
    pub fn keep_messages(mut self, v: usize) -> Self { self.keep_messages = v; self }
    pub fn keep_tokens(mut self, v: usize) -> Self { self.keep_tokens = v; self }
    pub fn max_arg_length(mut self, v: usize) -> Self { self.max_arg_length = v; self }
    pub fn truncation_text(mut self, v: impl Into<String>) -> Self { self.truncation_text = v.into(); self }
    pub fn build(self) -> TruncateArgsConfig {
        TruncateArgsConfig {
            trigger_messages: self.trigger_messages,
            trigger_tokens: self.trigger_tokens,
            keep_messages: self.keep_messages,
            keep_tokens: self.keep_tokens,
            max_arg_length: self.max_arg_length,
            truncation_text: self.truncation_text,
        }
    }
}

/// Configuration for aggregate tool-result pruning.
#[derive(Debug, Clone)]
pub struct PruneConfig {
    /// Token budget for recent tool outputs that are never pruned.
    pub protect_tokens: usize,
    /// Minimum prunable token total before pruning executes.
    pub minimum_tokens: usize,
    /// Max characters kept per pruned tool result (head+tail preview).
    pub max_output_chars: usize,
    /// Tool names excluded from pruning.
    pub excluded_tools: HashSet<String>,
}

impl PruneConfig {
    pub fn defaults() -> Self {
        Self {
            protect_tokens: 40_000,
            minimum_tokens: 20_000,
            max_output_chars: 2_000,
            excluded_tools: ["read_file", "memory_search", "memory_get", "session_search"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        }
    }
}

impl Default for PruneConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

// ---------------------------------------------------------------------------
// ConversationCompactor
// ---------------------------------------------------------------------------

/// Orchestrates conversation compaction (summarization).
///
/// Algorithm: truncate args -> prune tool results -> check trigger -> compute a safe
/// cutoff (never splitting ASSISTANT tool-call/TOOL-result pairs) -> summarize the
/// prefix (best-effort via an optional [`Model`]) -> return `[summary] + preserved_tail`.
pub struct ConversationCompactor {
    model: Option<Arc<dyn Model>>,
}

impl ConversationCompactor {
    pub fn new(model: Option<Arc<dyn Model>>) -> Self {
        Self { model }
    }

    pub fn with_model(model: Arc<dyn Model>) -> Self {
        Self { model: Some(model) }
    }

    /// Runs compaction on the conversation messages if a trigger is met.
    ///
    /// Only non-SYSTEM messages should be passed. Returns `None` when no compaction was
    /// needed; otherwise `Some([summary_user_msg] + preserved_tail)`.
    pub async fn compact_if_needed(
        &self,
        conversation: Vec<Msg>,
        config: &CompactionConfig,
        _agent_id: &str,
        _session_id: &str,
    ) -> agentscope_core::error::Result<Option<Vec<Msg>>> {
        if conversation.is_empty() {
            return Ok(None);
        }
        let messages =
            prune_tool_results(&truncate_args(&conversation, config.truncate_args.as_ref()),
                config.prune.as_ref());
        let total = calculate_token(&messages);
        if !should_compact(&messages, total, config) {
            return Ok(None);
        }
        let cutoff = determine_cutoff_index(&messages, total, config);
        if cutoff == 0 {
            return Ok(None);
        }
        let tail = messages[cutoff..].to_vec();
        let summary = self.summarize_prefix(&messages[..cutoff], config).await?;
        let summary_msg = build_summary_message(&summary, None);
        let mut compacted = Vec::with_capacity(1 + tail.len());
        compacted.push(summary_msg);
        compacted.extend(tail);
        Ok(Some(compacted))
    }

    /// Summarizes the prefix. A model failure or an empty summary is returned as an error so
    /// the caller keeps the original history instead of discarding it for a placeholder.
    async fn summarize_prefix(
        &self,
        prefix: &[Msg],
        config: &CompactionConfig,
    ) -> agentscope_core::error::Result<String> {
        if prefix.is_empty() {
            return Ok("No previous conversation history.".to_string());
        }
        let Some(model) = &self.model else {
            return Ok("(summary unavailable - no model configured)".to_string());
        };
        let formatted = format_messages_for_summary(prefix);
        let prompt = config.summary_prompt.replace("{messages}", &formatted);
        let input = vec![Msg::user(prompt)];
        let mut out = String::new();
        let mut stream = model.stream(input, Vec::new(), GenerateOptions::default());
        use futures::StreamExt;
        while let Some(resp) = stream.next().await {
            let r = resp?;
            for block in &r.content {
                if let ContentBlock::Text(t) = block {
                    out.push_str(&t.text);
                }
            }
        }
        let trimmed = out.trim();
        if trimmed.is_empty() {
            return Err(agentscope_core::error::Error::Agent(
                "conversation compaction produced an empty summary".into(),
            ));
        }
        Ok(trimmed.to_string())
    }
}

// ---------------------------------------------------------------------------
// Trigger / cutoff logic
// ---------------------------------------------------------------------------

fn should_compact(messages: &[Msg], total: usize, config: &CompactionConfig) -> bool {
    if config.trigger_messages > 0 && messages.len() >= config.trigger_messages {
        return true;
    }
    config.trigger_tokens > 0 && total >= config.trigger_tokens
}

fn determine_cutoff_index(messages: &[Msg], total: usize, config: &CompactionConfig) -> usize {
    let raw = if config.keep_tokens > 0 {
        find_token_based_cutoff(messages, total, config.keep_tokens as usize)
    } else {
        find_message_based_cutoff(messages, config.keep_messages)
    };
    find_safe_cutoff_point(messages, raw)
}

fn find_token_based_cutoff(messages: &[Msg], total: usize, keep_tokens: usize) -> usize {
    if total <= keep_tokens {
        return 0;
    }
    let mut left = 0usize;
    let mut right = messages.len();
    let mut candidate = messages.len();
    while left < right {
        let mid = left + (right - left) / 2;
        if calculate_token(&messages[mid..]) <= keep_tokens {
            candidate = mid;
            right = mid;
        } else {
            left = mid + 1;
        }
    }
    candidate.min(messages.len().saturating_sub(1))
}

fn find_message_based_cutoff(messages: &[Msg], keep_messages: usize) -> usize {
    if messages.len() <= keep_messages {
        return 0;
    }
    messages.len() - keep_messages
}

/// Adjusts the cutoff so ASSISTANT tool-call/TOOL-result pairs are never split.
fn find_safe_cutoff_point(messages: &[Msg], cutoff: usize) -> usize {
    if cutoff == 0 || cutoff >= messages.len() {
        return cutoff;
    }
    if messages[cutoff].role != MsgRole::Tool {
        return cutoff;
    }
    let mut tool_call_ids: Vec<String> = Vec::new();
    let mut idx = cutoff;
    while idx < messages.len() && messages[idx].role == MsgRole::Tool {
        for block in &messages[idx].content {
            if let ContentBlock::ToolResult(tr) = block {
                if let Some(id) = &tr.id {
                    tool_call_ids.push(id.clone());
                }
            }
        }
        idx += 1;
    }
    if tool_call_ids.is_empty() {
        return idx;
    }
    for i in (0..cutoff).rev() {
        let msg = &messages[i];
        if msg.role == MsgRole::Assistant {
            for block in &msg.content {
                if let ContentBlock::ToolUse(tu) = block {
                    if tool_call_ids.iter().any(|id| id == &tu.id) {
                        return i;
                    }
                }
            }
        }
    }
    idx
}

// ---------------------------------------------------------------------------
// Summarization formatting & summary message construction
// ---------------------------------------------------------------------------

/// Formats messages as a human-readable block for the summarization LLM.
pub fn format_messages_for_summary(messages: &[Msg]) -> String {
    messages
        .iter()
        .filter(|m| m.role != MsgRole::System)
        .map(render_message_for_summary)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn render_message_for_summary(msg: &Msg) -> String {
    let role_label = match msg.role {
        MsgRole::User => "Human",
        MsgRole::Assistant => "AI",
        MsgRole::Tool => "Tool",
        _ => role_name(msg.role),
    };
    let mut parts: Vec<String> = Vec::new();
    for block in &msg.content {
        match block {
            ContentBlock::Text(t) if !t.text.trim().is_empty() => {
                parts.push(t.text.trim().to_string());
            }
            ContentBlock::ToolUse(tu) => parts.push(format!("[tool_call: {}]", tu.name)),
            ContentBlock::ToolResult(tr) => {
                let text = extract_tool_result_text(tr);
                let mut s = format!("[tool_result: {}] ", tr.name.as_deref().unwrap_or("?"));
                if !text.trim().is_empty() {
                    let preview: String = text.chars().take(500).collect();
                    let preview = if text.chars().count() > 500 {
                        format!("{}...", preview)
                    } else {
                        preview
                    };
                    s.push_str(&preview);
                }
                parts.push(s.trim().to_string());
            }
            _ => {}
        }
    }
    let body = parts.join(" ");
    if body.is_empty() {
        String::new()
    } else {
        format!("{}: {}", role_label, body)
    }
}

fn extract_tool_result_text(tr: &ToolResultBlock) -> String {
    tr.output
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) if !t.text.trim().is_empty() => Some(t.text.trim().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Builds a USER message carrying the compaction summary.
pub fn build_summary_message(summary: &str, file_path: Option<&str>) -> Msg {
    let content = match file_path {
        Some(path) => format!(
            "You are in the middle of a conversation that has been summarized.\n\n\
             The full conversation history has been saved to {} should you need to refer back\
             to it for details.\n\nA condensed summary follows:\n\n<summary>\n{}\n</summary>",
            path, summary
        ),
        None => format!("Here is a summary of the conversation to date:\n\n{}", summary),
    };
    Msg::builder()
        .id(build_summary_message_id(&content))
        .role(MsgRole::User)
        .name(SUMMARY_MSG_NAME)
        .text_content(content)
        .build()
}

/// Deterministic FNV-1a hash over the summary content for idempotent offloads.
fn build_summary_message_id(content: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in content.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{}:{:016x}", SUMMARY_MSG_NAME, hash)
}

/// Removes previously injected summary messages from a list.
pub fn filter_summary_messages(messages: &[Msg]) -> Vec<Msg> {
    messages
        .iter()
        .filter(|m| m.name.as_deref() != Some(SUMMARY_MSG_NAME))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Pre-summarization passes (non-LLM)
// ---------------------------------------------------------------------------

/// Truncates large `ToolUseBlock` argument values in older messages.
pub fn truncate_args(messages: &[Msg], cfg: Option<&TruncateArgsConfig>) -> Vec<Msg> {
    let Some(cfg) = cfg else {
        return messages.to_vec();
    };
    if messages.is_empty() {
        return messages.to_vec();
    }
    let total = calculate_token(messages);
    if !should_truncate_args(messages, total, cfg) {
        return messages.to_vec();
    }
    let cutoff = determine_truncate_cutoff(messages, cfg);
    if cutoff >= messages.len() {
        return messages.to_vec();
    }
    let mut any_modified = false;
    let mut result: Vec<Msg> = Vec::with_capacity(messages.len());
    for (i, msg) in messages.iter().enumerate() {
        if i < cutoff && msg.role == MsgRole::Assistant {
            let truncated = truncate_tool_use_args(msg, cfg);
            if &truncated != msg {
                any_modified = true;
            }
            result.push(truncated);
        } else {
            result.push(msg.clone());
        }
    }
    if any_modified {
        result
    } else {
        messages.to_vec()
    }
}

fn should_truncate_args(messages: &[Msg], total: usize, cfg: &TruncateArgsConfig) -> bool {
    if cfg.trigger_messages > 0 && messages.len() >= cfg.trigger_messages {
        return true;
    }
    cfg.trigger_tokens > 0 && total >= cfg.trigger_tokens
}

fn determine_truncate_cutoff(messages: &[Msg], cfg: &TruncateArgsConfig) -> usize {
    if cfg.keep_tokens > 0 {
        let mut tokens_kept = 0usize;
        for i in (0..messages.len()).rev() {
            let msg_tokens = calculate_token(&messages[i..i + 1]);
            if tokens_kept + msg_tokens > cfg.keep_tokens {
                return i + 1;
            }
            tokens_kept += msg_tokens;
        }
        0
    } else {
        messages.len().saturating_sub(cfg.keep_messages)
    }
}

fn truncate_tool_use_args(msg: &Msg, cfg: &TruncateArgsConfig) -> Msg {
    let mut any_modified = false;
    let mut new_blocks: Vec<ContentBlock> = Vec::with_capacity(msg.content.len());
    for block in &msg.content {
        match block {
            ContentBlock::ToolUse(tu) => {
                let truncated = truncate_tool_use_block(tu, cfg);
                if &truncated != tu {
                    any_modified = true;
                }
                new_blocks.push(ContentBlock::ToolUse(truncated));
            }
            other => new_blocks.push(other.clone()),
        }
    }
    if !any_modified {
        return msg.clone();
    }
    let mut m = Msg::builder().role(msg.role).content(new_blocks).build();
    m.name = msg.name.clone();
    m
}

fn truncate_tool_use_block(tu: &ToolUseBlock, cfg: &TruncateArgsConfig) -> ToolUseBlock {
    let mut any_modified = false;
    let mut new_input = tu.input.clone();
    for (k, v) in &tu.input {
        if let Some(s) = v.as_str() {
            if s.chars().count() > cfg.max_arg_length {
                let head: String = s.chars().take(20).collect();
                new_input.insert(
                    k.clone(),
                    serde_json::Value::String(head + &cfg.truncation_text),
                );
                any_modified = true;
            }
        }
    }
    if !any_modified {
        return tu.clone();
    }
    ToolUseBlock::builder()
        .id(tu.id.clone())
        .name(tu.name.clone())
        .input(new_input)
        .build()
}

/// Prunes old oversized tool results, protecting the most recent budget.
pub fn prune_tool_results(messages: &[Msg], prune: Option<&PruneConfig>) -> Vec<Msg> {
    let Some(prune) = prune else {
        return messages.to_vec();
    };
    if messages.is_empty() {
        return messages.to_vec();
    }
    let protect_budget = prune.protect_tokens;
    let max_chars = prune.max_output_chars;
    let mut protected_tokens = 0usize;
    let mut prunable_tokens = 0usize;
    let mut to_prune: Vec<(usize, usize)> = Vec::new();
    for i in (0..messages.len()).rev() {
        let msg = &messages[i];
        if msg.role != MsgRole::Tool {
            continue;
        }
        for j in (0..msg.content.len()).rev() {
            let ContentBlock::ToolResult(tr) = &msg.content[j] else {
                continue;
            };
            if let Some(name) = &tr.name {
                if prune.excluded_tools.contains(name) {
                    continue;
                }
            }
            let text = extract_tool_result_text(tr);
            if text.trim().is_empty() {
                continue;
            }
            // Charge this result only, not every sibling in a batched tool message.
            let tokens = estimate_single_tool_result_cost(msg, tr);
            if protected_tokens < protect_budget {
                protected_tokens += tokens;
                continue;
            }
            if text.chars().count() > max_chars {
                prunable_tokens += tokens;
                to_prune.push((i, j));
            }
        }
    }
    if prunable_tokens < prune.minimum_tokens || to_prune.is_empty() {
        return messages.to_vec();
    }
    let mut result: Vec<Msg> = messages.to_vec();
    for (msg_idx, block_idx) in to_prune {
        let msg = &result[msg_idx];
        let ContentBlock::ToolResult(tr) = &msg.content[block_idx] else {
            continue;
        };
        let text = extract_tool_result_text(tr);
        let preview = build_prune_preview(&text, max_chars);
        let pruned = ToolResultBlock::builder()
            .id(tr.id.clone().unwrap_or_default())
            .name(tr.name.clone().unwrap_or_default())
            .output(ContentBlock::Text(TextBlock::new(preview)))
            .metadata(tr.metadata.clone())
            .state(tr.state)
            .build();
        let mut new_blocks = msg.content.clone();
        new_blocks[block_idx] = ContentBlock::ToolResult(pruned);
        let mut m = Msg::builder()
            .id(msg.id.clone())
            .role(msg.role)
            .content(new_blocks)
            .metadata(msg.metadata.clone())
            .timestamp(msg.timestamp.clone())
            .build();
        m.name = msg.name.clone();
        m.usage = msg.usage.clone();
        result[msg_idx] = m;
    }
    result
}

/// Charges a single tool result: the carrying message's structural overhead plus this
/// result block, mirroring `calculateToken(List.of(msg.withContent(List.of(tr))))`.
/// Sibling results in a batched TOOL message are not counted.
fn estimate_single_tool_result_cost(msg: &Msg, tr: &ToolResultBlock) -> usize {
    let mut tokens = MESSAGE_OVERHEAD + estimate_text_tokens(role_name(msg.role));
    if let Some(name) = &msg.name {
        tokens += estimate_text_tokens(name);
    }
    tokens + estimate_tool_result_tokens(tr)
}

fn build_prune_preview(text: &str, max_chars: usize) -> String {
    let half = max_chars / 2;
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    if len <= max_chars {
        return text.to_string();
    }
    let head: String = chars[..half.min(len)].iter().collect();
    let start = len - half;
    let tail: String = chars[start..].iter().collect();
    let removed = len - head.chars().count() - tail.chars().count();
    if removed == 0 {
        return text.to_string();
    }
    format!("{}\n\n...({} chars pruned)...\n\n{}", head, removed, tail)
}
