//! Host integrations (`io.agentscope.extensions.judge.jev.integration`).
//!
//! * [`knowledge`] — ACL-first retrieval decoration over a host knowledge source (`JevKnowledge`)
//!   plus the agent-visible projection used by `JevKnowledgeTool`.
//! * [`response`] — the pre-release screening shapes of `JevResponseMiddleware`: its quality state
//!   builder (fully ported and pure) and its limits; the reactive middleware itself needs the core
//!   event stream and is a documented skeleton (`docs/extensions/JUDGE__GAP_TRACKER.md` §4.2).

pub mod knowledge;
pub mod response;

pub use knowledge::{
    JevKnowledge, KnowledgePassage, KnowledgeSource, KnowledgeTool, KNOWLEDGE_TOOL_UNAVAILABLE,
};
pub use response::{
    conversation_text, quality_state, ResponseLimits, ResponseMiddlewareUnavailable,
    RESPONSE_MIDDLEWARE_UNAVAILABLE,
};
