//! Middleware that evicts oversized tool results to the filesystem before downstream
//! reasoning sees the bloated message list, mirroring
//! `io.agentscope.harness.agent.middleware.ToolResultEvictionMiddleware`.
//!
//! When the text content of a `ToolResultBlock` exceeds `max_result_chars`, the full result
//! is written to `{eviction_path}/{agent}/{toolCallId}-{contentHash}` and replaced by a
//! compact head+tail preview placeholder marked as evicted in metadata so it is not
//! recursively evicted on later turns.

use std::collections::HashMap;
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::{ContentBlock, Msg, TextBlock, ToolResultBlock};
use agentscope_core::middleware::{EventStream, Middleware, Next, ReasoningInput};
use futures::StreamExt;

use crate::agent::memory::ToolResultEvictionConfig;
use crate::agent::skill::sha256_hex;
use crate::filesystem::Filesystem;

/// Metadata key marking a tool result as already evicted.
pub const EVICTED_METADATA_KEY: &str = "agentscope.tool_result_evicted";

/// Fingerprint identifying an evictable tool result (id + name + text).
#[derive(PartialEq, Eq, Hash)]
pub(crate) struct ToolResultFingerprint {
    id: Option<String>,
    name: Option<String>,
    text: String,
}

/// Middleware that evicts oversized tool results to the filesystem.
pub struct ToolResultEvictionMiddleware {
    fs: Arc<dyn Filesystem>,
    config: ToolResultEvictionConfig,
}

impl ToolResultEvictionMiddleware {
    /// Creates a middleware with the given config.
    pub fn new(fs: Arc<dyn Filesystem>, config: ToolResultEvictionConfig) -> Self {
        Self { fs, config }
    }
}

impl Middleware for ToolResultEvictionMiddleware {
    fn on_reasoning(
        &self,
        ctx: &RuntimeContext,
        input: ReasoningInput,
        next: Next<ReasoningInput>,
    ) -> EventStream {
        let ctx = ctx.clone();
        let fs = self.fs.clone();
        let config = self.config.clone();
        let agent_name = ctx.agent_name.clone().unwrap_or_default();
        let messages = input.messages.clone();

        let fut = async move {
            let mut replacements: HashMap<ToolResultFingerprint, String> = HashMap::new();
            let (rebuilt, changed) =
                evict_messages(&fs, &config, &agent_name, &ctx, messages, &mut replacements).await;
            if !changed {
                return next(input);
            }
            let new_input = ReasoningInput::new(rebuilt, input.tools, input.options);
            next(new_input)
        };
        Box::pin(futures::stream::once(fut).flatten())
    }
}

/// Returns the eviction `(messages, changed)` after replacing oversized tool results.
///
/// Shared with the context compiler, which runs eviction in its budget pass so an
/// oversized historical tool result is offloaded before the request is layouted.
pub(crate) async fn evict_messages(
    fs: &Arc<dyn Filesystem>,
    config: &ToolResultEvictionConfig,
    agent_name: &str,
    ctx: &RuntimeContext,
    messages: Vec<Msg>,
    replacements: &mut HashMap<ToolResultFingerprint, String>,
) -> (Vec<Msg>, bool) {
    let mut rebuilt: Vec<Msg> = Vec::with_capacity(messages.len());
    let mut changed = false;
    for msg in messages {
        let new_msg = evict_message(fs, config, agent_name, ctx, msg.clone(), replacements).await;
        if let Some(m) = new_msg {
            changed = true;
            rebuilt.push(m);
        } else {
            rebuilt.push(msg);
        }
    }
    (rebuilt, changed)
}

