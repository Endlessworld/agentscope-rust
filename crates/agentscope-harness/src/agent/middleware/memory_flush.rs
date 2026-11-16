//! Middleware that triggers memory flush and message offload at the end of each agent call,
//! mirroring `io.agentscope.harness.agent.middleware.MemoryFlushMiddleware`.
//!
//! Runs in `on_agent`'s tail so long-term memories are extracted and persisted after every
//! call, even when conversation compaction was not triggered during that call. Flush is gated
//! by a `MemoryConfig.FlushTrigger`; message offload runs on every call so the session JSONL
//! stays complete.

use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::Msg;
use agentscope_core::middleware::{AgentInput, EventStream, Middleware, Next};
use agentscope_core::model::Model;
use futures::StreamExt;

use crate::agent::isolation_scope::IsolationScope;
use crate::agent::memory::compaction::{CompactionConfig, ConversationCompactor};
use crate::agent::memory::{FlushMode, FlushTrigger, MemoryFlushManager};
use crate::filesystem::Filesystem;

/// Process-wide per-isolation-key flush timestamps (millis since epoch). Static so the
/// throttle window survives across rebuilds of the middleware, mirroring the Java
/// `SHARED_LAST_FLUSH_AT` map.
static SHARED_LAST_FLUSH_AT: std::sync::Mutex<Vec<(String, u64)>> =
    std::sync::Mutex::new(Vec::new());

/// Returns the current time in millis for throttle bookkeeping.
fn now_millis() -> u64 {
    chrono::Utc::now().timestamp_millis() as u64
}

/// Per-session serial queues for the background flush/offload tail.
///
/// The tail used to be chained onto the agent stream, so every subscriber waited for one
/// extraction round-trip before `onComplete`. It now runs in its own task; the queue keeps
/// two tails of the same session from extracting concurrently, which would both read the
/// same "today's ledger so far" and duplicate bullets.
static FLUSH_QUEUES: std::sync::Mutex<Vec<(String, Arc<tokio::sync::Mutex<()>>)>> =
    std::sync::Mutex::new(Vec::new());

/// The serial queue for `key`, created on first use.
fn flush_queue(key: &str) -> Arc<tokio::sync::Mutex<()>> {
    let mut queues = FLUSH_QUEUES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((_, queue)) = queues.iter().find(|(candidate, _)| candidate == key) {
        return Arc::clone(queue);
    }
    let queue = Arc::new(tokio::sync::Mutex::new(()));
    queues.push((key.to_string(), Arc::clone(&queue)));
    queue
}

/// Pre-compaction memory work (`CompactionConfig::flush_before_compact` /
/// `offload_before_compact`).
struct PreCompactionMemory {
    compactor: Arc<ConversationCompactor>,
    config: CompactionConfig,
}

/// Middleware that triggers memory flush + offload after each agent call.
pub struct MemoryFlushMiddleware {
    fs: Arc<dyn Filesystem>,
    model: Arc<dyn Model>,
    flush_prompt: String,
    flush_trigger: FlushTrigger,
    isolation_scope: IsolationScope,
    /// Compaction settings read before the compaction middleware rewrites the history.
    pre_compaction: Option<PreCompactionMemory>,
}

impl MemoryFlushMiddleware {
    /// Creates a middleware with the default flush prompt, ALWAYS trigger, and USER isolation.
    pub fn new(fs: Arc<dyn Filesystem>, model: Arc<dyn Model>) -> Self {
        Self::new_with_config(
            fs,
            model,
            crate::agent::memory::flush::DEFAULT_FLUSH_PROMPT.to_string(),
            FlushTrigger::always(),
            IsolationScope::User,
        )
    }

    /// Creates a middleware with a custom flush prompt, trigger, and isolation scope.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_config(
        fs: Arc<dyn Filesystem>,
        model: Arc<dyn Model>,
        flush_prompt: String,
        flush_trigger: FlushTrigger,
        isolation_scope: IsolationScope,
    ) -> Self {
        Self {
            fs,
            model,
            flush_prompt,
            flush_trigger,
            isolation_scope,
            pre_compaction: None,
        }
    }

    /// Honors [`CompactionConfig::flush_before_compact`] and
    /// [`CompactionConfig::offload_before_compact`].
    ///
    /// This middleware runs before the conversation-compaction middleware in the chain, so
    /// it is the last point where the history that is about to be summarized away is still
    /// intact. Without it the two switches were configuration with no reader.
    pub fn with_pre_compaction(
        mut self,
        compactor: Arc<ConversationCompactor>,
        config: CompactionConfig,
    ) -> Self {
        self.pre_compaction = Some(PreCompactionMemory { compactor, config });
        self
    }
}

