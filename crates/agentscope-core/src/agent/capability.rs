//! Per-call capability surface, mirroring `CapabilitySurface` / `CapabilityResolver`.
//!
//! A multi-tenant agent exposes the same registered tools and skills to every caller unless a
//! [`CapabilityResolver`] is configured. The resolver computes, per call, the grants the caller
//! holds; the resulting [`CapabilitySurface`] is carried on the call's [`RuntimeContext`] and
//! enforced at the two points that matter:
//!
//! * what the model *sees* — backend tool schemas ([`Toolkit::tool_schemas_for_call`]) and the
//!   harness skill catalog/prompt;
//! * what it can *execute* — the [`ToolExecutor`](crate::tool::ToolExecutor) hard gate and the
//!   skill load tool.
//!
//! The surface is additive over the existing layers: it only ever narrows. Configuration intent
//! (`SkillFilter`), the current activation set and per-call permission approval each answer a
//! different question; this layer answers "may this caller use it at all".
//!
//! [`Toolkit::tool_schemas_for_call`]: crate::tool::Toolkit::tool_schemas_for_call

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::agent::RuntimeContext;
use crate::error::Result;

/// Extension key under which a call publishes its resolved surface on the [`RuntimeContext`].
pub const CAPABILITY_SURFACE_CONTEXT_KEY: &str = "capability_surface";

/// Entry name under which the surface travels in [`ToolExecutionContext::runtime`].
///
/// The runtime map is the only channel a tool sees, so the surface is serialized here for the
/// tool-side gates (the skill load tool) that cannot read the agent context directly.
///
/// [`ToolExecutionContext::runtime`]: crate::tool::ToolExecutionContext
pub const CAPABILITY_SURFACE_RUNTIME_KEY: &str = "agentscope_capability_surface";

/// An immutable per-call grant enumeration: allowed skills, tool groups and tool names.
///
/// Only one shape skips trimming entirely: **all three dimensions unset** ([`Self::allow_all`]).
/// As soon as any dimension is set — even to an explicit empty set ([`Self::deny_all`]) — the
/// surface is a complete grant enumeration and an unset dimension contributes no grants.
///
/// A tool is granted when it is system-marked (framework meta tools), its name is granted, or
/// one of its groups is granted. Skills are granted by *name* (one entry covers every copy of
/// that skill); a full skill id entry is also honoured as an exact-copy match.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilitySurface {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    allowed_skills: Option<BTreeSet<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    allowed_tool_groups: Option<BTreeSet<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    allowed_tools: Option<BTreeSet<String>>,
}

impl CapabilitySurface {
    /// Creates a surface from the three grant dimensions; `None` means "not set".
    pub fn new(
        allowed_skills: Option<BTreeSet<String>>,
        allowed_tool_groups: Option<BTreeSet<String>>,
        allowed_tools: Option<BTreeSet<String>>,
    ) -> Self {
        Self {
            allowed_skills,
            allowed_tool_groups,
            allowed_tools,
        }
    }

    /// Allow-everything sentinel (nothing set): the value used when no resolver is configured.
    pub fn allow_all() -> Self {
        Self::default()
    }

    /// Explicit deny-everything surface (three empty sets); system capabilities still pass.
    pub fn deny_all() -> Self {
        Self {
            allowed_skills: Some(BTreeSet::new()),
            allowed_tool_groups: Some(BTreeSet::new()),
            allowed_tools: Some(BTreeSet::new()),
        }
    }

    /// Whether nothing is set at all — the only shape that skips trimming.
    pub fn is_unrestricted(&self) -> bool {
        self.allowed_skills.is_none()
            && self.allowed_tool_groups.is_none()
            && self.allowed_tools.is_none()
    }

    /// Granted skill names (with full-id compatibility entries), if that dimension is set.
    pub fn allowed_skills(&self) -> Option<&BTreeSet<String>> {
        self.allowed_skills.as_ref()
    }

    /// Granted tool-group names, if that dimension is set.
    pub fn allowed_tool_groups(&self) -> Option<&BTreeSet<String>> {
        self.allowed_tool_groups.as_ref()
    }

    /// Granted tool names, if that dimension is set.
    pub fn allowed_tools(&self) -> Option<&BTreeSet<String>> {
        self.allowed_tools.as_ref()
    }

