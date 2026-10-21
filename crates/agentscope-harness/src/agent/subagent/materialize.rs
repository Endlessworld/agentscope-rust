//! Builder-side materialisation of subagent declarations into executable leaf
//! agents, mirroring `HarnessAgentBuilderSupport.buildDeclaredFactory` /
//! `buildGeneralPurposeFactory` / `allowlistedInheritedToolkit` /
//! `deriveChildSessionId`.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentscope_core::agent::{CallableAgent, RuntimeContext};
use agentscope_core::model::{GenerateOptions, Model};
use agentscope_core::state::AgentStateStore;
use agentscope_core::tool::Toolkit;

use super::{SubagentDeclaration, SubagentFactory, SubagentWorkspaceMode};
use crate::agent::memory::compaction::CompactionConfig;
use crate::agent::memory::{MemoryConfig, ToolResultEvictionConfig};
use crate::agent::skill::SkillFilter;
use crate::agent::HarnessAgent;


/// Subagent context section injected into every subagent's system prompt.
/// Establishes identity, rules, output format and prohibited behaviours for a leaf
/// worker (mirrors Java `HarnessAgentBuilderSupport.SUBAGENT_CONTEXT_SECTION`).
pub const SUBAGENT_CONTEXT_SECTION: &str = "# Subagent Context

You are a **subagent** spawned by the main agent for a specific task.

## Your Role
- Complete the assigned task. That's your entire purpose.
- You are NOT the main agent. Don't try to be.

## Rules
1. **Stay focused** — Do your assigned task, nothing else
2. **Complete the task** — Your final message will be automatically reported to the main agent
3. **Don't initiate** — No heartbeats, no proactive actions, no side quests
4. **Be ephemeral** — You may be terminated after task completion. That's fine.
5. **Recover from truncated tool output** — If you see `[truncated: output exceeded context limit]`, re-read only what you need using smaller chunks (read with offset/limit, or targeted grep/head/tail) instead of full re-reads

## Output Format
When complete, your final response should include:
- What you accomplished or found
- Any relevant details the main agent should know
- Keep it concise but informative

## What You DON'T Do
- NO user conversations (that's the main agent's job)
- NO spawning further subagents — you are a leaf worker
- NO pretending to be the main agent
- Return plain text results; let the main agent deliver them to the user";

/// Appends [`SUBAGENT_CONTEXT_SECTION`] to `base`; a blank base yields the section alone.
pub fn build_subagent_sys_prompt(base: &str) -> String {
    let base = base.trim_end();
    if base.is_empty() {
        SUBAGENT_CONTEXT_SECTION.to_string()
    } else {
        format!("{base}\n\n{SUBAGENT_CONTEXT_SECTION}")
    }
}

/// Copy of the parent toolkit narrowed to `allow`; an empty allowlist inherits
/// every tool (mirrors Java `allowlistedInheritedToolkit`).
pub fn allowlisted_inherited_toolkit(parent: &Toolkit, allow: &[String]) -> Toolkit {
    let toolkit = parent.copy();
    if allow.is_empty() {
        return toolkit;
    }
    for name in toolkit.tool_names() {
        if !allow.iter().any(|a| a == &name) {
            toolkit.remove_tool(&name);
        }
    }
    toolkit
}

/// `{name}[@{parentSessionId}][#{userId}]`, dropping blank segments; SHARED
/// workspace subagents keep the legacy single-bucket form (mirrors Java
/// `deriveChildSessionId`).
pub fn derive_child_session_id(decl: &SubagentDeclaration, parent_rc: &RuntimeContext) -> String {
    if decl.workspace_mode == SubagentWorkspaceMode::Shared {
        return decl.name.clone();
    }
    let sid = sanitize_identifier(parent_rc.session_id());
    let uid = sanitize_identifier(parent_rc.user_id());
    if sid.is_none() && uid.is_none() {
        return decl.name.clone();
    }
    let mut out = decl.name.clone();
    if let Some(sid) = sid {
        out.push('@');
        out.push_str(&sid);
    }
    if let Some(uid) = uid {
        out.push('#');
        out.push_str(&uid);
    }
    out
}

