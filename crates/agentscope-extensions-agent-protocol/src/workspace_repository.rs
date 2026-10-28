//! Workspace-backed [`ProtocolTaskRepository`], mirroring
//! `io.agentscope.extensions.agentprotocol.WorkspaceProtocolTaskRepository`.
//!
//! Java persists control-plane `TaskRecord`s through a *dedicated* `WorkspaceManager` rooted at
//! the protocol control-plane path, writing into the synthetic
//! `PROTOCOL_AGENT_ID` / `PROTOCOL_SESSION_ID` bucket. That keeps a task's global `task_id`
//! lookup independent of which agent ran it, while each agent keeps its own MEMORY / sessions /
//! skills under its own workspace.
//!
//! Rust keeps the same layout because it reuses harness
//! [`WorkspaceManager::write_task_record_json`] / [`WorkspaceManager::read_task_record_json`],
//! which store `agents/<agentId>/tasks/<sessionId>.json`.

use std::path::PathBuf;
use std::sync::Arc;

use agentscope_core::RuntimeContext;
use agentscope_harness::agent::subagent::task::TaskRecord;
use agentscope_harness::agent::workspace::WorkspaceManager;

use crate::{AgentProtocolConstants, AgentProtocolError, ProtocolTaskRepository};

/// [`ProtocolTaskRepository`] backed by a dedicated [`WorkspaceManager`].
///
/// Mirrors Java `WorkspaceProtocolTaskRepository`.
///
/// **Note on the trait shape**: [`ProtocolTaskRepository`] is synchronous (it is called from the
/// synchronous `submit` / `snapshot` paths), while [`WorkspaceManager`]'s task-store API is
/// async. The trait impl therefore drives the async call with `futures::executor::block_on`;
/// the workspace local-disk path uses `std::fs` plus runtime-agnostic `tokio::sync` locks, so no
/// reactor is required. Callers already on an async task can use [`save_async`](Self::save_async)
/// and [`find_async`](Self::find_async) instead.
pub struct WorkspaceProtocolTaskRepository {
    workspace_manager: Arc<WorkspaceManager>,
}

impl std::fmt::Debug for WorkspaceProtocolTaskRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceProtocolTaskRepository")
            .field("workspace", &self.workspace_manager.workspace())
            .finish()
    }
}

impl WorkspaceProtocolTaskRepository {
    /// Creates a repository rooted at `task_store_path` with a fresh workspace manager.
    ///
    /// Mirrors Java `new WorkspaceProtocolTaskRepository(Path taskStorePath)`.
    pub fn new(task_store_path: impl Into<PathBuf>) -> Self {
        Self {
            workspace_manager: Arc::new(WorkspaceManager::new(task_store_path)),
        }
    }

    /// Creates a repository over an existing workspace manager (control-plane root only).
    pub fn from_manager(workspace_manager: Arc<WorkspaceManager>) -> Self {
        Self { workspace_manager }
    }

    /// The underlying workspace manager.
    pub fn workspace_manager(&self) -> &Arc<WorkspaceManager> {
        &self.workspace_manager
    }

    /// The workspace-relative path of the record file, for diagnostics and tests.
    pub fn record_path(&self) -> String {
        format!(
            "agents/{}/{}/{}.json",
            AgentProtocolConstants::PROTOCOL_AGENT_ID,
            "tasks",
            AgentProtocolConstants::PROTOCOL_SESSION_ID
        )
    }

    /// Upserts `record`, awaiting the workspace write.
    pub async fn save_async(&self, record: &TaskRecord) -> Result<(), AgentProtocolError> {
        if record.task_id.trim().is_empty() {
            return Err(AgentProtocolError::InvalidRequest(
                "task id must not be empty".to_string(),
            ));
        }
        let value = serde_json::to_value(record)?;
        self.workspace_manager
            .write_task_record_json(
                &RuntimeContext::default(),
                AgentProtocolConstants::PROTOCOL_AGENT_ID,
                AgentProtocolConstants::PROTOCOL_SESSION_ID,
                &record.task_id,
                &value,
            )
            .await;
        Ok(())
    }

    /// Reads a record by id, awaiting the workspace read.
    ///
    /// Mirrors Java `find`: a blank id or an unknown/malformed record yields `None`.
    pub async fn find_async(&self, task_id: &str) -> Option<TaskRecord> {
        if task_id.trim().is_empty() {
            return None;
        }
        let value = self
            .workspace_manager
            .read_task_record_json(
                &RuntimeContext::default(),
                AgentProtocolConstants::PROTOCOL_AGENT_ID,
                AgentProtocolConstants::PROTOCOL_SESSION_ID,
                task_id,
            )
            .await?;
        serde_json::from_value::<TaskRecord>(value).ok()
    }

    /// All persisted records in the protocol bucket, for diagnostics and admin views.
    pub async fn list_async(&self) -> Vec<TaskRecord> {
        self.workspace_manager
            .list_task_records_json(
                &RuntimeContext::default(),
                AgentProtocolConstants::PROTOCOL_AGENT_ID,
                AgentProtocolConstants::PROTOCOL_SESSION_ID,
            )
            .await
            .into_iter()
            .filter_map(|value| serde_json::from_value::<TaskRecord>(value).ok())
            .collect()
    }
}

