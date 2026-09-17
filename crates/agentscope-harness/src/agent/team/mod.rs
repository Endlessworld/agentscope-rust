//! Agent teams coordination, mirroring `io.agentscope.harness.agent.team`.
//!
//! Provides the [`TeamClient`] contract (task board + mailbox coordination),
//! the data model (`TeamTask` / `TeamMessage` / member & spec types) and
//! [`LocalTeamClient`], a single-node implementation using versioned CAS
//! updates (the Java `LocalTeamClient` is backed by `BaseStore.putIfVersion`;
//! here the same semantics are provided by an in-process store with per-key
//! versions). A distributed store-backed client can be layered on later.
//!
//! Conflict detection mirrors Java `TeamConflictException`: optimistic
//! concurrent updates that lose the race surface as [`TeamError::Conflict`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use thiserror::Error;

/// Team coordination errors (Java `TeamConflictException` and friends).
#[derive(Debug, Error)]
pub enum TeamError {
    /// Optimistic-concurrency loss: the record changed under us.
    #[error("team conflict on `{key}`: expected version {expected}, actual {actual}")]
    Conflict { key: String, expected: u64, actual: u64 },
    #[error("team `{0}` not found")]
    TeamNotFound(String),
    #[error("task `{0}` not found")]
    TaskNotFound(String),
    #[error("member `{0}` not found")]
    MemberNotFound(String),
    #[error("task `{0}` is blocked by unfinished dependencies: {1:?}")]
    Blocked(String, Vec<String>),
    #[error("{0}")]
    Other(String),
}

pub type TeamResult<T> = std::result::Result<T, TeamError>;

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

/// Role of a member within a team.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TeamRole {
    Lead,
    #[default]
    Member,
}

impl TeamRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            TeamRole::Lead => "LEAD",
            TeamRole::Member => "MEMBER",
        }
    }
}

/// Lifecycle of a team task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TeamTaskStatus {
    #[default]
    Todo,
    InProgress,
    Done,
    Failed,
}

impl TeamTaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TeamTaskStatus::Todo => "TODO",
            TeamTaskStatus::InProgress => "IN_PROGRESS",
            TeamTaskStatus::Done => "DONE",
            TeamTaskStatus::Failed => "FAILED",
        }
    }
}

/// A task on the shared team board.
#[derive(Debug, Clone)]
pub struct TeamTask {
    pub id: String,
    pub team_id: String,
    pub subject: String,
    pub description: String,
    pub status: TeamTaskStatus,
    pub assignee: Option<String>,
    /// Ids of tasks that must complete before this one can start.
    pub blocked_by: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Optimistic-concurrency version (incremented on every mutation).
    pub version: u64,
}

/// A message in a member's mailbox.
#[derive(Debug, Clone)]
pub struct TeamMessage {
    pub id: String,
    pub team_id: String,
    pub from: String,
    /// `None` = broadcast to every member.
    pub to: Option<String>,
    pub content: String,
    pub created_at: DateTime<Utc>,
}

/// One member of a team.
#[derive(Debug, Clone)]
pub struct TeamMemberInfo {
    pub name: String,
    pub role: TeamRole,
    pub active: bool,
}

/// Aggregate view of a team.
#[derive(Debug, Clone, Default)]
pub struct TeamInfo {
    pub id: String,
    pub name: String,
    pub members: Vec<TeamMemberInfo>,
}

/// Spec used to create a team.
#[derive(Debug, Clone, Default)]
pub struct TeamCreateSpec {
    pub id: String,
    pub name: String,
    /// Members to seed; the first LEAD member becomes the lead.
    pub members: Vec<TeamMemberSpec>,
}

/// Spec for a single member (Java `TeamMemberSpec`).
#[derive(Debug, Clone)]
pub struct TeamMemberSpec {
    pub name: String,
    pub role: TeamRole,
}

impl TeamMemberSpec {
    pub fn new(name: impl Into<String>, role: TeamRole) -> Self {
        Self { name: name.into(), role }
    }
}

/// The calling agent's view of its team (role + what it may do).
#[derive(Debug, Clone, Default)]
pub struct TeamContext {
    pub team_id: Option<String>,
    /// This agent's member name on the team (used by `teamsMode` wiring).
    pub member: Option<String>,
    pub role: Option<TeamRole>,
    /// Operation names permitted for this role, e.g. `["claim_task", ...]`.
    pub available_operations: Vec<String>,
}

impl TeamContext {
    /// Builds a context for `member` in `team_id` with the given role.
    pub fn new(team_id: impl Into<String>, member: impl Into<String>, role: TeamRole) -> Self {
        let mut ctx = Self::for_role(&team_id.into(), role);
        ctx.member = Some(member.into());
        ctx
    }

