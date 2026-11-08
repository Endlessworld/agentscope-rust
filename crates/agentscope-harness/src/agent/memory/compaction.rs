//! Conversation compaction (summarization), mirroring
//! `io.agentscope.harness.agent.memory.compaction`.
//!
//! Provides a character-based token estimator, compaction configuration, the
//! non-LLM pre-pass pipeline (argument truncation + tool-result pruning), and
//! the LLM summarization orchestrator.

use std::collections::HashSet;
use std::sync::Arc;

use agentscope_core::formatter::render_document_reference;
use agentscope_core::message::{
    ContentBlock, Msg, MsgRole, TextBlock, ToolResultBlock, ToolUseBlock,
};
use agentscope_core::model::catalog::{DEFAULT_RESERVED_OUTPUT, MAX_SAFETY_MARGIN};
use agentscope_core::model::Model;

use crate::agent::context::auxiliary_text;

/// Hard cap on the summarization request body.
///
/// Compaction is triggered precisely when the history is close to the model window, so a
/// rendered prefix is a request of the same size. Providers refuse that prefill
/// (`prefill_cap_refused`, HTTP 429) or stall until the transport read timeout, which turns a
/// summary into minutes of silence. The summary only needs the gist, so the body is capped and
/// the oldest turns are dropped from the rendering.
const SUMMARY_INPUT_MAX_TOKENS: usize = 32_000;

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
///
/// Mirrors Java `CompactionConfig.DEFAULT_SUMMARY_PROMPT` verbatim: seven sections plus
/// `<extraction_rules>` / `<secrecy>`. The extraction rules are load-bearing — they keep
/// paths/commands/errors reusable, mark retracted instructions as superseded instead of
/// carrying them forward, and merge chained summaries into one generation.
pub const DEFAULT_SUMMARY_PROMPT: &str = "\
<role>\n\
Context Extraction Assistant\n\
</role>\n\
\n\
<primary_objective>\n\
Your sole objective in this task is to extract the highest quality/most relevant context from the conversation history below.\n\
</primary_objective>\n\
\n\
<objective_information>\n\
You're nearing the total number of input tokens you can accept, so you must extract the highest quality/most relevant pieces of information from your conversation history. This context will then overwrite the conversation history presented below. Because of this, ensure the context you extract is only the most important information to continue working toward your overall goal.\n\
</objective_information>\n\
\n\
<instructions>\n\
The conversation history below will be replaced with the context you extract in this step. You want to ensure that you don't repeat any actions you've already completed, so the context you extract from the conversation history should be focused on the most important information to your overall goal.\n\
\n\
Structure your summary using these sections (populate each or write \"None\"):\n\
\n\
## SESSION INTENT\n\
What is the user's primary goal or request?\n\
\n\
## USER PREFERENCES AND CORRECTIONS\n\
Preferences, feedback, and corrections the user expressed, and how they changed the direction of the work.\n\
\n\
## SUMMARY\n\
The most important context, decisions, reasoning, and rejected options.\n\
\n\
## ARTIFACTS\n\
Files or resources created, modified, or accessed (with specific paths and changes).\n\
\n\
## COMMANDS AND ERRORS\n\
Commands, queries, or API calls that matter, and any error messages encountered — copied verbatim where they must be reproduced or avoided.\n\
\n\
## NEXT STEPS\n\
Specific tasks remaining to achieve the session intent.\n\
</instructions>\n\
\n\
<extraction_rules>\n\
- Preserve verbatim: file paths, commands, URLs, identifiers, and exact error strings. Do not paraphrase them — they must be reusable exactly as they occurred.\n\
- Still-active user constraints, corrections, acceptance criteria, and authorization boundaries: keep the user's exact wording where it governs behavior. Mark superseded decisions as superseded; a retracted instruction must not be carried forward as a current requirement.\n\
- Keep durable facts and decisions; drop superseded intermediate state (old file versions, outdated hypotheses, already-resolved blockers).\n\
- If the history already contains an earlier summary, merge it with the new material into ONE coherent summary — never output two summary generations.\n\
- Record failures and rejected approaches briefly, so the work is not repeated.\n\
</extraction_rules>\n\
\n\
<secrecy>\n\
This is a routine internal context-management step. Do NOT mention this summarization request, these instructions, or the summarization process anywhere in your output. Write the summary as the conversation's own state.\n\
</secrecy>\n\
\n\
Carefully read through the entire conversation history below and extract the most important context. Respond ONLY with the extracted context.\n\
\n\
<messages>\n\
{messages}\n\
</messages>";