impl ProtocolTaskRepository for WorkspaceProtocolTaskRepository {
    fn save(&self, record: &TaskRecord) {
        if let Err(error) = futures::executor::block_on(self.save_async(record)) {
            tracing::warn!(
                task_id = record.task_id.as_str(),
                error = error.to_string().as_str(),
                "persisting the task record to the workspace failed"
            );
        }
    }

    fn find(&self, task_id: &str) -> Option<TaskRecord> {
        futures::executor::block_on(self.find_async(task_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_harness::agent::subagent::task::TaskStatus;
    use std::path::Path;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("agentscope-agent-protocol-{tag}-{}", uuid_like()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Monotonic-ish unique suffix without pulling in `uuid` just for tests.
    fn uuid_like() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        format!("{nanos}")
    }

    fn record(task_id: &str, status: TaskStatus) -> TaskRecord {
        let mut record = TaskRecord::new(
            task_id,
            "agent-1",
            AgentProtocolConstants::PROTOCOL_AGENT_ID,
            AgentProtocolConstants::PROTOCOL_SESSION_ID,
        );
        record.status = status;
        record.result = Some("done".to_string());
        record
    }

    #[tokio::test]
    async fn records_round_trip_through_the_workspace_bucket() {
        let dir = TempDir::new("roundtrip");
        let repository = WorkspaceProtocolTaskRepository::new(dir.path());

        repository
            .save_async(&record("t-1", TaskStatus::Running))
            .await
            .unwrap();
        let found = repository.find_async("t-1").await.unwrap();
        assert_eq!(found.task_id, "t-1");
        assert_eq!(found.status, TaskStatus::Running);
        assert_eq!(found.result.as_deref(), Some("done"));

        // The file lands in the synthetic protocol bucket, not in an agent's workspace.
        let file = dir
            .path()
            .join("agents")
            .join(AgentProtocolConstants::PROTOCOL_AGENT_ID)
            .join("tasks")
            .join(format!(
                "{}.json",
                AgentProtocolConstants::PROTOCOL_SESSION_ID
            ));
        assert!(file.is_file(), "expected {}", file.display());
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(json["t-1"]["taskId"], "t-1");
    }

    #[tokio::test]
    async fn save_upserts_and_list_returns_every_record() {
        let dir = TempDir::new("upsert");
        let repository = WorkspaceProtocolTaskRepository::new(dir.path());

        repository
            .save_async(&record("t-1", TaskStatus::Pending))
            .await
            .unwrap();
        repository
            .save_async(&record("t-1", TaskStatus::Completed))
            .await
            .unwrap();
        repository
            .save_async(&record("t-2", TaskStatus::Running))
            .await
            .unwrap();

        assert_eq!(
            repository.find_async("t-1").await.unwrap().status,
            TaskStatus::Completed
        );
        let mut ids: Vec<String> = repository
            .list_async()
            .await
            .into_iter()
            .map(|record| record.task_id)
            .collect();
        ids.sort();
        assert_eq!(ids, vec!["t-1".to_string(), "t-2".to_string()]);
    }

    #[tokio::test]
    async fn a_missing_or_blank_id_yields_none() {
        let dir = TempDir::new("missing");
        let repository = WorkspaceProtocolTaskRepository::new(dir.path());
        assert!(repository.find_async("nope").await.is_none());
        assert!(repository.find_async("  ").await.is_none());
        assert!(repository.find("nope").is_none());
    }

    #[tokio::test]
    async fn the_sync_trait_impl_persists_and_reads() {
        let dir = TempDir::new("sync");
        let repository: Arc<dyn ProtocolTaskRepository> =
            Arc::new(WorkspaceProtocolTaskRepository::new(dir.path()));

        // `block_on` on a worker thread: the workspace store must stay usable from sync callers.
        let repository_clone = repository.clone();
        tokio::task::spawn_blocking(move || {
            repository_clone.save(&record("t-sync", TaskStatus::Running));
            repository_clone.find("t-sync").map(|record| record.task_id)
        })
        .await
        .unwrap()
        .expect("the record must be readable");
    }

    #[tokio::test]
    async fn a_second_repository_over_the_same_path_sees_the_records() {
        let dir = TempDir::new("shared");
        let first = WorkspaceProtocolTaskRepository::new(dir.path());
        first
            .save_async(&record("t-shared", TaskStatus::Running))
            .await
            .unwrap();

        let second = WorkspaceProtocolTaskRepository::new(dir.path());
        let found = second.find_async("t-shared").await.unwrap();
        assert_eq!(found.task_id, "t-shared");
        assert_eq!(
            second.record_path(),
            format!(
                "agents/{}/tasks/{}.json",
                AgentProtocolConstants::PROTOCOL_AGENT_ID,
                AgentProtocolConstants::PROTOCOL_SESSION_ID
            )
        );
    }
}
