//! End-to-end tests for the quantified compaction facts (`compaction/start` /
//! `compaction/end`).
//!
//! The compromise this replaces reported the metrics in-band only, because Rust had no
//! durable session log. It does now, so the counts must be real session events that a
//! reader can recover after the fact — not a `Custom` payload that only a live stream sees.
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::Msg;
use agentscope_core::session::{
    InMemorySessionLogStore, SessionKey, SessionLog, SessionLogStore, SessionRecorder,
};
use agentscope_harness::{
    calculate_token, CompactionConfig, CompactionStatus, ConversationCompactor,
};

fn messages(count: usize) -> Vec<Msg> {
    (0..count)
        .map(|i| Msg::user(format!("message number {i}")))
        .collect()
}

/// A compactor whose summarization fails, to exercise the failed path.
struct FailingModel;

#[async_trait::async_trait]
impl agentscope_core::model::Model for FailingModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<agentscope_core::model::ToolSchema>,
        _options: agentscope_core::model::GenerateOptions,
    ) -> futures::stream::BoxStream<
        '_,
        agentscope_core::error::Result<agentscope_core::model::ChatResponse>,
    > {
        Box::pin(futures::stream::once(async {
            Err(agentscope_core::error::Error::Model(
                "summary failed".into(),
            ))
        }))
    }

    fn model_name(&self) -> &str {
        "failing"
    }
}

/// Opens a recorder on a fresh in-memory journal, returning it with the log handle.
async fn recorder() -> (Arc<SessionRecorder>, Arc<dyn SessionLog>) {
    let store = InMemorySessionLogStore::new();
    let context = RuntimeContext::new().with_user_id("u");
    let key = SessionKey::new(Some("u"), "agent", "s1").unwrap();
    let log = store.open(&key, &context).await.unwrap();
    let recorder = Arc::new(
        SessionRecorder::open(Arc::clone(&log), Some("turn"))
            .await
            .unwrap(),
    );
    (recorder, log)
}

/// Reads every committed `compaction/*` event as `(type, payload)`.
async fn compaction_events(log: &Arc<dyn SessionLog>) -> Vec<(String, serde_json::Value)> {
    log.read_after(0, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.event_type.starts_with("compaction/"))
        .map(|e| (e.event_type.clone(), e.data().unwrap()))
        .collect()
}

#[tokio::test]
async fn completed_compaction_records_before_and_after_counts() {
    let (recorder, log) = recorder().await;
    let input = messages(60);
    let compactor = ConversationCompactor::new(None); // no model -> fallback summary
    let cfg = CompactionConfig::default(); // trigger 50 messages, keep a 20-message tail

    let (replacement, metrics) = compactor
        .compact_if_needed_with_metrics(input.clone(), &cfg, "agent", "s1", Some(&recorder))
        .await
        .unwrap();
    let compacted = replacement.expect("compaction must trigger");

    assert_eq!(metrics.status, CompactionStatus::Completed);
    assert_eq!(metrics.before_msg_count, input.len());
    assert_eq!(metrics.before_token_count, calculate_token(&input));
    assert_eq!(metrics.after_msg_count, Some(compacted.len()));
    assert_eq!(metrics.after_token_count, Some(calculate_token(&compacted)));

    let events = compaction_events(&log).await;
    let start = events
        .iter()
        .find(|(kind, _)| kind == "compaction/start")
        .map(|(_, data)| data)
        .expect("compaction/start must be a durable fact");
    assert_eq!(
        start["compactionId"],
        serde_json::json!(metrics.compaction_id)
    );
    assert_eq!(
        start["inputMessages"].as_array().map(|a| a.len()),
        Some(input.len()),
        "the start event must record what was being attempted"
    );

    let end = events
        .iter()
        .find(|(kind, _)| kind == "compaction/end")
        .map(|(_, data)| data)
        .expect("compaction/end must be a durable fact");
    assert_eq!(end["status"], serde_json::json!("completed"));
    assert_eq!(
        end["compactionId"],
        serde_json::json!(metrics.compaction_id)
    );
    assert_eq!(end["beforeMsgCount"], serde_json::json!(input.len()));
    assert_eq!(
        end["beforeTokenCount"],
        serde_json::json!(calculate_token(&input))
    );
    assert_eq!(end["afterMsgCount"], serde_json::json!(compacted.len()));
    assert_eq!(
        end["afterTokenCount"],
        serde_json::json!(calculate_token(&compacted))
    );
}

#[tokio::test]
async fn failed_compaction_omits_the_after_counts() {
    let (recorder, log) = recorder().await;
    let input = messages(60);
    let compactor = ConversationCompactor::with_model(Arc::new(FailingModel));
    let cfg = CompactionConfig::builder()
        .trigger_messages(50)
        .trigger_tokens(0)
        .build();

    let result = compactor
        .compact_if_needed_with_metrics(input.clone(), &cfg, "agent", "s1", Some(&recorder))
        .await;
    assert!(result.is_err(), "a failed summary must stay an error");

    let events = compaction_events(&log).await;
    let end = events
        .iter()
        .find(|(kind, _)| kind == "compaction/end")
        .map(|(_, data)| data)
        .expect("a failed compaction must still be recorded");
    assert_eq!(end["status"], serde_json::json!("failed"));
    assert_eq!(end["beforeMsgCount"], serde_json::json!(input.len()));
    assert_eq!(
        end["beforeTokenCount"],
        serde_json::json!(calculate_token(&input))
    );
    // A failed run has no meaningful "after": the keys must be absent, not zero.
    assert!(end.get("afterMsgCount").is_none(), "got {end}");
    assert!(end.get("afterTokenCount").is_none(), "got {end}");
}

#[tokio::test]
async fn below_trigger_records_the_input_counts_and_no_compaction() {
    let (recorder, log) = recorder().await;
    let input = messages(2);
    let compactor = ConversationCompactor::new(None);
    let cfg = CompactionConfig::default();

    let (replacement, metrics) = compactor
        .compact_if_needed_with_metrics(input.clone(), &cfg, "agent", "s1", Some(&recorder))
        .await
        .unwrap();
    assert!(
        replacement.is_none(),
        "below the trigger nothing is rewritten"
    );
    // The effective history is still the input, so before == after.
    assert_eq!(metrics.after_msg_count, Some(input.len()));
    assert_eq!(metrics.after_token_count, Some(calculate_token(&input)));

    let end = compaction_events(&log)
        .await
        .into_iter()
        .find(|(kind, _)| kind == "compaction/end")
        .map(|(_, data)| data)
        .expect("compaction/end must exist");
    assert_eq!(end["afterMsgCount"], serde_json::json!(input.len()));
}

#[tokio::test]
async fn without_a_recorder_the_metrics_are_still_reported() {
    // The legacy `compact_if_needed` path has no recorder; callers that only want the counts
    // must not be forced to open a journal.
    let compactor = ConversationCompactor::new(None);
    let cfg = CompactionConfig::default();
    let (replacement, metrics) = compactor
        .compact_if_needed_with_metrics(messages(60), &cfg, "agent", "s1", None)
        .await
        .unwrap();
    assert!(replacement.is_some());
    assert!(metrics.after_msg_count.is_some());
    assert!(metrics.after_token_count.is_some());
}