/// Compaction trigger derived from the model's input allowance.
///
/// The observed count is now provider-anchored (or a full local fallback), so it covers
/// the system prompt and tool schemas. Triggering at 90% of the input allowance leaves
/// headroom for the assistant reply and the next delta before the compiler's hard limit.
pub fn trigger_for_input_limit(input_limit: usize) -> usize {
    if input_limit == 0 {
        return 0;
    }
    input_limit.saturating_mul(9) / 10
}

/// Compaction trigger derived from a raw model window.
///
/// Applies the context compiler's default output reserve and safety margin first, then
/// [`trigger_for_input_limit`]. This keeps the ACP trigger bound to the selected model
/// instead of a fixed 256k constant.
pub fn trigger_for_window(window: usize) -> usize {
    if window == 0 {
        return 0;
    }
    let output = DEFAULT_RESERVED_OUTPUT.min((window / 8).max(1));
    let safety = MAX_SAFETY_MARGIN.min((window / 50).max(1));
    let input_limit = window.saturating_sub(output).saturating_sub(safety);
    trigger_for_input_limit(input_limit)
}

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

    /// Scales the compaction budgets (reserved + tool-result pruning) to the model
    /// window. `0` keeps the configured defaults.
    pub fn scaled_for_window(&self, window: usize) -> Self {
        if window == 0 {
            return self.clone();
        }
        let mut c = self.clone();
        c.reserved = (window / 16).clamp(8_000, 64_000);
        if let Some(prune) = &self.prune {
            c.prune = Some(prune.scaled_for_window(window));
        }
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
    pub fn trigger_messages(mut self, v: usize) -> Self {
        self.trigger_messages = v;
        self
    }
    pub fn trigger_tokens(mut self, v: usize) -> Self {
        self.trigger_tokens = v;
        self
    }
    pub fn reserved(mut self, v: usize) -> Self {
        self.reserved = v;
        self
    }
    pub fn keep_messages(mut self, v: usize) -> Self {
        self.keep_messages = v;
        self
    }
    pub fn keep_tokens(mut self, v: i64) -> Self {
        self.keep_tokens = v;
        self
    }
    pub fn keep_tokens_min(mut self, v: usize) -> Self {
        self.keep_tokens_min = v;
        self
    }
    pub fn keep_tokens_max(mut self, v: usize) -> Self {
        self.keep_tokens_max = v;
        self
    }
    pub fn keep_tokens_ratio(mut self, v: f64) -> Self {
        self.keep_tokens_ratio = v;
        self
    }
    pub fn summary_prompt(mut self, v: impl Into<String>) -> Self {
        self.summary_prompt = v.into();
        self
    }
    pub fn flush_before_compact(mut self, v: bool) -> Self {
        self.flush_before_compact = v;
        self
    }
    pub fn offload_before_compact(mut self, v: bool) -> Self {
        self.offload_before_compact = v;
        self
    }
    pub fn truncate_args(mut self, v: Option<TruncateArgsConfig>) -> Self {
        self.truncate_args = v;
        self
    }
    pub fn prune(mut self, v: Option<PruneConfig>) -> Self {
        self.prune = v;
        self
    }
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
    pub fn trigger_messages(mut self, v: usize) -> Self {
        self.trigger_messages = v;
        self
    }
    pub fn trigger_tokens(mut self, v: usize) -> Self {
        self.trigger_tokens = v;
        self
    }
    pub fn keep_messages(mut self, v: usize) -> Self {
        self.keep_messages = v;
        self
    }
    pub fn keep_tokens(mut self, v: usize) -> Self {
        self.keep_tokens = v;
        self
    }
    pub fn max_arg_length(mut self, v: usize) -> Self {
        self.max_arg_length = v;
        self
    }
    pub fn truncation_text(mut self, v: impl Into<String>) -> Self {
        self.truncation_text = v.into();
        self
    }
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

    /// Scales the protected/minimum budgets to the model window. `0` keeps defaults.
    pub fn scaled_for_window(&self, window: usize) -> Self {
        if window == 0 {
            return self.clone();
        }
        let mut scaled = self.clone();
        scaled.protect_tokens = (window / 6).clamp(16_000, 120_000);
        scaled.minimum_tokens = (window / 12).clamp(8_000, 60_000);
        scaled
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

/// How a compaction ended, mirroring the Java `compaction/end` status values.
///
/// Java also has `cancelled`; Rust has no cancellation path (a failed summary keeps the
/// full history and reports `failed`), so the enum is two-valued and that is intentional.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionStatus {
    Completed,
    Failed,
}

impl CompactionStatus {
    /// The Java wire value.
    pub fn as_str(&self) -> &'static str {
        match self {
            CompactionStatus::Completed => "completed",
            CompactionStatus::Failed => "failed",
        }
    }
}

