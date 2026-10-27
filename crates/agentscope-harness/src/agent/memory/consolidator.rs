//! LLM-based consolidation of daily memory ledgers into the curated `MEMORY.md`,
//! mirroring `MemoryConsolidator`.
//!
//! This component owns the second layer of the two-layer memory model:
//! - Layer 1 — daily ledger (`memory/YYYY-MM-DD.md`), written by
//!   [`super::flush::MemoryFlushManager`], append-only.
//! - Layer 2 — curated `MEMORY.md`, owned here. Periodically reads the daily ledgers
//!   modified since the last consolidation watermark plus the current `MEMORY.md`, asks
//!   the LLM to merge / dedupe / trim, and overwrites `MEMORY.md` with the result.

use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;
use agentscope_core::message::{ContentBlock, Msg, MsgRole, TextBlock};
use agentscope_core::model::{GenerateOptions, Model};

use crate::filesystem::{FileInfo, Filesystem};

use super::flush::fileio;

/// Hidden state file inside `memory/` tracking the last consolidation Instant.
pub const STATE_FILE: &str = ".consolidation_state";

/// Default prompt for the consolidation step. Contains exactly two `%d` placeholders
/// (max-tokens, max-chars) filled in at call time.
pub const DEFAULT_CONSOLIDATION_PROMPT: &str = "\
You are a memory consolidation assistant. You own the curated long-term memory file \
MEMORY.md. Your job is to merge new daily ledger entries into MEMORY.md while keeping it \
concise, deduplicated, and high-signal.

You are given two inputs:
1. The current MEMORY.md content (the existing curated long-term memory).
2. New daily ledger entries that have been appended since the last consolidation.

Rules:
- MEMORY.md is the single source of truth for cross-day, cross-session knowledge. Keep it \
stable and authoritative.
- Daily ledger entries are stream-of-consciousness flush logs — they may be noisy or \
redundant. Promote only what is durable and reusable.
- Deduplicate: if a new entry restates something MEMORY.md already covers, skip it.
- Merge related facts: combine entries about the same topic into cohesive paragraphs with \
clear section headers.
- Update or remove stale information when new entries supersede it.
- Keep total output within %d tokens (approximately %d characters); prioritize recent and \
frequently-referenced information when trimming.

Output the COMPLETE new MEMORY.md content (not just a diff). Use markdown.";

/// Workspace-relative path to the consolidation watermark state file.
pub const STATE_REL_PATH: &str = "memory/.consolidation_state";

/// Consolidates daily memory ledgers into the curated `MEMORY.md` via an LLM.
pub struct MemoryConsolidator {
    fs: Arc<dyn Filesystem>,
    model: Arc<dyn Model>,
    consolidation_prompt: String,
    max_memory_tokens: usize,
}

impl MemoryConsolidator {
    /// Creates a consolidator using the default prompt and a 4000-token budget.
    pub fn new(fs: Arc<dyn Filesystem>, model: Arc<dyn Model>) -> Self {
        Self::new_with_prompt(fs, model, None, 4_000)
    }

    /// Creates a consolidator with an optional prompt override and token budget.
    pub fn new_with_prompt(
        fs: Arc<dyn Filesystem>,
        model: Arc<dyn Model>,
        consolidation_prompt: Option<String>,
        max_memory_tokens: usize,
    ) -> Self {
        Self {
            fs,
            model,
            consolidation_prompt: consolidation_prompt
                .unwrap_or_else(|| DEFAULT_CONSOLIDATION_PROMPT.to_string()),
            max_memory_tokens,
        }
    }

    /// Runs consolidation: reads daily files modified after the last watermark and the
    /// current `MEMORY.md`, uses the LLM to merge them, overwrites `MEMORY.md`, and
    /// advances the watermark on success. No-op when there are no fresh daily entries.
    pub async fn consolidate(&self, ctx: &RuntimeContext) -> Result<()> {
        let watermark = self.read_watermark(ctx).await;
        let current_memory = fileio::read_text(&self.fs, ctx, "MEMORY.md").await;
        let daily_entries = self.read_daily_entries(ctx, watermark).await;
        if daily_entries.trim().is_empty() {
            return Ok(());
        }

        let max_chars = self.max_memory_tokens * 4;
        let system_prompt = self
            .consolidation_prompt
            .replacen("%d", &self.max_memory_tokens.to_string(), 1)
            .replacen("%d", &max_chars.to_string(), 1);

        let mut user_content = String::new();
        user_content.push_str(&format!(
            "Current MEMORY.md:\n{}\n\nNew daily ledger entries to merge:\n{daily_entries}",
            if current_memory.trim().is_empty() {
                "(empty)"
            } else {
                &current_memory
            }
        ));

        let input = vec![
            Msg::builder()
                .role(MsgRole::System)
                .content(vec![ContentBlock::Text(TextBlock::new(system_prompt))])
                .build(),
            Msg::builder()
                .role(MsgRole::User)
                .content(vec![ContentBlock::Text(TextBlock::new(user_content))])
                .build(),
        ];

        let consolidated = self.collect_text(input).await.trim().to_string();
        if consolidated.is_empty() {
            return Ok(());
        }
        fileio::write_overwrite(&self.fs, ctx, "MEMORY.md", &consolidated).await?;
        self.write_watermark(ctx).await;
        Ok(())
    }

