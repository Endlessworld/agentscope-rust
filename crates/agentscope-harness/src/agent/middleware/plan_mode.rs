//! Plan-mode (read-only exploration) middleware.
//!
//! Mirrors `io.agentscope.harness.agent.middleware.PlanModeMiddleware`.
//!
//! The mode is **dynamic**: `PlanModeManager` reads `AgentState.plan_mode_context`,
//! so `plan_enter` / `plan_exit` can flip it mid-run. Java therefore refreshes the
//! instruction at *every model boundary* (not just once when the system prompt is
//! built), and registers the text as a `ContextItem.instruction("mode_rules", ...)`
//! so the context compiler can budget/evict it like any other item.
//!
//! Rust does not have `WorkspaceContextMaterials`, and plan-mode tool interception
//! lives in core (`react_agent.rs`), so this middleware only owns the **model-side
//! instruction**: the PLAN banner while active, and the "switched to BUILD" hint
//! once a plan file exists.

use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::{ContentBlock, Msg, MsgRole, TextBlock};
use agentscope_core::middleware::{EventStream, ExtensionPoint, Middleware, ModelCallInput, Next};
use agentscope_core::state::AgentState;

use crate::agent::workspace::PlanModeManager;

/// Banner shown while PLAN mode is active. Kept verbatim from Java
/// `PlanModeMiddleware.PLAN_BANNER_TEMPLATE`.
pub const PLAN_BANNER_TEMPLATE: &str = "
PLAN MODE is active (read-only). Plan file: {}
Investigate the problem and draft a plan, but do NOT modify files, run mutating commands,
or otherwise change state. Record your plan with the plan_write tool. When the plan is
complete, call plan_exit to ask the user for approval; only after approval will you return
to BUILD mode and be able to make changes.
ACT, do not just narrate: when you decide to record or finish the plan, call plan_write
(or plan_exit) in the SAME step — never say you \"will write the plan\" without actually
calling the tool, and never claim a plan exists unless you have called plan_write.
If you cannot produce a concrete plan because you lack information (for example the system
you were asked to work on is not present in this workspace), STOP and ask the user one
specific clarifying question instead of inventing a plan or assuming details.
";

/// Hint emitted after switching from PLAN back to BUILD while a plan file exists.
/// Mirrors Java `BUILD_MODE_PLAN_HINT`.
pub const BUILD_MODE_PLAN_HINT: &str = "

You have switched from PLAN to BUILD mode; the read-only restriction is lifted. A plan
reference exists at {} — this path alone is not proof of approval. Read it for the
details, then EXECUTE it step by step until the task is complete. Do NOT stop after
merely producing the plan. If a todo list is available, capture the plan's steps with
the todo_write tool and keep exactly one task in_progress as you work through them.
";

/// Extra hint listing tools additionally allowed during PLAN mode for read-only
/// investigation. Mirrors Java `PLAN_EXTRA_TOOLS_HINT`.
pub const PLAN_EXTRA_TOOLS_HINT: &str = "

The following tool(s) are additionally available during PLAN mode for read-only
investigation: {}. Use them ONLY to read/inspect (e.g. cat, ls, grep, git
log/diff/show/status). Do NOT run mutating commands (file writes, installs, git commit,
rm, mv, network side effects, etc.) until the plan is approved.
";

/// Config for plan-mode (read-only exploration) operation.
///
/// `manager` is the dynamic source of truth; `plan` is retained only as a static
/// fallback for callers that build the middleware without an agent filesystem.
#[derive(Debug, Clone, Default)]
pub struct PlanModeConfig {
    pub enabled: bool,
    /// Static plan text. Prefer [`PlanModeConfig::manager`]: plan mode can be
    /// toggled at runtime, so a compile-time string cannot describe it.
    pub plan: String,
    /// Shared plan-mode coordinator; when present, the instruction is recomputed
    /// from `AgentState` on every system-prompt / model-call boundary.
    ///
    /// Not `Debug`-significant for behaviour, but kept in the struct so the
    /// default derive still works.
    pub manager: Option<Arc<PlanModeManager>>,
    /// Tools additionally allowed for read-only investigation while PLAN is active.
    pub additional_allowed: Vec<String>,
}

impl PlanModeConfig {
    /// Config driven by a live [`PlanModeManager`].
    pub fn with_manager(manager: Arc<PlanModeManager>) -> Self {
        Self {
            enabled: true,
            plan: String::new(),
            manager: Some(manager),
            additional_allowed: Vec::new(),
        }
    }

