//! AG-UI request processing, mirroring `io.agentscope.core.agui.processor`.
//!
//! * [`resolver`] - `AgentResolver` (id/thread/user -> agent, plus memory probing).
//! * [`resume_coordinator`] - `AguiResumeCoordinator` (resume contract + thread serialization).
//! * [`request_processor`] - `AguiRequestProcessor` (agent-id resolution, follow-up extraction).

pub mod request_processor;
pub mod resolver;
pub mod resume_coordinator;

pub use request_processor::{
    resolve_agent_id, AguiRequestProcessor, AguiRequestProcessorBuilder, ProcessResult,
};
pub use resolver::AgentResolver;
pub use resume_coordinator::{AguiResumeCoordinator, ResumeContractResult, CONTRACT_ERROR_CODE};
