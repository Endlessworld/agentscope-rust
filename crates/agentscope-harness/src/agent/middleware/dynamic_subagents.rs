//! Dynamic counterpart to `SubagentsMiddleware` that re-resolves the registered
//! subagent set on every reasoning step, mirroring Java `DynamicSubagentsMiddleware`.
//!
//! **Two-layer load**: Layer 2 (base) scans the local workspace `subagents/`
//! directory; Layer 1 (override) globs `subagents/*.md` through the `Filesystem`
//! layer (namespace-isolated). Same-name declarations from Layer 1 override
//! Layer 2, and dynamic declarations override same-name programmatic (static)
//! entries. The merged set is pushed into a `DefaultAgentManager` when present,
//! and the subagent section + async-task summary are prepended to the SYSTEM msg.
use std::collections::BTreeMap;
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use futures::StreamExt;
use agentscope_core::middleware::{Middleware, Next, ReasoningInput};
use agentscope_core::tool::Tool;

use crate::agent::subagent::{
    load_from_directory, load_from_filesystem, DefaultAgentManager, SubagentDeclaration,
    SubagentEntry, SubagentFactory, SUBAGENTS_DIR,
};
use crate::agent::middleware::{
    build_task_summary, prepend_to_system_message, render_subagent_section,
};
use crate::agent::subagent::task::TaskRepository;
use crate::filesystem::Filesystem;
use crate::tool::TaskTool;

/// Re-materializes factories from a declaration; returns `None` to skip.
pub type FactoryBuilder =
    dyn Fn(&SubagentDeclaration) -> Option<Arc<dyn SubagentFactory>> + Send + Sync;

/// Middleware that reloads subagent declarations on every reasoning step.
pub struct DynamicSubagentsMiddleware {
    static_entries: Vec<SubagentEntry>,
    filesystem: Option<Arc<dyn Filesystem>>,
    main_workspace: Option<String>,
    factory_builder: Option<Arc<FactoryBuilder>>,
    agent_manager: Option<Arc<DefaultAgentManager>>,
    task_repository: Arc<dyn TaskRepository>,
    task_tool: Arc<TaskTool>,
    extra_tools: Vec<Arc<dyn Tool>>,
}

impl DynamicSubagentsMiddleware {
    /// Creates a middleware from static entries and a task repository. All other
    /// sources (filesystem, workspace dir, factory builder, manager) are empty.
    pub fn new(
        static_entries: Vec<SubagentEntry>,
        task_repository: Arc<dyn TaskRepository>,
    ) -> Self {
        Self {
            static_entries,
            filesystem: None,
            main_workspace: None,
            factory_builder: None,
            agent_manager: None,
            task_repository: Arc::clone(&task_repository),
            task_tool: TaskTool::new(task_repository),
            extra_tools: Vec::new(),
        }
    }

    /// Enables the namespace-aware filesystem layer (Layer 1 override source).
    pub fn with_filesystem(mut self, filesystem: Arc<dyn Filesystem>) -> Self {
        self.filesystem = Some(filesystem);
        self
    }

    /// Sets the local workspace root scanned for `subagents/*.md` (Layer 2).
    pub fn with_main_workspace(mut self, main_workspace: impl Into<String>) -> Self {
        self.main_workspace = Some(main_workspace.into());
        self
    }

    /// Provides the factory materializer used to turn declarations into entries.
    pub fn with_factory_builder<F>(mut self, builder: F) -> Self
    where
        F: Fn(&SubagentDeclaration) -> Option<Arc<dyn SubagentFactory>> + Send + Sync + 'static,
    {
        self.factory_builder = Some(Arc::new(builder));
        self
    }

    /// Wires a `DefaultAgentManager` that is refreshed with the merged set each step.
    pub fn with_agent_manager(mut self, manager: Arc<DefaultAgentManager>) -> Self {
        self.agent_manager = Some(manager);
        self
    }

    /// Adds extra agent tools (e.g. spawn/send/list) contributed to the toolkit.
    pub fn with_extra_tools(mut self, tools: Vec<Arc<dyn Tool>>) -> Self {
        self.extra_tools = tools;
        self
    }