    /// Reads daily memory files in `memory/`, excluding the state file and directories.
    async fn read_daily_entries(
        &self,
        ctx: &RuntimeContext,
        watermark: chrono::DateTime<chrono::Utc>,
    ) -> String {
        let mut eligible: Vec<FileInfo> = match self.fs.ls(ctx, "memory").await {
            Ok(files) => files
                .into_iter()
                .filter(|f| {
                    !f.is_directory
                        && file_name(&f.path) != STATE_FILE
                        && file_name(&f.path) != "archive"
                        && f.path.ends_with(".md")
                })
                .filter(|f| is_modified_after(f, watermark))
                .collect(),
            Err(_) => Vec::new(),
        };
        eligible.sort_by(|a, b| file_name(&a.path).cmp(&file_name(&b.path)));

        let mut sb = String::new();
        for fi in eligible {
            let rel = to_relative(&fi.path);
            let content = fileio::read_text(&self.fs, ctx, &rel).await;
            if !content.trim().is_empty() {
                sb.push_str(&format!(
                    "### {}\n{}\n\n",
                    file_name(&fi.path),
                    content.trim()
                ));
            }
        }
        sb
    }

    async fn collect_text(&self, messages: Vec<Msg>) -> String {
        use futures::StreamExt;
        let mut out = String::new();
        let mut stream = self
            .model
            .stream(messages, Vec::new(), GenerateOptions::default());
        while let Some(resp) = stream.next().await {
            if let Ok(r) = resp {
                for block in &r.content {
                    if let ContentBlock::Text(t) = block {
                        out.push_str(&t.text);
                    }
                }
            }
        }
        out
    }

    async fn read_watermark(&self, ctx: &RuntimeContext) -> chrono::DateTime<chrono::Utc> {
        let value = fileio::read_text(&self.fs, ctx, STATE_REL_PATH).await;
        let value = value.trim();
        if value.is_empty() {
            return chrono::DateTime::UNIX_EPOCH;
        }
        chrono::DateTime::parse_from_rfc3339(value)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or(chrono::DateTime::UNIX_EPOCH)
    }

    async fn write_watermark(&self, ctx: &RuntimeContext) {
        let _ = fileio::write_overwrite(
            &self.fs,
            ctx,
            STATE_REL_PATH,
            &chrono::Utc::now().to_rfc3339(),
        )
        .await;
    }
}

fn is_modified_after(fi: &FileInfo, watermark: chrono::DateTime<chrono::Utc>) -> bool {
    if fi.modified_at.is_empty() {
        return true; // be safe — include on unknown mtime
    }
    match chrono::DateTime::parse_from_rfc3339(&fi.modified_at) {
        Ok(dt) => dt.with_timezone(&chrono::Utc) > watermark,
        Err(_) => true,
    }
}

/// Extracts the last path segment (file name) from a slash path.
fn file_name(path: &str) -> String {
    let stripped = path.trim_end_matches('/');
    match stripped.rfind('/') {
        Some(idx) => stripped[idx + 1..].to_string(),
        None => stripped.to_string(),
    }
}

/// Converts an absolute slash path to a workspace-relative path.
fn to_relative(path: &str) -> String {
    path.trim_start_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agentscope_core::message::{ChatUsage, TextBlock};
    use agentscope_core::model::{ChatResponse, ToolSchema};
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream};
    use futures::StreamExt;

    use crate::filesystem::LocalFilesystem;

    use super::*;

    struct FakeModel {
        text: String,
    }

    #[async_trait]
    impl Model for FakeModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<ToolSchema>,
            _options: GenerateOptions,
        ) -> BoxStream<'_, Result<ChatResponse>> {
            let text = self.text.clone();
            let resp = ChatResponse::new(
                "r1",
                vec![ContentBlock::Text(TextBlock::new(text))],
                ChatUsage::default(),
                None,
                None,
            );
            stream::once(async { Ok(resp) }).boxed()
        }

        fn model_name(&self) -> &str {
            "fake"
        }
    }

    fn temp_fs(tag: &str) -> Arc<dyn Filesystem> {
        let dir = std::env::temp_dir().join(format!(
            "agentscope_mem_consol_{tag}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(LocalFilesystem::sandboxed(dir))
    }

    #[tokio::test]
    async fn consolidate_merges_daily_ledger_into_memory_md() {
        let fs = temp_fs("merge");
        let ctx = RuntimeContext::new();
        // Seed a daily ledger file.
        fileio::write_overwrite(&fs, &ctx, "memory/2025-01-01.md", "- team uses Rust\n")
            .await
            .unwrap();

        let model: Arc<dyn Model> = Arc::new(FakeModel {
            text: "# Long-term memory\n\n- We use Rust everywhere.\n".into(),
        });
        let consolidator = MemoryConsolidator::new(fs.clone(), model);
        consolidator.consolidate(&ctx).await.unwrap();

        let memory = fileio::read_text(&fs, &ctx, "MEMORY.md").await;
        assert!(memory.contains("We use Rust everywhere"), "got {memory}");
        // Watermark advanced.
        let state = fileio::read_text(&fs, &ctx, STATE_REL_PATH).await;
        assert!(!state.trim().is_empty(), "expected watermark to be written");
    }

    #[tokio::test]
    async fn consolidate_is_noop_without_daily_entries() {
        let fs = temp_fs("noop");
        let ctx = RuntimeContext::new();
        let model: Arc<dyn Model> = Arc::new(FakeModel {
            text: "# memory\n".into(),
        });
        let consolidator = MemoryConsolidator::new(fs.clone(), model);
        consolidator.consolidate(&ctx).await.unwrap();
        let memory = fileio::read_text(&fs, &ctx, "MEMORY.md").await;
        assert!(memory.is_empty());
    }

    #[test]
    fn file_name_and_relative_helpers() {
        assert_eq!(file_name("/memory/2025-01-01.md"), "2025-01-01.md");
        assert_eq!(file_name("2025-01-01.md"), "2025-01-01.md");
        assert_eq!(to_relative("/memory/2025-01-01.md"), "memory/2025-01-01.md");
    }
}
