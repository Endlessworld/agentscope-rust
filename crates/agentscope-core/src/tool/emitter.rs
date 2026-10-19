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

    /// Returns the ID of the tool call associated with this emitter.
    ///
    /// Mirrors Java `ToolEmitter.getToolCallId()`: a chunk callback usually knows
    /// which callback to invoke but not which tool call produced the chunk, so the
    /// id has to travel with the emitter. The default is `None`, keeping custom
    /// emitters that carry no tool-call context compatible.
    fn tool_call_id(&self) -> Option<&str> {
        None
    }
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

/// Default emitter that forwards chunks to a callback, optionally carrying the
/// originating tool call's id.
#[derive(Clone)]
pub struct DefaultToolEmitter {
    callback: ChunkCallback,
    tool_call_id: Option<String>,
}

impl DefaultToolEmitter {
    /// Creates an emitter that delivers chunks to `callback`.
    pub fn new(callback: ChunkCallback) -> Self {
        Self {
            callback,
            tool_call_id: None,
        }
    }

    /// Attaches the id of the tool call this emitter belongs to (see
    /// [`ToolEmitter::tool_call_id`]). Blank ids are ignored.
    pub fn with_tool_call_id(mut self, id: impl Into<String>) -> Self {
        let id = id.into();
        if !id.is_empty() {
            self.tool_call_id = Some(id);
        }
        self
    }

    /// Convenience constructor from a [`ToolUseBlock`].
    pub fn for_tool_call(
        callback: ChunkCallback,
        tool_call: &crate::message::ToolUseBlock,
    ) -> Self {
        Self::new(callback).with_tool_call_id(tool_call.id.clone())
    }
}

impl ToolEmitter for DefaultToolEmitter {
    fn emit(&self, chunk: ToolResultBlock) {
        (self.callback)(chunk);
    }

    fn tool_call_id(&self) -> Option<&str> {
        self.tool_call_id.as_deref()
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

/// Builds the per-call emitter handed to a tool through
/// [`ToolExecutionContext`](super::ToolExecutionContext).
///
/// The emitter carries the originating tool call's id, so a chunk callback can
/// correlate streamed progress with the exact call that produced it. Chunks are
/// forwarded to `callback`; when no callback is registered the emitter is still
/// constructed (tools call `emit` unconditionally) but discards everything.
pub fn tool_emitter_for_call(
    tool_call: &crate::message::ToolUseBlock,
    callback: Option<ChunkCallback>,
) -> Arc<dyn ToolEmitter> {
    match callback {
        Some(cb) => Arc::new(DefaultToolEmitter::for_tool_call(cb, tool_call)),
        None => Arc::new(NoOpToolEmitter),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::message::{TextBlock, ToolUseBlock};

    #[test]
    fn no_op_emitter_has_no_tool_call_id() {
        assert_eq!(NoOpToolEmitter.tool_call_id(), None);
    }

    #[test]
    fn copied_default_emitter_forwards_chunks() {
        let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        let emitter = DefaultToolEmitter::new(Arc::new(move |chunk: ToolResultBlock| {
            sink.lock().unwrap().push(chunk.text_content());
        }));
        emitter.emit(ToolResultBlock::text("partial"));
        assert_eq!(*received.lock().unwrap(), vec!["partial".to_string()]);
    }

    #[test]
    fn default_emitter_exposes_the_tool_call_id() {
        let emitter = DefaultToolEmitter::new(Arc::new(|_| {}))
            .with_tool_call_id("call-42");
        assert_eq!(emitter.tool_call_id(), Some("call-42"));
    }

    #[test]
    fn default_emitter_defaults_to_no_id_and_ignores_blank_ids() {
        assert_eq!(DefaultToolEmitter::new(Arc::new(|_| {})).tool_call_id(), None);
        assert_eq!(
            DefaultToolEmitter::new(Arc::new(|_| {}))
                .with_tool_call_id("")
                .tool_call_id(),
            None
        );
    }

    #[test]
    fn tool_call_id_round_trips_through_the_trait_object() {
        let call = ToolUseBlock::new("call-7", "search", Default::default());
        let emitter = DefaultToolEmitter::for_tool_call(Arc::new(|_| {}), &call);
        let dynamic: Arc<dyn ToolEmitter> = Arc::new(emitter);
        // Read through `dyn ToolEmitter`, as tools holding the context do.
        assert_eq!(dynamic.tool_call_id(), Some("call-7"));
    }

    #[test]
    fn tool_emitter_for_call_carries_the_id_and_forwards() {
        let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        let call = ToolUseBlock::new("call-9", "search", Default::default());
        let emitter = tool_emitter_for_call(
            &call,
            Some(Arc::new(move |chunk: ToolResultBlock| {
                sink.lock().unwrap().push(chunk.text_content());
            })),
        );
        assert_eq!(emitter.tool_call_id(), Some("call-9"));
        emitter.emit(ToolResultBlock::text("chunk"));
        assert_eq!(*received.lock().unwrap(), vec!["chunk".to_string()]);
    }

    #[test]
    fn tool_emitter_for_call_without_callback_is_a_safe_no_op() {
        let call = ToolUseBlock::new("call-9", "search", Default::default());
        let emitter = tool_emitter_for_call(&call, None);
        // Must not panic when a tool emits with no registered callback.
        emitter.emit(ToolResultBlock::of(crate::message::ContentBlock::Text(
            TextBlock::new("dropped"),
        )));
        emit_to(&Some(emitter), ToolResultBlock::text("x"));
    }
}