impl MemoryFlushMiddleware {
    /// Derives the per-call identity portion of the timer key from the isolation scope and
    /// context, mirroring `MemoryFlushMiddleware#timerKeyFor`.
    fn timer_key_for(&self, ctx: &RuntimeContext) -> String {
        match self.isolation_scope {
            IsolationScope::User => ctx
                .user_id()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_default(),
            IsolationScope::Session => ctx
                .session_id()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_default(),
            IsolationScope::Agent | IsolationScope::Global => String::new(),
        }
    }

    /// Whether this call should trigger a flush, applying the configured trigger policy.
    fn should_flush_now(&self, ctx: &RuntimeContext) -> bool {
        match self.flush_trigger.mode() {
            FlushMode::Always => true,
            FlushMode::Never => false,
            FlushMode::Throttled => {
                let key = format!("{:?}:{}", self.isolation_scope, self.timer_key_for(ctx));
                let now = now_millis();
                let min_gap_ms = self.flush_trigger.min_gap().num_milliseconds().max(0) as u64;
                let mut map = SHARED_LAST_FLUSH_AT
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let Some(last) = map.iter().find(|(k, _)| *k == key).map(|(_, t)| *t) else {
                    // Cold start: no timestamp is not the epoch. Treating it as "never"
                    // made the very first call flush unconditionally, so the very first
                    // turn of every session paid for an extraction round-trip.
                    map.push((key, now));
                    return false;
                };
                if now.saturating_sub(last) < min_gap_ms {
                    return false;
                }
                if let Some(slot) = map.iter_mut().find(|(k, _)| *k == key) {
                    slot.1 = now;
                }
                true
            }
        }
    }
}
impl Middleware for MemoryFlushMiddleware {
    fn on_agent(
        &self,
        ctx: &RuntimeContext,
        input: AgentInput,
        next: Next<AgentInput>,
    ) -> EventStream {
        let ctx = ctx.clone();
        let fs = self.fs.clone();
        let model = self.model.clone();
        let flush_prompt = self.flush_prompt.clone();
        let should_flush = self.should_flush_now(&ctx);
        let messages = input.messages.clone();
        // The agent *id*, not the display name: the filesystem routes (and the task/offload
        // paths) are registered under `agents/<agent_id>/...`, so a name that differs from the
        // id never matched a route and the offload stayed on replica-local disk (#2298).
        let agent_id = ctx
            .agent_id()
            .map(str::to_string)
            .filter(|id| !id.trim().is_empty())
            .or_else(|| ctx.agent_name.clone())
            .unwrap_or_default();
        let session_id = ctx
            .session_id()
            .map(str::to_string)
            .unwrap_or_else(|| "default".to_string());
        let pre_compaction = self
            .pre_compaction
            .as_ref()
            .map(|pre| (Arc::clone(&pre.compactor), pre.config.clone()));

        // The pre-compaction steps must run before `next` (which holds the compaction
        // middleware) rewrites the history away; the post-call flush/offload runs in its
        // own task so the event stream completes without waiting for it.
        let fut = async move {
            if let Some((compactor, config)) = pre_compaction {
                pre_compaction_memory(
                    &fs,
                    &model,
                    &flush_prompt,
                    &ctx,
                    &messages,
                    &compactor,
                    &config,
                    &agent_id,
                    &session_id,
                )
                .await;
            }

            let downstream = next(input);
            let queue = flush_queue(&session_id);
            let tail = async move {
                if messages.is_empty() {
                    return;
                }
                let manager = MemoryFlushManager::new_with_prompt(
                    fs.clone(),
                    model.clone(),
                    Some(flush_prompt.clone()),
                );
                if should_flush {
                    // Best-effort: a flush failure must never break the agent call.
                    if let Err(error) = manager.flush_memories(&ctx, &messages).await {
                        tracing::warn!(%error, "long-term memory flush failed");
                    }
                }
                // Message offload runs on every call regardless of the flush trigger.
                if let Err(error) =
                    offload_messages(&fs, &manager, &ctx, &messages, &agent_id, &session_id).await
                {
                    tracing::warn!(%error, "session message offload failed");
                }
            };

            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    // Start the tail only once this turn's stream is finished: the flush issues
                    // its own extraction call on the same model, so running it concurrently with
                    // the reasoning requests both scrambles the provider traffic and lets a slow
                    // backend overlap the next turn. Spawning (instead of awaiting) keeps the
                    // completion event immediate.
                    //
                    // One tail per session at a time: two concurrent extractions would both
                    // read the same "today's ledger so far" and duplicate bullets.
                    let tail_handle = handle.clone();
                    let start = futures::stream::once(async move {
                        tail_handle.spawn(async move {
                            let _serial = queue.lock().await;
                            tail.await;
                        });
                        Vec::new()
                    })
                    .map(futures::stream::iter)
                    .flatten();
                    let out: EventStream = Box::pin(downstream.chain(start));
                    out
                }
                // Without a runtime no task can be spawned; running the tail inline is the
                // only way not to lose the memories, and nothing is streaming anyway.
                Err(_) => Box::pin(
                    downstream.chain(
                        futures::stream::once(async move {
                            tail.await;
                            Vec::new()
                        })
                        .map(futures::stream::iter)
                        .flatten(),
                    ),
                ),
            }
        };
        Box::pin(futures::stream::once(fut).flatten())
    }
}

