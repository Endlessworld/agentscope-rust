//! Tests for the harness memory module: compaction (token counter / config /
//! pre-pass pipeline / summary building) and session (entries / tree / freshness).

use std::sync::Arc;

use agentscope_core::message::{ContentBlock, Msg, MsgRole, TextBlock, ToolResultBlock, ToolUseBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_harness::acp::ACP_CONTEXT_WINDOW_TOKENS;
use agentscope_harness::{
    build_summary_message, calculate_token, filter_summary_messages, format_messages_for_summary,
    prune_tool_results, truncate_args, CompactionConfig, ConversationCompactor,
    PruneConfig, SUMMARY_MSG_NAME, SessionEntry, SessionFreshnessEvaluator, SessionTree,
    TruncateArgsConfig,
};
use chrono::{Duration as ChronoDuration, Local, Timelike};

/// A summarization model whose stream always fails.
struct FailingModel;

#[async_trait::async_trait]
impl Model for FailingModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        let items: Vec<agentscope_core::error::Result<ChatResponse>> =
            vec![Err(agentscope_core::error::Error::Agent("model down".into()))];
        Box::pin(futures::stream::iter(items))
    }
    fn model_name(&self) -> &str {
        "failing"
    }
}

fn assistant_with_tool_use(name: &str, id: &str, arg_key: &str, arg_value: String) -> Msg {
    let mut input = agentscope_core::message::JsonMap::new();
    input.insert(arg_key.to_string(), serde_json::Value::String(arg_value));
    Msg::builder()
        .role(MsgRole::Assistant)
        .content(vec![ContentBlock::ToolUse(ToolUseBlock::new(id, name, input))])
        .build()
}

fn tool_result_msg(id: &str, name: &str, text: String) -> Msg {
    Msg::builder()
        .role(MsgRole::Tool)
        .content(vec![ContentBlock::ToolResult(ToolResultBlock::new(
            id,
            name,
            vec![ContentBlock::Text(TextBlock::new(text))],
        ))])
        .build()
}

#[test]
fn token_counter_estimates_text_messages() {
    // "hello world" = 11 chars -> ceil(11 / 2.5) = 5 text tokens, plus role + overhead.
    let msg = Msg::user("hello world");
    let tokens = calculate_token(&[msg]);
    assert_eq!(tokens, 12);
}

#[test]
fn token_counter_handles_empty_and_tool_blocks() {
    assert_eq!(calculate_token(&[]), 0);
    let tu = assistant_with_tool_use("write_file", "call-1", "content", "x".repeat(100));
    let tr = tool_result_msg("call-1", "write_file", "y".repeat(200));
    let total = calculate_token(&[tu, tr]);
    assert!(total > 30, "expected structural overhead, got {}", total);
}


#[test]
fn token_counter_counts_thinking_content() {
    use agentscope_core::message::ThinkingBlock;

    // direct thinking block in an assistant message
    let thinking = "x".repeat(1_000);
    let msg = Msg::builder()
        .role(MsgRole::Assistant)
        .content(vec![ContentBlock::Thinking(ThinkingBlock::new(thinking.clone()))])
        .build();
    let direct = calculate_token(&[msg]);
    // 1000 chars / 2.5 = 400, plus role + message overhead
    assert_eq!(direct, 400 + 5 + 4, "direct thinking counted, got {direct}");

    // thinking nested inside a ToolResultBlock
    let msg = Msg::builder()
        .role(MsgRole::Tool)
        .content(vec![ContentBlock::ToolResult(ToolResultBlock::new(
            "call-1",
            "search",
            vec![ContentBlock::Thinking(ThinkingBlock::new(thinking))],
        ))])
        .build();
    let nested = calculate_token(&[msg]);
    // 400 thinking + tool result overhead (8) + name/id + role + message overhead
    assert!(nested > 400, "nested thinking counted, got {nested}");
}
#[test]
fn compaction_config_defaults_and_builder() {
    let cfg = CompactionConfig::default();
    assert_eq!(cfg.trigger_messages, 50);
    assert_eq!(cfg.trigger_tokens, 0);
    assert_eq!(cfg.keep_tokens, -1);
    assert_eq!(cfg.keep_messages, 20);
    assert!(cfg.flush_before_compact);
    assert!(cfg.offload_before_compact);
    assert!(cfg.prune.is_some());
    assert!(cfg.truncate_args.is_none());

    let built = CompactionConfig::builder().trigger_tokens(10_000).keep_tokens(2_000).build();
    assert_eq!(built.trigger_tokens, 10_000);
    assert_eq!(built.keep_tokens, 2_000);

    let eff = built.with_effective(100_000, 5_000);
    assert_eq!(eff.trigger_tokens, 100_000);
    assert_eq!(eff.keep_tokens, 5_000);
}

