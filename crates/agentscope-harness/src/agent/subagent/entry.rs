//! Descriptor for a subagent identified by an agent id, mirroring `SubagentEntry`
//! and `SubagentFactory`.
use std::sync::Arc;

use agentscope_core::agent::{CallableAgent, RuntimeContext};

use super::SubagentDeclaration;

/// Creates a new subagent instance for a single spawn or session. Implementations
/// are responsible for translating `parent_rc` into state-isolation hints.
pub trait SubagentFactory: Send + Sync {
    /// Builds a new subagent instance for the given parent runtime context.
    fn create(&self, parent_rc: &RuntimeContext) -> Arc<dyn CallableAgent>;
}

/// Descriptor for a subagent identified by agent id, with its description,
/// factory, and optional declaration (for remote URL / headers / visibility).
#[derive(Clone)]
pub struct SubagentEntry {
    pub name: String,
    pub description: String,
    pub factory: Arc<dyn SubagentFactory>,
    pub declaration: Option<SubagentDeclaration>,
}

impl SubagentEntry {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        factory: Arc<dyn SubagentFactory>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            factory,
            declaration: None,
        }
    }

    /// Attaches a declaration to this entry (enables remote config / visibility rules).
    pub fn with_declaration(mut self, declaration: SubagentDeclaration) -> Self {
        self.declaration = Some(declaration);
        self
    }

    /// Whether this entry should appear in the LLM-facing list of available subagents.
    /// Programmatic entries with no declaration default to visible.
    pub fn is_visible_to_llm(&self) -> bool {
        match &self.declaration {
            Some(decl) => decl.is_visible_to_llm(),
            None => true,
        }
    }
}