/// Replaces path-hostile characters so the id stays usable as a store key.
fn sanitize_identifier(s: Option<&str>) -> Option<String> {
    let s = s?.trim();
    if s.is_empty() {
        return None;
    }
    Some(
        s.chars()
            .map(|c| {
                if c == '/' || c == '\\' || c.is_whitespace() || c.is_control() {
                    '_'
                } else {
                    c
                }
            })
            .collect(),
    )
}

/// Resolves the declared subagent's system-prompt base.
///
/// Mirrors Java `resolveDeclaredSysPromptBase`: a declaration workspace reads
/// `<workspace_path>/AGENTS.md`; otherwise the inline prompt body is used.
fn resolve_declared_sys_prompt_base(decl: &SubagentDeclaration) -> String {
    let Some(workspace_path) = &decl.workspace_path else {
        return decl.prompt.clone();
    };
    let agents_md = Path::new(workspace_path).join("AGENTS.md");
    if !agents_md.is_file() {
        return String::new();
    }
    match std::fs::read_to_string(&agents_md) {
        Ok(content) => content,
        Err(error) => {
            tracing::warn!(
                subagent = %decl.name,
                path = %agents_md.display(),
                %error,
                "failed to read subagent AGENTS.md; using an empty base prompt"
            );
            String::new()
        }
    }
}

/// Runtime workspace root for a declared subagent: SHARED reuses the main
/// workspace, ISOLATED gets `agents/<name>/workspace` (mirrors Java
/// `resolveDeclaredWorkspace`).
pub fn resolve_declared_workspace(decl: &SubagentDeclaration, main_workspace: &Path) -> PathBuf {
    if let Some(path) = &decl.workspace_path {
        if decl.workspace_mode == SubagentWorkspaceMode::Shared {
            return main_workspace.to_path_buf();
        }
        return PathBuf::from(path);
    }
    if decl.workspace_mode == SubagentWorkspaceMode::Shared {
        return main_workspace.to_path_buf();
    }
    let isolated = main_workspace.join("agents").join(&decl.name).join("workspace");
    let _ = std::fs::create_dir_all(&isolated);
    isolated
}

/// Resolves the effective model for a declared subagent (mirrors Java `resolveModel`): a blank
/// override inherits the parent model; a failing/absent resolver falls back to it as well.
pub fn resolve_declared_model(
    model_override: Option<&str>,
    parent_model: Option<&Arc<dyn Model>>,
    resolver: Option<&(dyn Fn(&str) -> Option<Arc<dyn Model>> + Send + Sync)>,
    subagent_name: &str,
) -> Option<Arc<dyn Model>> {
    let Some(model_id) = model_override.map(str::trim).filter(|s| !s.is_empty()) else {
        return parent_model.cloned();
    };
    let Some(resolver) = resolver else {
        tracing::warn!(
            "subagent '{subagent_name}': model override '{model_id}' ignored (no model resolver \
             configured), falling back to the parent model"
        );
        return parent_model.cloned();
    };
    match resolver(model_id) {
        Some(model) => Some(model),
        None => {
            tracing::warn!(
                "subagent '{subagent_name}': failed to resolve model '{model_id}', falling back \
                 to the parent model"
            );
            parent_model.cloned()
        }
    }
}

/// Skill filter for a declared subagent (mirrors Java `buildDeclaredFactory`): the declaration's
/// allowlist narrowed by the parent filter, or the parent filter itself when it names none.
pub fn declared_skill_filter(
    parent: &SkillFilter,
    decl: &SubagentDeclaration,
) -> SkillFilter {
    if decl.skills.is_empty() {
        return parent.clone();
    }
    let allowed: Vec<&str> = decl
        .skills
        .iter()
        .map(String::as_str)
        .filter(|name| parent.is_allowed(name))
        .collect();
    SkillFilter::only(&allowed)
}

