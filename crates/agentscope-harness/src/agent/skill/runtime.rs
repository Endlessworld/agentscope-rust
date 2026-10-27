//! Aggregates the skill load tool and the skill prompt builder, mirroring
//! `io.agentscope.harness.agent.skill.runtime.SkillRuntime`.
//!
//! The filtered catalog is the authorization boundary for `load_skill_through_path`:
//! a skill absent from the call's snapshot cannot be loaded even while another session
//! exposes it. Rust middlewares receive an immutable `&RuntimeContext`, so the runtime
//! binds the snapshot into `RuntimeContext.metadata` (see [`install`](SkillRuntime::install))
//! for hosts that own a mutable context, and retains a legacy process-wide catalog for
//! deprecated context-free callers.
use std::sync::{Arc, Mutex};

use agentscope_core::agent::RuntimeContext;
use agentscope_core::tool::Toolkit;

use super::load_tool::LoadSkillTool;
use super::model::{SkillCatalog, SkillCatalogSnapshot, SkillFilter};
use super::prompt::SkillPromptBuilder;

/// Metadata key under which a [`SkillRuntime`] binds the per-call catalog snapshot.
pub const SKILL_CATALOG_METADATA_KEY: &str = "agentscope_skill_catalog";

/// Aggregates the skill prompt builder and the skill load tool.
#[derive(Clone)]
pub struct SkillRuntime {
    legacy_catalog: Arc<Mutex<SkillCatalog>>,
    prompt_builder: Arc<SkillPromptBuilder>,
}

impl SkillRuntime {
    pub fn new() -> Self {
        Self::with_prompt_builder(SkillPromptBuilder::new())
    }

    pub fn with_prompt_builder(prompt_builder: SkillPromptBuilder) -> Self {
        Self {
            legacy_catalog: Arc::new(Mutex::new(SkillCatalog::empty())),
            prompt_builder: Arc::new(prompt_builder),
        }
    }

    /// Returns the catalog installed through the deprecated context-free API.
    pub fn current_catalog(&self) -> SkillCatalog {
        self.legacy_catalog.lock().unwrap().clone()
    }

    /// Returns the catalog snapshot bound to `context`, or `None` when none is bound.
    pub fn catalog_snapshot_for(context: &RuntimeContext) -> Option<SkillCatalogSnapshot> {
        context
            .metadata
            .get(SKILL_CATALOG_METADATA_KEY)
            .map(SkillCatalogSnapshot::from_json)
    }

    /// Installs a catalog for legacy context-free callers.
    pub fn install_legacy(&self, catalog: SkillCatalog) {
        *self.legacy_catalog.lock().unwrap() = catalog;
    }

    /// Binds a catalog snapshot to `context` and updates the legacy catalog.
    ///
    /// This is the authorization boundary used by the skill load tool: callers pass
    /// [`SkillCatalog::empty`] to clear visibility for the call.
    pub fn install(&self, catalog: SkillCatalog, context: &mut RuntimeContext) {
        let snapshot = SkillCatalogSnapshot::from_catalog(&catalog);
        context
            .metadata
            .insert(SKILL_CATALOG_METADATA_KEY.to_string(), snapshot.to_json());
        self.install_legacy(catalog);
    }

    /// Ensures the skill loading tool is present on `toolkit`, replacing any existing
    /// registration (Java parity: the loader stays ungrouped so it is visible regardless
    /// of a session's persisted active-group list).
    pub fn prepare_toolkit(&self, toolkit: &Toolkit, snapshot: &SkillCatalogSnapshot) {
        if toolkit.get_tool(LoadSkillTool::TOOL_NAME).is_some() {
            toolkit.remove_tool(LoadSkillTool::TOOL_NAME);
        }
        toolkit.register_tool(Arc::new(LoadSkillTool::from_snapshot(snapshot)));
    }

    /// Returns a [`LoadSkillTool`] restricted to the legacy catalog.
    pub fn load_tool(&self) -> LoadSkillTool {
        let catalog = self.current_catalog();
        LoadSkillTool::from_snapshot(&SkillCatalogSnapshot::from_catalog(&catalog))
    }

