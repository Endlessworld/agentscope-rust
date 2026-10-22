//! Incrementally maintains one session's effective-context view and its `contextHash`
//! from the Level-2 event stream, mirroring `ContextTracker`.
//!
//! A `compaction` event resets the view down to a single summary message, matching what
//! the framework itself will send to the model afterwards.
//!
//! This is an approximation rebuilt from events; the authoritative context still comes
//! from `FrameworkAdapter::extract_context`. Its value is that it is always available in
//! memory, so Level-1 snapshots and change detection cost nothing.

use super::context_snapshot::{ContextMessage, ContextSnapshot, ToolInfo};
use super::session_event::{SessionEvent, ROLE_ASSISTANT, ROLE_TOOL, ROLE_USER, TYPE_COMPACTION,
    TYPE_MESSAGE, TYPE_TOOL_CALL, TYPE_TOOL_RESULT};

/// Incremental effective-context view for one session.
#[derive(Debug, Clone)]
pub struct ContextTracker {
    session_id: String,
    framework: String,
    messages: Vec<ContextMessage>,
    system_prompt: String,
    tools: Vec<ToolInfo>,
    max_tokens: i64,
    compacted: bool,
    compaction_summary: String,
    original_message_count: i64,
    compacted_at: i64,
    tokens_in: i64,
    tokens_out: i64,
    context_used_tokens: i64,
    message_count: i64,
    last_hash: String,
    model: String,
    hashes: Vec<String>,
}

impl ContextTracker {
    /// Creates a tracker for `session_id` / `framework`.
    pub fn new(session_id: impl Into<String>, framework: impl Into<String>) -> Self {
        let mut tracker = Self {
            session_id: session_id.into(),
            framework: framework.into(),
            messages: Vec::new(),
            system_prompt: String::new(),
            tools: Vec::new(),
            max_tokens: 0,
            compacted: false,
            compaction_summary: String::new(),
            original_message_count: 0,
            compacted_at: 0,
            tokens_in: 0,
            tokens_out: 0,
            context_used_tokens: 0,
            message_count: 0,
            last_hash: String::new(),
            model: String::new(),
            hashes: Vec::new(),
        };
        tracker.last_hash = tracker.snapshot().hash();
        tracker
    }

    /// Session id.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Latest context hash.
    pub fn context_hash(&self) -> &str {
        &self.last_hash
    }

    /// Total messages observed (including compacted-away ones).
    pub fn message_count(&self) -> i64 {
        self.message_count
    }

    /// Messages currently in the effective view.
    pub fn effective_message_count(&self) -> usize {
        self.messages.len()
    }

    /// Cumulative input tokens.
    pub fn tokens_in(&self) -> i64 {
        self.tokens_in
    }

    /// Cumulative output tokens.
    pub fn tokens_out(&self) -> i64 {
        self.tokens_out
    }

    /// Tokens in the live effective context (for pressure = used / maxTokens).
    pub fn context_used_tokens(&self) -> i64 {
        self.context_used_tokens
    }

    /// Sets tokens in the live effective context.
    pub fn set_context_used_tokens(&mut self, v: i64) {
        self.context_used_tokens = v.max(0);
    }

    /// Sets the system prompt in effect.
    pub fn set_system_prompt(&mut self, v: impl Into<String>) {
        self.system_prompt = v.into();
    }

    /// Sets the tool list exposed to the model.
    pub fn set_tools(&mut self, v: Vec<ToolInfo>) {
        self.tools = v;
    }

    /// Sets the context-window maximum.
    pub fn set_max_tokens(&mut self, v: i64) {
        self.max_tokens = v;
    }

    /// Sets the model identifier.
    pub fn set_model(&mut self, v: impl Into<String>) {
        self.model = v.into();
    }

    /// Folds one Level-2 event into the effective-context view.
    ///
    /// A `compaction` event resets the view to a single summary message.
    pub fn apply(&mut self, event: &SessionEvent) {
        self.message_count += 1;
        self.tokens_in = self.tokens_in.saturating_add(event.tokens_in);
        self.tokens_out = self.tokens_out.saturating_add(event.tokens_out);
        if event.tokens_in > 0 {
            self.context_used_tokens = event.tokens_in;
        }
        match event.event_type.as_str() {
            TYPE_MESSAGE => {
                let role = if event.role.is_empty() { ROLE_USER } else { &event.role };
                self.messages.push(ContextMessage::new(role, event.content.clone()));
            }
            TYPE_TOOL_CALL => {
                self.messages.push(ContextMessage::new(
                    ROLE_ASSISTANT,
                    format!("[tool_call] {}", event.tool_name),
                ));
            }
            TYPE_TOOL_RESULT => {
                let mut m = ContextMessage::new(ROLE_TOOL, event.tool_output.clone());
                m.tool_name = Some(event.tool_name.clone());
                self.messages.push(m);
            }
            TYPE_COMPACTION => {
                self.compacted = true;
                self.compaction_summary = event.content.clone();
                self.original_message_count = self.message_count;
                self.compacted_at = event.occurred_at;
                self.messages = vec![ContextMessage::new(ROLE_ASSISTANT, event.content.clone())];
            }
            _ => {}
        }
        self.last_hash = self.snapshot().hash();
        if self.compacted {
            self.hashes.push(self.last_hash.clone());
            if self.hashes.len() > 64 {
                self.hashes.remove(0);
            }
        }
    }

    /// Materializes the current effective-context snapshot.
    pub fn snapshot(&self) -> ContextSnapshot {
        let catalog = agentscope_core::model::catalog::ModelCatalog::global();
        let resolution = catalog.resolve(
            &agentscope_core::model::catalog::ResolveRequest::new(&self.model),
        );
        let catalog_key = resolution
            .descriptor
            .as_ref()
            .map(|descriptor| descriptor.key.clone());
        ContextSnapshot::builder(self.session_id.clone())
            .system_prompt(self.system_prompt.clone())
            .messages(self.messages.clone())
            .tools(self.tools.clone())
            .compacted(self.compacted)
            .compaction_summary(self.compaction_summary.clone())
            .original_message_count(self.original_message_count)
            .compacted_at(self.compacted_at)
            .total_tokens(self.context_used_tokens)
            .max_tokens(self.max_tokens)
            .framework(self.framework.clone())
            .model(self.model.clone())
            .catalog(catalog_key, catalog.generation())
            .captured_at(chrono::Utc::now().timestamp_millis())
            .build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::session_event::TYPE_MESSAGE;

    #[test]
    fn message_events_grow_effective_context() {
        let mut t = ContextTracker::new("s1", "agentscope-rust");
        let before = t.context_hash().to_string();
        t.apply(
            &SessionEvent::builder("s1", TYPE_MESSAGE)
                .role(ROLE_USER)
                .content("hello")
                .build(),
        );
        assert_eq!(t.effective_message_count(), 1);
        assert_ne!(t.context_hash(), before);
    }

    #[test]
    fn compaction_resets_to_summary() {
        let mut t = ContextTracker::new("s1", "agentscope-rust");
        for i in 0..5 {
            t.apply(
                &SessionEvent::builder("s1", TYPE_MESSAGE)
                    .role(ROLE_USER)
                    .content(format!("m{i}"))
                    .build(),
            );
        }
        assert_eq!(t.effective_message_count(), 5);
        t.apply(
            &SessionEvent::builder("s1", TYPE_COMPACTION)
                .content("summary of 6 messages")
                .build(),
        );
        assert_eq!(t.effective_message_count(), 1);
        let snap = t.snapshot();
        assert!(snap.compacted);
        assert_eq!(snap.compaction_summary, "summary of 6 messages");
    }
}