    /// Returns the tools this middleware contributes to the agent toolkit.
    pub fn get_tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut out: Vec<Arc<dyn Tool>> = Vec::new();
        out.push(self.task_tool.output_tool());
        out.push(self.task_tool.cancel_tool());
        out.push(self.task_tool.list_tool());
        out.extend(self.extra_tools.iter().cloned());
        out
    }

    /// The internal task repository (used for async-task bookkeeping).
    pub fn task_repository(&self) -> &Arc<dyn TaskRepository> {
        &self.task_repository
    }

    /// The internal agent manager, when one was wired.
    pub fn agent_manager(&self) -> Option<Arc<DefaultAgentManager>> {
        self.agent_manager.clone()
    }
}

impl Middleware for DynamicSubagentsMiddleware {
    fn on_reasoning(
        &self,
        ctx: &RuntimeContext,
        input: ReasoningInput,
        next: Next<ReasoningInput>,
    ) -> agentscope_core::middleware::EventStream {
        let rc = ctx.clone();
        let fs = self.filesystem.clone();
        let mw = self.main_workspace.clone();
        let fb = self.factory_builder.clone();
        let mgr = self.agent_manager.clone();
        let repo = Arc::clone(&self.task_repository);
        let static_entries = self.static_entries.clone();

        let fut = async move {
            let merged = reload_entries(
                &rc,
                &static_entries,
                fs.as_deref(),
                mw.as_deref(),
                fb.as_deref(),
            )
            .await;
            if let Some(mgr) = &mgr {
                mgr.replace_agents(&merged);
            }

            let mut addition = String::new();
            if !merged.is_empty() {
                addition.push_str(&render_subagent_section(&merged, false));
            }
            if let Some(summary) = build_task_summary(&*repo, &rc).await {
                addition.push_str(&summary);
            }
            if addition.is_empty() {
                return next(input);
            }
            let rebuilt = prepend_to_system_message(input.messages, &addition);
            next(ReasoningInput::new(rebuilt, input.tools, input.options))
        };
        Box::pin(futures::stream::once(fut).flatten())
    }
}

/// Reloads the merged subagent set: Layer 2 (workspace dir) then Layer 1
/// (filesystem), dynamic overriding static on name conflicts.
async fn reload_entries(
    rc: &RuntimeContext,
    static_entries: &[SubagentEntry],
    filesystem: Option<&dyn Filesystem>,
    main_workspace: Option<&str>,
    factory_builder: Option<&FactoryBuilder>,
) -> Vec<SubagentEntry> {
    // ---- Layer 2 (base): local workspace scan ----
    let mut decls: BTreeMap<String, SubagentDeclaration> = BTreeMap::new();
    if let Some(mw) = main_workspace {
        let subagents_dir = format!("{}/{SUBAGENTS_DIR}", mw.trim_end_matches('/'));
        for d in load_from_directory(&subagents_dir, main_workspace) {
            decls.insert(d.name.clone(), d);
        }
    }

    // ---- Layer 1 (override): filesystem with namespace ----
    if let Some(fs) = filesystem {
        for d in load_from_filesystem(fs, rc, main_workspace).await {
            decls.insert(d.name.clone(), d);
        }
    }

    // ---- Materialise factories (skip when no builder is available) ----
    let mut dynamic_entries = Vec::new();
    if let Some(fb) = factory_builder {
        for decl in decls.values() {
            if let Some(factory) = fb(decl) {
                dynamic_entries.push(
                    SubagentEntry::new(decl.name.clone(), decl.description.clone(), factory)
                        .with_declaration(decl.clone()),
                );
            }
        }
    }

    // ---- Combine: static + dynamic, dynamic wins on name conflict ----
    let mut combined: BTreeMap<String, SubagentEntry> = BTreeMap::new();
    for e in static_entries {
        combined.insert(e.name.clone(), e.clone());
    }
    for e in dynamic_entries {
        combined.insert(e.name.clone(), e);
    }
    combined.into_values().collect()
}
