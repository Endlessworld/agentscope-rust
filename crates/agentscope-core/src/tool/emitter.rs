//! Streaming tool-result emitter, mirroring `ToolEmitter`/`DefaultToolEmitter`/
//! `NoOpToolEmitter`.
//!
//! A tool may emit intermediate progress chunks during execution via its
//! [`ToolExecutionContext`]. These chunks are delivered to registered callbacks
//! (for monitoring/hooks) but are **not** sent to the LLM — only the tool's
//! final return value becomes the tool result.
use std::sync::Arc;

use crate::message::ToolResultBlock;

/// Emits streaming response chunks during tool execution.
///
/// Emitted chunks are delivered to hooks for monitoring/logging only; they do
/// not affect the schema shown to the LLM nor the final tool result.
pub trait ToolEmitter: Send + Sync {
    /// Emits an intermediate chunk.
    fn emit(&self, chunk: ToolResultBlock);
}

/// A no-op emitter that silently discards every chunk. Used as the default so
/// tools can call `emit` without null checks.
#[derive(Clone, Copy, Default)]
pub struct NoOpToolEmitter;

impl ToolEmitter for NoOpToolEmitter {
    fn emit(&self, _chunk: ToolResultBlock) {
        // No-op: silently discard.
    }
}

/// A boxed callback that receives each emitted chunk (typically forwarding to
/// an acting-chunk hook).
pub type ChunkCallback = Arc<dyn Fn(ToolResultBlock) + Send + Sync>;

/// Default emitter that forwards chunks to a callback.
#[derive(Clone)]
pub struct DefaultToolEmitter {
    callback: ChunkCallback,
}

impl DefaultToolEmitter {
    /// Creates an emitter that delivers chunks to `callback`.
    pub fn new(callback: ChunkCallback) -> Self {
        Self { callback }
    }
}

impl ToolEmitter for DefaultToolEmitter {
    fn emit(&self, chunk: ToolResultBlock) {
        (self.callback)(chunk);
    }
}

/// Convenience wrapper so an [`Option<Arc<dyn ToolEmitter>>`] can be invoked
/// without null checks (falls back to [`NoOpToolEmitter`]).
#[inline]
pub fn emit_to(emitter: &Option<Arc<dyn ToolEmitter>>, chunk: ToolResultBlock) {
    match emitter {
        Some(e) => e.emit(chunk),
        None => NoOpToolEmitter.emit(chunk),
    }
}