/// Evicts oversized tool results in a single message. Returns `Some(msg)` if rebuilt.
async fn evict_message(
    fs: &Arc<dyn Filesystem>,
    config: &ToolResultEvictionConfig,
    agent_name: &str,
    ctx: &RuntimeContext,
    msg: Msg,
    replacements: &mut HashMap<ToolResultFingerprint, String>,
) -> Option<Msg> {
    if msg.content.is_empty() {
        return None;
    }
    let mut rebuilt_blocks: Vec<ContentBlock> = Vec::with_capacity(msg.content.len());
    let mut changed = false;
    for block in msg.content {
        match block {
            ContentBlock::ToolResult(tr) if !is_evicted(&tr) => {
                let fp = fingerprint(&tr);
                let placeholder = if let Some(p) = replacements.get(&fp) {
                    Some(p.clone())
                } else {
                    let p = maybe_evict(fs, config, agent_name, ctx, &tr).await;
                    if let Some(ref p) = p {
                        replacements.insert(fp, p.clone());
                    }
                    p
                };
                if let Some(p) = placeholder {
                    changed = true;
                    rebuilt_blocks.push(ContentBlock::ToolResult(with_placeholder(tr, p)));
                } else {
                    rebuilt_blocks.push(ContentBlock::ToolResult(tr));
                }
            }
            other => rebuilt_blocks.push(other),
        }
    }
    if !changed {
        return None;
    }
    Some(
        Msg::builder_for_role(Some(msg.role))
            .id(msg.id)
            .name(msg.name.unwrap_or_default())
            .content(rebuilt_blocks)
            .metadata(msg.metadata.clone())
            .timestamp(msg.timestamp)
            .build(),
    )
}

/// Evicts a single oversized tool result, writing it to disk and returning the placeholder.
async fn maybe_evict(
    fs: &Arc<dyn Filesystem>,
    config: &ToolResultEvictionConfig,
    agent_name: &str,
    ctx: &RuntimeContext,
    tr: &ToolResultBlock,
) -> Option<String> {
    let tool_name = tr.name.as_deref().unwrap_or_default();
    if config.excluded_tool_names.contains(tool_name) {
        return None;
    }
    let full_text = tr.text_content();
    if full_text.chars().count() <= config.max_result_chars {
        return None;
    }
    let tool_call_id = tr.id.as_deref().unwrap_or("unknown");
    let eviction_path = build_eviction_path(config, agent_name, tool_call_id, &full_text);
    if fs.exists(ctx, &eviction_path).await.unwrap_or(false) {
        let _ = fs.delete(ctx, &eviction_path).await;
    }
    if fs.write(ctx, &eviction_path, &full_text).await.is_err() {
        return None;
    }
    Some(build_placeholder(config, &full_text, &eviction_path))
}

