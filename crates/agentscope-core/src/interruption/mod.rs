//! Per-session interrupt signalling, mirroring `io.agentscope.core.interruption`.
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::message::{Msg, ToolUseBlock};

/// Who or what initiated the interruption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum InterruptSource {
    User,
    Tool,
    System,
}

impl InterruptSource {
    /// Returns the wire name used in JSON.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "USER",
            Self::Tool => "TOOL",
            Self::System => "SYSTEM",
        }
    }
}

/// Context information about an interruption.
#[derive(Debug, Clone)]
pub struct InterruptContext {
    source: InterruptSource,
    timestamp: DateTime<Local>,
    user_message: Option<Msg>,
    pending_tool_calls: Vec<ToolUseBlock>,
}

impl InterruptContext {
    pub fn new(
        source: InterruptSource,
        timestamp: DateTime<Local>,
        user_message: Option<Msg>,
        pending_tool_calls: Vec<ToolUseBlock>,
    ) -> Self {
        Self { source, timestamp, user_message, pending_tool_calls }
    }

    pub fn source(&self) -> InterruptSource {
        self.source
    }
    pub fn timestamp(&self) -> DateTime<Local> {
        self.timestamp
    }
    pub fn user_message(&self) -> Option<&Msg> {
        self.user_message.as_ref()
    }
    pub fn pending_tool_calls(&self) -> &[ToolUseBlock] {
        &self.pending_tool_calls
    }
    pub fn builder() -> InterruptContextBuilder {
        InterruptContextBuilder::new()
    }
}


/// Builder for [`InterruptContext`].
#[derive(Debug, Clone)]
pub struct InterruptContextBuilder {
    source: InterruptSource,
    timestamp: DateTime<Local>,
    user_message: Option<Msg>,
    pending_tool_calls: Vec<ToolUseBlock>,
}

impl Default for InterruptContextBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl InterruptContextBuilder {
    pub fn new() -> Self {
        Self {
            source: InterruptSource::User,
            timestamp: Local::now(),
            user_message: None,
            pending_tool_calls: Vec::new(),
        }
    }
    pub fn source(mut self, s: InterruptSource) -> Self {
        self.source = s;
        self
    }
    pub fn timestamp(mut self, t: DateTime<Local>) -> Self {
        self.timestamp = t;
        self
    }
    pub fn user_message(mut self, m: Msg) -> Self {
        self.user_message = Some(m);
        self
    }
    pub fn pending_tool_calls(mut self, c: Vec<ToolUseBlock>) -> Self {
        self.pending_tool_calls = c;
        self
    }
    pub fn build(self) -> InterruptContext {
        InterruptContext::new(self.source, self.timestamp, self.user_message, self.pending_tool_calls)
    }
}

/// Per-session interrupt signal (runtime-only, never serialized).
#[derive(Debug, Clone, Default)]
pub struct InterruptControl {
    flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    user_message: std::sync::Arc<std::sync::Mutex<Option<Msg>>>,
    source: std::sync::Arc<std::sync::atomic::AtomicU8>,
}

impl InterruptControl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn trigger(&self, src: Option<InterruptSource>, msg: Option<Msg>) {
        let code = match src.unwrap_or(InterruptSource::User) {
            InterruptSource::User => 0u8,
            InterruptSource::Tool => 1u8,
            InterruptSource::System => 2u8,
        };
        self.source.store(code, std::sync::atomic::Ordering::SeqCst);
        if let Some(m) = msg {
            *self.user_message.lock().unwrap() = Some(m);
        }
        self.flag.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn is_interrupted(&self) -> bool {
        self.flag.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn source(&self) -> InterruptSource {
        match self.source.load(std::sync::atomic::Ordering::SeqCst) {
            1 => InterruptSource::Tool,
            2 => InterruptSource::System,
            _ => InterruptSource::User,
        }
    }

    pub fn user_message(&self) -> Option<Msg> {
        self.user_message.lock().unwrap().clone()
    }

    pub fn reset(&self) {
        self.flag.store(false, std::sync::atomic::Ordering::SeqCst);
        *self.user_message.lock().unwrap() = None;
        self.source.store(0, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn to_context(&self) -> InterruptContext {
        InterruptContext::builder()
            .source(self.source())
            .user_message(self.user_message().unwrap_or_else(|| Msg::user("")))
            .build()
    }
}
