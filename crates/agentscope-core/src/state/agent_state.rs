//! Mutable per-agent runtime state, mirroring `io.agentscope.core.state.AgentState`.
use std::sync::Arc;
use uuid::Uuid;

use crate::interruption::InterruptControl;
use crate::message::Msg;
use crate::permission::PermissionContextState;

use super::context::{PlanModeContextState, TaskContextState, ToolContextState};

/// Mutable per-agent runtime state.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AgentState {
    /// Decodes with an empty id when a snapshot predates the field instead of failing; the
    /// durable adapter mints a fresh id for such a state, because a state that names no session
    /// would be carried forward as an unnamed one.
    #[serde(default)]
    pub session_id: String,
    pub user_id: Option<String>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub context: Arc<Vec<Msg>>,
    #[serde(default)]
    pub reply_id: String,
    #[serde(default)]
    pub cur_iter: i32,
    #[serde(default)]
    pub shutdown_interrupted: bool,
    #[serde(rename = "permission_context", default)]
    pub permission_context: Arc<PermissionContextState>,
    #[serde(rename = "tool_context", default)]
    pub tool_context: Arc<ToolContextState>,
    #[serde(rename = "tasks_context", default)]
    pub tasks_context: Arc<TaskContextState>,
    #[serde(rename = "plan_mode_context", default)]
    pub plan_mode_context: Arc<PlanModeContextState>,
}

fn new_hex() -> String {
    Uuid::new_v4().to_string().replace('-', "")
}

/// A borrowing view of an [`AgentState`] that serializes without its `context`.
///
/// This is what a `state/checkpoint` writes: the conversation is journalled once, as
/// `message/*` facts, and repeating it inside every checkpoint made the checkpoint the largest
/// payload of a session. See [`AgentState::serialize_without_context`].
pub struct SansContext<'a>(pub &'a AgentState);

impl serde::Serialize for SansContext<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize_without_context(serializer)
    }
}

impl Default for AgentState {
    fn default() -> Self {
        Self {
            session_id: new_hex(),
            user_id: None,
            summary: String::new(),
            context: Arc::new(Vec::new()),
            reply_id: new_hex(),
            cur_iter: 0,
            shutdown_interrupted: false,
            permission_context: Arc::new(PermissionContextState::default()),
            tool_context: Arc::new(ToolContextState::new()),
            tasks_context: Arc::new(TaskContextState::new()),
            plan_mode_context: Arc::new(PlanModeContextState::new()),
        }
    }
}

impl AgentState {
    pub fn builder() -> AgentStateBuilder {
        AgentStateBuilder::new()
    }

    /// Serializes this state without its `context`.
    ///
    /// A `state/checkpoint` repeats the conversation whenever it carries `context`, and the
    /// conversation is already the largest part of the journal: every message lives once as a
    /// `message/*` fact. A checkpoint therefore carries only what a replay cannot derive, and
    /// recovery splices the messages back in (see `JournalProjection`). The wrapper keeps that
    /// decision next to the fields it serializes, so a field added to `AgentState` shows up in
    /// one place instead of silently going missing from every checkpoint.
    pub fn serialize_without_context<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut out = serializer.serialize_struct("AgentState", 10)?;
        out.serialize_field("session_id", &self.session_id)?;
        out.serialize_field("user_id", &self.user_id)?;
        out.serialize_field("summary", &self.summary)?;
        out.serialize_field("reply_id", &self.reply_id)?;
        out.serialize_field("cur_iter", &self.cur_iter)?;
        out.serialize_field("shutdown_interrupted", &self.shutdown_interrupted)?;
        out.serialize_field("permission_context", &self.permission_context)?;
        out.serialize_field("tool_context", &self.tool_context)?;
        out.serialize_field("tasks_context", &self.tasks_context)?;
        out.serialize_field("plan_mode_context", &self.plan_mode_context)?;
        out.end()
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
        self.context.as_slice()
    }
    pub fn context_mut(&mut self) -> &mut Vec<Msg> {
        Arc::make_mut(&mut self.context)
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
        self.permission_context = Arc::new(c);
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
    context: Option<Arc<Vec<Msg>>>,
    reply_id: Option<String>,
    cur_iter: i32,
    shutdown_interrupted: bool,
    permission_context: Option<Arc<PermissionContextState>>,
    tool_context: Option<Arc<ToolContextState>>,
    tasks_context: Option<Arc<TaskContextState>>,
    plan_mode_context: Option<Arc<PlanModeContextState>>,
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
        self.context = Some(Arc::new(c));
        self
    }
    pub fn add_message(mut self, m: Msg) -> Self {
        Arc::make_mut(self.context.get_or_insert_with(|| Arc::new(Vec::new()))).push(m);
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
        self.permission_context = Some(Arc::new(c));
        self
    }
    pub fn tool_context(mut self, c: ToolContextState) -> Self {
        self.tool_context = Some(Arc::new(c));
        self
    }
    pub fn tasks_context(mut self, c: TaskContextState) -> Self {
        self.tasks_context = Some(Arc::new(c));
        self
    }
    pub fn plan_mode_context(mut self, c: PlanModeContextState) -> Self {
        self.plan_mode_context = Some(Arc::new(c));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The checkpoint serializer is the one place that decides what a checkpoint leaves out.
    /// If a field is added to `AgentState` and not to it, every checkpoint silently loses that
    /// field and recovery returns a state that differs from the live one. Comparing against the
    /// full serialization makes that omission a test failure instead.
    #[test]
    fn the_checkpoint_form_keeps_every_field_but_the_context() {
        let state = AgentState::builder()
            .session_id("session-1")
            .user_id("user-1")
            .summary("a summary")
            .context(vec![Msg::user("hello")])
            .build();
        let full = serde_json::to_value(&state).expect("full");
        let without = serde_json::to_value(SansContext(&state)).expect("without");
        let mut expected = full.as_object().expect("object").clone();
        expected.remove("context");
        assert_eq!(
            without,
            serde_json::Value::Object(expected),
            "a checkpoint must carry every field except the conversation"
        );
        // The conversation is the one thing missing, and it is not empty in the full form.
        assert!(full.get("context").is_some());
        assert!(without.get("context").is_none());
    }
}
