//! `/compact` is a host-level prompt shortcut: the session compacts the live conversation
//! itself instead of starting a model turn for it.
//!
//! The tests assert the two properties that make the shortcut worth having: it compacts
//! immediately (no turn admission, no client-visible turn), and the only model call it pays
//! is the summarization itself.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::v1::schema::{
    ClientCapabilities, ClientSessionCapabilities, CompactionCapabilities, CompactionStatus,
    ContentBlock, SessionId, SessionNotification, SessionUpdate, StopReason, TextContent,
};
use agentscope_core::message::{ContentBlock as CoreBlock, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_extensions_acp::{AcpAgentSession, ClientSupport};
use agentscope_harness::agent::{CompactionConfig, HarnessAgent};

/// Answers every call with a short text; the counter records how many were made.
struct CountingModel {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Model for CountingModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(futures::stream::once(async {
            Ok(ChatResponse::builder()
                .content(vec![CoreBlock::Text(TextBlock::new("summary"))])
                .build())
        }))
    }

    fn model_name(&self) -> &str {
        "counting"
    }
}

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir =
        std::env::temp_dir().join(format!("acp-compact-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    dir
}

/// Builds a session over an agent whose history already has `turns` messages.
fn build_session(
    tag: &str,
    model: Arc<dyn Model>,
    turns: usize,
    compaction: bool,
) -> AcpAgentSession {
    let dir = scratch_dir(tag);
    let mut builder = HarnessAgent::builder()
        .name("acp-compact")
        .model(model)
        .session_id(format!("s-{tag}"))
        .cwd(dir.clone())
        .workspace(dir.join(".agentscope").join("workspace"))
        .disable_memory_hooks()
        .max_iters(3);
    if compaction {
        builder = builder.compaction(
            CompactionConfig::builder()
                .trigger_tokens(1_000_000)
                .keep_messages(2)
                .build(),
        );
    }
    let agent = builder.build();
    {
        let state = agent.agent().expect("embedded agent").state_handle();
        let mut guard = state.lock().unwrap();
        for i in 0..turns {
            guard.context_mut().push(Msg::user(format!("turn {i}")));
        }
    }
    AcpAgentSession::new(
        SessionId::new(format!("s-{tag}")),
        dir,
        agent,
        Vec::new(),
        Vec::new(),
    )
}

fn prompt(text: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text(TextContent::new(text.to_string()))]
}

/// Advertises `session.compaction`, the capability that gates the compaction updates.
fn advertise_compaction(session: &mut AcpAgentSession) {
    let support = ClientSupport::new();
    support.set_capabilities(
        ClientCapabilities::new()
            .session(ClientSessionCapabilities::new().compaction(CompactionCapabilities::new())),
    );
    session.set_client_support(support);
}

/// One captured `session/update`, reduced to what the assertions need.
fn update_kind(notification: &SessionNotification) -> (&'static str, Option<String>) {
    match &notification.update {
        SessionUpdate::CompactionUpdate(update) => match update.status {
            CompactionStatus::InProgress => ("compaction:in_progress", None),
            CompactionStatus::Completed => ("compaction:completed", None),
            _ => ("compaction:other", None),
        },
        SessionUpdate::CompactionSummaryChunk(_) => ("compaction:summary", None),
        SessionUpdate::AgentMessageChunk(chunk) => {
            let text = match &chunk.content {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            };
            ("message", text)
        }
        _ => ("other", None),
    }
}

#[tokio::test]
async fn the_compact_shortcut_compacts_without_a_turn() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut session = build_session(
        "runs",
        Arc::new(CountingModel {
            calls: Arc::clone(&calls),
        }),
        6,
        true,
    );
    advertise_compaction(&mut session);
    let seen: Arc<Mutex<Vec<(&'static str, Option<String>)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);

    let stop = session
        .run_prompt(prompt("/compact"), move |notification| {
            sink.lock().unwrap().push(update_kind(&notification));
        })
        .await;

    assert_eq!(stop, StopReason::EndTurn);
    let seen = seen.lock().unwrap();
    let kinds: Vec<&str> = seen.iter().map(|(kind, _)| *kind).collect();
    assert_eq!(
        kinds,
        vec![
            "compaction:in_progress",
            "compaction:summary",
            "compaction:completed",
            "message",
        ],
        "the client sees the capability-gated compaction updates, then the report"
    );
    let text: String = seen
        .iter()
        .filter_map(|(_, text)| text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Context compacted"), "{text}");
    assert!(text.contains("6 → 3"), "summary plus the kept tail: {text}");
    // Only the summarizer ran: a real turn would have asked the model for an answer too.
    assert_eq!(calls.load(Ordering::SeqCst), 1, "{text}");
}

#[tokio::test]
async fn the_compact_shortcut_reports_a_disabled_compactor() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut session = build_session(
        "disabled",
        Arc::new(CountingModel {
            calls: Arc::clone(&calls),
        }),
        6,
        false,
    );
    advertise_compaction(&mut session);
    let seen: Arc<Mutex<Vec<(&'static str, Option<String>)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);

    let stop = session
        .run_prompt(prompt("/compact"), move |notification| {
            sink.lock().unwrap().push(update_kind(&notification));
        })
        .await;

    assert_eq!(stop, StopReason::EndTurn);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "one report, no compaction updates");
    let text = seen[0].1.clone().unwrap_or_default();
    assert!(text.contains("Nothing to compact"), "{text}");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no model call at all");
}