/// Per-call generation defaults for a declared subagent (mirrors Java `buildDeclaredFactory`):
/// `decl.temperature` / `decl.top_p` overlay the parent's options; unset fields fall back.
pub fn declared_generate_options(
    parent: Option<&GenerateOptions>,
    decl: &SubagentDeclaration,
) -> Option<GenerateOptions> {
    let overlay = (decl.temperature.is_some() || decl.top_p.is_some()).then(|| {
        let mut b = GenerateOptions::builder();
        b.temperature = decl.temperature;
        b.top_p = decl.top_p;
        b.build()
    });
    match (overlay, parent) {
        (Some(overlay), Some(parent)) => Some(GenerateOptions::merge_options(&overlay, parent)),
        (Some(overlay), None) => Some(overlay),
        (None, parent) => parent.cloned(),
    }
}

/// Immutable snapshot of the parent build used to materialise children.
#[derive(Clone)]
pub struct SubagentBlueprint {
    pub model: Option<Arc<dyn Model>>,
    /// Turns a `decl.model` override id into a model (mirrors Java `Builder.modelResolver`).
    pub model_resolver: Option<Arc<dyn Fn(&str) -> Option<Arc<dyn Model>> + Send + Sync>>,
    /// Parent skill filter that children inherit (mirrors Java `capturedSkillFilter`).
    pub skill_filter: SkillFilter,
    /// Parent per-call generation defaults children inherit (Java `capturedGenOpts`).
    pub generate_options: Option<GenerateOptions>,
    pub toolkit: Toolkit,
    pub cwd: Option<PathBuf>,
    pub main_workspace: PathBuf,
    pub max_iters: i32,
    pub state_store: Option<Arc<dyn AgentStateStore>>,
    /// Parent default children inherit unless their declaration overrides it
    /// (mirrors Java `capturedPendingToolRecovery`).
    pub enable_pending_tool_recovery: bool,
    /// Memory capability flags inherited from the parent
    /// (mirrors Java `capturedDisableMemoryTools` / `capturedDisableMemoryHooks`).
    pub disable_memory_tools: bool,
    pub disable_memory_hooks: bool,
    /// Java `capturedMemoryConfig`: inherited by both general-purpose and declared
    /// children.
    pub memory_config: Option<MemoryConfig>,
    /// Java `capturedDisableWorkspaceContext`: general-purpose children inherit it.
    pub disable_workspace_context: bool,
    /// Java `configurePlanMode`: both general-purpose and declared children inherit
    /// the parent's plan-mode configuration.
    pub plan_mode_enabled: bool,
    pub plan_mode_allow_shell: bool,
    pub plan_file_dir: String,
    /// Java `capturedCompactionConfig` / `capturedDisableCompaction`: general-purpose
    /// children inherit the parent's conversation-compaction settings.
    pub compaction_config: Option<CompactionConfig>,
    pub disable_compaction: bool,
    /// Java `capturedToolResultEvictionConfig` / `capturedDisableToolResultEviction`:
    /// general-purpose children inherit the parent's eviction settings.
    pub tool_result_eviction_config: Option<ToolResultEvictionConfig>,
    pub disable_tool_result_eviction: bool,
    /// Java `capturedAgentTracingLogEnabled`: general-purpose children inherit it.
    pub agent_tracing_log_enabled: bool,
    /// Java `capturedAdditionalContextFiles`: general-purpose children inherit them.
    pub additional_context_files: Vec<String>,
    /// Java `capturedMaxContextTokens`: general-purpose children inherit it.
    pub max_context_tokens: usize,
    /// Java `capturedEnvMemory`: general-purpose children inherit it.
    pub environment_memory: Option<String>,
    /// Java propagates this to both child kinds (even though the flag is currently
    /// not consumed by the main build path).
    pub use_legacy_xml_workspace_context: bool,
    /// Mirrors Java `capturedDisableSessionPersistence`; both child kinds inherit it.
    pub disable_session_persistence: bool,
}

impl SubagentBlueprint {
    /// `general-purpose` (Java `buildGeneralPurposeFactory`): parent model and
    /// the full parent toolkit, static leaf so it cannot spawn further.
    pub fn general_purpose_factory(self: &Arc<Self>) -> Arc<dyn SubagentFactory> {
        Arc::new(self.general_purpose_leaf())
    }