    /// Renders the `<available_skills>` block (plus code-execution prompt when
    /// applicable) from the given catalog and filter.
    pub fn render_prompt(&self, catalog: &SkillCatalog, filter: &SkillFilter) -> String {
        self.prompt_builder.render(catalog, filter)
    }

    /// Renders the prompt for the legacy catalog with all skills visible.
    pub fn render(&self) -> String {
        let catalog = self.current_catalog();
        self.prompt_builder.render_all(&catalog)
    }
}

impl Default for SkillRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::skill::model::{AgentSkill, HarnessSkillEntry};

    fn catalog() -> SkillCatalog {
        let skill = AgentSkill::builder()
            .name("data")
            .source("workspace")
            .description("d")
            .build();
        SkillCatalog::of(vec![
            HarnessSkillEntry::new(skill).with_files_root("/workspace/skills/data")
        ])
    }

    #[test]
    fn legacy_install_and_render() {
        let rt = SkillRuntime::new();
        assert!(rt.render().is_empty());

        rt.install_legacy(catalog());
        assert!(rt.current_catalog().len() == 1);
        assert!(rt.render().contains("data_workspace"));
    }

    #[test]
    fn snapshot_captures_ids_and_files_roots() {
        let snapshot = SkillCatalogSnapshot::from_catalog(&catalog());
        assert_eq!(snapshot.ids, vec!["data_workspace".to_string()]);
        assert_eq!(
            snapshot.files_root("data_workspace"),
            Some("/workspace/skills/data")
        );
        assert!(snapshot.contains("data_workspace"));
        assert!(!snapshot.contains("other"));
    }

    #[test]
    fn install_binds_snapshot_to_context_metadata() {
        let rt = SkillRuntime::new();
        let mut ctx = RuntimeContext::new();
        assert!(SkillRuntime::catalog_snapshot_for(&ctx).is_none());

        rt.install(catalog(), &mut ctx);
        let snapshot = SkillRuntime::catalog_snapshot_for(&ctx).unwrap();
        assert!(snapshot.contains("data_workspace"));
        // Legacy catalog is updated too.
        assert_eq!(rt.current_catalog().len(), 1);
    }

    #[test]
    fn snapshot_json_roundtrips() {
        let snapshot = SkillCatalogSnapshot::from_catalog(&catalog());
        let restored = SkillCatalogSnapshot::from_json(&snapshot.to_json());
        assert_eq!(restored, snapshot);
    }

    #[test]
    fn prepare_toolkit_registers_boundary_tool() {
        let rt = SkillRuntime::new();
        let toolkit = Toolkit::new();
        let snapshot = SkillCatalogSnapshot::from_catalog(&catalog());
        rt.prepare_toolkit(&toolkit, &snapshot);
        assert!(toolkit.get_tool(LoadSkillTool::TOOL_NAME).is_some());

        // Re-preparing replaces without duplicating.
        rt.prepare_toolkit(&toolkit, &snapshot);
        let names = toolkit.tool_names();
        assert_eq!(
            names
                .iter()
                .filter(|n| n.as_str() == LoadSkillTool::TOOL_NAME)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn prepared_tool_enforces_catalog_boundary() {
        use agentscope_core::message::JsonMap;
        use agentscope_core::tool::Tool;
        let rt = SkillRuntime::new();
        let toolkit = Toolkit::new();
        rt.prepare_toolkit(&toolkit, &SkillCatalogSnapshot::from_catalog(&catalog()));
        let tool = toolkit.get_tool(LoadSkillTool::TOOL_NAME).unwrap();

        let mut args = JsonMap::new();
        args.insert("skillId".into(), serde_json::json!("unknown_skill"));
        let result = tool.call(args, Default::default()).await.unwrap();
        let text = format!("{result:?}");
        assert!(text.contains("Unknown skillId"));
    }
}
