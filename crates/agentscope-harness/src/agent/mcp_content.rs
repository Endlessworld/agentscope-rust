//! MCP `CallToolResult` → core [`ToolResultBlock`] conversion.
//!
//! Mirrors Java `McpContentConverter` (called from `McpTool#convertCallToolResult`):
//! text, image and audio content become the matching core content block, embedded
//! resources are rendered into their richest supported form, and anything without a
//! core representation degrades to descriptive text. Nothing here panics or fails the
//! tool call: a result the model can read is always produced.

use agentscope_core::message::{
    AudioBlock, Base64Source, ContentBlock, ImageBlock, Source, TextBlock, ToolResultBlock,
};
use rust_mcp_sdk::schema::{
    CallToolResult, ContentBlock as McpContentBlock, EmbeddedResource, EmbeddedResourceResource,
    ResourceLink,
};

/// Converts an MCP tool call result into a core [`ToolResultBlock`].
///
/// * text / image / audio content map to the matching core block (base64 sources, like
///   the providers expect);
/// * embedded text resources keep their text, blob resources become an image/audio
///   block when their MIME type says so and a descriptive text otherwise (the base64
///   payload is never dumped into the context);
/// * resource links render as `uri` + `name` text;
/// * an empty `content` falls back to the pretty-printed `structuredContent`;
/// * `isError` keeps the historical error semantics: the text still reaches the model,
///   the block is marked as an error.
///
/// The MCP content enum is closed, so there is no catch-all arm: a new content type is
/// a compile error here rather than a silently dropped block.
pub fn convert_call_tool_result(tool_name: &str, result: &CallToolResult) -> ToolResultBlock {
    let mut blocks: Vec<ContentBlock> = Vec::new();
    for block in &result.content {
        match block {
            McpContentBlock::TextContent(text) => {
                blocks.push(ContentBlock::Text(TextBlock::new(text.text.clone())))
            }
            McpContentBlock::ImageContent(image) => {
                blocks.push(ContentBlock::Image(ImageBlock::new(Source::Base64(
                    Base64Source::new(image.mime_type.clone(), image.data.clone()),
                ))))
            }
            McpContentBlock::AudioContent(audio) => {
                blocks.push(ContentBlock::Audio(AudioBlock::new(Source::Base64(
                    Base64Source::new(audio.mime_type.clone(), audio.data.clone()),
                ))))
            }
            McpContentBlock::EmbeddedResource(resource) => {
                blocks.extend(embedded_resource_blocks(resource))
            }
            McpContentBlock::ResourceLink(link) => blocks.push(ContentBlock::Text(TextBlock::new(
                render_resource_link(link),
            ))),
        }
    }

    if blocks.is_empty() {
        if let Some(structured) = &result.structured_content {
            let text = serde_json::to_string_pretty(&serde_json::Value::Object(structured.clone()))
                .unwrap_or_default();
            blocks.push(ContentBlock::Text(TextBlock::new(text)));
        }
    }

    if result.is_error == Some(true) {
        return ToolResultBlock::error(plain_text(&blocks));
    }

    if blocks.is_empty() {
        // Every provider accepts text; a bare `output: []` reads as a malformed result.
        blocks.push(ContentBlock::Text(TextBlock::new(String::new())));
    }

    ToolResultBlock::new("", tool_name, blocks)
}

/// Converts the JSON form a [`McpClientManager`](super::mcp_client_manager::McpClientManager)
/// tool call returns, falling back to a text rendering of the raw payload when it is not
/// a valid `CallToolResult`.
///
/// The fallback keeps the previous behaviour (pretty JSON) instead of introducing a new
/// failure path on an unparseable result.
pub fn convert_call_tool_result_value(
    tool_name: &str,
    value: &serde_json::Value,
) -> ToolResultBlock {
    match serde_json::from_value::<CallToolResult>(value.clone()) {
        Ok(result) => convert_call_tool_result(tool_name, &result),
        Err(_) => {
            let text = match value {
                serde_json::Value::String(text) => text.clone(),
                other => serde_json::to_string_pretty(other).unwrap_or_default(),
            };
            ToolResultBlock::new(
                "",
                tool_name,
                vec![ContentBlock::Text(TextBlock::new(text))],
            )
        }
    }
}

/// Renders one embedded resource: text keeps its body, media blobs become the matching
/// block, everything else becomes a short description of what was omitted.
fn embedded_resource_blocks(resource: &EmbeddedResource) -> Vec<ContentBlock> {
    match &resource.resource {
        EmbeddedResourceResource::TextResourceContents(text) => {
            vec![ContentBlock::Text(TextBlock::new(text.text.clone()))]
        }
        EmbeddedResourceResource::BlobResourceContents(blob) => {
            let media_type = blob.mime_type.clone().unwrap_or_default();
            if media_type.starts_with("image/") {
                vec![ContentBlock::Image(ImageBlock::new(Source::Base64(
                    Base64Source::new(media_type, blob.blob.clone()),
                )))]
            } else if media_type.starts_with("audio/") {
                vec![ContentBlock::Audio(AudioBlock::new(Source::Base64(
                    Base64Source::new(media_type, blob.blob.clone()),
                )))]
            } else {
                let kind = if media_type.is_empty() {
                    "unknown type"
                } else {
                    media_type.as_str()
                };
                vec![ContentBlock::Text(TextBlock::new(format!(
                    "[binary resource {} ({kind}, {} bytes, base64 omitted)]",
                    blob.uri,
                    blob.blob.len()
                )))]
            }
        }
    }
}