    /// Role-clipped operation set (leads may administer membership).
    pub fn for_role(team_id: &str, role: TeamRole) -> Self {
        let mut ops = vec![
            "list_tasks",
            "create_task",
            "assign_task",
            "claim_task",
            "complete_task",
            "fail_task",
            "send_message",
            "broadcast_message",
        ];
        if role == TeamRole::Lead {
            ops.extend_from_slice(&["spawn_member", "shutdown_member"]);
        }
        Self {
            team_id: Some(team_id.to_string()),
            member: None,
            role: Some(role),
            available_operations: ops.into_iter().map(String::from).collect(),
        }
    }

    pub fn allows(&self, op: &str) -> bool {
        self.available_operations.iter().any(|o| o == op)
    }
}

/// Wakeup notifications delivered when messages arrive (Java `TeamWakeups`).
#[derive(Clone, Default)]
pub struct TeamWakeups {
    rx: Arc<tokio::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<TeamMessage>>>>,
}

impl TeamWakeups {
    /// Waits for the next wakeup, or `None` when the channel closed.
    pub async fn recv(&self) -> Option<TeamMessage> {
        let mut guard = self.rx.lock().await;
        let rx = guard.as_mut()?;
        rx.recv().await
    }
}

// ---------------------------------------------------------------------------
// Client contract
// ---------------------------------------------------------------------------

/// Coordination client over a team task board + mailboxes (Java `TeamClient`).
pub trait TeamClient: Send + Sync {
    /// Creates a team from a spec and returns its info.
    fn create_team(&self, spec: TeamCreateSpec) -> TeamResult<TeamInfo>;

    /// Info about a team, if it exists.
    fn team_info(&self, team_id: &str) -> TeamResult<TeamInfo>;

    /// Creates a task on the board (status TODO, unassigned).
    fn create_task(
        &self,
        team_id: &str,
        subject: &str,
        description: &str,
        blocked_by: Vec<String>,
    ) -> TeamResult<TeamTask>;

    /// Assigns a task to a member (CAS-guarded on the task version).
    fn assign_task(
        &self,
        team_id: &str,
        task_id: &str,
        assignee: &str,
        expected_version: u64,
    ) -> TeamResult<TeamTask>;

    /// Claims a task for self (TODO → IN_PROGRESS); fails when blocked.
    fn claim_task(&self, team_id: &str, task_id: &str, claimant: &str) -> TeamResult<TeamTask>;

    /// Completes a task.
    fn complete_task(&self, team_id: &str, task_id: &str, by: &str) -> TeamResult<TeamTask>;

    /// Marks a task failed.
    fn fail_task(&self, team_id: &str, task_id: &str, by: &str) -> TeamResult<TeamTask>;

    /// Lists all tasks on the board.
    fn list_tasks(&self, team_id: &str) -> TeamResult<Vec<TeamTask>>;

    /// Sends a direct message to a member (delivers a wakeup).
    fn send_message(&self, team_id: &str, from: &str, to: &str, content: &str)
        -> TeamResult<TeamMessage>;

    /// Broadcasts to every active member except `from`.
    fn broadcast_message(
        &self,
        team_id: &str,
        from: &str,
        content: &str,
    ) -> TeamResult<Vec<TeamMessage>>;

    /// Drains the member's mailbox.
    fn drain_mailbox(&self, team_id: &str, member: &str) -> Vec<TeamMessage>;

    /// Registers a wakeup receiver for a member.
    fn subscribe_wakeups(&self, team_id: &str, member: &str) -> TeamWakeups;

    /// Adds a member at runtime (lead-only in Java role clipping).
    fn spawn_member(&self, team_id: &str, spec: TeamMemberSpec) -> TeamResult<TeamMemberInfo>;

    /// Deactivates a member (their mailbox is preserved).
    fn shutdown_member(&self, team_id: &str, member: &str) -> TeamResult<()>;
}

// ---------------------------------------------------------------------------
// Local implementation (versioned CAS store)
// ---------------------------------------------------------------------------

struct TeamBoard {
    members: HashMap<String, TeamMemberInfo>,
    tasks: HashMap<String, TeamTask>,
    mailboxes: HashMap<String, Vec<TeamMessage>>,
    wakeups: HashMap<String, tokio::sync::mpsc::UnboundedSender<TeamMessage>>,
}

/// Single-node [`TeamClient`] with per-task versioned CAS updates.
pub struct LocalTeamClient {
    boards: Mutex<HashMap<String, Arc<Mutex<TeamBoard>>>>,
    names: Mutex<HashMap<String, String>>,
}

impl LocalTeamClient {
    pub fn new() -> Self {
        Self {
            boards: Mutex::new(HashMap::new()),
            names: Mutex::new(HashMap::new()),
        }
    }

    fn next_id(prefix: &str) -> String {
        thread_local! {
            static COUNTER: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
        }
        let n = COUNTER.with(|c| {
            c.set(c.get() + 1);
            c.get()
        });
        format!("{prefix}-{n}")
    }

