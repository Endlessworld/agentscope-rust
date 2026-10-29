//! Mutable per-agent runtime state, mirroring `io.agentscope.core.state.AgentState`.
use uuid::Uuid;

use crate::interruption::InterruptControl;
use crate::message::Msg;
use crate::permission::PermissionContextState;

use super::context::{PlanModeContextState, TaskContextState, ToolContextState};

/// Mutable per-agent runtime state.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AgentState {
    pub session_id: String,
    pub user_id: Option<String>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub context: Vec<Msg>,
    pub reply_id: String,
    #[serde(default)]
    pub cur_iter: i32,
    #[serde(default)]
    pub shutdown_interrupted: bool,
    #[serde(rename = "permission_context", default)]
    pub permission_context: PermissionContextState,
    #[serde(rename = "tool_context", default)]
    pub tool_context: ToolContextState,
    #[serde(rename = "tasks_context", default)]
    pub tasks_context: TaskContextState,
    #[serde(rename = "plan_mode_context", default)]
    pub plan_mode_context: PlanModeContextState,
}

fn new_hex() -> String {
    Uuid::new_v4().to_string().replace('-', "")
}

impl Default for AgentState {
    fn default() -> Self {
        Self {
            session_id: new_hex(),
            user_id: None,
            summary: String::new(),
            context: Vec::new(),
            reply_id: new_hex(),
            cur_iter: 0,
            shutdown_interrupted: false,
            permission_context: PermissionContextState::default(),
            tool_context: ToolContextState::new(),
            tasks_context: TaskContextState::new(),
            plan_mode_context: PlanModeContextState::new(),
        }
    }
}

impl AgentState {
    pub fn builder() -> AgentStateBuilder {
        AgentStateBuilder::new()
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    pub fn user_id(&self) -> Option<&str> {
        self.user_id.as_deref()
    }
    pub fn summary(&self) -> &str {
        &self.summary
    }
    pub fn set_summary(&mut self, s: impl Into<String>) {
        self.summary = s.into();
    }
    pub fn context(&self) -> &[Msg] {
        &self.context
    }
    pub fn context_mut(&mut self) -> &mut Vec<Msg> {
        &mut self.context
    }
    pub fn reply_id(&self) -> &str {
        &self.reply_id
    }
    pub fn set_reply_id(&mut self, r: impl Into<String>) {
        self.reply_id = r.into();
    }
    pub fn cur_iter(&self) -> i32 {
        self.cur_iter
    }
    pub fn set_cur_iter(&mut self, v: i32) {
        self.cur_iter = v;
    }
    pub fn shutdown_interrupted(&self) -> bool {
        self.shutdown_interrupted
    }
    pub fn set_shutdown_interrupted(&mut self, v: bool) {
        self.shutdown_interrupted = v;
    }
    pub fn permission_context(&self) -> &PermissionContextState {
        &self.permission_context
    }
    pub fn set_permission_context(&mut self, c: PermissionContextState) {
        self.permission_context = c;
    }
    pub fn tool_context(&self) -> &ToolContextState {
        &self.tool_context
    }
    pub fn tasks_context(&self) -> &TaskContextState {
        &self.tasks_context
    }
    pub fn plan_mode_context(&self) -> &PlanModeContextState {
        &self.plan_mode_context
    }

    /// Per-session interrupt signal (runtime-only, not serialized).
    #[allow(clippy::new_ret_no_self)]
    pub fn interrupt_control(&self) -> InterruptControl {
        InterruptControl::new()
    }

    /// Serialize this state to a pretty-printed JSON string.
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }

    /// Deserialize an [`AgentState`] from a JSON string produced by [`to_json`](Self::to_json).
    pub fn from_json_str(json: &str) -> serde_json::Result<Self> {
        serde_json::from_str(json)
    }
}

/// Builder for [`AgentState`].
#[derive(Debug, Clone, Default)]
pub struct AgentStateBuilder {
    session_id: Option<String>,
    user_id: Option<String>,
    summary: Option<String>,
    context: Option<Vec<Msg>>,
    reply_id: Option<String>,
    cur_iter: i32,
    shutdown_interrupted: bool,
    permission_context: Option<PermissionContextState>,
    tool_context: Option<ToolContextState>,
    tasks_context: Option<TaskContextState>,
    plan_mode_context: Option<PlanModeContextState>,
}

impl AgentStateBuilder {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn session_id(mut self, s: impl Into<String>) -> Self {
        self.session_id = Some(s.into());
        self
    }
    pub fn user_id(mut self, s: impl Into<String>) -> Self {
        self.user_id = Some(s.into());
        self
    }
    pub fn summary(mut self, s: impl Into<String>) -> Self {
        self.summary = Some(s.into());
        self
    }
    pub fn context(mut self, c: Vec<Msg>) -> Self {
        self.context = Some(c);
        self
    }
    pub fn add_message(mut self, m: Msg) -> Self {
        self.context.get_or_insert_with(Vec::new).push(m);
        self
    }
    pub fn reply_id(mut self, s: impl Into<String>) -> Self {
        self.reply_id = Some(s.into());
        self
    }
    pub fn cur_iter(mut self, v: i32) -> Self {
        self.cur_iter = v;
        self
    }
    pub fn shutdown_interrupted(mut self, v: bool) -> Self {
        self.shutdown_interrupted = v;
        self
    }
    pub fn permission_context(mut self, c: PermissionContextState) -> Self {
        self.permission_context = Some(c);
        self
    }
    pub fn tool_context(mut self, c: ToolContextState) -> Self {
        self.tool_context = Some(c);
        self
    }
    pub fn tasks_context(mut self, c: TaskContextState) -> Self {
        self.tasks_context = Some(c);
        self
    }
    pub fn plan_mode_context(mut self, c: PlanModeContextState) -> Self {
        self.plan_mode_context = Some(c);
        self
    }
    pub fn build(self) -> AgentState {
        let def = AgentState::default();
        AgentState {
            session_id: self.session_id.unwrap_or(def.session_id),
            user_id: self.user_id.or(def.user_id),
            summary: self.summary.unwrap_or(def.summary),
            context: self.context.unwrap_or(def.context),
            reply_id: self.reply_id.unwrap_or(def.reply_id),
            cur_iter: self.cur_iter,
            shutdown_interrupted: self.shutdown_interrupted,
            permission_context: self.permission_context.unwrap_or(def.permission_context),
            tool_context: self.tool_context.unwrap_or(def.tool_context),
            tasks_context: self.tasks_context.unwrap_or(def.tasks_context),
            plan_mode_context: self.plan_mode_context.unwrap_or(def.plan_mode_context),
        }
    }
}