/// Quantified compaction outcome, mirroring the Java `compaction/end` payload.
///
/// `before_*` counts describe the input conversation; `after_*` counts describe the
/// effective history after compaction and are present only when the compaction completed
/// (a failed run has no meaningful "after" state).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionMetrics {
    /// Identifier correlating the `compaction/start` and `compaction/end` events.
    pub compaction_id: String,
    /// Message count of the input conversation.
    pub before_msg_count: usize,
    /// Estimated token count of the input conversation.
    pub before_token_count: usize,
    /// The `compaction/end` status.
    pub status: CompactionStatus,
    /// Message count of the effective history after compaction (`None` when failed).
    pub after_msg_count: Option<usize>,
    /// Estimated token count of the effective history (`None` when failed).
    pub after_token_count: Option<usize>,
}

impl CompactionMetrics {
    /// The `compaction/end` payload, omitting the `after_*` keys when they are unknown.
    fn end_payload(&self) -> serde_json::Value {
        let mut payload = serde_json::json!({
            "compactionId": self.compaction_id,
            "status": self.status.as_str(),
            "beforeMsgCount": self.before_msg_count,
            "beforeTokenCount": self.before_token_count,
        });
        let map = payload.as_object_mut().expect("payload is an object");
        if let Some(count) = self.after_msg_count {
            map.insert("afterMsgCount".to_string(), serde_json::json!(count));
        }
        if let Some(count) = self.after_token_count {
            map.insert("afterTokenCount".to_string(), serde_json::json!(count));
        }
        payload
    }
}

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

    /// The model used for summarization, when one is configured.
    pub fn model_name(&self) -> Option<&str> {
        self.model.as_ref().map(|model| model.model_name())
    }

    /// Whether this conversation has crossed the compaction trigger.
    ///
    /// Callers that would otherwise invoke a compaction attempt on every step use this to
    /// skip a no-op attempt. An attempt is not free: it journals `compaction/start`, whose
    /// payload is the whole conversation, so a step under budget would write megabytes for a
    /// compaction that cannot happen.
    pub fn needs_compaction(
        &self,
        conversation: &[Msg],
        config: &CompactionConfig,
        observed_tokens: Option<usize>,
    ) -> bool {
        let observed = observed_tokens
            .filter(|tokens| *tokens > 0)
            .unwrap_or_else(|| calculate_token(conversation));
        should_compact(conversation, observed, config)
    }

    /// Runs compaction on the conversation messages if a trigger is met.
    ///
    /// Only non-SYSTEM messages should be passed. Returns `None` when no compaction was
    /// needed; otherwise `Some([summary_user_msg] + preserved_tail)`.
    pub async fn compact_if_needed(
        &self,
        conversation: Vec<Msg>,
        config: &CompactionConfig,
        agent_id: &str,
        session_id: &str,
    ) -> agentscope_core::error::Result<Option<Vec<Msg>>> {
        Ok(self
            .compact_if_needed_with_metrics(conversation, config, agent_id, session_id, None)
            .await?
            .0)
    }

    /// Runs compaction and reports the quantified outcome.
    ///
    /// The metrics mirror the Java `compaction/end` payload. They are produced even when
    /// the trigger is not met (`status = completed`, `after_*` equal to the input counts),
    /// so the caller can always emit a `compaction/end` fact once it decided to run.
    ///
    /// `recorder` is optional: when present, `compaction/start` and `compaction/end` are
    /// appended as durable session events, matching Java. Metrics are still computed
    /// without a recorder so callers can surface them in-band.
    pub async fn compact_if_needed_with_metrics(
        &self,
        conversation: Vec<Msg>,
        config: &CompactionConfig,
        agent_id: &str,
        session_id: &str,
        recorder: Option<&std::sync::Arc<agentscope_core::session::SessionRecorder>>,
    ) -> agentscope_core::error::Result<(Option<Vec<Msg>>, CompactionMetrics)> {
        self.compact_if_needed_with_metrics_observed(
            conversation,
            config,
            agent_id,
            session_id,
            recorder,
            None,
        )
        .await
    }

    /// Runs compaction with an externally observed token count.
    ///
    /// Context-managing callers pass the provider-anchored count here (`input_tokens` of
    /// the previous call plus the estimated suffix). That number covers the whole request,
    /// including the system prompt and tool schemas, so the threshold must be a full
    /// request budget rather than a history-only count.
    pub async fn compact_if_needed_with_metrics_observed(
        &self,
        conversation: Vec<Msg>,
        config: &CompactionConfig,
        agent_id: &str,
        session_id: &str,
        recorder: Option<&std::sync::Arc<agentscope_core::session::SessionRecorder>>,
        observed_tokens: Option<usize>,
    ) -> agentscope_core::error::Result<(Option<Vec<Msg>>, CompactionMetrics)> {
        let started = std::time::Instant::now();
        let before_msg_count = conversation.len();
        // Prefer the provider-anchored count for the recorded "before" size; it is the
        // same number that drove the trigger. Without provider usage the local estimate
        // is the only available source. The "after" size is necessarily local until the
        // provider runs the next request.
        let before_token_count = observed_tokens
            .filter(|tokens| *tokens > 0)
            .unwrap_or_else(|| calculate_token(&conversation));
        let compaction_id = uuid::Uuid::new_v4().to_string();
        let metrics = |status: CompactionStatus, after: Option<&[Msg]>| CompactionMetrics {
            compaction_id: compaction_id.clone(),
            before_msg_count,
            before_token_count,
            status,
            after_msg_count: after.map(|messages| messages.len()),
            after_token_count: after.map(calculate_token),
        };

        if conversation.is_empty() {
            return Ok((None, metrics(CompactionStatus::Completed, Some(&[]))));
        }

        // `compaction/start` is a durable fact about the *attempt*, so it is recorded
        // before any work: if the summarizer dies, the journal still shows what was
        // being attempted and which messages it started from.
        if let Some(recorder) = recorder {
            recorder
                .record(
                    "compaction/start",
                    &serde_json::json!({
                        "compactionId": compaction_id,
                        "inputMessages": conversation,
                    }),
                )
                .await?;
        }

        let outcome = self
            .compact_candidate(conversation, config, observed_tokens)
            .await;
        let metrics = match &outcome {
            Ok(replacement) => match replacement {
                Some(compacted) => metrics(CompactionStatus::Completed, Some(compacted)),
                None => {
                    // No compaction needed: the effective history is the input itself.
                    CompactionMetrics {
                        after_msg_count: Some(before_msg_count),
                        after_token_count: Some(before_token_count),
                        ..metrics(CompactionStatus::Completed, None)
                    }
                }
            },
            Err(_) => metrics(CompactionStatus::Failed, None),
        };

        if let Some(recorder) = recorder {
            recorder
                .record("compaction/end", &metrics.end_payload())
                .await?;
        }

        // One line per attempt: the status, the sizes and the wall-clock cost show whether a
        // stall was the summarizer (seconds) or the journal (immediate failures).
        tracing::info!(
            agent = %agent_id,
            session = %session_id,
            status = metrics.status.as_str(),
            before_messages = before_msg_count,
            before_tokens = before_token_count,
            after_messages = metrics.after_msg_count.unwrap_or(0),
            after_tokens = metrics.after_token_count.unwrap_or(0),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "conversation compaction finished"
        );

        Ok((outcome?, metrics))
    }

    /// The summarization itself, with no metric bookkeeping.
    async fn compact_candidate(
        &self,
        conversation: Vec<Msg>,
        config: &CompactionConfig,
        observed_tokens: Option<usize>,
    ) -> agentscope_core::error::Result<Option<Vec<Msg>>> {
        if conversation.is_empty() {
            return Ok(None);
        }
        // Trigger on the caller-observed size (provider-anchored when available) before
        // the non-LLM pre-pass. The pre-pass prunes only a local copy: if it were measured
        // first, a conversation with large historical tool results could look small and
        // skip compaction while the original history still reaches the provider.
        let observed = observed_tokens
            .filter(|tokens| *tokens > 0)
            .unwrap_or_else(|| calculate_token(&conversation));
        if !should_compact(&conversation, observed, config) {
            return Ok(None);
        }
        let messages = prune_tool_results(
            &truncate_args(&conversation, config.truncate_args.as_ref()),
            config.prune.as_ref(),
        );
        let total = calculate_token(&messages);
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
        // The template travels with the rendered messages, so its instructions must come out of
        // the same budget or the request overshoots the cap by the prompt body.
        let reserve = config
            .summary_prompt
            .chars()
            .count()
            .saturating_sub("{messages}".chars().count());
        let (formatted, truncated) = clamp_summary_input(prefix, formatted, reserve);
        if truncated {
            tracing::info!(
                prefix_messages = prefix.len(),
                max_tokens = SUMMARY_INPUT_MAX_TOKENS,
                "conversation compaction truncated the summary input"
            );
        }
        let prompt = config.summary_prompt.replace("{messages}", &formatted);
        let input = vec![Msg::user(prompt)];
        // Compaction summarizes through the budget-only auxiliary path: it cannot re-enter the
        // context middleware, so a summary can never trigger another compaction. A model
        // failure stays an error so the caller keeps the original history.
        let out = auxiliary_text(Arc::clone(model), input, None).await?;
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
                // Documents are rendered outside the preview budget on purpose: a long result
                // would otherwise truncate the reference away and the attachment would
                // disappear from the summary again.
                for doc in extract_tool_result_documents(tr) {
                    if !s.ends_with(char::is_whitespace) {
                        s.push(' ');
                    }
                    s.push_str(&render_document_reference(doc));
                }
                parts.push(s.trim().to_string());
            }
            ContentBlock::Document(doc) => parts.push(render_document_reference(doc)),
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

