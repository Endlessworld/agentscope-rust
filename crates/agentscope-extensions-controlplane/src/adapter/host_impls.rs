//! Harness-backed host implementations for the optional adapter SPIs.
//!
//! Java keeps `SessionCompactor`, `SessionHistorySource`, `AgentRuntimeSource` and
//! `AgentTaskStarter` as host hooks because the extension must not depend on
//! `agentscope-harness`; the application wires e.g. a `ConversationCompactor` by hand
//! (`SessionCompactor.java:20-46`). This crate does depend on the harness, so it ships
//! ready-to-use implementations that a host installs with
//! `AgentScopeAdapter::set_history_source`, `AgentScopeAdapter::new(Some(compactor))` or
//! `AgentScopeAdapter::set_runtime_source`. Nothing here opens a network connection, so
//! every hook is exercisable offline.
//!
//! `AgentTaskStarter` already has its harness implementation in
//! [`HarnessAgentTaskStarter`](super::HarnessAgentTaskStarter), so it gets no second
//! default here.

use std::sync::Arc;

use async_trait::async_trait;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::{Msg, MsgRole};
use agentscope_core::model::Model;
use agentscope_core::session::{SessionKey, SessionLogStore};
use agentscope_core::state::AgentState;
use agentscope_harness::agent::memory::compaction::{CompactionConfig, ConversationCompactor};
use agentscope_harness::agent::subagent::task::{BackgroundTask, TaskRepository};

use crate::adapter::agent_scope_adapter::message_item;
use crate::adapter::support::{AgentRuntimeSource, SessionCompactor, SessionHistorySource};
use crate::error::{ControlPlaneError, ControlPlaneResult};
use crate::model::{MessageItem, SubagentInfo};

/// Reads the full session history from the harness native session log.
///
/// Mirrors the Java guidance on `SessionHistorySource.java:18-24`: the harness keeps an
/// append-only log, so a source can return messages that were already compacted out of
/// the live `AgentState`. A session with no committed events yields an empty list (the
/// adapter then serves an empty page); an identity that cannot form a [`SessionKey`]
/// returns `None`, which makes the adapter fall back to live state.
pub struct StoreSessionHistorySource {
    store: Arc<dyn SessionLogStore>,
    agent_id: String,
    context: RuntimeContext,
}

impl StoreSessionHistorySource {
    /// Wraps `store` for `agent_id`, opening journals under `context`.
    pub fn new(
        store: Arc<dyn SessionLogStore>,
        agent_id: impl Into<String>,
        context: RuntimeContext,
    ) -> Self {
        Self {
            store,
            agent_id: agent_id.into(),
            context,
        }
    }

    /// Same as [`new`](Self::new) with an empty runtime context.
    pub fn with_default_context(
        store: Arc<dyn SessionLogStore>,
        agent_id: impl Into<String>,
    ) -> Self {
        Self::new(store, agent_id, RuntimeContext::default())
    }
}

#[async_trait]
impl SessionHistorySource for StoreSessionHistorySource {
    async fn load_messages(
        &self,
        session_id: &str,
        user_id: Option<&str>,
    ) -> Option<Vec<MessageItem>> {
        let key = SessionKey::new(user_id, self.agent_id.as_str(), session_id).ok()?;
        let log = self.store.open(&key, &self.context).await.ok()?;
        let transcript = agentscope_core::session::transcript(log.as_ref())
            .await
            .ok()?;
        Some(
            transcript
                .messages
                .iter()
                .enumerate()
                .map(|(index, message)| message_item(index as i64 + 1, message))
                .collect(),
        )
    }
}

/// Compacts the live conversation through the harness [`ConversationCompactor`].
///
/// Mirrors the Java guidance on `SessionCompactor.java:20-46`: delegate to
/// `ConversationCompactor`, hand it everything but SYSTEM messages, and write the
/// replacement back into `state.context`. The returned string is the text of the
/// summary message that was injected, which the adapter publishes as the `compaction`
/// event; an empty string means no compaction was needed.
///
/// With no model the compactor still runs the deterministic pre-pass (argument
/// truncation + tool-result pruning) and injects the
/// `(summary unavailable - no model configured)` placeholder, so the behaviour stays
/// observable offline.
pub struct HarnessSessionCompactor {
    compactor: ConversationCompactor,
    config: CompactionConfig,
    agent_id: String,
}

impl HarnessSessionCompactor {
    /// Builds a compactor over `model` (optional) with an explicit policy.
    pub fn new(
        model: Option<Arc<dyn Model>>,
        config: CompactionConfig,
        agent_id: impl Into<String>,
    ) -> Self {
        Self {
            compactor: ConversationCompactor::new(model),
            config,
            agent_id: agent_id.into(),
        }
    }

    /// Builds a compactor over `model` with the harness default policy.
    pub fn with_default_config(model: Option<Arc<dyn Model>>, agent_id: impl Into<String>) -> Self {
        Self::new(model, CompactionConfig::default(), agent_id)
    }

    /// The compaction policy in force.
    pub fn config(&self) -> &CompactionConfig {
        &self.config
    }
}

