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
pub mod conversation_compaction;
pub mod dynamic_subagents;
pub mod inbox;
pub mod memory_flush;
pub mod memory_maintenance;
pub mod plan_mode;
pub mod sandbox_lifecycle;
pub mod skill_middleware;
pub mod state_checkpoint;
pub mod subagents;
pub mod teams;
pub mod tool_call_sanitizer;
pub mod tool_result_eviction;
pub mod workspace_context;

pub use agent_trace::AgentTraceMiddleware;
pub use async_tool::AsyncToolMiddleware;
pub use at_path::AtPathExpansionMiddleware;
pub use conversation_compaction::ConversationCompactionMiddleware;
pub use dynamic_subagents::DynamicSubagentsMiddleware;
pub use inbox::InboxMiddleware;
pub use memory_flush::MemoryFlushMiddleware;
pub use memory_maintenance::MemoryMaintenanceMiddleware;
pub use sandbox_lifecycle::SandboxLifecycleMiddleware;
pub use skill_middleware::{
    build_skill_entries, HarnessSkillMiddleware, SkillCuratorMiddleware, SkillUsageMiddleware,
};
pub use state_checkpoint::{StateCheckpointMiddleware, AGENT_STATE_KEY};
pub use subagents::{
    build_delivery_reminder, build_task_summary, prepend_to_system_message,
    render_subagent_section, SubagentsMiddleware,
};
pub use teams::TeamsMiddleware;
pub use tool_call_sanitizer::ToolCallPairSanitizerMiddleware;
pub use tool_result_eviction::{ToolResultEvictionMiddleware, EVICTED_METADATA_KEY};
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

#[async_trait::async_trait]
impl Middleware for CompactionMiddleware {
    async fn on_system_prompt(&self, _ctx: &RuntimeContext, current_prompt: String) -> String {
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
///
/// Moved to [`plan_mode::PlanModeConfig`]; re-exported so existing import paths
/// (`agentscope_harness::agent::middleware::PlanModeConfig`) keep working.
pub use plan_mode::{PlanModeConfig, PlanModeMiddleware};
