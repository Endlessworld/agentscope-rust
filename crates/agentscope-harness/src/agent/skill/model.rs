//! Skill data model for the harness skill runtime, mirroring the harness
//! `SkillCatalog` / `HarnessSkillEntry` alongside the core `AgentSkill` and
//! `SkillFilter` types (defined here until the core `skill` module is ported).
use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// A skill's visible metadata. A JSON object with scalar / nested-map / list
/// values, rendered as XML in the system prompt.
pub type Metadata = Map<String, Value>;

/// A skill resource (relative path -> content, optionally `base64:` prefixed).
pub type SkillResources = BTreeMap<String, String>;

/// An agent skill: name, source, description, metadata and resources.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentSkill {
    name: String,
    source: String,
    description: String,
    metadata: Metadata,
    resources: SkillResources,
}

impl AgentSkill {
    pub fn new(
        name: impl Into<String>,
        source: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            source: source.into(),
            description: description.into(),
            metadata: Metadata::new(),
            resources: SkillResources::new(),
        }
    }

    pub fn builder() -> AgentSkillBuilder {
        AgentSkillBuilder::default()
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn source(&self) -> &str {
        &self.source
    }
    pub fn description(&self) -> &str {
        &self.description
    }
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }
    pub fn resources(&self) -> &SkillResources {
        &self.resources
    }
    /// Composite id `<name>_<source>`, mirroring `AgentSkill#getSkillId()`.
    pub fn skill_id(&self) -> String {
        if self.source.is_empty() {
            self.name.clone()
        } else {
            format!("{}_{}", self.name, self.source)
        }
    }
}

/// Builder for [`AgentSkill`].
#[derive(Debug, Clone, Default)]
pub struct AgentSkillBuilder {
    name: Option<String>,
    source: Option<String>,
    description: Option<String>,
    metadata: Option<Metadata>,
    resources: Option<SkillResources>,
}

impl AgentSkillBuilder {
    pub fn name(mut self, v: impl Into<String>) -> Self {
        self.name = Some(v.into());
        self
    }
    pub fn source(mut self, v: impl Into<String>) -> Self {
        self.source = Some(v.into());
        self
    }
    pub fn description(mut self, v: impl Into<String>) -> Self {
        self.description = Some(v.into());
        self
    }
    pub fn metadata(mut self, v: Metadata) -> Self {
        self.metadata = Some(v);
        self
    }
    pub fn add_metadata(mut self, k: impl Into<String>, v: Value) -> Self {
        self.metadata.get_or_insert_with(Metadata::new).insert(k.into(), v);
        self
    }
    pub fn resources(mut self, v: SkillResources) -> Self {
        self.resources = Some(v);
        self
    }
    pub fn add_resource(mut self, path: impl Into<String>, content: impl Into<String>) -> Self {
        self.resources.get_or_insert_with(SkillResources::new).insert(path.into(), content.into());
        self
    }
    pub fn build(self) -> AgentSkill {
        AgentSkill {
            name: self.name.unwrap_or_default(),
            source: self.source.unwrap_or_default(),
            description: self.description.unwrap_or_default(),
            metadata: self.metadata.unwrap_or_default(),
            resources: self.resources.unwrap_or_default(),
        }
    }
}

/// Immutable filter deciding which skills are allowed in a prompt, mirroring
/// `SkillFilter`.
#[derive(Debug, Clone)]
pub enum SkillFilter {
    All,
    None,
    Whitelist(Vec<String>),
    Blacklist(Vec<String>),
    OverlayEnable(Vec<String>),
    OverlayDisable(Vec<String>),
    /// A base filter merged with a runtime overlay: skills explicitly named in
    /// the overlay use its decision, everything else falls through to `base`.
    Merged {
        base: Box<SkillFilter>,
        overlay: Box<SkillFilter>,
    },
}

impl SkillFilter {
    pub fn all() -> Self {
        SkillFilter::All
    }
    pub fn none() -> Self {
        SkillFilter::None
    }
    pub fn only(names: &[&str]) -> Self {
        SkillFilter::Whitelist(names.iter().map(|s| s.to_string()).collect())
    }
    pub fn except(names: &[&str]) -> Self {
        SkillFilter::Blacklist(names.iter().map(|s| s.to_string()).collect())
    }
    pub fn enable(names: &[&str]) -> Self {
        SkillFilter::OverlayEnable(names.iter().map(|s| s.to_string()).collect())
    }
    pub fn disable(names: &[&str]) -> Self {
        SkillFilter::OverlayDisable(names.iter().map(|s| s.to_string()).collect())
    }