#[test]
fn build_summary_message_is_a_flagged_user_msg() {
    let m = build_summary_message("key insight", None);
    assert_eq!(m.role, MsgRole::User);
    assert_eq!(m.name.as_deref(), Some(SUMMARY_MSG_NAME));
    assert!(m.text_content().contains("key insight"));
    // Deterministic id: same content yields same id.
    let m2 = build_summary_message("key insight", None);
    assert_eq!(m.id, m2.id);
    assert!(m.id.starts_with(SUMMARY_MSG_NAME));
}

#[test]
fn filter_summary_messages_removes_summaries() {
    let summary = build_summary_message("x", None);
    let normal = Msg::user("hello");
    let kept = filter_summary_messages(&[summary, normal]);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].role, MsgRole::User);
    assert!(kept[0].name.is_none());
}

#[test]
fn format_messages_for_summary_renders_roles_and_blocks() {
    let msgs = vec![Msg::user("hi there"), assistant_with_tool_use("ls", "c1", "path", "/tmp".into())];
    let out = format_messages_for_summary(&msgs);
    assert!(out.contains("Human: hi there"));
    assert!(out.contains("[tool_call: ls]"));
}

#[test]
fn truncate_args_clips_oversized_tool_arguments() {
    let big = assistant_with_tool_use("write_file", "c1", "content", "A".repeat(5_000));
    // keep_messages=1 keeps the last message untouched; the earlier tool-use is truncated.
    let cfg = TruncateArgsConfig::builder()
        .trigger_messages(2)
        .keep_messages(1)
        .max_arg_length(100)
        .build();
    let out = truncate_args(&[big.clone(), Msg::user("recent")], Some(&cfg));
    assert_eq!(out.len(), 2);
    if let ContentBlock::ToolUse(tu) = &out[0].content[0] {
        let val = tu.input.get("content").and_then(|v| v.as_str()).unwrap();
        assert!(val.len() < 100, "expected truncated arg, got len {}", val.len());
        assert!(val.ends_with("...(argument truncated)"));
    } else {
        panic!("expected tool_use block");
    }
}

#[test]
fn truncate_args_is_noop_without_trigger() {
    let msg = Msg::user("short");
    let cfg = TruncateArgsConfig::builder().trigger_messages(100).build();
    let out = truncate_args(std::slice::from_ref(&msg), Some(&cfg));
    assert_eq!(out.len(), 1);
    assert_eq!(out[0], msg);
}

#[test]
fn prune_tool_results_trims_old_oversized_output() {
    let mut prune = PruneConfig::defaults();
    prune.protect_tokens = 0; // no protected budget -> everything prunable
    prune.minimum_tokens = 1;
    prune.max_output_chars = 100;
    let old = tool_result_msg("c-old", "shell_execute", "O".repeat(10_000));
    let out = prune_tool_results(&[old], Some(&prune));
    if let ContentBlock::ToolResult(tr) = &out[0].content[0] {
        let text = tr.text_content();
        assert!(text.chars().count() < 10_000, "expected pruned preview");
        assert!(text.contains("chars pruned"));
    } else {
        panic!("expected tool_result block");
    }
}

#[tokio::test]
async fn compact_if_needed_returns_summary_plus_tail() {
    let mut messages = Vec::new();
    for i in 0..60 {
        messages.push(Msg::user(format!("message number {}", i)));
    }
    let compactor = ConversationCompactor::new(None); // no model -> fallback summary
    let cfg = CompactionConfig::default(); // trigger 50 msgs, keep 20 tail
    let result = compactor
        .compact_if_needed(messages, &cfg, "agent", "session")
        .await
        .unwrap();
    let compacted = result.expect("compaction should trigger");
    assert_eq!(compacted.len(), 21); // 1 summary + 20 tail
    assert_eq!(compacted[0].name.as_deref(), Some(SUMMARY_MSG_NAME));
    assert_eq!(compacted[0].role, MsgRole::User);
    assert_eq!(compacted[1].role, MsgRole::User);
}

