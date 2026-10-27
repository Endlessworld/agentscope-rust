//! `AtPathExpansionMiddleware` — expands `@path` references in inbound messages by
//! inlining the referenced file contents, mirroring
//! `io.agentscope.harness.agent.middleware.AtPathExpansionMiddleware`.
//!
//! Expansion happens at the `on_agent` interception point so the model sees the full
//! file text instead of the `@`-reference. Reads go through the agent's `Filesystem`,
//! so sandbox/remote backends are honoured. Tokens that do not resolve to a readable
//! text file are left untouched.
//!
//! Two properties matter and are easy to get wrong:
//!
//! 1. Previews are **appended as a new block**. The original blocks (images, audio,
//!    structured content) must survive, so the text is never rewritten in place.
//! 2. Attached content is **neutralized**, not escaped: a file containing
//!    `</attached_file>` must not be able to close the wrapper early, but ordinary
//!    prose (`&&`, `<T>`) must stay readable.
use std::collections::BTreeMap;
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::{ContentBlock, Msg, TextBlock};
use agentscope_core::middleware::{AgentInput, EventStream, ExtensionPoint, Middleware, Next};
use futures::stream::{self, StreamExt};

use crate::filesystem::Filesystem;

/// Maximum lines pulled in from a single referenced file, mirroring Java
/// `MAX_ATTACHED_LINES`.
const MAX_ATTACHED_LINES: usize = 1000;

/// Closing tag of an attachment block; also the string neutralised inside attached
/// content.
const ATTACHED_FILE_CLOSE_TAG: &str = "</attached_file>";

/// Lead-in for the appended preview block; states up front that previews may be
/// truncated so the model reads the original path when completeness matters.
const ATTACHMENT_LEAD_IN: &str = "Attached file previews follow; previews may be truncated - read \
                                   the original path with file tools when complete content \
                                   matters.";

/// Total budget for all attachments across one message, mirroring Java's 64_000 chars.
const DEFAULT_MAX_TOTAL_CHARS: usize = 64_000;

/// Middleware that expands `@path` references in agent input messages.
pub struct AtPathExpansionMiddleware {
    fs: Arc<dyn Filesystem>,
    max_total_chars: usize,
}

impl AtPathExpansionMiddleware {
    pub fn new(fs: Arc<dyn Filesystem>) -> Self {
        Self {
            fs,
            max_total_chars: DEFAULT_MAX_TOTAL_CHARS,
        }
    }

    /// Caps the total number of characters attached per message.
    pub fn with_max_total_chars(mut self, max: usize) -> Self {
        self.max_total_chars = max;
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
        let max_total = self.max_total_chars;
        let fut = async move {
            let mut input = input;
            let mut messages = input.messages.clone();
            for msg in &mut messages {
                expand_message(msg, &fs, max_total).await;
            }
            input.messages = messages;
            next(input)
        };
        stream::once(fut).flatten().boxed()
    }

    /// Narrow declaration, mirroring Java `AtPathExpansionMiddleware.activePoints()`.
    fn active_points(&self) -> Vec<ExtensionPoint> {
        vec![ExtensionPoint::OnAgent]
    }
}

async fn expand_message(msg: &mut Msg, fs: &Arc<dyn Filesystem>, max_total_chars: usize) {
    // Only user messages are expanded (Java skips every other role).
    if msg.role != agentscope_core::message::MsgRole::User {
        return;
    }
    let Some(text) = collect_text(msg) else {
        return;
    };
    if !text.contains('@') {
        return;
    }

    let mut remaining = max_total_chars;
    // Insertion order matters for the rendered output, so keep it stable.
    let mut attached: BTreeMap<String, String> = BTreeMap::new();
    for reference in scan_references(&text) {
        if remaining == 0 {
            break;
        }
        if attached.contains_key(&reference) {
            continue;
        }
        let Some(mut content) = try_read(fs, &reference).await else {
            continue;
        };
        if content.chars().count() > remaining {
            let truncated: String = content.chars().take(remaining).collect();
            content = format!(
                "{truncated}\n[Attachment preview truncated; read the original path for more.]"
            );
        }
        remaining = remaining.saturating_sub(content.chars().count());
        attached.insert(reference, content);
    }

    if attached.is_empty() {
        return;
    }

    let mut preview = String::from(ATTACHMENT_LEAD_IN);
    for (path, content) in attached {
        let content = neutralize_close_tag(&content);
        preview.push_str("\n\n<attached_file path=\"");
        preview.push_str(&path);
        preview.push_str("\">\n");
        preview.push_str(&content);
        if !content.ends_with('\n') {
            preview.push('\n');
        }
        preview.push_str(ATTACHED_FILE_CLOSE_TAG);
    }

    // Append as a new block: image and other non-text blocks must survive expansion.
    msg.content
        .push(ContentBlock::Text(TextBlock::new(preview)));
}

