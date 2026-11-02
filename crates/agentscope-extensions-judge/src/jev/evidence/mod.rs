//! Evidence retrieval screening (`io.agentscope.extensions.judge.jev.evidence`).
//!
//! * [`passage`] — immutable, versioned retrieval evidence (`JevPassage`).
//! * [`processor`] — per-passage triage and independent reranking (`JevEvidenceProcessor`), fully
//!   ported: thresholds, screening questions, classification order, request/state limits, call
//!   records and the deterministic ranking/delivery fold.
//! * [`tool`] — the agent-visible projection of a [`processor::Report`] (`JevEvidenceTool`); the
//!   `@Tool` registration itself needs the core `Toolkit`, so the projection is a pure function and
//!   the registration is a documented skeleton.

pub mod passage;
pub mod processor;
pub mod tool;

pub use passage::JevPassage;
pub use processor::{
    questions, Assessment, Call, Classification, JevEvidenceProcessor, Limits, Policy, Report,
    Threshold, Truth,
};
pub use tool::{evidence_result, EvidenceResult, JevEvidenceTool, ToolPassage, TOOL_UNAVAILABLE};