/// Runs the pre-compaction memory steps when the history is about to be summarized away.
///
/// `flush_before_compact` extracts the facts that only exist in the messages the summary
/// will replace; `offload_before_compact` keeps the raw messages in the session JSONL.
#[allow(clippy::too_many_arguments)]
async fn pre_compaction_memory(
    fs: &Arc<dyn Filesystem>,
    model: &Arc<dyn Model>,
    flush_prompt: &str,
    ctx: &RuntimeContext,
    messages: &[Msg],
    compactor: &ConversationCompactor,
    config: &CompactionConfig,
    agent_id: &str,
    session_id: &str,
) {
    if messages.is_empty() || !compactor.needs_compaction(messages, config, None) {
        return;
    }
    let manager = MemoryFlushManager::new_with_prompt(
        Arc::clone(fs),
        Arc::clone(model),
        Some(flush_prompt.to_string()),
    );
    if config.flush_before_compact {
        if let Err(error) = manager.flush_memories(ctx, messages).await {
            tracing::warn!(%error, "pre-compaction memory flush failed");
        }
    }
    if config.offload_before_compact {
        if let Err(error) =
            offload_messages(fs, &manager, ctx, messages, agent_id, session_id).await
        {
            tracing::warn!(%error, "pre-compaction message offload failed");
        }
    }
}

