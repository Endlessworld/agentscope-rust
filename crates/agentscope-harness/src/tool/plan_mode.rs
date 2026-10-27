//! `PlanModeTools` — plan_enter / plan_write / plan_exit, mirroring
//! `io.agentscope.harness.agent.tool.PlanModeTools`.
//!
//! The three tools share one `Arc<tokio::sync::Mutex<AgentState>>` so the
//! plan-mode flag and the current plan file path stay consistent across calls.
//! Wiring to the session's real `AgentState` happens when `RuntimeContext`
//! gains an agent-state binding (see root tracker 2.4); until then the shared
//! state is owned by the tool set, which is sufficient for single-agent usage.
use std::sync::{Arc, Mutex};

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::state::AgentState;
use agentscope_core::tool::{Tool, ToolExecutionContext};
use async_trait::async_trait;

use crate::agent::workspace::PlanModeManager;
use crate::filesystem::Filesystem;

/// Shared handle behind the three plan-mode tools.
#[derive(Clone)]
pub struct PlanModeHandle {
    manager: Arc<PlanModeManager>,
    state: Arc<Mutex<AgentState>>,
}

impl PlanModeHandle {
    /// Builds a handle over a private state (standalone / test use).
    pub fn new(fs: Arc<dyn Filesystem>, plan_dir: Option<&str>) -> Self {
        Self {
            manager: Arc::new(PlanModeManager::new(fs, plan_dir)),
            state: Arc::new(Mutex::new(AgentState::default())),
        }
    }

    /// Builds a handle bound to an existing (session) state handle, so plan-mode
    /// changes persist with the session and are visible to the acting phase.
    pub fn with_state(
        fs: Arc<dyn Filesystem>,
        plan_dir: Option<&str>,
        state: Arc<Mutex<AgentState>>,
    ) -> Self {
        Self {
            manager: Arc::new(PlanModeManager::new(fs, plan_dir)),
            state,
        }
    }

    pub fn manager(&self) -> &PlanModeManager {
        &self.manager
    }

    /// The shared agent-state handle behind these tools.
    pub fn state(&self) -> &Arc<Mutex<AgentState>> {
        &self.state
    }

    /// Whether plan mode is currently active.
    pub fn is_plan_active(&self) -> bool {
        let state = self.state.lock().unwrap();
        self.manager.is_plan_active(&state)
    }

    pub fn register(self, toolkit: &agentscope_core::tool::Toolkit) {
        toolkit.register_tool(Arc::new(PlanEnterTool::new(self.clone())));
        toolkit.register_tool(Arc::new(PlanWriteTool::new(self.clone())));
        toolkit.register_tool(Arc::new(PlanExitTool::new(self)));
    }
}

/// Enters plan mode and reports the plan file path.
pub struct PlanEnterTool {
    handle: PlanModeHandle,
}

impl PlanEnterTool {
    pub fn new(handle: PlanModeHandle) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl Tool for PlanEnterTool {
    fn name(&self) -> &str {
        "plan_enter"
    }

    fn description(&self) -> &str {
        "Enter plan mode: switch the agent into planning (research + plan writing) before \
         implementation. Returns the workspace-relative plan file path."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn call(&self, _args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let mut state = self.handle.state.lock().unwrap();
        let path = self.handle.manager.enter(&mut state);
        Ok(ToolResultBlock::text(format!(
            "Entered plan mode. Write your plan with plan_write to `{path}`, then call \
             plan_exit when done."
        )))
    }
}

/// Writes (or overwrites) the plan file.
pub struct PlanWriteTool {
    handle: PlanModeHandle,
}

impl PlanWriteTool {
    pub fn new(handle: PlanModeHandle) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl Tool for PlanWriteTool {
    fn name(&self) -> &str {
        "plan_write"
    }

    fn description(&self) -> &str {
        "Write the full markdown content of the current plan. Overwrites any previous \
         version of the plan file."
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "content": {"type": "string", "description": "Full markdown plan content"}
            },
            "required": ["content"]
        })
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let Some(content) = args.get("content").and_then(|v| v.as_str()) else {
            return Ok(ToolResultBlock::error("Missing string argument: content"));
        };
        let rt = RuntimeContext::default();
        // Compute/record the path under the lock, then release it before the async file
        // write (the std guard is not held across the await).
        let path = {
            let mut state = self.handle.state.lock().unwrap();
            let path = self.handle.manager.plan_file_path(&state);
            state.plan_mode_context.current_plan_file = Some(path.clone());
            path
        };
        match self
            .handle
            .manager
            .write_plan_file(&rt, &path, content)
            .await
        {
            Ok(()) => Ok(ToolResultBlock::text(format!("Plan written to {path}"))),
            Err(e) => Ok(ToolResultBlock::error(e.to_string())),
        }
    }
}

/// Exits plan mode.
pub struct PlanExitTool {
    handle: PlanModeHandle,
}

impl PlanExitTool {
    pub fn new(handle: PlanModeHandle) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl Tool for PlanExitTool {
    fn name(&self) -> &str {
        "plan_exit"
    }

    fn description(&self) -> &str {
        "Exit plan mode and return to normal execution. The plan file is kept."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn call(&self, _args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let mut state = self.handle.state.lock().unwrap();
        let file = self.handle.manager.plan_file_path(&state);
        self.handle.manager.exit(&mut state);
        Ok(ToolResultBlock::text(format!(
            "Exited plan mode. Plan preserved at {file}."
        )))
    }
}