/// Concatenates the message's text blocks with `\n`, mirroring Java
/// `Msg#getTextContent()` (`Collectors.joining("\n")`).
///
/// Joining without a separator would splice the tail of one block onto the head of the
/// next, turning `"see @src/a.rs"` + `"and more"` into the bogus reference `src/a.rsand`.
fn collect_text(msg: &Msg) -> Option<String> {
    let texts: Vec<&str> = msg
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(tb) => Some(tb.text.as_str()),
            _ => None,
        })
        .collect();
    if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n"))
    }
}

/// Neutralizes `</attached_file>` inside attached content by escaping its slash, so a
/// file cannot terminate the wrapper early. Everything else is left verbatim.
fn neutralize_close_tag(content: &str) -> String {
    content.replace(ATTACHED_FILE_CLOSE_TAG, "<\\/attached_file>")
}

/// Extracts `@`-references from `text`, preserving first-seen order and skipping
/// duplicates.
///
/// Mirrors the Java `AT_PATH` regex closely enough for the shapes that matter:
/// absolute POSIX paths, Windows drive paths, `~/`, `./`, `../`, and `word/word` /
/// `word.word` forms. A leading `@` preceded by an identifier character is not a
/// reference (so `foo@bar.com` stays an email address).
fn scan_references(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'@' {
            i += 1;
            continue;
        }
        // `(?<![A-Za-z0-9_])`: a reference cannot start mid-identifier.
        if i > 0 {
            let prev = bytes[i - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' {
                i += 1;
                continue;
            }
        }
        let start = i + 1;
        let mut j = start;
        while j < bytes.len() {
            let c = bytes[j];
            let allowed = c.is_ascii_alphanumeric()
                || matches!(c, b'.' | b'/' | b'\\' | b'-' | b'_' | b'~' | b':');
            if !allowed {
                break;
            }
            j += 1;
        }
        let token = &text[start..j];
        if !token.is_empty() && looks_like_path(token) {
            let normalized = token.trim_start_matches("./").to_string();
            let key = if normalized.is_empty() {
                token.to_string()
            } else {
                normalized
            };
            if !out.contains(&key) {
                out.push(key);
            }
        }
        i = if j > start { j } else { start };
    }
    out
}

fn looks_like_path(token: &str) -> bool {
    token.contains('/') || token.contains('\\') || token.contains('.') || token.starts_with('~')
}