    /// Leaf configuration for the built-in `general-purpose` subagent, split out of
    /// [`Self::general_purpose_factory`] so a test can inspect the resulting agent.
    fn general_purpose_leaf(self: &Arc<Self>) -> LeafFactory {
        LeafFactory {
            blueprint: Arc::clone(self),
            name: "general-purpose-subagent".into(),
            description: "General-purpose subagent for isolated task execution".into(),
            sys_prompt: build_subagent_sys_prompt(""),
            toolkit: self.toolkit.copy(),
            workspace: self.main_workspace.clone(),
            max_iters: self.max_iters,
            model_override: None,
            skill_filter: Some(self.skill_filter.clone()),
            generate_options: self.generate_options.clone(),
            decl: None,
            enable_pending_tool_recovery: self.enable_pending_tool_recovery,
        }
    }

    /// Declared subagent (Java `buildDeclaredFactory`); `None` for remote targets.
    pub fn declared_factory(
        self: &Arc<Self>,
        decl: &SubagentDeclaration,
    ) -> Option<Arc<dyn SubagentFactory>> {
        Some(Arc::new(self.declared_leaf(decl)?))
    }

    /// Leaf configuration for a declaration, split out of [`Self::declared_factory`] so a
    /// test can inspect the resulting `HarnessAgent` (the factory trait erases the type).
    fn declared_leaf(
        self: &Arc<Self>,
        decl: &SubagentDeclaration,
    ) -> Option<LeafFactory> {
        if decl.is_remote() {
            return None;
        }
        Some(LeafFactory {
            blueprint: Arc::clone(self),
            name: decl.name.clone(),
            description: decl.description.clone(),
            sys_prompt: build_subagent_sys_prompt(&resolve_declared_sys_prompt_base(decl)),
            toolkit: allowlisted_inherited_toolkit(&self.toolkit, &decl.tools),
            workspace: resolve_declared_workspace(decl, &self.main_workspace),
            max_iters: decl.steps.map(|s| s as i32).unwrap_or(self.max_iters),
            model_override: decl.model.clone(),
            skill_filter: Some(declared_skill_filter(&self.skill_filter, decl)),
            generate_options: declared_generate_options(self.generate_options.as_ref(), decl),
            // A declaration may override the parent default (Java: `decl.getEnablePendingToolRecovery() != null ? ... : b.enablePendingToolRecovery`).
            enable_pending_tool_recovery: decl
                .enable_pending_tool_recovery
                .unwrap_or(self.enable_pending_tool_recovery),
            decl: Some(decl.clone()),
        })
    }
}

/// Builds one leaf `HarnessAgent` per spawn from the captured blueprint.
struct LeafFactory {
    blueprint: Arc<SubagentBlueprint>,
    name: String,
    description: String,
    sys_prompt: String,
    toolkit: Toolkit,
    workspace: PathBuf,
    max_iters: i32,
    /// Model id from the declaration; `None` inherits the parent model.
    model_override: Option<String>,
    /// Skill filter handed to the child (parent filter, narrowed by `decl.skills`).
    skill_filter: Option<SkillFilter>,
    /// Per-call generation defaults handed to the child.
    generate_options: Option<GenerateOptions>,
    /// `None` for `general-purpose` (no parent-identity bucketing).
    decl: Option<SubagentDeclaration>,
    /// Effective pending-tool recovery flag for the child (declaration override or
    /// parent default). Mirrors Java `capturedPendingToolRecovery`.
    enable_pending_tool_recovery: bool,
}