/// Offloads raw messages to the session JSONL tree (idempotent by message id).
/// Mirrors `MemoryFlushManager#offloadMessages` / `offloadToSessionTree`.
async fn offload_messages(
    fs: &Arc<dyn Filesystem>,
    manager: &MemoryFlushManager,
    ctx: &RuntimeContext,
    messages: &[Msg],
    agent_id: &str,
    session_id: &str,
) -> agentscope_core::error::Result<()> {
    let path = manager.resolve_offload_path(agent_id, session_id);
    let mut existing = String::new();
    if fs.exists(ctx, &path).await.unwrap_or(false) {
        existing = fs
            .read(ctx, &path, 0, 0)
            .await
            .map(|d| d.content)
            .unwrap_or_default();
    }
    // Idempotency: skip messages whose id already appears in the JSONL.
    let mut seen: std::collections::HashSet<String> = existing
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| v.get("id").and_then(|i| i.as_str()).map(str::to_string))
        .collect();

    let mut additions = Vec::new();
    for m in messages {
        if m.role == agentscope_core::message::MsgRole::System {
            continue;
        }
        if seen.contains(m.id()) {
            continue;
        }
        if let Ok(json) = serde_json::to_string(m) {
            additions.push(json);
            seen.insert(m.id().to_string());
        }
    }
    if additions.is_empty() {
        return Ok(());
    }
    let mut content = existing;
    for line in additions {
        content.push_str(&line);
        content.push('\n');
    }
    let _ = fs.delete(ctx, &path).await;
    fs.write(ctx, &path, &content).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::LocalFilesystem;

    struct DummyModel;
    #[async_trait::async_trait]
    impl Model for DummyModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<agentscope_core::model::ToolSchema>,
            _options: agentscope_core::model::GenerateOptions,
        ) -> futures::stream::BoxStream<
            '_,
            agentscope_core::error::Result<agentscope_core::model::ChatResponse>,
        > {
            Box::pin(futures::stream::empty())
        }
        fn model_name(&self) -> &str {
            "dummy"
        }
    }

    #[test]
    fn flush_trigger_always_and_never() {
        let ctx = RuntimeContext::new();
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(std::env::temp_dir()));
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let mw = MemoryFlushMiddleware::new(fs, model);
        assert!(mw.should_flush_now(&ctx)); // ALWAYS

        let mw_never = MemoryFlushMiddleware::new_with_config(
            mw.fs.clone(),
            mw.model.clone(),
            mw.flush_prompt.clone(),
            FlushTrigger::never(),
            IsolationScope::User,
        );
        assert!(!mw_never.should_flush_now(&ctx)); // NEVER
    }

    #[tokio::test]
    async fn offload_is_idempotent_by_message_id() {
        let dir = std::env::temp_dir().join(format!("agentscope_off_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(dir));
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let manager = MemoryFlushManager::new(fs.clone(), model);
        let ctx = RuntimeContext::new();
        let msgs = vec![Msg::user("hello"), Msg::assistant("hi")];
        offload_messages(&fs, &manager, &ctx, &msgs, "agent", "sess")
            .await
            .unwrap();
        offload_messages(&fs, &manager, &ctx, &msgs, "agent", "sess")
            .await
            .unwrap();
        let path = manager.resolve_offload_path("agent", "sess");
        let content = fs.read(&ctx, &path, 0, 0).await.unwrap().content;
        assert_eq!(content.lines().count(), 2);
    }

    #[test]
    fn a_throttled_trigger_does_not_flush_on_its_first_call() {
        let ctx = RuntimeContext::new()
            .with_user_id("cold-start-user")
            .with_session_id("cold-start-session");
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(std::env::temp_dir()));
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let mw = MemoryFlushMiddleware::new_with_config(
            fs,
            model,
            "prompt".to_string(),
            FlushTrigger::throttled(chrono::Duration::seconds(600)),
            IsolationScope::Session,
        );
        // "No timestamp" is not the epoch: the first call only opens the window, so every
        // session's first turn no longer pays for an extraction round-trip.
        assert!(!mw.should_flush_now(&ctx));
        assert!(!mw.should_flush_now(&ctx));
    }

    #[tokio::test]
    async fn pre_compaction_offload_keeps_the_history_that_is_summarized_away() {
        let dir = std::env::temp_dir().join(format!("agentscope_pre_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(dir));
        let ctx = RuntimeContext::new();
        let history: Vec<Msg> = (0..4).map(|i| Msg::user(format!("turn {i}"))).collect();
        let compactor = ConversationCompactor::new(None);
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let config = CompactionConfig::builder()
            .trigger_messages(2)
            .flush_before_compact(false)
            .offload_before_compact(true)
            .build();

        pre_compaction_memory(
            &fs, &model, "prompt", &ctx, &history, &compactor, &config, "agent", "sess",
        )
        .await;

        let manager = MemoryFlushManager::new(Arc::clone(&fs), Arc::clone(&model));
        let path = manager.resolve_offload_path("agent", "sess");
        assert!(
            fs.exists(&ctx, &path).await.unwrap(),
            "offload_before_compact must save the messages the summary will replace"
        );
        // `flush_before_compact` is off, so no daily ledger entry was written.
        assert!(fs.glob(&ctx, "memory/*.md", "/").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn pre_compaction_work_is_skipped_below_the_trigger() {
        let dir = std::env::temp_dir().join(format!("agentscope_pre_skip_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(dir));
        let ctx = RuntimeContext::new();
        let history: Vec<Msg> = vec![Msg::user("only turn")];
        let compactor = ConversationCompactor::new(None);
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let config = CompactionConfig::builder().trigger_messages(50).build();

        pre_compaction_memory(
            &fs, &model, "prompt", &ctx, &history, &compactor, &config, "agent", "sess",
        )
        .await;

        let manager = MemoryFlushManager::new(Arc::clone(&fs), Arc::clone(&model));
        assert!(!fs
            .exists(&ctx, &manager.resolve_offload_path("agent", "sess"))
            .await
            .unwrap());
    }
}
