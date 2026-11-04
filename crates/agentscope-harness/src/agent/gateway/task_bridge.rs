//! Bridge components connecting subagent task lifecycle to the gateway.
//!
//! - `DefaultSubagentMaterializer`: rebuilds a `GatewayAgent` from a `SubagentRecord`
//!   using the `DefaultAgentManager` factory registry (same-node recovery).
//! - `TaskCompletionBridge`: hooks `WorkspaceTaskRepository` completion callbacks to
//!   enqueue wakeup messages via `MessageBus`, triggering the gateway to re-run idle
//!   sessions and surface delivered task results through the `SubagentsMiddleware`.

use std::sync::Arc;

use agentscope_core::error::Result;
use agentscope_core::message::Msg;
use async_trait::async_trait;

use crate::agent::bus::MessageBus;
use crate::agent::subagent::DefaultAgentManager;

use super::harness_gateway::{GatewayAgent, GatewayTurnContext, SubagentMaterializer};
use crate::agent::subagent::task::{TaskCompletionCallback, WorkspaceTaskRepository};

// ---------------------------------------------------------------------------
// DefaultSubagentMaterializer
// ---------------------------------------------------------------------------

/// A `SubagentMaterializer` that uses the `DefaultAgentManager` factory registry
/// to rebuild a live `GatewayAgent` from a persisted `SubagentRecord`.
///
/// This is the "same-node recovery" path: when a subagent session is not in the
/// live `exposed_sessions` cache (e.g. after gateway restart), this materializer
/// creates a fresh agent instance from the registered factory.
pub struct DefaultSubagentMaterializer {
    agent_manager: Arc<DefaultAgentManager>,
}

impl DefaultSubagentMaterializer {
    pub fn new(agent_manager: Arc<DefaultAgentManager>) -> Self {
        Self { agent_manager }
    }
}

#[async_trait]
impl SubagentMaterializer for DefaultSubagentMaterializer {
    async fn materialize(
        &self,
        agent_id: &str,
        _session_id: &str,
    ) -> Option<Arc<dyn GatewayAgent>> {
        let factory = self.agent_manager.get_factory(agent_id)?;
        // Create a stub RuntimeContext for the materialized agent. The real context
        // will be provided at call-time via GatewayTurnContext.
        let stub_rc = agentscope_core::agent::RuntimeContext::default();
        let callable = factory.create(&stub_rc);
        Some(Arc::new(CallableAgentBridge(callable)) as Arc<dyn GatewayAgent>)
    }
}

/// Wraps a `CallableAgent` (from `SubagentFactory`) as a `GatewayAgent` (for
/// `HarnessGateway`). The `GatewayTurnContext` carries the caller's cancellation
/// signal, which is forwarded to the agent so a turn can actually be stopped.
pub struct CallableAgentBridge(Arc<dyn agentscope_core::agent::CallableAgent>);

impl CallableAgentBridge {
    /// Wraps `callable` so the gateway can drive it.
    pub fn new(callable: Arc<dyn agentscope_core::agent::CallableAgent>) -> Self {
        Self(callable)
    }
}

#[async_trait]
impl GatewayAgent for CallableAgentBridge {
    async fn call(&self, messages: Vec<Msg>, ctx: &GatewayTurnContext) -> Result<Msg> {
        match &ctx.cancel {
            Some(signal) => self.0.call_with_cancel(messages, signal.clone()).await,
            None => self.0.call(messages).await,
        }
    }
}

// ---------------------------------------------------------------------------
// TaskCompletionBridge
// ---------------------------------------------------------------------------

/// Bridges task completion events to the gateway wakeup mechanism.
///
/// When a `WorkspaceTaskRepository` task reaches a terminal state (COMPLETED or
/// FAILED), the callback fires and enqueues a wakeup message into the
/// `MessageBus`. The `WakeupDispatcher` then triggers a `run_wakeup` on the
/// gateway, which causes the `SubagentsMiddleware` to surface the delivered
/// task results via `<system-reminder>` blocks.
pub struct TaskCompletionBridge {
    message_bus: Arc<dyn MessageBus>,
}

impl TaskCompletionBridge {
    pub fn new(message_bus: Arc<dyn MessageBus>) -> Self {
        Self { message_bus }
    }

