//! Harness middleware, mirroring `io.agentscope.harness.agent.middleware`.
//!
//! System-prompt transformers (compaction, plan mode, `@`-path expansion) plus
//! runtime middlewares that integrate with the harness bus/gateway (inbox
//! draining, async tool offloading). The latter consume the session id from
//! `RuntimeContext` to scope their work.

use agentscope_core::agent::RuntimeContext;
use agentscope_core::middleware::Middleware;

pub mod agent_trace;
pub mod async_tool;
pub mod at_path;
pub mod dynamic_subagents;
pub mod inbox;
pub mod memory_flush;
pub mod memory_maintenance;
pub mod sandbox_lifecycle;
pub mod skill_middleware;
pub mod subagents;
pub mod tool_result_eviction;
pub mod teams;
pub mod transcript;
pub mod workspace_context;

pub use agent_trace::AgentTraceMiddleware;
pub use async_tool::AsyncToolMiddleware;
pub use at_path::AtPathExpansionMiddleware;
pub use dynamic_subagents::DynamicSubagentsMiddleware;
pub use inbox::InboxMiddleware;
pub use memory_flush::MemoryFlushMiddleware;
pub use memory_maintenance::MemoryMaintenanceMiddleware;
pub use sandbox_lifecycle::SandboxLifecycleMiddleware;
pub use skill_middleware::{
    build_skill_entries, HarnessSkillMiddleware, SkillCuratorMiddleware, SkillUsageMiddleware,
};
pub use subagents::{
    build_delivery_reminder, build_task_summary, prepend_to_system_message,
    render_subagent_section, SubagentsMiddleware,
};
pub use tool_result_eviction::{EVICTED_METADATA_KEY, ToolResultEvictionMiddleware};
pub use teams::TeamsMiddleware;
pub use transcript::TranscriptMiddleware;
pub use workspace_context::WorkspaceContextMiddleware;

/// Config for compaction-based conversation trimming.
#[derive(Debug, Clone, Default)]
pub struct CompactionMiddlewareConfig {
    pub enabled: bool,
    pub max_tokens: usize,
}

/// Middleware that appends a compaction reminder to the system prompt.
pub struct CompactionMiddleware {
    config: CompactionMiddlewareConfig,
}
impl CompactionMiddleware {
    pub fn new(config: CompactionMiddlewareConfig) -> Self {
        Self { config }
    }
}

impl Middleware for CompactionMiddleware {
    fn on_system_prompt(&self, _ctx: &RuntimeContext, current_prompt: String) -> String {
        if self.config.enabled {
            format!(
                "{}\n\n[compaction] Max tokens: {}",
                current_prompt, self.config.max_tokens
            )
        } else {
            current_prompt
        }
    }
}

/// Config for plan-mode (read-only exploration) operation.
#[derive(Debug, Clone, Default)]
pub struct PlanModeConfig {
    pub enabled: bool,
    pub plan: String,
}

/// Middleware that injects the current plan into the system prompt.
pub struct PlanModeMiddleware {
    pub config: PlanModeConfig,
}

impl PlanModeMiddleware {
    pub fn new(config: PlanModeConfig) -> Self {
        Self { config }
    }
}

impl Middleware for PlanModeMiddleware {
    fn on_system_prompt(&self, _ctx: &RuntimeContext, current_prompt: String) -> String {
        if self.config.enabled && !self.config.plan.is_empty() {
            format!("{}\n\nCurrent plan:\n{}", current_prompt, self.config.plan)
        } else {
            current_prompt
        }
    }
}
