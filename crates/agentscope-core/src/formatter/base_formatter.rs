//! Base formatter providing common Msg→provider-request conversion helpers,
//! mirroring `io.agentscope.core.formatter.AbstractBaseFormatter`.
//!
//! The Java formatter is generic over provider-specific request/response types.
//! In Rust we keep the shared transformation logic provider-agnostic: providers
//! (e.g. `OpenAiChatModel`) reuse these helpers to convert `Msg` into their own
//! request message shape.

use crate::message::{ContentBlock, Msg, MsgRole, Source};

/// Metadata key controlling whether a message bypasses multi-agent history merging.
///
/// Re-exported from [`crate::message::keys::metadata_keys`] so both formatter paths agree:
/// the Java SDK writes `_bypass_multiagent_history_merge`, and this module previously kept a
/// second, differing literal (`bypassMultiagentHistoryMerge`) which made the check blind to
/// Java-written markers.
pub const BYPASS_MULTIAGENT_HISTORY_MERGE: &str =
    crate::message::keys::metadata_keys::BYPASS_MULTIAGENT_HISTORY_MERGE;

/// Extracts plain text from a message, skipping `ThinkingBlock`s (they are never
/// sent back to LLM APIs) and flattening `ToolResultBlock` outputs.
pub fn extract_text_content(msg: &Msg) -> String {
    msg.content
        .iter()
        .flat_map(|block| match block {
            ContentBlock::Text(tb) => vec![tb.text.clone()],
            ContentBlock::Hint(hb) => vec![hb.hint.clone().unwrap_or_default()],
            // IMPORTANT: ThinkingBlock is stored in memory but skipped when formatting.
            ContentBlock::Thinking(_) => Vec::new(),
            ContentBlock::ToolResult(tr) => tr
                .output
                .iter()
                .filter_map(|output| match output {
                    ContentBlock::Text(tb) => Some(tb.text.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extracts text from a single content block (text/hint), empty string otherwise.
pub fn extract_text_content_from_block(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(tb) => tb.text.clone(),
        ContentBlock::Hint(hb) => hb.hint.clone().unwrap_or_default(),
        _ => String::new(),
    }
}

/// Returns `true` when the message contains multimodal content
/// (image/audio/video/data blocks).
pub fn has_media_content(msg: &Msg) -> bool {
    msg.content.iter().any(|block| {
        matches!(
            block,
            ContentBlock::Image(_)
                | ContentBlock::Audio(_)
                | ContentBlock::Video(_)
                | ContentBlock::Data(_)
        )
    })
}

/// Formats a role into a human-readable label.
pub fn format_role_label(role: MsgRole) -> &'static str {
    match role {
        MsgRole::User => "User",
        MsgRole::Assistant => "Assistant",
        MsgRole::System => "System",
        MsgRole::Tool => "Tool",
    }
}

/// Returns `true` when the message should bypass history merging in multi-agent
/// formatters, based on the `bypassMultiagentHistoryMerge` metadata flag.
pub fn should_bypass_history(msg: &Msg) -> bool {
    msg.metadata
        .get(BYPASS_MULTIAGENT_HISTORY_MERGE)
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Converts tool result output blocks to a string representation. Multimodal
/// blocks are replaced with textual references (URL or temp file path).
pub fn convert_tool_result_to_string(output: &[ContentBlock]) -> String {
    if output.is_empty() {
        return String::new();
    }
    let textual: Vec<String> = output
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(tb) => Some(tb.text.clone()),
            ContentBlock::Image(ib) => {
                Some(convert_media_block_to_text_reference(&ib.source, "image"))
            }
            ContentBlock::Audio(ab) => {
                Some(convert_media_block_to_text_reference(&ab.source, "audio"))
            }
            ContentBlock::Video(vb) => {
                Some(convert_media_block_to_text_reference(&vb.source, "video"))
            }
            ContentBlock::Data(db) => {
                Some(convert_media_block_to_text_reference(&db.source, "data"))
            }
            // Other block types (e.g. ThinkingBlock) are ignored.
            _ => None,
        })
        .collect();

    // Single item returns directly; multiple items use "- " prefix.
    if textual.len() == 1 {
        textual.into_iter().next().unwrap_or_default()
    } else {
        textual
            .into_iter()
            .map(|s| format!("- {s}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Converts a media block (image/audio/video/data) into a textual reference.
fn convert_media_block_to_text_reference(source: &Source, media_type: &str) -> String {
    match source {
        Source::Url(url_source) => {
            format!(
                "The returned {media_type} can be found at: {}",
                url_source.url
            )
        }
        Source::Base64(b64) => match save_base64_data_to_temp_file(&b64.media_type, &b64.data) {
            Ok(file_path) => format!("The returned {media_type} can be found at: {file_path}"),
            Err(e) => format!("[{media_type} - failed to save file: {e}]"),
        },
        Source::FileId(file) => {
            format!("The returned {media_type} can be found at: fileid://{}", file.file_id)
        }
    }
}

/// Saves base64-encoded data to a temporary file and returns its absolute path.
pub fn save_base64_data_to_temp_file(
    media_type: &str,
    base64_data: &str,
) -> std::io::Result<String> {
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine as _;

    // Extract extension from MIME type (e.g. "image/png" -> "png").
    let extension = if media_type.contains('/') {
        media_type.split('/').nth(1).unwrap_or(media_type)
    } else {
        media_type
    };

    let mut path = std::env::temp_dir();
    path.push(format!("agentscope_{}", uuid::Uuid::new_v4()));
    path.set_extension(extension);

    let decoded = BASE64
        .decode(base64_data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&path, decoded)?;
    Ok(path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{
        Base64Source, HintBlock, ImageBlock, Source, TextBlock, ThinkingBlock, ToolResultBlock,
        URLSource,
    };

    fn user_msg(blocks: Vec<ContentBlock>) -> Msg {
        Msg::builder().role(MsgRole::User).content(blocks).build()
    }

    /// Builds a message with arbitrary content (role validation is skipped for
    /// non-user roles; Tool role accepts any block type).
    fn any_msg(blocks: Vec<ContentBlock>) -> Msg {
        Msg::builder().role(MsgRole::Tool).content(blocks).build()
    }

    #[test]
    fn extract_text_skips_thinking_and_flattens_tool_results() {
        let msg = any_msg(vec![
            ContentBlock::Text(TextBlock::new("hello")),
            ContentBlock::Thinking(ThinkingBlock::new("reasoning")),
            ContentBlock::Hint(HintBlock::new("hint-1", "hint")),
        ]);
        assert_eq!(extract_text_content(&msg), "hello\nhint");
    }

    #[test]
    fn extract_text_from_tool_result_output() {
        let tr = ToolResultBlock::text("result text");
        let msg = any_msg(vec![ContentBlock::ToolResult(tr)]);
        assert_eq!(extract_text_content(&msg), "result text");
    }

    #[test]
    fn media_content_detection() {
        let text_only = user_msg(vec![ContentBlock::Text(TextBlock::new("x"))]);
        assert!(!has_media_content(&text_only));

        let with_image = user_msg(vec![ContentBlock::Image(ImageBlock::new(Source::Url(
            URLSource::new("https://x/a.png"),
        )))]);
        assert!(has_media_content(&with_image));
    }

    #[test]
    fn role_labels() {
        assert_eq!(format_role_label(MsgRole::User), "User");
        assert_eq!(format_role_label(MsgRole::Assistant), "Assistant");
        assert_eq!(format_role_label(MsgRole::System), "System");
        assert_eq!(format_role_label(MsgRole::Tool), "Tool");
    }

    #[test]
    fn bypass_history_flag() {
        let normal = Msg::user("x");
        assert!(!should_bypass_history(&normal));

        let bypass = Msg::builder()
            .role(MsgRole::User)
            .text_content("x")
            .metadata_entry(BYPASS_MULTIAGENT_HISTORY_MERGE, serde_json::json!(true))
            .build();
        assert!(should_bypass_history(&bypass));
    }

    /// The Java SDK writes `_bypass_multiagent_history_merge`; this formatter used to look for a
    /// different literal (`bypassMultiagentHistoryMerge`), so Java-written markers were invisible.
    #[test]
    fn bypass_history_honours_the_java_metadata_key() {
        let java_style = Msg::builder()
            .role(MsgRole::User)
            .text_content("x")
            .metadata_entry("_bypass_multiagent_history_merge", serde_json::json!(true))
            .build();
        assert!(should_bypass_history(&java_style));
        assert_eq!(
            BYPASS_MULTIAGENT_HISTORY_MERGE,
            crate::message::keys::metadata_keys::BYPASS_MULTIAGENT_HISTORY_MERGE
        );
    }

    #[test]
    fn convert_tool_result_string_shapes() {
        // Empty -> empty
        assert_eq!(convert_tool_result_to_string(&[]), "");

        // Single text -> direct
        let single = vec![ContentBlock::Text(TextBlock::new("done"))];
        assert_eq!(convert_tool_result_to_string(&single), "done");

        // Multiple -> "- " prefixed, newline joined
        let multi = vec![
            ContentBlock::Text(TextBlock::new("a")),
            ContentBlock::Text(TextBlock::new("b")),
        ];
        assert_eq!(convert_tool_result_to_string(&multi), "- a\n- b");

        // URL image -> textual reference
        let img = vec![ContentBlock::Image(ImageBlock::new(Source::Url(
            URLSource::new("https://x/a.png"),
        )))];
        assert_eq!(
            convert_tool_result_to_string(&img),
            "The returned image can be found at: https://x/a.png"
        );
    }

    #[test]
    fn convert_tool_result_base64_saves_temp_file() {
        let b64 = "aGVsbG8="; // "hello"
        let img = vec![ContentBlock::Image(ImageBlock::new(Source::Base64(
            Base64Source::new("image/png", b64),
        )))];
        let out = convert_tool_result_to_string(&img);
        assert!(out.starts_with("The returned image can be found at: "));
        let path = out.trim_start_matches("The returned image can be found at: ");
        assert!(std::path::Path::new(path).exists());
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn save_base64_temp_file_roundtrip() {
        let path = save_base64_data_to_temp_file("image/png", "aGVsbG8=").unwrap();
        assert!(path.ends_with(".png"));
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "hello");
        std::fs::remove_file(&path).ok();
    }
}
