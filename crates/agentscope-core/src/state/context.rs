//! Sub-contexts carried by [`AgentState`](super::AgentState): tool cache, task
//! bag and plan-mode flag, mirroring `io.agentscope.core.state`.
use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::message::JsonMap;

use super::requirement::{
    require_text, RequirementDecision, RequirementKind, RequirementStatus, TaskRequirement,
    MAX_ID_CHARS, MAX_REQUIREMENTS, MAX_TEXT_CHARS,
};
use super::verification::{EvidenceBinding, TaskVerification};

/// Maximum length (in characters) of [`TaskScope::objective`].
const MAX_OBJECTIVE_CHARS: usize = 4000;

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

/// Explicit task identity carried by [`TaskContextState`].
///
/// This is the *application* task identity, not an automatically allocated control-plane
/// task. Mirrors `TaskContextState.Scope`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "TaskScopeWire")]
pub struct TaskScope {
    /// Application task id.
    pub task_id: String,
    /// What the task is meant to achieve.
    pub objective: String,
    /// Where the objective came from (user message id, ticket, ...). Untrusted text.
    pub source_ref: String,
}

impl TaskScope {
    /// Creates a scope, validating every bounded field.
    pub fn new(
        task_id: impl Into<String>,
        objective: impl Into<String>,
        source_ref: impl Into<String>,
    ) -> Result<Self> {
        let scope = Self {
            task_id: task_id.into(),
            objective: objective.into(),
            source_ref: source_ref.into(),
        };
        scope.validate()?;
        Ok(scope)
    }

    /// Re-validates the invariants (useful for values built by struct literal).
    pub fn validate(&self) -> Result<()> {
        require_text(&self.task_id, "task ID", MAX_ID_CHARS)?;
        require_text(&self.objective, "objective", MAX_OBJECTIVE_CHARS)?;
        require_text(&self.source_ref, "objective source", MAX_TEXT_CHARS)
    }
}

impl TryFrom<TaskScopeWire> for TaskScope {
    type Error = String;

    fn try_from(wire: TaskScopeWire) -> std::result::Result<Self, Self::Error> {
        Self::new(wire.task_id, wire.objective, wire.source_ref)
            .map_err(|error| error.to_string())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskScopeWire {
    task_id: String,
    objective: String,
    source_ref: String,
}

/// Session task aggregate carried by [`AgentState`](super::AgentState).
///
/// Todo progress and requirements share one revision; neither implies independently
/// verified task completion. Mirrors `io.agentscope.core.state.TaskContextState`.
///
/// Every field added on top of the legacy `tasks` bag is `#[serde(default)]`, so state
/// persisted before the task-context contract existed still deserializes (revision `0`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskContextState {
    #[serde(default)]
    pub tasks: Vec<Task>,
    #[serde(default)]
    pub requirements: Vec<TaskRequirement>,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub scope: Option<TaskScope>,
    #[serde(default)]
    pub contract_version: u64,
    #[serde(default)]
    pub subject_version: Option<String>,
    #[serde(default)]
    pub verifications: Vec<TaskVerification>,
}

impl TaskContextState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    /// Mutable access to the task list that **bypasses** the revision counter.
    ///
    /// Kept for compatibility; new code must call [`Self::replace_tasks`] so concurrent
    /// readers of the task context can detect the change through [`Self::revision`].
    pub fn tasks_mut(&mut self) -> &mut Vec<Task> {
        &mut self.tasks
    }