/// Collects the documents of a tool result so their references survive summarization.
fn extract_tool_result_documents(
    tr: &ToolResultBlock,
) -> impl Iterator<Item = &agentscope_core::message::DocumentBlock> {
    tr.output.iter().filter_map(|b| match b {
        ContentBlock::Document(doc) => Some(doc),
        _ => None,
    })
}

/// Caps the rendered summary body so the summarizer never receives a near-window request.
///
/// The previous summary message is kept even when it sits at the head of the prefix: it is the
/// only record of the turns dropped by the previous compaction, so trimming it would lose
/// history cumulatively. Returns the body and whether anything was dropped.
fn clamp_summary_input(prefix: &[Msg], formatted: String, reserve_chars: usize) -> (String, bool) {
    let budget_chars = ((SUMMARY_INPUT_MAX_TOKENS as f64 * CHARS_PER_TOKEN) as usize)
        .saturating_sub(reserve_chars);
    let total_chars = formatted.chars().count();
    if total_chars <= budget_chars {
        return (formatted, false);
    }
    let head = prefix
        .iter()
        .find(|msg| msg.name.as_deref() == Some(SUMMARY_MSG_NAME))
        .map(|msg| format!("[earlier summary] {}", render_message_for_summary(msg)))
        .unwrap_or_default();
    let head_chars = head.chars().count();
    let tail_budget = budget_chars.saturating_sub(head_chars + 40);
    let tail: String = formatted
        .chars()
        .skip(total_chars.saturating_sub(tail_budget))
        .collect();
    let mut capped = String::with_capacity(head_chars + tail.len() + 64);
    if !head.is_empty() {
        capped.push_str(&head);
        capped.push_str("\n\n[earlier turns omitted]\n");
    }
    capped.push_str(&tail);
    (capped, true)
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
        None => format!(
            "Here is a summary of the conversation to date:\n\n{}",
            summary
        ),
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    use agentscope_core::message::ChatUsage;
    use agentscope_core::model::{ChatResponse, GenerateOptions, ToolSchema};
    use futures::stream::{self, BoxStream};

    use super::*;

    /// Records the prompt it was asked to summarize.
    struct PromptRecordingModel {
        seen: Arc<Mutex<String>>,
        window: usize,
    }

    impl Model for PromptRecordingModel {
        fn stream(
            &self,
            messages: Vec<Msg>,
            _tools: Vec<ToolSchema>,
            _options: GenerateOptions,
        ) -> BoxStream<'_, crate::Result<ChatResponse>> {
            *self.seen.lock().unwrap() = messages
                .iter()
                .map(Msg::text_content)
                .collect::<Vec<_>>()
                .join("\n");
            Box::pin(stream::once(async {
                Ok(ChatResponse::new(
                    "r1",
                    vec![ContentBlock::Text(TextBlock::new("summary"))],
                    ChatUsage::default(),
                    None,
                    None,
                ))
            }))
        }

        fn model_name(&self) -> &str {
            "prompt-recorder"
        }

        fn context_window_size(&self) -> usize {
            self.window
        }
    }

    #[tokio::test]
    async fn the_summary_request_is_capped_and_keeps_the_previous_summary() {
        // ~240k tokens of history is exactly what compaction sees when the trigger fires.
        // Sending that as the summarizer prompt is what the provider answered with
        // `prefill_cap_refused` (429) or a 133s stall into the read timeout.
        let mut conversation = vec![build_summary_message("PREVIOUS SUMMARY MARKER", None)];
        for i in 0..300 {
            conversation.push(Msg::user(format!("turn {i}: {}", "x".repeat(2_000))));
        }
        let seen = Arc::new(Mutex::new(String::new()));
        let compactor = ConversationCompactor::with_model(Arc::new(PromptRecordingModel {
            seen: Arc::clone(&seen),
            window: 256_000,
        }));
        let config = CompactionConfig::builder()
            .trigger_messages(0)
            .trigger_tokens(1_000)
            .keep_messages(1)
            .build();

        let replacement = compactor
            .compact_if_needed_with_metrics_observed(
                conversation,
                &config,
                "agent",
                "session",
                None,
                None,
            )
            .await
            .unwrap()
            .0;
        assert!(replacement.is_some());

        let prompt = seen.lock().unwrap().clone();
        let tokens = estimate_text_tokens(&prompt);
        assert!(
            tokens <= SUMMARY_INPUT_MAX_TOKENS + 100,
            "summary prompt was {tokens} tokens"
        );
        assert!(
            prompt.contains("PREVIOUS SUMMARY MARKER"),
            "dropping the previous summary would lose history cumulatively"
        );
        assert!(
            prompt.contains("turn 298:"),
            "the newest summarized turn must survive"
        );
        assert!(
            !prompt.contains("turn 0:"),
            "the oldest turns must be dropped"
        );
    }

    #[test]
    fn compaction_triggers_below_the_compiler_limit() {
        // The compactor counts history only; the trigger must leave room for the system
        // prompt and tool schemas that also travel with the request.
        assert_eq!(trigger_for_window(256_000), 218_419);
        assert_eq!(trigger_for_window(100_000), 80_827);
        assert_eq!(trigger_for_window(0), 0);
        assert!(trigger_for_window(256_000) < 242_688);
    }

    #[tokio::test]
    async fn the_observed_provider_count_drives_the_trigger() {
        let compactor = ConversationCompactor::new(None);
        let config = CompactionConfig::builder()
            .trigger_messages(0)
            .trigger_tokens(10_000)
            .keep_messages(1)
            .build();

        // Locally small, but the provider says the real request is over the trigger.
        let replacement = compactor
            .compact_if_needed_with_metrics_observed(
                vec![Msg::user("a"), Msg::user("b")],
                &config,
                "agent",
                "session",
                None,
                Some(20_000),
            )
            .await
            .unwrap()
            .0;
        assert!(replacement.is_some());

        // Locally large, but the provider says the real request is below the trigger.
        let conversation: Vec<Msg> = (0..6)
            .map(|i| Msg::user(format!("turn {i}: {}", "x".repeat(5_000))))
            .collect();
        assert!(calculate_token(&conversation) > 10_000);
        let replacement = compactor
            .compact_if_needed_with_metrics_observed(
                conversation,
                &config,
                "agent",
                "session",
                None,
                Some(100),
            )
            .await
            .unwrap()
            .0;
        assert!(replacement.is_none());
    }

    #[tokio::test]
    async fn the_trigger_uses_the_unpruned_conversation_size() {
        // Large historical tool results make the real history expensive; the pre-pass
        // prunes only its local copy. If the trigger were computed after pruning, the
        // oversized state would never compact.
        let mut messages = Vec::new();
        for i in 0..8 {
            let id = format!("call-{i}");
            messages.push(Msg::assistant_blocks(vec![ContentBlock::ToolUse(
                ToolUseBlock::new(id.clone(), "shell", Default::default()),
            )]));
            messages.push(Msg::tool_blocks(vec![ToolResultBlock::new(
                &id,
                "shell",
                vec![ContentBlock::Text(TextBlock::new("x".repeat(5_000)))],
            )]));
        }
        let prune = PruneConfig {
            protect_tokens: 0,
            minimum_tokens: 0,
            max_output_chars: 100,
            excluded_tools: HashSet::new(),
        };
        let config = CompactionConfig::builder()
            .trigger_messages(0)
            .trigger_tokens(1_000)
            .keep_messages(1)
            .prune(Some(prune))
            .build();
        let compactor = ConversationCompactor::new(None);
        let replacement = compactor
            .compact_if_needed(messages, &config, "agent", "session")
            .await
            .unwrap();
        assert!(replacement.is_some(), "pruning must not hide the real size");
    }

    #[test]
    fn the_summary_keeps_document_references_outside_the_preview_budget() {
        use agentscope_core::message::{DocumentBlock, Source, URLSource};

        let document = DocumentBlock::builder()
            .source(Source::Url(URLSource::new(
                "https://example.com/report.pdf",
            )))
            .name("report.pdf")
            .mime_type("application/pdf")
            .build();
        let messages = vec![
            Msg::user_blocks(vec![ContentBlock::Document(document.clone())]),
            Msg::tool_blocks(vec![ToolResultBlock::new(
                "call-1",
                "read_file",
                vec![
                    ContentBlock::Text(TextBlock::new("x".repeat(2_000))),
                    ContentBlock::Document(document),
                ],
            )]),
        ];

        let rendered = format_messages_for_summary(&messages);

        // The preview is capped at 500 chars; the reference must survive it anyway.
        assert_eq!(rendered.matches("[document:").count(), 2, "{rendered}");
        assert!(
            rendered.contains(
                "[document: report.pdf, application/pdf, url: https://example.com/report.pdf]"
            ),
            "{rendered}"
        );
    }
}