#[tokio::test]
async fn compact_if_needed_is_noop_below_trigger() {
    let messages = vec![Msg::user("a"), Msg::user("b")];
    let compactor = ConversationCompactor::new(None);
    let cfg = CompactionConfig::default(); // trigger 50
    let result = compactor.compact_if_needed(messages, &cfg, "a", "s").await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn compact_if_needed_triggers_only_at_acp_token_window() {
    let cfg = CompactionConfig::builder()
        .trigger_messages(0)
        .trigger_tokens(ACP_CONTEXT_WINDOW_TOKENS)
        .build();
    let chunk = Msg::user("x".repeat(10_000));
    let per_chunk = calculate_token(std::slice::from_ref(&chunk)).max(1);
    let below = ACP_CONTEXT_WINDOW_TOKENS / per_chunk - 1;
    let compactor = ConversationCompactor::new(None);

    let under: Vec<Msg> = vec![chunk.clone(); below];
    assert!(calculate_token(&under) < ACP_CONTEXT_WINDOW_TOKENS);
    let result = compactor.compact_if_needed(under, &cfg, "a", "s").await.unwrap();
    assert!(result.is_none(), "must not compact below the 256k window");

    let over: Vec<Msg> = vec![chunk; below + 2];
    assert!(calculate_token(&over) >= ACP_CONTEXT_WINDOW_TOKENS);
    let result = compactor.compact_if_needed(over, &cfg, "a", "s").await.unwrap();
    assert!(result.is_some(), "must compact once the context reaches 256k");
}

#[tokio::test]
async fn compaction_failure_keeps_full_history() {
    let messages: Vec<Msg> = (0..60)
        .map(|i| Msg::user(format!("message number {}", i)))
        .collect();
    let compactor = ConversationCompactor::with_model(Arc::new(FailingModel));
    let cfg = CompactionConfig::builder()
        .trigger_messages(50)
        .trigger_tokens(0)
        .build();
    let result = compactor.compact_if_needed(messages, &cfg, "agent", "session").await;
    assert!(result.is_err(), "a failed summary must surface as an error, not a placeholder");
}

#[test]
fn session_entry_serde_roundtrip_with_type_tag() {
    let e = SessionEntry::message(None, "user", "hello");
    let json = serde_json::to_string(&e).unwrap();
    assert!(json.contains("\"type\":\"message\""), "got {}", json);
    assert!(json.contains("\"role\":\"user\""));
    let back: SessionEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(back, e);

    let s = SessionEntry::summary(Some("parent"), "the summary");
    let json = serde_json::to_string(&s).unwrap();
    assert!(json.contains("\"type\":\"summary\""));
    assert!(json.contains("\"format\":\"structured\""));
    let back: SessionEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(back, s);
}

#[test]
fn session_tree_build_context_respects_compaction() {
    let mut tree = SessionTree::new();
    let m1 = tree.append(SessionEntry::message(None, "user", "a"));
    let m2 = tree.append(SessionEntry::message(Some(&m1), "assistant", "b"));
    let m3 = tree.append(SessionEntry::message(Some(&m2), "user", "c"));
    // No compaction yet -> all messages visible.
    assert_eq!(tree.build_context().len(), 3);

    let s = tree.append(SessionEntry::summary(Some(&m2), "summary text"));
    tree.append(SessionEntry::compaction(Some(&m2), m2.clone(), s.clone()));
    let ctx = tree.build_context();
    assert_eq!(ctx.len(), 3); // summary + m2 + m3
    assert_eq!(ctx[0].id(), s);
    assert_eq!(ctx[1].id(), m2);
    assert_eq!(ctx[2].id(), m3);
}

#[test]
fn session_tree_jsonl_roundtrip() {
    let mut tree = SessionTree::new();
    let m1 = tree.append(SessionEntry::message(None, "user", "a"));
    tree.append(SessionEntry::message(Some(&m1), "assistant", "b"));
    tree.append(SessionEntry::summary(Some(&m1), "s"));
    assert_eq!(tree.size(), 3);

    let jsonl = tree.to_jsonl();
    assert_eq!(jsonl.lines().count(), 3);

    let mut loaded = SessionTree::new();
    let added = loaded.load_jsonl(&jsonl);
    assert_eq!(added, 3);
    assert_eq!(loaded.size(), 3);
    assert_eq!(loaded.get_all_entries(), tree.get_all_entries());
}

#[test]
fn freshness_evaluator_handles_idle_and_reset() {
    let ev = SessionFreshnessEvaluator::new(4, Some(ChronoDuration::hours(2)));
    assert!(ev.is_stale(None));
    assert!(!ev.is_stale(Some(Local::now())));
    let old = Local::now() - ChronoDuration::hours(5);
    assert!(ev.is_stale(Some(old)));
}

#[test]
fn freshness_evaluator_daily_reset_flag() {
    // Daily reset at 23:00; last activity before today 23:00 and now after -> stale.
    let ev = SessionFreshnessEvaluator::new(23, None);
    let now = Local::now();
    let last = now.date_naive().and_hms_opt(10, 0, 0).unwrap();
    // If it's currently before 23:00, the daily reset has not passed yet -> not stale.
    let last_activity = last.and_local_timezone(Local).single();
    let is_after_reset = now.hour() >= 23;
    assert_eq!(ev.is_stale(last_activity), is_after_reset);
}
