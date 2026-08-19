//! Sub-contexts carried by [`AgentState`](super::AgentState): tool cache, task
//! bag and plan-mode flag, mirroring `io.agentscope.core.state`.
use serde::{Deserialize, Serialize};

use crate::message::JsonMap;

/// Allowed values for [`Task::state`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TaskState {
    #[serde(rename = "pending")]
    #[default]
    Pending,
    #[serde(rename = "in_progress")]
    InProgress,
    #[serde(rename = "completed")]
    Completed,
}

/// Persistent agent task record carried inside [`TaskContextState`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub subject: String,
    pub description: String,
    #[serde(default)]
    pub metadata: JsonMap,
    #[serde(rename = "created_at")]
    pub created_at: String,
    #[serde(default)]
    pub state: TaskState,
    pub id: String,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub blocks: Vec<String>,
    #[serde(rename = "blocked_by", default)]
    pub blocked_by: Vec<String>,
}

impl Task {
    pub fn new(subject: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            description: description.into(),
            metadata: JsonMap::new(),
            created_at: crate::message::current_timestamp(),
            state: TaskState::Pending,
            id: uuid::Uuid::new_v4().to_string().replace('-', ""),
            owner: None,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
        }
    }
    pub fn builder() -> TaskBuilder {
        TaskBuilder::new()
    }
}

/// Builder for [`Task`].
#[derive(Debug, Clone, Default)]
pub struct TaskBuilder {
    subject: Option<String>,
    description: Option<String>,
    metadata: Option<JsonMap>,
    created_at: Option<String>,
    state: Option<TaskState>,
    id: Option<String>,
    owner: Option<String>,
    blocks: Option<Vec<String>>,
    blocked_by: Option<Vec<String>>,
}

impl TaskBuilder {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn subject(mut self, s: impl Into<String>) -> Self {
        self.subject = Some(s.into());
        self
    }
    pub fn description(mut self, s: impl Into<String>) -> Self {
        self.description = Some(s.into());
        self
    }
    pub fn metadata(mut self, m: JsonMap) -> Self {
        self.metadata = Some(m);
        self
    }
    pub fn created_at(mut self, s: impl Into<String>) -> Self {
        self.created_at = Some(s.into());
        self
    }
    pub fn state(mut self, s: TaskState) -> Self {
        self.state = Some(s);
        self
    }
    pub fn id(mut self, s: impl Into<String>) -> Self {
        self.id = Some(s.into());
        self
    }
    pub fn owner(mut self, s: impl Into<String>) -> Self {
        self.owner = Some(s.into());
        self
    }
    pub fn blocks(mut self, v: Vec<String>) -> Self {
        self.blocks = Some(v);
        self
    }
    pub fn blocked_by(mut self, v: Vec<String>) -> Self {
        self.blocked_by = Some(v);
        self
    }
    pub fn build(self) -> Task {
        let subject = self.subject.unwrap_or_default();
        let description = self.description.unwrap_or_default();
        let metadata = self.metadata.unwrap_or_default();
        let created_at = self.created_at.unwrap_or_else(crate::message::current_timestamp);
        let state = self.state.unwrap_or(TaskState::Pending);
        let id = self.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string().replace('-', ""));
        Task {
            subject,
            description,
            metadata,
            created_at,
            state,
            id,
            owner: self.owner,
            blocks: self.blocks.unwrap_or_default(),
            blocked_by: self.blocked_by.unwrap_or_default(),
        }
    }
}

/// Snapshot of a file's contents kept in the tool read cache.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadCacheEntry {
    pub lines: Vec<String>,
    #[serde(rename = "updated_at")]
    pub updated_at: f64,
    pub bytes: f64,
    #[serde(rename = "file_path")]
    pub file_path: String,
}

impl ReadCacheEntry {
    pub fn new(lines: Vec<String>, updated_at: f64, bytes: f64, file_path: String) -> Self {
        Self { lines, updated_at, bytes, file_path }
    }
}

/// Mutable bag of [`Task`]s carried by [`AgentState`](super::AgentState).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TaskContextState {
    #[serde(default)]
    pub tasks: Vec<Task>,
}

impl TaskContextState {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }
    pub fn tasks_mut(&mut self) -> &mut Vec<Task> {
        &mut self.tasks
    }
}

/// Persisted plan-mode flag for an agent.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PlanModeContextState {
    #[serde(rename = "plan_active", default)]
    pub plan_active: bool,
    #[serde(rename = "current_plan_file", default)]
    pub current_plan_file: Option<String>,
}

impl PlanModeContextState {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn plan_active(&self) -> bool {
        self.plan_active
    }
    pub fn set_plan_active(&mut self, v: bool) {
        self.plan_active = v;
    }
    pub fn current_plan_file(&self) -> Option<&str> {
        self.current_plan_file.as_deref()
    }
}

/// Read-cache configuration + spawn registry carried by [`AgentState`](super::AgentState).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolContextState {
    #[serde(rename = "max_cache_files")]
    pub max_cache_files: i32,
    #[serde(rename = "max_cache_bytes")]
    pub max_cache_bytes: f64,
    #[serde(rename = "activated_groups", default)]
    pub activated_groups: Vec<String>,
    #[serde(rename = "spawn_registry", default)]
    pub spawn_registry: std::collections::HashMap<String, SpawnEntry>,
}

impl Default for ToolContextState {
    fn default() -> Self {
        Self {
            max_cache_files: 100,
            max_cache_bytes: 25_000.0,
            activated_groups: Vec::new(),
            spawn_registry: std::collections::HashMap::new(),
        }
    }
}

impl ToolContextState {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn builder() -> ToolContextBuilder {
        ToolContextBuilder::default()
    }
    pub fn activated_groups(&self) -> &[String] {
        &self.activated_groups
    }
    pub fn set_activated_groups(&mut self, groups: Vec<String>) {
        self.activated_groups = groups;
    }
}

/// Serializable metadata for a spawned subagent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpawnEntry {
    pub key: String,
    #[serde(rename = "agent_id")]
    pub agent_id: String,
    #[serde(rename = "session_id")]
    pub session_id: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub depth: i32,
}

/// Builder for [`ToolContextState`].
#[derive(Debug, Clone, Default)]
pub struct ToolContextBuilder {
    max_cache_files: i32,
    max_cache_bytes: f64,
    activated_groups: Vec<String>,
    spawn_registry: std::collections::HashMap<String, SpawnEntry>,
}

impl ToolContextBuilder {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn max_cache_files(mut self, v: i32) -> Self {
        self.max_cache_files = v;
        self
    }
    pub fn max_cache_bytes(mut self, v: f64) -> Self {
        self.max_cache_bytes = v;
        self
    }
    pub fn add_activated_group(mut self, g: impl Into<String>) -> Self {
        self.activated_groups.push(g.into());
        self
    }
    pub fn build(self) -> ToolContextState {
        ToolContextState {
            max_cache_files: self.max_cache_files.max(2),
            max_cache_bytes: self.max_cache_bytes.max(10_001.0),
            activated_groups: self.activated_groups,
            spawn_registry: self.spawn_registry,
        }
    }
}