    pub fn is_allowed(&self, skill_name: &str) -> bool {
        match self {
            SkillFilter::All => true,
            SkillFilter::None => false,
            SkillFilter::Whitelist(set) => set.iter().any(|s| s == skill_name),
            SkillFilter::Blacklist(set) => !set.iter().any(|s| s == skill_name),
            SkillFilter::OverlayEnable(set) => set.iter().any(|s| s == skill_name),
            SkillFilter::OverlayDisable(set) => !set.iter().any(|s| s == skill_name),
            SkillFilter::Merged { base, overlay } => {
                let named = match overlay.as_ref() {
                    SkillFilter::OverlayEnable(set) | SkillFilter::OverlayDisable(set) => {
                        set.iter().any(|s| s == skill_name)
                    }
                    _ => false,
                };
                if named {
                    overlay.is_allowed(skill_name)
                } else {
                    base.is_allowed(skill_name)
                }
            }
        }
    }

    pub fn is_overlay(&self) -> bool {
        matches!(
            self,
            SkillFilter::OverlayEnable(_) | SkillFilter::OverlayDisable(_) | SkillFilter::Merged { .. }
        )
    }

    /// Merges this (base) filter with a runtime overlay. Non-overlay overlays
    /// replace this filter entirely.
    pub fn overlay(&self, runtime_overlay: &SkillFilter) -> SkillFilter {
        if !runtime_overlay.is_overlay() {
            return runtime_overlay.clone();
        }
        match runtime_overlay {
            SkillFilter::OverlayEnable(_) | SkillFilter::OverlayDisable(_) => SkillFilter::Merged {
                base: Box::new(self.clone()),
                overlay: Box::new(runtime_overlay.clone()),
            },
            _ => self.clone(),
        }
    }
}

/// Per-call wrapper around an [`AgentSkill`] carrying harness-only metadata
/// (shell-reachable files root).
#[derive(Debug, Clone, PartialEq)]
pub struct HarnessSkillEntry {
    skill: AgentSkill,
    files_root: Option<String>,
}

impl HarnessSkillEntry {
    pub fn new(skill: AgentSkill) -> Self {
        Self { skill, files_root: None }
    }

    pub fn with_files_root(mut self, root: impl Into<String>) -> Self {
        self.files_root = Some(root.into());
        self
    }

    pub fn skill(&self) -> &AgentSkill {
        &self.skill
    }
    pub fn files_root(&self) -> Option<&str> {
        self.files_root.as_deref()
    }
}

/// Immutable snapshot of the skills visible to a single `onSystemPrompt` pass.
/// Keyed by `skillId`, insertion-order preserved.
#[derive(Debug, Clone, Default)]
pub struct SkillCatalog {
    entries: Vec<HarnessSkillEntry>,
}

impl SkillCatalog {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn of(ordered_entries: Vec<HarnessSkillEntry>) -> Self {
        Self { entries: ordered_entries }
    }

    /// Lookup by `skillId` (`name_source`).
    pub fn get(&self, skill_id: &str) -> Option<&HarnessSkillEntry> {
        self.entries.iter().find(|e| e.skill().skill_id() == skill_id)
    }

    pub fn all(&self) -> &[HarnessSkillEntry] {
        &self.entries
    }

    pub fn ids(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.skill().skill_id()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str) -> AgentSkill {
        AgentSkill::builder().name(name).source("workspace").description("d").build()
    }

    #[test]
    fn skill_id_is_name_source() {
        assert_eq!(skill("data").skill_id(), "data_workspace");
        assert_eq!(AgentSkill::builder().name("x").build().skill_id(), "x");
    }

    #[test]
    fn skill_catalog_keyed_by_id() {
        let cat = SkillCatalog::of(vec![HarnessSkillEntry::new(skill("data"))]);
        assert_eq!(cat.len(), 1);
        assert!(cat.get("data_workspace").is_some());
        assert!(cat.get("other_workspace").is_none());
        assert_eq!(cat.ids(), vec!["data_workspace".to_string()]);
    }

    #[test]
    fn filter_all_none_only() {
        assert!(SkillFilter::all().is_allowed("x"));
        assert!(!SkillFilter::none().is_allowed("x"));
        assert!(SkillFilter::only(&["a", "b"]).is_allowed("a"));
        assert!(!SkillFilter::only(&["a"]).is_allowed("b"));
        assert!(SkillFilter::except(&["a"]).is_allowed("b"));
        assert!(!SkillFilter::except(&["a"]).is_allowed("a"));
    }

    #[test]
    fn filter_overlay_merge() {
        let base = SkillFilter::except(&["secret"]);
        let effective = base.overlay(&SkillFilter::enable(&["secret"]));
        assert!(effective.is_allowed("secret"));
        assert!(effective.is_allowed("other"));
    }

    #[test]
    fn entry_carries_files_root() {
        let e = HarnessSkillEntry::new(skill("data")).with_files_root("/workspace/skills/data");
        assert_eq!(e.files_root(), Some("/workspace/skills/data"));
    }
}
