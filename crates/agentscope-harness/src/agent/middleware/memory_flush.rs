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
use crate::agent::memory::{FlushMode, FlushTrigger, MemoryFlushManager};
use crate::filesystem::Filesystem;

/// Process-wide per-isolation-key flush timestamps (millis since epoch). Static so the
/// throttle window survives across rebuilds of the middleware, mirroring the Java
/// `SHARED_LAST_FLUSH_AT` map.
static SHARED_LAST_FLUSH_AT: std::sync::Mutex<Vec<(String, u64)>> = std::sync::Mutex::new(Vec::new());

/// Returns the current time in millis for throttle bookkeeping.
fn now_millis() -> u64 {
    chrono::Utc::now().timestamp_millis() as u64
}

/// Middleware that triggers memory flush + offload after each agent call.
pub struct MemoryFlushMiddleware {
    fs: Arc<dyn Filesystem>,
    model: Arc<dyn Model>,
    flush_prompt: String,
    flush_trigger: FlushTrigger,
    isolation_scope: IsolationScope,
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
        Self { fs, model, flush_prompt, flush_trigger, isolation_scope }
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
                let mut map = SHARED_LAST_FLUSH_AT.lock().unwrap();
                let last = map
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, t)| *t)
                    .unwrap_or(0);
                if now.saturating_sub(last) < min_gap_ms {
                    return false;
                }
                if let Some(slot) = map.iter_mut().find(|(k, _)| *k == key) {
                    slot.1 = now;
                } else {
                    map.push((key, now));
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
        let agent_id = ctx.agent_name.clone().unwrap_or_default();
        let session_id = ctx
            .session_id()
            .map(str::to_string)
            .unwrap_or_else(|| "default".to_string());

        // Run the downstream agent, then perform flush + offload after completion.
        let downstream = next(input);
        let tail = async move {
            if messages.is_empty() {
                return Vec::new();
            }
            if should_flush {
                let manager = MemoryFlushManager::new_with_prompt(
                    fs.clone(),
                    model.clone(),
                    Some(flush_prompt.clone()),
                );
                // Best-effort: a flush failure must never break the agent call.
                let _ = manager.flush_memories(&ctx, &messages).await;
            }
            // Message offload runs on every call regardless of the flush trigger.
            let manager = MemoryFlushManager::new_with_prompt(fs.clone(), model, Some(flush_prompt));
            let _ = offload_messages(&fs, &manager, &ctx, &messages, &agent_id, &session_id).await;
            Vec::new()
        };
        Box::pin(
            downstream.chain(
                futures::stream::once(tail)
                    .map(futures::stream::iter)
                    .flatten(),
            ),
        )
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
        existing = fs.read(ctx, &path, 0, 0).await.map(|d| d.content).unwrap_or_default();
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
        ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<agentscope_core::model::ChatResponse>> {
            Box::pin(futures::stream::empty())
        }
        fn model_name(&self) -> &str {
            "dummy"
        }
    }

    #[test]
    fn flush_trigger_always_and_never() {
        let ctx = RuntimeContext::new();
        let fs: Arc<dyn Filesystem> =
            Arc::new(LocalFilesystem::sandboxed(std::env::temp_dir()));
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
        offload_messages(&fs, &manager, &ctx, &msgs, "agent", "sess").await.unwrap();
        offload_messages(&fs, &manager, &ctx, &msgs, "agent", "sess").await.unwrap();
        let path = manager.resolve_offload_path("agent", "sess");
        let content = fs.read(&ctx, &path, 0, 0).await.unwrap().content;
        assert_eq!(content.lines().count(), 2);
    }
}
