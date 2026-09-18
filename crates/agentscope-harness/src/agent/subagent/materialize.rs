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
}

impl SubagentBlueprint {
    /// `general-purpose` (Java `buildGeneralPurposeFactory`): parent model and
    /// the full parent toolkit, static leaf so it cannot spawn further.
    pub fn general_purpose_factory(self: &Arc<Self>) -> Arc<dyn SubagentFactory> {
        Arc::new(LeafFactory {
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
        })
    }

    /// Declared subagent (Java `buildDeclaredFactory`); `None` for remote targets.
    pub fn declared_factory(
        self: &Arc<Self>,
        decl: &SubagentDeclaration,
    ) -> Option<Arc<dyn SubagentFactory>> {
        if decl.is_remote() {
            return None;
        }
        Some(Arc::new(LeafFactory {
            blueprint: Arc::clone(self),
            name: decl.name.clone(),
            description: decl.description.clone(),
            sys_prompt: build_subagent_sys_prompt(&decl.prompt),
            toolkit: allowlisted_inherited_toolkit(&self.toolkit, &decl.tools),
            workspace: resolve_declared_workspace(decl, &self.main_workspace),
            max_iters: decl.steps.map(|s| s as i32).unwrap_or(self.max_iters),
            model_override: decl.model.clone(),
            skill_filter: Some(declared_skill_filter(&self.skill_filter, decl)),
            generate_options: declared_generate_options(self.generate_options.as_ref(), decl),
            decl: Some(decl.clone()),
        }))
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
}

impl SubagentFactory for LeafFactory {
    fn create(&self, parent_rc: &RuntimeContext) -> Arc<dyn CallableAgent> {
        let mut builder = HarnessAgent::builder()
            .name(self.name.clone())
            .description(self.description.clone())
            .as_leaf_subagent()
            .toolkit(self.toolkit.copy())
            .workspace(self.workspace.clone())
            .sys_prompt(self.sys_prompt.clone())
            .max_iters(self.max_iters);
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
        Arc::new(builder.build()) as Arc<dyn CallableAgent>
    }
}