    /// Skill decision: an unrestricted surface, or a grant entry naming this skill.
    ///
    /// The grant vocabulary is the skill *name*; `skill_id` is consulted as an exact-copy
    /// compatibility match.
    pub fn grants_skill(&self, name: &str, skill_id: Option<&str>) -> bool {
        if self.is_unrestricted() {
            return true;
        }
        let Some(allowed) = &self.allowed_skills else {
            return false;
        };
        allowed.contains(name) || skill_id.is_some_and(|id| allowed.contains(id))
    }

    /// Tool-group decision: unrestricted, or a whitelisted group.
    pub fn grants_tool_group(&self, group: &str) -> bool {
        self.is_unrestricted()
            || self
                .allowed_tool_groups
                .as_ref()
                .is_some_and(|groups| groups.contains(group))
    }

    /// Tool decision; any matching path grants (per-name or one of its groups).
    ///
    /// `is_system` marks framework capabilities (meta tools), which bypass trimming exactly like
    /// an unrestricted surface: revoking them would break the agent's own bookkeeping rather
    /// than withhold a user capability.
    ///
    /// A granted skill also grants its companion tool list (`<skillId>_skill_tools`): granting a
    /// skill without its tools left the model able to see the skill but unable to call anything
    /// it documents, reported as the indistinguishable "tool not found" (#2169). A bare
    /// `<skillId>_` prefix is deliberately *not* used: it would also admit unrelated siblings
    /// such as `pdf_export_v2_merge` when only `pdf_export` is granted.
    pub fn grants_tool(&self, name: &str, groups: &[String], is_system: bool) -> bool {
        if is_system || self.is_unrestricted() {
            return true;
        }
        if self
            .allowed_tools
            .as_ref()
            .is_some_and(|tools| tools.contains(name))
        {
            return true;
        }
        if self.grants_skill_tools(name) {
            return true;
        }
        groups.iter().any(|group| self.grants_tool_group(group))
    }

    /// Whether a granted skill owns `tool` through the `<skillId>_skill_tools` convention.
    fn grants_skill_tools(&self, tool: &str) -> bool {
        let Some(skills) = &self.allowed_skills else {
            return false;
        };
        skills.iter().any(|skill| {
            tool.len() == skill.len() + "_skill_tools".len()
                && tool.starts_with(skill.as_str())
                && tool[skill.len()..] == *"_skill_tools"
        })
    }
}

impl fmt::Debug for CapabilitySurface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapabilitySurface")
            .field("allowed_skills", &self.allowed_skills)
            .field("allowed_tool_groups", &self.allowed_tool_groups)
            .field("allowed_tools", &self.allowed_tools)
            .finish()
    }
}

/// Computes the capabilities a caller is granted (pure whitelists).
///
/// Mirrors Java `CapabilityResolver`: `Ok(None)` means "no trimming" (same as not configured);
/// an `Err` aborts the call — an authorization infrastructure failure is not "no grants". A
/// resolver that wants to keep serving should degrade to [`CapabilitySurface::deny_all`]
/// instead of failing.
///
/// Resolution happens once per call, on the call's [`RuntimeContext`] (which carries the caller
/// identity), and the resulting surface is inherited by derived contexts (sub-agents) without
/// re-resolving.
pub trait CapabilityResolver: Send + Sync {
    /// Resolves the surface for this call; `None` = unrestricted.
    fn resolve(&self, ctx: &RuntimeContext) -> Result<Option<CapabilitySurface>>;
}

impl<F> CapabilityResolver for F
where
    F: Fn(&RuntimeContext) -> Result<Option<CapabilitySurface>> + Send + Sync,
{
    fn resolve(&self, ctx: &RuntimeContext) -> Result<Option<CapabilitySurface>> {
        self(ctx)
    }
}