    /// Creates a `TaskCompletionCallback` that pushes a `<system-reminder>` hint into the
    /// session inbox and enqueues a wakeup, mirroring Java
    /// `HarnessAgent#wireTaskRepositoryMessageBus`.
    ///
    /// The inbox hint is what makes the result survive a process restart (the queue lives on
    /// the filesystem); the wakeup is what re-runs an idle session right away.
    pub fn callback(&self) -> TaskCompletionCallback {
        let bus = Arc::clone(&self.message_bus);
        Arc::new(
            move |task_id: String, session_id: String, result: Option<String>| {
                let bus = bus.clone();
                tokio::spawn(async move {
                    // A pointer, not the payload: `SubagentsMiddleware` already delivers the
                    // full result on the next reasoning step (and stamps `delivered_at` there).
                    // Carrying the body here as well made the model read the same long result
                    // twice in one request. The hint itself is still what survives a process
                    // restart — the queue lives on the filesystem.
                    let hint = if result.is_some() {
                        format!(
                            "<system-reminder>Background subagent task '{task_id}' has completed. \
                             Its result is delivered on the next step; call \
                             task_output(task_id='{task_id}') only if you need it now.\
                             </system-reminder>"
                        )
                    } else {
                        format!(
                            "<system-reminder>Background subagent task '{task_id}' has failed. \
                             Call task_output(task_id='{task_id}') to read the error.\
                             </system-reminder>"
                        )
                    };
                    let payload = serde_json::json!({
                        "type": "hint",
                        "id": new_hex(),
                        "hint": hint,
                        "source": "subagent_task",
                    });
                    if let Err(e) = bus.inbox_push(&session_id, payload).await {
                        tracing::warn!(task = %task_id, %e, "task completion hint was not pushed");
                    }
                    // Enqueue wakeup with session_id so the dispatcher can route to the
                    // correct session. The agent_id is empty (main agent handles delivery).
                    let anonymous = "";
                    if let Err(e) = bus.enqueue_wakeup(anonymous, &session_id, anonymous).await {
                        tracing::warn!(task = %task_id, %e, "task completion wakeup was not enqueued");
                    }
                });
            },
        )
    }
}

/// Generates a UUID without dashes, matching the Java side.
fn new_hex() -> String {
    uuid::Uuid::new_v4().to_string().replace('-', "")
}

/// Wires the `TaskCompletionBridge` into a `WorkspaceTaskRepository` and
/// `MessageBus` pair. Call this during gateway bootstrap to connect task
/// completions to the wakeup dispatch loop.
pub async fn wire_task_completion(
    repo: &WorkspaceTaskRepository,
    message_bus: Arc<dyn MessageBus>,
) {
    let bridge = TaskCompletionBridge::new(message_bus);
    repo.set_completion_callback(bridge.callback());
}