impl LeafFactory {
    /// Builds the child agent. Split out of [`SubagentFactory::create`] so a test can
    /// read the concrete `HarnessAgent` configuration (the trait returns
    /// `Arc<dyn CallableAgent>`, which has no downcast hook).
    fn build(&self, parent_rc: &RuntimeContext) -> HarnessAgent {
        let mut builder = HarnessAgent::builder()
            .name(self.name.clone())
            .description(self.description.clone())
            .as_leaf_subagent()
            .toolkit(self.toolkit.copy())
            .workspace(self.workspace.clone())
            .sys_prompt(self.sys_prompt.clone())
            .max_iters(self.max_iters)
            .enable_pending_tool_recovery(self.enable_pending_tool_recovery);
        let model = resolve_declared_model(
            self.model_override.as_deref(),
            self.blueprint.model.as_ref(),
            self.blueprint.model_resolver.as_ref().map(|r| r.as_ref()),
            &self.name,
        );
        if let Some(model) = model {
            builder = builder.model(model);
        }
        if let Some(filter) = &self.skill_filter {
            builder = builder.skill_filter(filter.clone());
        }
        if let Some(options) = &self.generate_options {
            builder = builder.generate_options_defaults(options.clone());
        }
        if let Some(cwd) = &self.blueprint.cwd {
            builder = builder.cwd(cwd.clone());
        }
        if let Some(store) = &self.blueprint.state_store {
            builder = builder.state_store(Arc::clone(store));
        }
        if let Some(decl) = &self.decl {
            builder = builder.session_id(derive_child_session_id(decl, parent_rc));
        }
        // Java `configurePlanMode` propagates plan mode to both child kinds.
        if self.blueprint.plan_mode_enabled {
            builder = builder
                .enable_plan_mode()
                .plan_file_directory(self.blueprint.plan_file_dir.clone());
            if self.blueprint.plan_mode_allow_shell {
                builder = builder.allow_shell_in_plan_mode();
            }
        }
        // Java propagates the memory pipeline config to both child kinds.
        if let Some(memory_config) = &self.blueprint.memory_config {
            builder = builder.memory(memory_config.clone());
        }
        if self.blueprint.use_legacy_xml_workspace_context {
            builder = builder.use_legacy_xml_workspace_context(true);
        }
        if self.blueprint.disable_session_persistence {
            builder = builder.disable_session_persistence();
        }

        // The following were captured only by Java's `buildGeneralPurposeFactory`.
        // Declared children keep the parent defaults, matching Java's current
        // asymmetry.
        if self.decl.is_none() {
            if self.blueprint.disable_workspace_context {
                builder = builder.disable_workspace_context();
            }
            if let Some(environment_memory) = &self.blueprint.environment_memory {
                builder = builder.environment_memory(environment_memory.clone());
            }
            for relative_path in &self.blueprint.additional_context_files {
                builder = builder.additional_context_file(relative_path.clone());
            }
            builder = builder
                .max_context_tokens(self.blueprint.max_context_tokens)
                .enable_agent_tracing_log(self.blueprint.agent_tracing_log_enabled);

            if self.blueprint.disable_compaction {
                builder = builder.disable_compaction();
            } else if let Some(config) = &self.blueprint.compaction_config {
                builder = builder.compaction(config.clone());
            }

            if self.blueprint.disable_tool_result_eviction {
                builder = builder.disable_tool_result_eviction();
            } else if let Some(config) = &self.blueprint.tool_result_eviction_config {
                builder = builder.tool_result_eviction(config.clone());
            }
        }

        // Java `HarnessAgentBuilderSupport` propagates both memory flags to every
        // declared / general-purpose child. Do the same so a child prompt never
        // advertises memory tools or hooks the parent turned off.
        if self.blueprint.disable_memory_tools {
            builder = builder.disable_memory_tools();
        }
        if self.blueprint.disable_memory_hooks {
            builder = builder.disable_memory_hooks();
        }
        builder.build()
    }
}

impl SubagentFactory for LeafFactory {
    fn create(&self, parent_rc: &RuntimeContext) -> Arc<dyn CallableAgent> {
        Arc::new(self.build(parent_rc))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::{ContentBlock, Msg, TextBlock};
    use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};

    /// A model that always answers with a fixed text block.
    struct StubModel;