    /// Current shared revision of tasks/requirements/contract/subject/verifications.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn requirements(&self) -> &[TaskRequirement] {
        &self.requirements
    }

    pub fn scope(&self) -> Option<&TaskScope> {
        self.scope.as_ref()
    }

    pub fn contract_version(&self) -> u64 {
        self.contract_version
    }

    pub fn subject_version(&self) -> Option<&str> {
        self.subject_version.as_deref()
    }

    pub fn verifications(&self) -> &[TaskVerification] {
        &self.verifications
    }

    /// One requirement by identifier.
    pub fn requirement(&self, id: &str) -> Option<&TaskRequirement> {
        self.requirements.iter().find(|requirement| requirement.id == id)
    }

    /// Detached copy, used when a caller must mutate without touching the live state.
    pub fn snapshot(&self) -> Self {
        self.clone()
    }

    /// The binding a verification must cite to count as current evidence.
    ///
    /// `None` until both a scope and a subject version are pinned.
    pub fn evidence_binding(&self) -> Option<EvidenceBinding> {
        let scope = self.scope.as_ref()?;
        let subject_version = self.subject_version.as_deref()?;
        EvidenceBinding::new(
            scope.task_id.clone(),
            self.contract_version as i64,
            subject_version,
        )
        .ok()
    }

    /// Explicit trusted task switch. Clears the old task's progress and requirements together.
    ///
    /// Re-beginning the identical scope is a no-op; changing the objective under the same
    /// task id is refused (use a new task identity instead).
    pub fn begin_task(&mut self, scope: TaskScope, expected_revision: u64) -> Result<()> {
        self.check_revision(expected_revision)?;
        scope.validate()?;
        if self.scope.as_ref() == Some(&scope) {
            return Ok(());
        }
        if let Some(current) = &self.scope {
            if current.task_id == scope.task_id {
                return Err(Error::Other(
                    "Use a new task identity when changing the objective".to_string(),
                ));
            }
        }
        self.scope = Some(scope);
        self.tasks.clear();
        self.requirements.clear();
        self.verifications.clear();
        self.subject_version = None;
        self.contract_version += 1;
        self.revision += 1;
        Ok(())
    }

    /// Atomically replaces progress without overwriting requirements or evidence.
    pub fn replace_tasks(&mut self, replacement: Vec<Task>, expected_revision: u64) -> Result<()> {
        self.check_revision(expected_revision)?;
        self.tasks = replacement;
        self.revision += 1;
        Ok(())
    }

    /// Records an unconfirmed proposal and returns its identifier.
    ///
    /// Source references supplied here are untrusted claims until explicitly confirmed.
    /// Proposing the same `(kind, text, source_ref)` triple again is idempotent and does
    /// not advance the revision.
    pub fn propose(
        &mut self,
        kind: RequirementKind,
        text: &str,
        source_ref: &str,
    ) -> Result<String> {
        let candidate = TaskRequirement::candidate(kind, text, source_ref)?;
        if let Some(existing) = self.requirements.iter().find(|existing| {
            existing.kind == kind
                && existing.text == text
                && existing.proposed_source_ref == source_ref
        }) {
            return Ok(existing.id.clone());
        }
        if self.requirements.len() >= MAX_REQUIREMENTS {
            return Err(Error::Other("Task requirement limit reached".to_string()));
        }
        let id = candidate.id.clone();
        self.requirements.push(candidate);
        self.revision += 1;
        Ok(id)
    }

    /// Trusted application API. Not registered as a model-callable tool.
    pub fn decide(
        &mut self,
        id: &str,
        status: RequirementStatus,
        decision: RequirementDecision,
        expected_revision: u64,
    ) -> Result<()> {
        self.check_revision(expected_revision)?;
        if status == RequirementStatus::Candidate {
            return Err(Error::Other("Decision must confirm or reject".to_string()));
        }
        decision.validate()?;
        let position = self
            .requirements
            .iter()
            .position(|existing| existing.id == id)
            .ok_or_else(|| Error::Other(format!("Unknown requirement: {id}")))?;
        let existing = self.requirements[position].clone();
        if existing.status == status && existing.decision.as_ref() == Some(&decision) {
            return Ok(());
        }
        let replacement = TaskRequirement::new(
            existing.id,
            existing.kind,
            existing.text,
            existing.proposed_source_ref,
            status,
            Some(decision),
        )?;
        self.requirements[position] = replacement;
        self.revision += 1;
        self.contract_version += 1;
        Ok(())
    }

    /// Pins the checked workspace/artifact version. Callers must update this before
    /// execution whenever the subject changes.
    pub fn set_subject_version(&mut self, version: &str, expected_revision: u64) -> Result<()> {
        self.check_revision(expected_revision)?;
        require_text(version, "subject version", MAX_TEXT_CHARS)?;
        if self.scope.is_none() {
            return Err(Error::Other("Task scope required for evidence".to_string()));
        }
        if self.subject_version.as_deref() == Some(version) {
            return Ok(());
        }
        self.subject_version = Some(version.to_string());
        self.contract_version += 1;
        self.revision += 1;
        Ok(())
    }

    /// Invalidate immediately when a relevant mutation starts; repin only after it settles.
    pub fn invalidate_subject(&mut self, expected_revision: u64) -> Result<()> {
        self.check_revision(expected_revision)?;
        self.subject_version = None;
        self.contract_version += 1;
        self.revision += 1;
        Ok(())
    }

    /// Called after durable verification storage. Does not mark todos or the task completed.
    pub fn record_verification(
        &mut self,
        report: TaskVerification,
        expected_revision: u64,
    ) -> Result<()> {
        self.check_revision(expected_revision)?;
        if self.evidence_binding().as_ref() != Some(&report.binding) {
            return Err(Error::Other(
                "Verification subject or contract changed".to_string(),
            ));
        }
        let confirmed = self.requirements.iter().any(|requirement| {
            requirement.id == report.requirement_id
                && requirement.kind == RequirementKind::AcceptanceCriterion
                && requirement.status == RequirementStatus::Confirmed
        });
        if !confirmed {
            return Err(Error::Other(
                "Confirmed acceptance criterion required".to_string(),
            ));
        }
        self.verifications
            .retain(|previous| previous.requirement_id != report.requirement_id);
        self.verifications.push(report);
        self.revision += 1;
        Ok(())
    }

    /// Re-checks the Java constructor invariants (bounds, unique ids, one report per
    /// requirement). Construction paths maintain them; this exists for values restored
    /// from an untrusted store.
    pub fn validate(&self) -> Result<()> {
        if self.requirements.len() > MAX_REQUIREMENTS {
            return Err(Error::Other(
                "At most 64 uniquely identified requirements are allowed".to_string(),
            ));
        }
        let mut ids = HashSet::new();
        for requirement in &self.requirements {
            requirement.validate()?;
            if !ids.insert(requirement.id.as_str()) {
                return Err(Error::Other(
                    "At most 64 uniquely identified requirements are allowed".to_string(),
                ));
            }
        }
        if let Some(scope) = &self.scope {
            scope.validate()?;
        }
        if let Some(version) = &self.subject_version {
            require_text(version, "subject version", MAX_TEXT_CHARS)?;
        }
        if self.verifications.len() > MAX_REQUIREMENTS {
            return Err(Error::Other(
                "At most one current report per requirement is allowed".to_string(),
            ));
        }
        let mut verified = HashSet::new();
        for report in &self.verifications {
            report.validate()?;
            if !verified.insert(report.requirement_id.as_str()) {
                return Err(Error::Other(
                    "At most one current report per requirement is allowed".to_string(),
                ));
            }
        }
        Ok(())
    }

    fn check_revision(&self, expected_revision: u64) -> Result<()> {
        if self.revision != expected_revision {
            return Err(Error::Other("Task revision changed".to_string()));
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::requirement::RequirementAuthority;
    use crate::state::verification::VerificationOutcome;
    use serde_json::json;

    fn scope(id: &str) -> TaskScope {
        TaskScope::new(id, "ship the migration", "user message 1").unwrap()
    }

    fn todo(subject: &str) -> Task {
        Task::builder().subject(subject).description(subject).build()
    }

    fn report(
        state: &TaskContextState,
        requirement_id: &str,
        id: &str,
    ) -> TaskVerification {
        TaskVerification::new(
            id,
            requirement_id,
            "call-1",
            "cargo test",
            VerificationOutcome::Passed,
            "suite is green",
            state.evidence_binding().unwrap(),
            1_700_000_000_000,
        )
        .unwrap()
    }

    #[test]
    fn replace_tasks_bumps_the_revision_and_rejects_a_stale_one() {
        let mut state = TaskContextState::new();
        assert_eq!(state.revision(), 0);
        state.replace_tasks(vec![todo("step one")], 0).unwrap();
        assert_eq!(state.revision(), 1);
        assert_eq!(state.tasks().len(), 1);

        // A writer still holding revision 0 must not clobber revision 1.
        let error = state.replace_tasks(vec![todo("stale")], 0).unwrap_err();
        assert!(error.to_string().contains("Task revision changed"), "{error}");
        assert_eq!(state.tasks()[0].subject, "step one");
        assert_eq!(state.revision(), 1);
    }

    #[test]
    fn begin_task_resets_progress_and_bumps_the_contract_version() {
        let mut state = TaskContextState::new();
        state.replace_tasks(vec![todo("step one")], 0).unwrap();
        state
            .propose(
                RequirementKind::Constraint,
                "keep tests offline",
                "user message 2",
            )
            .unwrap();

        let revision = state.revision();
        state.begin_task(scope("task-1"), revision).unwrap();
        assert_eq!(state.revision(), revision + 1);
        assert_eq!(state.contract_version(), 1);
        assert!(state.tasks().is_empty());
        assert!(state.requirements().is_empty());
        assert!(state.verifications().is_empty());
        assert!(state.subject_version().is_none());
        assert_eq!(state.scope().unwrap().task_id, "task-1");

        // Re-beginning the identical scope is a no-op.
        let revision = state.revision();
        state.begin_task(scope("task-1"), revision).unwrap();
        assert_eq!(state.revision(), revision);

        // Changing the objective under the same task id is refused.
        let changed = TaskScope::new("task-1", "something else", "user message 9").unwrap();
        let error = state.begin_task(changed, state.revision()).unwrap_err();
        assert!(error.to_string().contains("new task identity"), "{error}");
    }

    #[test]
    fn propose_is_idempotent_and_decide_advances_the_contract() {
        let mut state = TaskContextState::new();
        let id = state
            .propose(
                RequirementKind::AcceptanceCriterion,
                "suite is green",
                "user message 3",
            )
            .unwrap();
        let revision = state.revision();

        // The same (kind, text, source) triple does not create a second candidate.
        let again = state
            .propose(
                RequirementKind::AcceptanceCriterion,
                "suite is green",
                "user message 3",
            )
            .unwrap();
        assert_eq!(again, id);
        assert_eq!(state.revision(), revision);
        let candidate = state.requirement(&id).unwrap();
        assert!(candidate.is_candidate());
        assert!(candidate.decision.is_none());

        let decision = RequirementDecision::new(RequirementAuthority::User, "user message 3").unwrap();
        state
            .decide(&id, RequirementStatus::Confirmed, decision.clone(), revision)
            .unwrap();
        assert_eq!(state.contract_version(), 1);
        let confirmed = state.requirement(&id).unwrap();
        assert_eq!(confirmed.status, RequirementStatus::Confirmed);
        assert_eq!(confirmed.decision.as_ref(), Some(&decision));

        // Re-applying the same decision is a no-op.
        let revision = state.revision();
        state.decide(&id, RequirementStatus::Confirmed, decision, revision).unwrap();
        assert_eq!(state.revision(), revision);
        assert_eq!(state.contract_version(), 1);

        // A candidate is not a valid decision target.
        let error = state
            .decide(
                &id,
                RequirementStatus::Candidate,
                RequirementDecision::new(RequirementAuthority::User, "user message 3").unwrap(),
                revision,
            )
            .unwrap_err();
        assert!(error.to_string().contains("Decision must confirm or reject"), "{error}");

        // Unknown identifiers are refused.
        let error = state
            .decide(
                "req-missing",
                RequirementStatus::Rejected,
                RequirementDecision::new(RequirementAuthority::Caller, "ticket-1").unwrap(),
                revision,
            )
            .unwrap_err();
        assert!(error.to_string().contains("Unknown requirement"), "{error}");
        state.validate().unwrap();
    }

    #[test]
    fn verification_needs_a_confirmed_criterion_and_the_pinned_subject() {
        // No scope means no evidence binding, so nothing may be pinned.
        let mut bare = TaskContextState::new();
        let error = bare.set_subject_version("v1", 0).unwrap_err();
        assert!(error.to_string().contains("Task scope required for evidence"), "{error}");

        let mut state = TaskContextState::new();
        state.begin_task(scope("task-1"), 0).unwrap();
        let revision = state.revision();
        state.set_subject_version("v1", revision).unwrap();
        let binding = state.evidence_binding().unwrap();
        assert_eq!(binding.task_id, "task-1");
        assert_eq!(binding.subject_version, "v1");
        assert_eq!(binding.contract_version, state.contract_version() as i64);

        let id = state
            .propose(
                RequirementKind::AcceptanceCriterion,
                "suite is green",
                "user message 3",
            )
            .unwrap();
        let revision = state.revision();
        state
            .decide(
                &id,
                RequirementStatus::Confirmed,
                RequirementDecision::new(RequirementAuthority::User, "user message 3").unwrap(),
                revision,
            )
            .unwrap();

        let revision = state.revision();
        let first = report(&state, &id, "ver-1");
        state.record_verification(first, revision).unwrap();
        assert_eq!(state.verifications().len(), 1);

        // Recording again for the same requirement replaces the previous report.
        let revision = state.revision();
        let second = report(&state, &id, "ver-2");
        state.record_verification(second, revision).unwrap();
        assert_eq!(state.verifications().len(), 1);
        assert_eq!(state.verifications()[0].id, "ver-2");

        // Evidence recorded against a different subject is refused.
        let revision = state.revision();
        let stale = TaskVerification::new(
            "ver-3",
            id.clone(),
            "call-2",
            "cargo test",
            VerificationOutcome::Failed,
            "red",
            EvidenceBinding::new("task-1", 1, "v0").unwrap(),
            2,
        )
        .unwrap();
        let error = state.record_verification(stale, revision).unwrap_err();
        assert!(
            error.to_string().contains("Verification subject or contract changed"),
            "{error}"
        );

        // Only a confirmed acceptance criterion may carry evidence.
        let constraint = state
            .propose(RequirementKind::Constraint, "keep tests offline", "user message 4")
            .unwrap();
        let revision = state.revision();
        state
            .decide(
                &constraint,
                RequirementStatus::Confirmed,
                RequirementDecision::new(RequirementAuthority::Caller, "ticket-7").unwrap(),
                revision,
            )
            .unwrap();
        let revision = state.revision();
        let wrong_kind = report(&state, &constraint, "ver-4");
        let error = state.record_verification(wrong_kind, revision).unwrap_err();
        assert!(
            error.to_string().contains("Confirmed acceptance criterion required"),
            "{error}"
        );
        state.validate().unwrap();
    }

    #[test]
    fn invalidate_subject_forces_a_repin_before_more_evidence() {
        let mut state = TaskContextState::new();
        state.begin_task(scope("task-1"), 0).unwrap();
        let revision = state.revision();
        state.set_subject_version("v1", revision).unwrap();
        let revision = state.revision();
        state.invalidate_subject(revision).unwrap();
        assert!(state.subject_version().is_none());
        assert!(state.evidence_binding().is_none());
        assert_eq!(state.contract_version(), 3);
    }

    #[test]
    fn legacy_state_without_the_new_fields_still_deserializes() {
        let legacy: TaskContextState = serde_json::from_value(json!({"tasks": []})).unwrap();
        assert_eq!(legacy.revision(), 0);
        assert_eq!(legacy.contract_version(), 0);
        assert!(legacy.scope().is_none());
        assert!(legacy.subject_version().is_none());
        assert!(legacy.requirements().is_empty());
        assert!(legacy.verifications().is_empty());
    }

    #[test]
    fn serde_uses_java_camel_case_keys() {
        let mut state = TaskContextState::new();
        state.begin_task(scope("task-1"), 0).unwrap();
        let revision = state.revision();
        state.set_subject_version("v1", revision).unwrap();

        let value = serde_json::to_value(&state).unwrap();
        assert_eq!(value["revision"], json!(2));
        assert_eq!(value["contractVersion"], json!(2));
        assert_eq!(value["subjectVersion"], json!("v1"));
        assert_eq!(value["scope"]["taskId"], json!("task-1"));
        assert_eq!(value["scope"]["objective"], json!("ship the migration"));
        assert_eq!(value["scope"]["sourceRef"], json!("user message 1"));

        let back: TaskContextState = serde_json::from_value(value).unwrap();
        assert_eq!(back, state);
    }
}