fn is_evicted(tr: &ToolResultBlock) -> bool {
    tr.metadata
        .get(EVICTED_METADATA_KEY)
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

fn fingerprint(tr: &ToolResultBlock) -> ToolResultFingerprint {
    ToolResultFingerprint {
        id: tr.id.clone(),
        name: tr.name.clone(),
        text: tr.text_content(),
    }
}

fn with_placeholder(tr: ToolResultBlock, placeholder: String) -> ToolResultBlock {
    let mut metadata = tr.metadata.clone();
    metadata.insert(EVICTED_METADATA_KEY.to_string(), serde_json::Value::Bool(true));
    ToolResultBlock {
        id: tr.id,
        name: tr.name,
        output: vec![ContentBlock::Text(TextBlock::new(placeholder))],
        metadata,
        state: tr.state,
    }
}

fn build_eviction_path(config: &ToolResultEvictionConfig, agent_name: &str, tool_call_id: &str, full_text: &str) -> String {
    let mut base = config.eviction_path.trim_end_matches('/').to_string();
    let safe_agent: String = agent_name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    let safe_id: String = tool_call_id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    let hash_full = sha256_hex(full_text.as_bytes());
    let hash = &hash_full[..8];
    base.push_str(&format!("/{safe_agent}/{safe_id}-{hash}"));
    base
}

fn build_placeholder(config: &ToolResultEvictionConfig, full_text: &str, eviction_path: &str) -> String {
    let len = full_text.chars().count();
    let p_len = config.preview_chars.min(len / 2);
    let head: String = full_text.chars().take(p_len).collect();
    let tail: String = full_text.chars().skip(len.saturating_sub(p_len)).take(p_len).collect();
    let mut sb = String::new();
    sb.push_str(&format!(
        "Tool output was too large ({len} chars) and has been saved to `{eviction_path}`.\n\n"
    ));
    if p_len > 0 {
        sb.push_str(&format!("Preview (first {p_len} chars):\n{head}\n\n... and last {p_len} chars:\n{tail}\n"));
    }
    sb
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::{MsgRole, TextBlock};
    use crate::filesystem::LocalFilesystem;

    fn big_tool_result(text: &str) -> ToolResultBlock {
        ToolResultBlock::new(
            "call_1",
            "execute",
            vec![ContentBlock::Text(TextBlock::new(text))],
        )
    }

    #[tokio::test]
    async fn maybe_evict_writes_file_and_returns_placeholder() {
        let dir = std::env::temp_dir().join(format!("agentscope_evict_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(dir));
        let config = ToolResultEvictionConfig::builder()
            .max_result_chars(10)
            .preview_chars(4)
            .eviction_path("blobs")
            .excluded_tool_names(std::collections::HashSet::new())
            .build();
        let ctx = RuntimeContext::new();
        let tr = big_tool_result("0123456789ABCDEF");
        let placeholder = maybe_evict(&fs, &config, "agent", &ctx, &tr).await;
        assert!(placeholder.is_some());
        let p = placeholder.unwrap();
        assert!(p.contains("blobs"));
        assert!(p.contains("0123")); // head preview
        // file written to disk
        let path = build_eviction_path(&config, "agent", "call_1", "0123456789ABCDEF");
        assert!(fs.exists(&ctx, &path).await.unwrap());
    }

    #[tokio::test]
    async fn small_tool_result_is_not_evicted() {
        let dir = std::env::temp_dir().join(format!("agentscope_evict_small_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(dir));
        let config = ToolResultEvictionConfig::builder()
            .max_result_chars(1000)
            .eviction_path("blobs")
            .excluded_tool_names(std::collections::HashSet::new())
            .build();
        let ctx = RuntimeContext::new();
        let tr = big_tool_result("small");
        assert!(maybe_evict(&fs, &config, "agent", &ctx, &tr).await.is_none());
    }

    #[tokio::test]
    async fn evict_messages_rebuilds_only_oversized() {
        let dir = std::env::temp_dir().join(format!("agentscope_evict_msg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(dir));
        let config = ToolResultEvictionConfig::builder()
            .max_result_chars(10)
            .preview_chars(4)
            .eviction_path("blobs")
            .excluded_tool_names(std::collections::HashSet::new())
            .build();
        let ctx = RuntimeContext::new();
        let big = Msg::builder_for_role(Some(MsgRole::Tool))
            .content(vec![ContentBlock::ToolResult(big_tool_result("0123456789ABCDEF"))])
            .build();
        let small = Msg::tool("call_2", "execute", "tiny");
        let messages = vec![big, small];
        let (rebuilt, changed) =
            evict_messages(&fs, &config, "agent", &ctx, messages, &mut HashMap::new()).await;
        assert!(changed);
        assert_eq!(rebuilt.len(), 2);
        // oversized block replaced with placeholder + evicted metadata
        let tr = match &rebuilt[0].content[0] {
            ContentBlock::ToolResult(t) => t,
            _ => panic!("expected tool result"),
        };
        assert!(is_evicted(tr));
        assert!(tr.text_content().contains("blobs"));
        // small untouched
        let small_tr = match &rebuilt[1].content[0] {
            ContentBlock::ToolResult(t) => t,
            _ => panic!("expected tool result"),
        };
        assert_eq!(small_tr.text_content(), "tiny");
        assert!(!is_evicted(small_tr));
    }
}
