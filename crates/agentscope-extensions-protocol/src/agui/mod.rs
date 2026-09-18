//! AG-UI protocol support, mirroring `io.agentscope.core.agui`.
//!
//! Currently migrated:
//!
//! * [`event`] - the AG-UI event taxonomy ([`event::AguiEvent`]).
//! * [`model`] - the AG-UI message/content model ([`model::AguiMessage`]).
//!
//! Remaining upstream areas (`adapter`, `converter`, `encoder`, `processor`,
//! `registry`, `runtime`) are tracked in `PROTOCOL__GAP_TRACKER.md`.

pub mod event;
pub mod model;

pub use event::{AguiEvent, AguiEventType, AguiEvents};
pub use model::{
    AguiFunctionCall, AguiMessage, AguiToolCall, InputContent, InputContentSource, MessageContent,
};