/// Convenience: creates a `TaskCompletionBridge` and returns the callback for
/// use with any `TaskRepository` that supports `set_completion_callback`.
pub fn create_completion_callback(message_bus: Arc<dyn MessageBus>) -> TaskCompletionCallback {
    TaskCompletionBridge::new(message_bus).callback()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::bus::WorkspaceMessageBus;
    use crate::agent::subagent::{SubagentDeclaration, SubagentEntry, SubagentFactory};
    use crate::filesystem::{Filesystem, LocalFilesystem};
    use agentscope_core::agent::{Agent, CallableAgent};
    use agentscope_core::formatter::JsonSchema;
    use agentscope_core::state::AgentState;
    use agentscope_core::tool::Toolkit;

    struct DummyAgent {
        id: String,
        toolkit: Toolkit,
    }

    #[async_trait]
    impl CallableAgent for DummyAgent {
        async fn call(&self, _messages: Vec<Msg>) -> Result<Msg> {
            Ok(Msg::assistant(format!("dummy reply from {}", self.id)))
        }
        async fn call_with_schema(&self, _messages: Vec<Msg>, _schema: JsonSchema) -> Result<Msg> {
            Ok(Msg::assistant("{}"))
        }
    }

    impl Agent for DummyAgent {
        fn agent_id(&self) -> &str {
            &self.id
        }
        fn name(&self) -> &str {
            &self.id
        }
        fn description(&self) -> &str {
            "dummy subagent"
        }
        fn interrupt(&self) {}
        fn agent_state(&self) -> AgentState {
            AgentState::default()
        }
        fn toolkit(&self) -> &Toolkit {
            &self.toolkit
        }
    }

    struct DummyFactory;
    impl SubagentFactory for DummyFactory {
        fn create(
            &self,
            _parent_rc: &agentscope_core::agent::RuntimeContext,
        ) -> Arc<dyn agentscope_core::agent::CallableAgent> {
            Arc::new(DummyAgent {
                id: "coder".to_string(),
                toolkit: Toolkit::default(),
            })
        }
    }

    use super::super::subagent::SubagentRecord;

    #[tokio::test]
    async fn materializer_creates_agent_from_factory() {
        let entries = vec![SubagentEntry::new(
            "coder",
            "A coding agent",
            Arc::new(DummyFactory) as Arc<dyn SubagentFactory>,
        )
        .with_declaration(SubagentDeclaration::new(
            "coder",
            "A coding agent",
            "Do coding",
        ))];
        let manager = Arc::new(DefaultAgentManager::new(&entries));
        let mat = DefaultSubagentMaterializer::new(manager);

        let record = SubagentRecord::new("sub-1", "coder", "sess-1", None, None, None);
        let agent = mat.materialize(&record.agent_id, &record.session_id).await;
        assert!(agent.is_some());

        let agent = agent.unwrap();
        let reply = agent
            .call(
                vec![Msg::user("hello")],
                &GatewayTurnContext {
                    session_id: "sess-1".into(),
                    gate_key: "k".into(),
                    msg_context: super::super::msg_context::MsgContext::default_context(),
                    user_id: None,
                    outbound: None,
                    cancel: None,
                },
            )
            .await
            .unwrap();
        assert!(reply.text_content().contains("dummy reply from coder"));
    }

    #[tokio::test]
    async fn materializer_returns_none_for_unknown_agent() {
        let manager = Arc::new(DefaultAgentManager::new(&[]));
        let mat = DefaultSubagentMaterializer::new(manager);
        assert!(mat.materialize("nonexistent", "sess-1").await.is_none());
    }

    #[tokio::test]
    async fn task_completion_bridge_enqueues_wakeup() {
        let dir = std::env::temp_dir().join(format!("task_bridge_test_{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
        let bus: Arc<dyn MessageBus> = Arc::new(WorkspaceMessageBus::new(fs.clone(), "/bus"));

        let bridge = TaskCompletionBridge::new(bus.clone());
        let cb = bridge.callback();
        cb(
            "task-42".to_string(),
            "sess-99".to_string(),
            Some("result text".to_string()),
        );

        // Give the spawned task time to complete
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Verify the callback ran without panicking. In WorkspaceMessageBus Mode D,
        // publish() is a no-op (subscribe is polling-based), so we cannot directly
        // observe the signal on the channel. Instead, use queue_drain to check the
        // wakeup was enqueued into the filesystem-backed queue.
        let entries = bus
            .queue_drain(crate::agent::bus::WAKEUPS_KEY, 10)
            .await
            .unwrap();
        assert!(
            !entries.is_empty(),
            "Expected at least one wakeup entry in the queue"
        );
        // Verify the payload structure
        let payload = &entries[0].payload;
        assert_eq!(payload["sessionId"].as_str(), Some("sess-99"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn the_hint_points_at_the_result_instead_of_copying_it() {
        let dir = std::env::temp_dir().join(format!(
            "task_bridge_hint_test_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
        let bus: Arc<dyn MessageBus> = Arc::new(WorkspaceMessageBus::new(fs.clone(), "/bus"));

        let cb = TaskCompletionBridge::new(bus.clone()).callback();
        cb(
            "task-42".to_string(),
            "sess-99".to_string(),
            Some("result text".to_string()),
        );
        cb("task-43".to_string(), "sess-99".to_string(), None);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let entries = bus.inbox_drain("sess-99", 10).await.unwrap();
        assert_eq!(entries.len(), 2, "one hint per completion");

        let completed = entries[0].payload["hint"].as_str().unwrap();
        assert!(completed.contains("task-42"));
        assert!(completed.contains("task_output"));
        assert!(
            !completed.contains("result text"),
            "the hint must not duplicate the result body: {completed}"
        );

        let failed = entries[1].payload["hint"].as_str().unwrap();
        assert!(failed.contains("task-43"));
        assert!(failed.contains("failed"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