/// The surface published on `ctx`, if a resolver ran for this call.
pub fn capability_surface_from_context(ctx: &RuntimeContext) -> Option<Arc<CapabilitySurface>> {
    ctx.extension::<CapabilitySurface>(CAPABILITY_SURFACE_CONTEXT_KEY)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    fn groups(groups: &[&str]) -> Vec<String> {
        groups.iter().map(|g| g.to_string()).collect()
    }

    #[test]
    fn an_unset_surface_grants_everything() {
        let surface = CapabilitySurface::allow_all();
        assert!(surface.is_unrestricted());
        assert!(surface.grants_tool("anything", &[], false));
        assert!(surface.grants_skill("pdf_export", None));
    }

    #[test]
    fn an_empty_set_is_a_complete_enumeration_not_unrestricted() {
        let surface = CapabilitySurface::deny_all();
        assert!(!surface.is_unrestricted());
        assert!(!surface.grants_tool("anything", &groups(&["search"]), false));
        assert!(!surface.grants_skill("pdf_export", Some("pdf_export_local")));
        // System capabilities still pass: they are not user grants.
        assert!(surface.grants_tool("reset_equipped_tools", &[], true));
    }

    #[test]
    fn a_tool_is_granted_by_name_group_or_system_mark() {
        let surface =
            CapabilitySurface::new(None, Some(names(&["search"])), Some(names(&["web_read"])));
        assert!(surface.grants_tool("web_read", &[], false));
        assert!(surface.grants_tool("web_search", &groups(&["search"]), false));
        assert!(!surface.grants_tool("execute", &groups(&["shell"]), false));
        // A set-but-unrelated group does not leak other groups.
        assert!(!surface.grants_tool_group("shell"));
    }

    #[test]
    fn skills_are_granted_by_name_and_by_exact_id() {
        let by_name = CapabilitySurface::new(Some(names(&["pdf_export"])), None, None);
        assert!(by_name.grants_skill("pdf_export", Some("pdf_export_repo")));
        // A name entry covers every copy of that name, not other names.
        assert!(!by_name.grants_skill("docx_export", Some("docx_export_repo")));

        let by_id = CapabilitySurface::new(Some(names(&["pdf_export_repo"])), None, None);
        assert!(by_id.grants_skill("pdf_export", Some("pdf_export_repo")));
        assert!(!by_id.grants_skill("pdf_export", Some("pdf_export_other")));
    }

    #[test]
    fn a_skill_grant_does_not_grant_tools() {
        // Layers are independent: granting a skill says nothing about tool groups/names.
        let surface = CapabilitySurface::new(Some(names(&["pdf_export"])), Some(names(&[])), None);
        assert!(!surface.grants_tool("web_read", &[], false));
    }

    #[test]
    fn a_skill_grant_admits_its_companion_tool_list() {
        // `<skillId>_skill_tools` belongs to the skill (#2169).
        let surface =
            CapabilitySurface::new(Some(names(&["pdf_export"])), Some(names(&["search"])), None);
        assert!(surface.grants_tool("pdf_export_skill_tools", &[], false));
        // A bare prefix would also admit unrelated siblings, so prefixed tools stay withheld.
        assert!(!surface.grants_tool("pdf_export_merge", &[], false));
        assert!(!surface.grants_tool("pdf_export_v2_merge", &[], false));
        assert!(!surface.grants_tool("pdfexport_merge", &[], false));
        assert!(!surface.grants_tool("web_read", &[], false));
        // An explicit positive group grant and the system mark keep working.
        assert!(surface.grants_tool("anything", &groups(&["search"]), false));
        assert!(surface.grants_tool("reset_equipped_tools", &[], true));
    }

    #[test]
    fn the_surface_survives_the_runtime_map_round_trip() {
        let surface =
            CapabilitySurface::new(Some(names(&["pdf_export"])), Some(names(&["search"])), None);
        let value = serde_json::to_value(&surface).unwrap();
        let back: CapabilitySurface = serde_json::from_value(value).unwrap();
        assert_eq!(back, surface);
        // An unset dimension stays absent from the payload, not an empty list.
        assert!(serde_json::to_value(CapabilitySurface::allow_all())
            .unwrap()
            .as_object()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_closure_is_a_resolver() {
        let resolver = |ctx: &RuntimeContext| {
            Ok(Some(CapabilitySurface::new(None, None, {
                let mut set = BTreeSet::new();
                if ctx.user_id() == Some("u1") {
                    set.insert("web_read".to_string());
                }
                Some(set)
            })))
        };
        let ctx = RuntimeContext::new().with_user_id("u1");
        let surface = resolver.resolve(&ctx).unwrap().unwrap();
        assert!(surface.grants_tool("web_read", &[], false));
        let ctx = RuntimeContext::new().with_user_id("u2");
        let surface = resolver.resolve(&ctx).unwrap().unwrap();
        assert!(!surface.grants_tool("web_read", &[], false));
    }
}