    /// Runs `f` with exclusive access to the team board, mapping a missing
    /// team to [`TeamError::TeamNotFound`].
    fn with_board<T>(
        &self,
        team_id: &str,
        f: impl FnOnce(&mut TeamBoard) -> TeamResult<T>,
    ) -> TeamResult<T> {
        let entry = self
            .boards
            .lock()
            .unwrap()
            .get(team_id)
            .cloned()
            .ok_or_else(|| TeamError::TeamNotFound(team_id.to_string()))?;
        let mut board = entry.lock().unwrap();
        f(&mut board)
    }

    /// CAS update of a task under the board lock; bumps version + timestamp.
    fn cas_update<F>(&self, team_id: &str, task_id: &str, expected_version: u64, mutate: F) -> TeamResult<TeamTask>
    where
        F: FnOnce(&mut TeamTask),
    {
        self.with_board(team_id, |board| {
            let task = board
                .tasks
                .get_mut(task_id)
                .ok_or_else(|| TeamError::TaskNotFound(task_id.to_string()))?;
            if task.version != expected_version {
                return Err(TeamError::Conflict {
                    key: format!("{team_id}/{task_id}"),
                    expected: expected_version,
                    actual: task.version,
                });
            }
            mutate(task);
            task.version += 1;
            task.updated_at = Utc::now();
            Ok(task.clone())
        })
    }

    fn deliver(board: &mut TeamBoard, msg: TeamMessage) {
        if let Some(to) = &msg.to {
            board.mailboxes.entry(to.clone()).or_default().push(msg.clone());
            if let Some(tx) = board.wakeups.get(to) {
                let _ = tx.send(msg);
            }
        }
    }
}

impl Default for LocalTeamClient {
    fn default() -> Self {
        Self::new()
    }
}

impl TeamClient for LocalTeamClient {
    fn create_team(&self, spec: TeamCreateSpec) -> TeamResult<TeamInfo> {
        let mut board = TeamBoard {
            members: HashMap::new(),
            tasks: HashMap::new(),
            mailboxes: HashMap::new(),
            wakeups: HashMap::new(),
        };
        let members: Vec<TeamMemberInfo> = spec
            .members
            .iter()
            .map(|m| TeamMemberInfo { name: m.name.clone(), role: m.role, active: true })
            .collect();
        for m in &members {
            board.members.insert(m.name.clone(), m.clone());
        }
        let info = TeamInfo { id: spec.id.clone(), name: spec.name.clone(), members };
        self.names.lock().unwrap().insert(spec.id.clone(), spec.name);
        self.boards
            .lock()
            .unwrap()
            .insert(spec.id, Arc::new(Mutex::new(board)));
        Ok(info)
    }

    fn team_info(&self, team_id: &str) -> TeamResult<TeamInfo> {
        let name = self
            .names
            .lock()
            .unwrap()
            .get(team_id)
            .cloned()
            .ok_or_else(|| TeamError::TeamNotFound(team_id.to_string()))?;
        self.with_board(team_id, |board| {
            let mut members: Vec<TeamMemberInfo> = board.members.values().cloned().collect();
            members.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(TeamInfo { id: team_id.to_string(), name, members })
        })
    }

    fn create_task(
        &self,
        team_id: &str,
        subject: &str,
        description: &str,
        blocked_by: Vec<String>,
    ) -> TeamResult<TeamTask> {
        let now = Utc::now();
        let task = TeamTask {
            id: Self::next_id("task"),
            team_id: team_id.to_string(),
            subject: subject.to_string(),
            description: description.to_string(),
            status: TeamTaskStatus::Todo,
            assignee: None,
            blocked_by,
            created_at: now,
            updated_at: now,
            version: 1,
        };
        self.with_board(team_id, |board| {
            board.tasks.insert(task.id.clone(), task.clone());
            Ok(task.clone())
        })
    }

    fn assign_task(
        &self,
        team_id: &str,
        task_id: &str,
        assignee: &str,
        expected_version: u64,
    ) -> TeamResult<TeamTask> {
        self.with_board(team_id, |board| {
            if !board.members.contains_key(assignee) {
                return Err(TeamError::MemberNotFound(assignee.to_string()));
            }
            Ok(())
        })?;
        self.cas_update(team_id, task_id, expected_version, |t| {
            t.assignee = Some(assignee.to_string());
        })
    }

