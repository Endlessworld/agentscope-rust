//! Shared helpers for the integration tests.
//!
//! Every test binary compiles its own copy of this module, so nothing here reaches the
//! crate's public API. Helpers are `pub` for the tests that use them and
//! `allow(dead_code)` for the ones that do not.

#![allow(dead_code)]

use std::path::PathBuf;

use agent_client_protocol::v1::schema::{ContentBlock, TextContent};
use agentscope_core::message::{ContentBlock as CoreBlock, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};

/// A model that answers every turn with one fixed text block.
///
/// Tests that only need *a* turn use this instead of rolling their own `Model`; a test that
/// scripts several turns keeps its own model.
pub struct StubModel(&'static str);

impl StubModel {
    /// A stub whose single reply is `text` (`done` in most tests).
    pub fn new(text: &'static str) -> Self {
        Self(text)
    }
}

#[async_trait::async_trait]
impl Model for StubModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        let text = self.0;
        Box::pin(futures::stream::once(async move {
            Ok(ChatResponse::builder()
                .content(vec![CoreBlock::Text(TextBlock::new(text))])
                .build())
        }))
    }

    fn model_name(&self) -> &str {
        "stub"
    }
}

/// One text block, the shape `session/prompt` sends.
pub fn prompt(text: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text(TextContent::new(text.to_string()))]
}

/// A per-process scratch directory, emptied first so a re-run starts clean.
pub fn scratch_dir(prefix: &str, tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("{prefix}-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    dir
}

/// A unique scratch directory (timestamp-suffixed), created but never emptied.
///
/// Use it when several runs of the same test may share the machine and must not see each
/// other's state. An empty `prefix` names the directory after `tag` alone.
pub fn unique_scratch_dir(prefix: &str, tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let stem = if prefix.is_empty() {
        tag.to_string()
    } else {
        format!("{prefix}-{tag}")
    };
    let dir = std::env::temp_dir().join(format!("{stem}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    dir
}