    #[async_trait::async_trait]
    impl Model for StubModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<ToolSchema>,
            _options: GenerateOptions,
        ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
            Box::pin(futures::stream::once(async {
                Ok(ChatResponse::builder()
                    .content(vec![ContentBlock::Text(TextBlock::new("ok"))])
                    .build())
            }))
        }

        fn model_name(&self) -> &str {
            "stub"
        }
    }

    /// Isolated scratch dir so a child build never touches the repository tree.
    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agentscope_subagent_recovery_{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// A blueprint carrying the parent's pending-tool-recovery default.
    fn blueprint(parent_flag: bool) -> Arc<SubagentBlueprint> {
        let dir = scratch();
        Arc::new(SubagentBlueprint {
            model: Some(Arc::new(StubModel) as Arc<dyn Model>),
            model_resolver: None,
            skill_filter: SkillFilter::only(&[]),
            generate_options: None,
            toolkit: Toolkit::new(),
            cwd: Some(dir.clone()),
            main_workspace: dir,
            max_iters: 5,
            state_store: None,
            enable_pending_tool_recovery: parent_flag,
            disable_memory_tools: false,
            disable_memory_hooks: false,
            memory_config: None,
            disable_workspace_context: false,
            plan_mode_enabled: false,
            plan_mode_allow_shell: false,
            plan_file_dir: "plans".into(),
            compaction_config: None,
            disable_compaction: false,
            tool_result_eviction_config: None,
            disable_tool_result_eviction: false,
            agent_tracing_log_enabled: true,
            additional_context_files: Vec::new(),
            max_context_tokens: 8000,
            environment_memory: None,
            use_legacy_xml_workspace_context: false,
            disable_session_persistence: false,
        })
    }

    /// The flag a declared child would actually run with.
    fn declared_child_flag(parent_flag: bool, decl_flag: Option<bool>) -> bool {
        let mut decl = SubagentDeclaration::new("worker", "does work", "You work.");
        decl.enable_pending_tool_recovery = decl_flag;
        blueprint(parent_flag)
            .declared_leaf(&decl)
            .expect("a local declaration yields a leaf")
            .build(&RuntimeContext::new())
            .agent()
            .expect("the child is built with a model")
            .enable_pending_tool_recovery()
    }

    #[test]
    fn disable_session_persistence_is_inherited() {
        let mut blueprint = (*blueprint(false)).clone();
        blueprint.disable_session_persistence = true;
        let child = Arc::new(blueprint)
            .general_purpose_leaf()
            .build(&RuntimeContext::new());
        assert!(child.disable_session_persistence());
    }

    #[test]
    fn declared_workspace_reads_agents_md_for_the_system_prompt() {
        let workspace = scratch().join("declared_agents_md");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            workspace.join("AGENTS.md"),
            "# Workspace Persona\nUse the project conventions.",
        )
        .unwrap();

        let mut decl = SubagentDeclaration::new("worker", "does work", "");
        decl.workspace_path = Some(workspace.to_string_lossy().to_string());
        let child = blueprint(false)
            .declared_leaf(&decl)
            .expect("a local declaration yields a leaf")
            .build(&RuntimeContext::new());
        let prompt = child.agent().expect("the child is built with a model").sys_prompt();
        assert!(prompt.contains("Workspace Persona"), "{prompt}");
        assert!(prompt.contains("Use the project conventions."), "{prompt}");
        assert!(prompt.contains("Subagent Context"), "{prompt}");
    }

    #[test]
    fn declared_inline_prompt_is_used_without_a_workspace() {
        let mut decl = SubagentDeclaration::new("worker", "does work", "Inline Persona");
        decl.workspace_path = None;
        let child = blueprint(false)
            .declared_leaf(&decl)
            .expect("a local declaration yields a leaf")
            .build(&RuntimeContext::new());
        let prompt = child.agent().expect("the child is built with a model").sys_prompt();
        assert!(prompt.contains("Inline Persona"), "{prompt}");
        assert!(prompt.contains("Subagent Context"), "{prompt}");
    }

    #[test]
    fn declared_child_inherits_the_parent_default() {
        assert!(
            declared_child_flag(true, None),
            "a declaration without an override must inherit the parent's true"
        );
        assert!(
            !declared_child_flag(false, None),
            "a declaration without an override must inherit the parent's false"
        );
    }

    #[test]
    fn declaration_override_beats_the_parent_default() {
        assert!(
            declared_child_flag(false, Some(true)),
            "declaration true must beat parent false"
        );
        assert!(
            !declared_child_flag(true, Some(false)),
            "declaration false must beat parent true"
        );
    }

    #[test]
    fn general_purpose_child_inherits_the_parent_default() {
        for flag in [true, false] {
            let child = blueprint(flag)
                .general_purpose_leaf()
                .build(&RuntimeContext::new());
            assert_eq!(
                child
                    .agent()
                    .expect("the child is built with a model")
                    .enable_pending_tool_recovery(),
                flag,
                "general-purpose must inherit the parent flag ({flag})"
            );
        }
    }
}