#[async_trait]
impl SessionCompactor for HarnessSessionCompactor {
    async fn compact(
        &self,
        session_id: &str,
        state: &mut AgentState,
    ) -> ControlPlaneResult<String> {
        let conversation: Vec<Msg> = state
            .context
            .iter()
            .filter(|message| !matches!(message.role(), MsgRole::System))
            .cloned()
            .collect();
        let replacement = self
            .compactor
            .compact_if_needed(conversation, &self.config, &self.agent_id, session_id)
            .await
            .map_err(|error| {
                ControlPlaneError::Internal(format!("conversation compaction failed: {error}"))
            })?;
        let Some(messages) = replacement else {
            return Ok(String::new());
        };
        let summary = messages
            .first()
            .map(|message| message.text_content())
            .unwrap_or_default();
        state.context = messages;
        Ok(summary)
    }
}

/// Exposes harness background subagent tasks to the control plane.
///
/// Mirrors the task half of the host-side `AgentRuntimeSource`: Java cannot see harness
/// tasks, so the application supplies them. Listing and cancelling are scoped to the
/// session key the harness already uses. The subagent/workspace inventories return empty
/// lists (Java's defaults) instead of `Unsupported`, because a repository-backed source
/// genuinely advertises those capabilities but has no separate registry to read here.
/// Plan-mode hooks keep the Java defaults (`setPlanMode` -> `false`, no excerpt): the
/// harness plan-mode handle is bound to a specific agent state handle that this source
/// does not own.
pub struct TaskRepositoryRuntimeSource {
    repository: Arc<dyn TaskRepository>,
    context: RuntimeContext,
}

impl TaskRepositoryRuntimeSource {
    /// Wraps `repository`, querying tasks under `context`.
    pub fn new(repository: Arc<dyn TaskRepository>, context: RuntimeContext) -> Self {
        Self {
            repository,
            context,
        }
    }

    /// Same as [`new`](Self::new) with an empty runtime context.
    pub fn with_default_context(repository: Arc<dyn TaskRepository>) -> Self {
        Self::new(repository, RuntimeContext::default())
    }
}

#[async_trait]
impl AgentRuntimeSource for TaskRepositoryRuntimeSource {
    async fn list_subagents(&self) -> ControlPlaneResult<Vec<SubagentInfo>> {
        Ok(Vec::new())
    }

    async fn list_workspaces(&self) -> ControlPlaneResult<Vec<serde_json::Value>> {
        Ok(Vec::new())
    }

    async fn list_subagent_tasks(
        &self,
        session_id: &str,
    ) -> ControlPlaneResult<Vec<serde_json::Value>> {
        let tasks = self
            .repository
            .list_tasks(&self.context, session_id, None)
            .await;
        Ok(tasks.iter().map(|task| subagent_task_json(task)).collect())
    }

    async fn cancel_subagent_task(
        &self,
        session_id: &str,
        task_id: &str,
    ) -> ControlPlaneResult<bool> {
        Ok(self
            .repository
            .cancel_task(&self.context, session_id, task_id)
            .await)
    }
}