    /// Declares extra tools permitted in PLAN mode (read-only investigation only).
    pub fn with_additional_allowed(mut self, tools: impl IntoIterator<Item = String>) -> Self {
        self.additional_allowed = tools.into_iter().collect();
        self
    }
}

/// Middleware that injects the current plan-mode instruction into the model input.
///
/// Tool interception itself lives in core; this middleware exists so the model is
/// actually told which mode it is in — without it plan mode silently degrades to
/// "tools get denied and the model never learns why".
pub struct PlanModeMiddleware {
    pub config: PlanModeConfig,
}

impl PlanModeMiddleware {
    pub fn new(config: PlanModeConfig) -> Self {
        Self { config }
    }

    /// Computes the mode instruction for the agent state reachable from `ctx`.
    ///
    /// Mirrors Java `modeInstructions`: PLAN → banner (+ optional extra-tools hint),
    /// BUILD → hint only when a plan file has been recorded.
    pub fn mode_instructions(&self, ctx: &RuntimeContext) -> String {
        if !self.config.enabled {
            return String::new();
        }
        let Some(manager) = self.config.manager.as_ref() else {
            // No manager: fall back to the static plan text (legacy behaviour).
            return if self.config.plan.trim().is_empty() {
                String::new()
            } else {
                format!("\n\nCurrent plan:\n{}", self.config.plan)
            };
        };
        let Some(state) = ctx.state.clone() else {
            return String::new();
        };
        let instructions = {
            // Hold the lock only for the read; never across an await.
            let guard = match state.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            self.instructions_for(manager, &guard)
        };
        instructions
    }

    fn instructions_for(&self, manager: &PlanModeManager, state: &AgentState) -> String {
        if manager.is_plan_active(state) {
            let path = manager.plan_file_path(state);
            let mut banner = PLAN_BANNER_TEMPLATE.replace("{}", &path);
            if !self.config.additional_allowed.is_empty() {
                banner.push_str(
                    &PLAN_EXTRA_TOOLS_HINT
                        .replace("{}", &self.config.additional_allowed.join(", ")),
                );
            }
            return banner;
        }
        let plan_file = state.plan_mode_context.current_plan_file.as_deref();
        match plan_file.map(str::trim).filter(|s| !s.is_empty()) {
            Some(file) => BUILD_MODE_PLAN_HINT.replace("{}", file),
            None => String::new(),
        }
    }

    fn append_instruction(&self, ctx: &RuntimeContext, current_prompt: String) -> String {
        let instructions = self.mode_instructions(ctx);
        if instructions.trim().is_empty() {
            current_prompt
        } else {
            format!("{current_prompt}\n{instructions}")
        }
    }
}

#[async_trait::async_trait]
impl Middleware for PlanModeMiddleware {
    async fn on_system_prompt(&self, ctx: &RuntimeContext, current_prompt: String) -> String {
        self.append_instruction(ctx, current_prompt)
    }

    /// Refreshes the mode rules at every model boundary so a mid-call
    /// `plan_enter` / `plan_exit` takes effect on the next request.
    ///
    /// Java re-runs `onSystemPrompt` here; Rust has no shared system-prompt slot
    /// on `ModelCallInput`, so the equivalent is prepending a system message —
    /// the mode rules must reach the model on the very next request either way.
    fn on_model_call(
        &self,
        ctx: &RuntimeContext,
        mut input: ModelCallInput,
        next: Next<ModelCallInput>,
    ) -> EventStream {
        let refreshed = self.mode_instructions(ctx);
        if refreshed.trim().is_empty() {
            return next(input);
        }
        if !input
            .messages
            .iter()
            .any(|m| m.role == MsgRole::System && carries(&refreshed, m))
        {
            let msg = Msg::builder()
                .role(MsgRole::System)
                .content(vec![ContentBlock::Text(TextBlock { text: refreshed })])
                .build();
            input.messages.insert(0, msg);
        }
        next(input)
    }

    /// Narrow declaration, mirroring Java `PlanModeMiddleware.activePoints()`.
    fn active_points(&self) -> Vec<ExtensionPoint> {
        vec![ExtensionPoint::OnSystemPrompt, ExtensionPoint::OnModelCall]
    }
}

