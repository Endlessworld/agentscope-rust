//! `AtPathExpansionMiddleware` — expands `@path` references in inbound messages by
//! inlining the referenced file contents, mirroring the real behaviour of
//! `io.agentscope.harness.agent.middleware.AtPathExpansionMiddleware` (the Rust port
//! previously shipped this as a no-op struct).
//!
//! Expansion happens at the `on_agent` interception point so the model sees the full
//! file text instead of the `@`-reference. Reads go through the agent's `Filesystem`,
//! so sandbox/remote backends are honoured. Tokens that do not resolve to a readable
//! text file are left untouched.
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::{ContentBlock, Msg, TextBlock};
use agentscope_core::middleware::{AgentInput, EventStream, Middleware, Next};
use futures::stream::{self, StreamExt};

use crate::filesystem::Filesystem;

/// Maximum characters inlined from a single referenced file.
const DEFAULT_MAX_READ_CHARS: usize = 20_000;

/// Middleware that expands `@path` references in agent input messages.
pub struct AtPathExpansionMiddleware {
    fs: Arc<dyn Filesystem>,
    max_read_chars: usize,
}

impl AtPathExpansionMiddleware {
    pub fn new(fs: Arc<dyn Filesystem>) -> Self {
        Self { fs, max_read_chars: DEFAULT_MAX_READ_CHARS }
    }

    /// Caps the number of characters inlined per referenced file.
    pub fn with_max_read_chars(mut self, max: usize) -> Self {
        self.max_read_chars = max;
        self
    }
}

impl Middleware for AtPathExpansionMiddleware {
    fn on_agent(
        &self,
        _ctx: &RuntimeContext,
        input: AgentInput,
        next: Next<AgentInput>,
    ) -> EventStream {
        let fs = Arc::clone(&self.fs);
        let max = self.max_read_chars;
        let fut = async move {
            let mut messages = input.messages.clone();
            for msg in &mut messages {
                expand_message(msg, &fs, max).await;
            }
            next(AgentInput::new(messages))
        };
        stream::once(fut).flatten().boxed()
    }
}

async fn expand_message(msg: &mut Msg, fs: &Arc<dyn Filesystem>, max_read_chars: usize) {
    for block in &mut msg.content {
        if let ContentBlock::Text(tb) = block {
            tb.text = expand_text(&tb.text, fs, max_read_chars).await;
        }
    }
}

async fn expand_text(text: &str, fs: &Arc<dyn Filesystem>, max_read_chars: usize) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() {
                let c = bytes[j];
                if c.is_ascii_whitespace() || c == b'@' || c == b'"' || c == b'`' || c == b')' {
                    break;
                }
                j += 1;
            }
            let token = &text[start..j];
            if !token.is_empty() && looks_like_path(token) {
                let path = token.trim_start_matches("./");
                if let Some(content) = read_capped(fs, path, max_read_chars).await {
                    out.push_str(&format!("\n<file path=\"{path}\">\n{content}\n</file>\n"));
                    i = j;
                    continue;
                }
            }
            out.push('@');
            i += 1;
        } else {
            let next = text[i..].find('@').map(|p| i + p).unwrap_or(bytes.len());
            out.push_str(&text[i..next]);
            i = next;
        }
    }
    out
}

fn looks_like_path(token: &str) -> bool {
    token.contains('/')
        || token.contains('\\')
        || token.contains('.')
        || token.starts_with("..")
}

async fn read_capped(fs: &Arc<dyn Filesystem>, path: &str, max_read_chars: usize) -> Option<String> {
    let rt = RuntimeContext::default();
    let data = fs.read(&rt, path, 0, 0).await.ok()?;
    if data.encoding != "utf-8" {
        return None;
    }
    let content = data.content;
    Some(if content.chars().count() > max_read_chars {
        let truncated: String = content.chars().take(max_read_chars).collect();
        format!("{truncated}\n…(truncated to {max_read_chars} chars)")
    } else {
        content
    })
}