/// Projects a harness [`BackgroundTask`] into the Level-3 task payload served by
/// `GET /agentscope/sessions/{id}/subagent-tasks`.
pub fn subagent_task_json(task: &BackgroundTask) -> serde_json::Value {
    serde_json::json!({
        "taskId": task.task_id(),
        "agentId": task.agent_id(),
        "status": task.get_status_string(),
        "createdAt": task.created_at(),
        "lastCheckedAt": task.last_checked_at(),
        "result": task.get_result(),
        "error": task.get_error(),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use agentscope_core::session::{InMemorySessionLogStore, SessionEvent, SessionLog};
    use agentscope_harness::agent::subagent::task::{TaskRunSpec, TaskStatus};
    use async_trait::async_trait;

    use super::*;
    use crate::model::{ROLE_ASSISTANT, ROLE_USER};

    async fn seed_session(store: &Arc<dyn SessionLogStore>, key: &SessionKey) {
        let log = store.open(key, &RuntimeContext::default()).await.unwrap();
        let writer = log.acquire("run-1", Duration::from_secs(60)).await.unwrap();
        let head = log.head().await.unwrap();
        let events = vec![
            SessionEvent::from_payload(
                "e1",
                1,
                0,
                "message/user",
                Some("run-1".into()),
                Some("t1".into()),
                false,
                &serde_json::json!({ "message": Msg::user("hello") }),
            )
            .unwrap(),
            SessionEvent::from_payload(
                "e2",
                2,
                0,
                "message/assistant",
                Some("run-1".into()),
                Some("t1".into()),
                false,
                &serde_json::json!({ "message": Msg::assistant("hi there") }),
            )
            .unwrap(),
        ];
        log.commit(&writer, "b1", head.seq, &events).await.unwrap();
    }

    #[tokio::test]
    async fn store_history_source_reads_the_native_transcript() {
        let store: Arc<dyn SessionLogStore> = Arc::new(InMemorySessionLogStore::new());
        let key = SessionKey::new(Some("u1"), "agent", "s1").unwrap();
        seed_session(&store, &key).await;

        let source = StoreSessionHistorySource::with_default_context(store, "agent");
        let items = source.load_messages("s1", Some("u1")).await.unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].seq, 1);
        assert_eq!(items[0].role, ROLE_USER);
        assert_eq!(items[0].content, "hello");
        assert_eq!(items[1].role, ROLE_ASSISTANT);
        assert_eq!(items[1].content, "hi there");
    }

    #[tokio::test]
    async fn store_history_source_rejects_an_empty_session_id() {
        let store: Arc<dyn SessionLogStore> = Arc::new(InMemorySessionLogStore::new());
        let source = StoreSessionHistorySource::with_default_context(store, "agent");
        assert!(source.load_messages("", None).await.is_none());
    }

    #[tokio::test]
    async fn harness_compactor_rewrites_the_context_and_returns_the_summary() {
        let mut state = AgentState::default();
        for index in 0..6 {
            state.context.push(Msg::user(format!("message {index}")));
        }
        let config = CompactionConfig {
            trigger_messages: 4,
            trigger_tokens: 0,
            keep_messages: 1,
            keep_tokens: 0,
            prune: None,
            truncate_args: None,
            ..CompactionConfig::default()
        };
        let compactor = HarnessSessionCompactor::new(None, config, "agent");

        let summary = compactor.compact("s1", &mut state).await.unwrap();

        assert!(summary.contains("summary unavailable - no model configured"));
        assert_eq!(state.context.len(), 2);
        assert!(state
            .context
            .first()
            .unwrap()
            .text_content()
            .contains("Here is a summary of the conversation to date"));
        assert_eq!(state.context[1].text_content(), "message 5");
    }

    #[tokio::test]
    async fn harness_compactor_is_a_noop_below_the_trigger() {
        let mut state = AgentState::default();
        state.context.push(Msg::user("only one"));
        let compactor = HarnessSessionCompactor::with_default_config(None, "agent");

        let summary = compactor.compact("s1", &mut state).await.unwrap();

        assert!(summary.is_empty());
        assert_eq!(state.context.len(), 1);
        assert_eq!(compactor.config().trigger_messages, 50);
    }

    #[derive(Default)]
    struct FakeTaskRepository {
        tasks: Mutex<Vec<(String, Arc<BackgroundTask>)>>,
    }

    #[async_trait]
    impl TaskRepository for FakeTaskRepository {
        async fn get_task(
            &self,
            _ctx: &RuntimeContext,
            session_id: &str,
            task_id: &str,
        ) -> Option<Arc<BackgroundTask>> {
            self.tasks
                .lock()
                .unwrap()
                .iter()
                .find(|(session, task)| session == session_id && task.task_id() == task_id)
                .map(|(_, task)| task.clone())
        }

        async fn put_task(
            &self,
            _ctx: &RuntimeContext,
            task_id: &str,
            sub_agent_id: &str,
            session_id: &str,
            _spec: TaskRunSpec,
        ) -> Arc<BackgroundTask> {
            let task = BackgroundTask::controlled(task_id, sub_agent_id);
            self.tasks
                .lock()
                .unwrap()
                .push((session_id.to_string(), task.clone()));
            task
        }

        async fn remove_task(&self, _ctx: &RuntimeContext, session_id: &str, task_id: &str) {
            self.tasks
                .lock()
                .unwrap()
                .retain(|(session, task)| !(session == session_id && task.task_id() == task_id));
        }

        fn clear(&self) {
            self.tasks.lock().unwrap().clear();
        }

        async fn list_tasks(
            &self,
            _ctx: &RuntimeContext,
            session_id: &str,
            _filter: Option<TaskStatus>,
        ) -> Vec<Arc<BackgroundTask>> {
            self.tasks
                .lock()
                .unwrap()
                .iter()
                .filter(|(session, _)| session == session_id)
                .map(|(_, task)| task.clone())
                .collect()
        }

        async fn cancel_task(&self, ctx: &RuntimeContext, session_id: &str, task_id: &str) -> bool {
            match self.get_task(ctx, session_id, task_id).await {
                Some(task) => {
                    task.cancel();
                    true
                }
                None => false,
            }
        }
    }

    #[tokio::test]
    async fn task_source_lists_and_cancels_background_tasks() {
        let repository = Arc::new(FakeTaskRepository::default());
        repository
            .put_task(
                &RuntimeContext::default(),
                "t1",
                "worker",
                "s1",
                TaskRunSpec::local(|| Ok("done".to_string())),
            )
            .await;
        let source = TaskRepositoryRuntimeSource::with_default_context(repository.clone());

        let tasks = source.list_subagent_tasks("s1").await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["taskId"], "t1");
        assert_eq!(tasks[0]["agentId"], "worker");
        assert_eq!(tasks[0]["status"], "Running");

        assert!(source.cancel_subagent_task("s1", "t1").await.unwrap());
        assert!(!source.cancel_subagent_task("s1", "missing").await.unwrap());
        // A repository-backed source advertises the inventories without inventing rows.
        assert!(source.list_subagents().await.unwrap().is_empty());
        assert!(source.list_workspaces().await.unwrap().is_empty());
    }
}