/// True when `msg` already carries `instructions` verbatim.
fn carries(instructions: &str, msg: &Msg) -> bool {
    msg.content.iter().any(|block| match block {
        ContentBlock::Text(tb) => tb.text.contains(instructions.trim()),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager() -> Arc<PlanModeManager> {
        Arc::new(PlanModeManager::new(
            Arc::new(crate::filesystem::LocalFilesystem::new(std::env::temp_dir())),
            Some("plans"),
        ))
    }

    fn ctx_with(state: Arc<std::sync::Mutex<AgentState>>) -> RuntimeContext {
        let mut ctx = RuntimeContext::default();
        ctx.state = Some(state);
        ctx
    }

    #[test]
    fn banner_is_emitted_while_plan_mode_is_active() {
        let mgr = manager();
        let mut state = AgentState::default();
        mgr.enter(&mut state);
        let mw = PlanModeMiddleware::new(PlanModeConfig::with_manager(Arc::clone(&mgr)));
        let prompt = mw.mode_instructions(&ctx_with(Arc::new(std::sync::Mutex::new(state))));
        assert!(
            prompt.contains("PLAN MODE is active (read-only). Plan file:"),
            "expected the PLAN banner, got: {prompt}"
        );
        assert!(
            prompt.contains("plans/PLAN.md"),
            "banner must carry the plan path"
        );
    }

    #[test]
    fn build_mode_emits_the_switch_hint_only_when_a_plan_file_exists() {
        let mgr = manager();
        let mut state = AgentState::default();
        mgr.enter(&mut state);
        mgr.exit(&mut state);
        let mw = PlanModeMiddleware::new(PlanModeConfig::with_manager(Arc::clone(&mgr)));
        let prompt = mw.mode_instructions(&ctx_with(Arc::new(std::sync::Mutex::new(state))));
        assert!(!prompt.contains("PLAN MODE is active"));
        assert!(
            prompt.contains("You have switched from PLAN to BUILD mode"),
            "expected the BUILD hint, got: {prompt}"
        );
    }

    #[test]
    fn build_mode_without_a_plan_file_is_silent() {
        let mgr = manager();
        let state = AgentState::default();
        let mw = PlanModeMiddleware::new(PlanModeConfig::with_manager(Arc::clone(&mgr)));
        let prompt = mw.mode_instructions(&ctx_with(Arc::new(std::sync::Mutex::new(state))));
        assert!(prompt.trim().is_empty(), "expected silence, got: {prompt}");
    }

    #[tokio::test]
    async fn on_system_prompt_appends_the_banner() {
        let mgr = manager();
        let mut state = AgentState::default();
        mgr.enter(&mut state);
        let mw = PlanModeMiddleware::new(PlanModeConfig::with_manager(Arc::clone(&mgr)));
        let out = mw
            .on_system_prompt(
                &ctx_with(Arc::new(std::sync::Mutex::new(state))),
                "base".to_string(),
            )
            .await;
        assert!(out.starts_with("base"));
        assert!(out.contains("PLAN MODE is active (read-only)"));
    }

    #[test]
    fn active_points_declares_system_prompt_and_model_call() {
        let mw = PlanModeMiddleware::new(PlanModeConfig::default());
        let points = mw.active_points();
        assert!(points.contains(&ExtensionPoint::OnSystemPrompt));
        assert!(points.contains(&ExtensionPoint::OnModelCall));
        assert!(!points.contains(&ExtensionPoint::OnActing));
    }

    #[test]
    fn disabled_config_stays_silent() {
        let mgr = manager();
        let mut state = AgentState::default();
        mgr.enter(&mut state);
        let mw = PlanModeMiddleware::new(PlanModeConfig {
            enabled: false,
            manager: Some(Arc::clone(&mgr)),
            ..Default::default()
        });
        let prompt = mw.mode_instructions(&ctx_with(Arc::new(std::sync::Mutex::new(state))));
        assert!(prompt.is_empty());
    }

    #[test]
    fn extra_tools_hint_is_appended_when_declared() {
        let mgr = manager();
        let mut state = AgentState::default();
        mgr.enter(&mut state);
        let mw = PlanModeMiddleware::new(
            PlanModeConfig::with_manager(Arc::clone(&mgr))
                .with_additional_allowed(vec!["git_log".to_string()]),
        );
        let prompt = mw.mode_instructions(&ctx_with(Arc::new(std::sync::Mutex::new(state))));
        assert!(prompt.contains("git_log"));
        assert!(prompt.contains("additionally available during PLAN mode"));
    }
}