    fn claim_task(&self, team_id: &str, task_id: &str, claimant: &str) -> TeamResult<TeamTask> {
        self.with_board(team_id, |board| {
            let blocked: Vec<String> = {
                let task = board
                    .tasks
                    .get(task_id)
                    .ok_or_else(|| TeamError::TaskNotFound(task_id.to_string()))?;
                task.blocked_by
                    .iter()
                    .filter(|dep| {
                        board
                            .tasks
                            .get(*dep)
                            .map(|d| d.status != TeamTaskStatus::Done)
                            .unwrap_or(true)
                    })
                    .cloned()
                    .collect()
            };
            if !blocked.is_empty() {
                return Err(TeamError::Blocked(task_id.to_string(), blocked));
            }
            let task = board
                .tasks
                .get_mut(task_id)
                .ok_or_else(|| TeamError::TaskNotFound(task_id.to_string()))?;
            task.assignee = Some(claimant.to_string());
            task.status = TeamTaskStatus::InProgress;
            task.version += 1;
            task.updated_at = Utc::now();
            Ok(task.clone())
        })
    }

    fn complete_task(&self, team_id: &str, task_id: &str, by: &str) -> TeamResult<TeamTask> {
        // Read current version outside CAS so concurrent writers surface as conflicts.
        let version = self.current_version(team_id, task_id)?;
        self.cas_update(team_id, task_id, version, |t| {
            t.status = TeamTaskStatus::Done;
            t.assignee.get_or_insert_with(|| by.to_string());
        })
    }

    fn fail_task(&self, team_id: &str, task_id: &str, _by: &str) -> TeamResult<TeamTask> {
        let version = self.current_version(team_id, task_id)?;
        self.cas_update(team_id, task_id, version, |t| {
            t.status = TeamTaskStatus::Failed;
        })
    }

    fn list_tasks(&self, team_id: &str) -> TeamResult<Vec<TeamTask>> {
        self.with_board(team_id, |board| {
            let mut tasks: Vec<TeamTask> = board.tasks.values().cloned().collect();
            tasks.sort_by_key(|t| t.created_at);
            Ok(tasks)
        })
    }

    fn send_message(
        &self,
        team_id: &str,
        from: &str,
        to: &str,
        content: &str,
    ) -> TeamResult<TeamMessage> {
        let _ = from;
        self.with_board(team_id, |board| {
            if !board.members.contains_key(to) {
                return Err(TeamError::MemberNotFound(to.to_string()));
            }
            let msg = TeamMessage {
                id: Self::next_id("msg"),
                team_id: team_id.to_string(),
                from: from.to_string(),
                to: Some(to.to_string()),
                content: content.to_string(),
                created_at: Utc::now(),
            };
            Self::deliver(board, msg.clone());
            Ok(msg)
        })
    }

    fn broadcast_message(
        &self,
        team_id: &str,
        from: &str,
        content: &str,
    ) -> TeamResult<Vec<TeamMessage>> {
        self.with_board(team_id, |board| {
            let recipients: Vec<String> = board
                .members
                .values()
                .filter(|m| m.active && m.name != from)
                .map(|m| m.name.clone())
                .collect();
            let mut sent = Vec::with_capacity(recipients.len());
            for to in recipients {
                let msg = TeamMessage {
                    id: Self::next_id("msg"),
                    team_id: team_id.to_string(),
                    from: from.to_string(),
                    to: Some(to),
                    content: content.to_string(),
                    created_at: Utc::now(),
                };
                Self::deliver(board, msg.clone());
                sent.push(msg);
            }
            Ok(sent)
        })
    }

    fn drain_mailbox(&self, team_id: &str, member: &str) -> Vec<TeamMessage> {
        self.with_board(team_id, |board| {
            Ok(board.mailboxes.remove(member).unwrap_or_default())
        })
        .unwrap_or_default()
    }

    fn subscribe_wakeups(&self, team_id: &str, member: &str) -> TeamWakeups {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = self.with_board(team_id, |board| {
            board.wakeups.insert(member.to_string(), tx);
            Ok(())
        });
        TeamWakeups { rx: Arc::new(tokio::sync::Mutex::new(Some(rx))) }
    }

    fn spawn_member(&self, team_id: &str, spec: TeamMemberSpec) -> TeamResult<TeamMemberInfo> {
        self.with_board(team_id, |board| {
            let info = TeamMemberInfo { name: spec.name.clone(), role: spec.role, active: true };
            board.members.insert(spec.name, info.clone());
            Ok(info)
        })
    }

    fn shutdown_member(&self, team_id: &str, member: &str) -> TeamResult<()> {
        self.with_board(team_id, |board| {
            let m = board
                .members
                .get_mut(member)
                .ok_or_else(|| TeamError::MemberNotFound(member.to_string()))?;
            m.active = false;
            board.wakeups.remove(member);
            Ok(())
        })
    }
}

impl LocalTeamClient {
    fn current_version(&self, team_id: &str, task_id: &str) -> TeamResult<u64> {
        self.with_board(team_id, |board| {
            board
                .tasks
                .get(task_id)
                .map(|t| t.version)
                .ok_or_else(|| TeamError::TaskNotFound(task_id.to_string()))
        })
    }
}
