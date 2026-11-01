//! Framework-owned, domain-neutral context conventions.
//!
//! Java ships these rules as the classpath resource
//! `/io/agentscope/harness/context-conventions.md` and injects them from
//! `ContextConventionsMiddleware` as an `instruction_rules` item. Rust keeps the resource
//! (so the wording has one home) and the same trusted-instruction path, but registration is
//! explicit: the context compiler is opt-in here, and turning it on for every agent by
//! default would also turn on budget accounting and compaction for them.
//!
//! Register with [`HarnessAgentBuilder::instruction`](crate::HarnessAgentBuilder::instruction):
//!
//! ```ignore
//! let agent = HarnessAgent::builder()
//!     .model(model)
//!     .instruction("context-conventions", CONTEXT_CONVENTIONS)
//!     .build();
//! ```

/// The conventions text (System placement, required, highest priority).
pub const CONTEXT_CONVENTIONS: &str = include_str!("../../../assets/context-conventions.md");

/// The source id of the registered instruction, mirroring Java's `harness:context-conventions`.
pub const CONTEXT_CONVENTIONS_INSTRUCTION_ID: &str = "context-conventions";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::context::{ContextItemPlacement, ContextSources};

    #[test]
    fn the_resource_carries_the_java_conventions() {
        assert!(CONTEXT_CONVENTIONS.contains("TASK_STATE"));
        assert!(CONTEXT_CONVENTIONS.contains("HARNESS_CONTEXT"));
        assert!(CONTEXT_CONVENTIONS.trim().len() > 100);
    }

    #[test]
    fn registering_the_conventions_yields_a_required_system_instruction() {
        let sources = ContextSources::empty()
            .with_instruction(CONTEXT_CONVENTIONS_INSTRUCTION_ID, CONTEXT_CONVENTIONS)
            .expect("valid instruction");
        let instruction = &sources.instructions()[0];
        assert_eq!(instruction.placement, ContextItemPlacement::System);
        assert!(instruction.required);
        assert_eq!(instruction.source_id, "instruction/context-conventions");
        assert!(instruction.content.contains("RUNTIME_STATE"));
    }
}
