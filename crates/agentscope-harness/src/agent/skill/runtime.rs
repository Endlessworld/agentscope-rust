//! Aggregates the skill prompt rendering and a legacy (context-free) catalog,
//! mirroring `SkillRuntime`.
//!
//! The catalog is deliberately stored in-process, not on the shared runtime, so
//! concurrent sessions cannot overwrite one another's filtered skill view. A
//! context-scoped catalog (bound per `RuntimeContext`) is staged until the core
//! `RuntimeContext` gains a generic value slot.
use std::sync::{Arc, Mutex};

use super::model::{SkillCatalog, SkillFilter};
use super::prompt::SkillPromptBuilder;

/// Aggregates the skill prompt builder and a legacy catalog reference.
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

    /// Installs a catalog for legacy context-free callers.
    pub fn install_legacy(&self, catalog: SkillCatalog) {
        *self.legacy_catalog.lock().unwrap() = catalog;
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

    #[test]
    fn legacy_install_and_render() {
        let rt = SkillRuntime::new();
        assert!(rt.render().is_empty());

        let skill = AgentSkill::builder().name("data").source("workspace").description("d").build();
        rt.install_legacy(SkillCatalog::of(vec![HarnessSkillEntry::new(skill)]));
        assert!(rt.current_catalog().len() == 1);
        assert!(rt.render().contains("data_workspace"));
    }
}