/// Reads at most [`MAX_ATTACHED_LINES`] lines, skipping binary attachments.
async fn try_read(fs: &Arc<dyn Filesystem>, path: &str) -> Option<String> {
    let rt = RuntimeContext::default();
    let data = fs.read(&rt, path, 0, MAX_ATTACHED_LINES).await.ok()?;
    // Base64 in a user message is rarely useful and just burns tokens.
    if data.encoding != "utf-8" {
        return None;
    }
    let mut content = data.content;
    if content.lines().count() >= MAX_ATTACHED_LINES {
        content.push_str(&format!(
            "\n[Attachment preview limited to the first {MAX_ATTACHED_LINES} lines; read the \
             original path for more.]"
        ));
    }
    Some(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::LocalFilesystem;

    fn middleware(root: &std::path::Path) -> AtPathExpansionMiddleware {
        AtPathExpansionMiddleware::new(Arc::new(LocalFilesystem::new(root.to_path_buf())))
    }

    fn user_msg(text: &str) -> Msg {
        Msg::builder()
            .role(agentscope_core::message::MsgRole::User)
            .content(vec![ContentBlock::Text(TextBlock::new(text))])
            .build()
    }

    #[test]
    fn closing_tags_are_neutralized_but_prose_is_verbatim() {
        let neutralized = neutralize_close_tag("a && b </attached_file> <T>");
        assert!(neutralized.contains("a && b"), "{neutralized}");
        assert!(neutralized.contains("<T>"), "{neutralized}");
        assert!(neutralized.contains("<\\/attached_file>"), "{neutralized}");
        assert_eq!(
            neutralized.matches("</attached_file>").count(),
            0,
            "{neutralized}"
        );
    }

    #[test]
    fn email_addresses_are_not_treated_as_references() {
        assert!(scan_references("write to foo@bar.com please").is_empty());
    }

    #[test]
    fn references_are_deduplicated_in_first_seen_order() {
        let refs = scan_references("see @src/a.rs and @src/b.rs then @src/a.rs");
        assert_eq!(refs, vec!["src/a.rs".to_string(), "src/b.rs".to_string()]);
    }

    #[test]
    fn dot_slash_prefixes_are_stripped() {
        assert_eq!(scan_references("@./src/a.rs"), vec!["src/a.rs".to_string()]);
    }

    #[tokio::test]
    async fn previews_are_appended_as_a_new_block_and_keep_existing_blocks() {
        let dir = std::env::temp_dir().join(format!("atpath-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "fn main() {}\n").unwrap();

        let mw = middleware(&dir);
        let mut msg = user_msg("look at @src/a.rs");
        // A pre-existing non-text block must survive expansion.
        msg.content
            .push(ContentBlock::Text(TextBlock::new("keep me")));
        expand_message(&mut msg, &mw.fs, DEFAULT_MAX_TOTAL_CHARS).await;

        assert_eq!(
            msg.content.len(),
            3,
            "preview must be appended, not replace"
        );
        let preview = match msg.content.last() {
            Some(ContentBlock::Text(tb)) => tb.text.clone(),
            other => panic!("expected the appended text block, got {other:?}"),
        };
        assert!(preview.starts_with(ATTACHMENT_LEAD_IN), "{preview}");
        assert!(
            preview.contains("<attached_file path=\"src/a.rs\">"),
            "{preview}"
        );
        assert!(preview.contains("fn main() {}"), "{preview}");
        assert!(preview.contains("</attached_file>"), "{preview}");

        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn non_user_messages_are_not_expanded() {
        let dir = std::env::temp_dir().join(format!("atpath-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "secret content\n").unwrap();

        let mw = middleware(&dir);
        let mut msg = Msg::builder()
            .role(agentscope_core::message::MsgRole::Assistant)
            .content(vec![ContentBlock::Text(TextBlock::new("@src/a.rs"))])
            .build();
        expand_message(&mut msg, &mw.fs, DEFAULT_MAX_TOTAL_CHARS).await;
        assert_eq!(
            msg.content.len(),
            1,
            "assistant messages must be left alone"
        );

        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn oversized_files_are_truncated_with_a_notice() {
        let dir = std::env::temp_dir().join(format!("atpath-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/big.rs"),
            "line\n".repeat(MAX_ATTACHED_LINES + 50),
        )
        .unwrap();

        let mw = middleware(&dir);
        let mut msg = user_msg("@src/big.rs");
        expand_message(&mut msg, &mw.fs, DEFAULT_MAX_TOTAL_CHARS).await;
        let preview = match msg.content.last() {
            Some(ContentBlock::Text(tb)) => tb.text.clone(),
            other => panic!("expected preview, got {other:?}"),
        };
        assert!(
            preview.contains("Attachment preview limited to the first 1000 lines"),
            "{preview}"
        );

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn active_points_declares_only_the_agent_hook() {
        let dir = std::env::temp_dir();
        let points = middleware(&dir).active_points();
        assert_eq!(points, vec![ExtensionPoint::OnAgent]);
    }
}