/// Renders a resource link as text (`name` + `uri`), the only representation a core
/// message has for a remote reference.
fn render_resource_link(link: &ResourceLink) -> String {
    format!("[resource link: {} ({})]", link.name, link.uri)
}

/// Flattens blocks to plain text for the error branch, where only text survives.
fn plain_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            ContentBlock::Image(_) => Some("[image]".to_string()),
            ContentBlock::Audio(_) => Some("[audio]".to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::ToolResultState;

    fn result_from(value: serde_json::Value) -> CallToolResult {
        serde_json::from_value(value).expect("valid CallToolResult")
    }

    fn text_of(block: &ToolResultBlock) -> String {
        block
            .output
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn text_content_becomes_a_text_block() {
        let result = result_from(serde_json::json!({
            "content": [{ "type": "text", "text": "hello" }]
        }));
        let block = convert_call_tool_result("echo", &result);
        assert_eq!(block.name.as_deref(), Some("echo"));
        assert!(matches!(
            block.output.as_slice(),
            [ContentBlock::Text(text)] if text.text == "hello"
        ));
    }

    #[test]
    fn text_and_image_keep_both_blocks() {
        let result = result_from(serde_json::json!({
            "content": [
                { "type": "text", "text": "see image" },
                { "type": "image", "data": "QUJD", "mimeType": "image/png" }
            ]
        }));
        let block = convert_call_tool_result("shot", &result);
        assert_eq!(block.output.len(), 2);
        let image = block
            .output
            .iter()
            .find_map(|b| match b {
                ContentBlock::Image(image) => Some(image),
                _ => None,
            })
            .expect("image block survives the conversion");
        assert_eq!(
            image.source,
            Source::Base64(Base64Source::new("image/png", "QUJD"))
        );
    }

    #[test]
    fn audio_content_becomes_an_audio_block() {
        let result = result_from(serde_json::json!({
            "content": [{ "type": "audio", "data": "QUJD", "mimeType": "audio/wav" }]
        }));
        let block = convert_call_tool_result("listen", &result);
        assert!(matches!(
            block.output.as_slice(),
            [ContentBlock::Audio(audio)]
                if audio.source == Source::Base64(Base64Source::new("audio/wav", "QUJD"))
        ));
    }

    #[test]
    fn embedded_text_resource_keeps_its_body() {
        let result = result_from(serde_json::json!({
            "content": [{
                "type": "resource",
                "resource": {
                    "uri": "file:///notes.md",
                    "mimeType": "text/markdown",
                    "text": "# notes"
                }
            }]
        }));
        let block = convert_call_tool_result("read", &result);
        assert!(matches!(
            block.output.as_slice(),
            [ContentBlock::Text(text)] if text.text == "# notes"
        ));
    }

    #[test]
    fn binary_resource_degrades_to_text_without_dumping_the_payload() {
        // The schema enum has no "unknown" variant, so the unknown-content path is the
        // one blob resource type core cannot represent: it must not panic and must not
        // paste base64 into the context.
        let result = result_from(serde_json::json!({
            "content": [{
                "type": "resource",
                "resource": {
                    "uri": "file:///archive.zip",
                    "blob": "QUJD"
                }
            }]
        }));
        let block = convert_call_tool_result("download", &result);
        let text = text_of(&block);
        assert!(text.contains("file:///archive.zip"), "{text}");
        assert!(text.contains("base64 omitted"), "{text}");
        assert!(!text.contains("QUJD"), "payload must stay out: {text}");
    }

    #[test]
    fn image_blob_resource_becomes_an_image_block() {
        let result = result_from(serde_json::json!({
            "content": [{
                "type": "resource",
                "resource": {
                    "uri": "file:///plot.png",
                    "mimeType": "image/png",
                    "blob": "QUJD"
                }
            }]
        }));
        let block = convert_call_tool_result("plot", &result);
        assert!(matches!(
            block.output.as_slice(),
            [ContentBlock::Image(image)]
                if image.source == Source::Base64(Base64Source::new("image/png", "QUJD"))
        ));
    }

    #[test]
    fn resource_link_renders_uri_and_name() {
        let result = result_from(serde_json::json!({
            "content": [{
                "type": "resource_link",
                "uri": "file:///report.csv",
                "name": "report.csv"
            }]
        }));
        let text = text_of(&convert_call_tool_result("link", &result));
        assert!(
            text.contains("report.csv") && text.contains("file:///report.csv"),
            "{text}"
        );
    }

    #[test]
    fn empty_content_falls_back_to_structured_content() {
        let result = result_from(serde_json::json!({
            "content": [],
            "structuredContent": { "total": 3 }
        }));
        let block = convert_call_tool_result("count", &result);
        assert_eq!(block.output.len(), 1);
        let text = text_of(&block);
        assert!(text.contains("\"total\": 3"), "{text}");
    }

    #[test]
    fn error_result_keeps_the_text_and_marks_the_state() {
        let result = result_from(serde_json::json!({
            "content": [{ "type": "text", "text": "boom" }],
            "isError": true
        }));
        let block = convert_call_tool_result("failing", &result);
        assert_eq!(block.state, ToolResultState::Error);
        assert!(text_of(&block).contains("boom"));
    }

    #[test]
    fn unparseable_payload_falls_back_to_rendered_text() {
        let value = serde_json::json!({ "not": "a CallToolResult" });
        let block = convert_call_tool_result_value("odd", &value);
        assert_eq!(block.name.as_deref(), Some("odd"));
        let text = text_of(&block);
        assert!(text.contains("\"not\""), "{text}");
    }
}
