//! Conversion between AgentScope [`Msg`]s and A2A messages / artifacts / parts.
//!
//! Mirrors `io.agentscope.core.a2a.agent.utils.MessageConvertUtil` plus the
//! `io.agentscope.core.a2a.agent.message` parser family (`PartParserRouter`,
//! `ContentBlockParserRouter`, and the per-type parsers).
//!
//! The wire format carries AgentScope-specific information in A2A part metadata, keyed by
//! the constants in [`MessageConstants`], so that a round trip is lossless for the block
//! types that A2A has no native representation for (thinking, hint, tool use / result).

use agentscope_core::message::JsonMap;
use agentscope_core::message::{
    AudioBlock, Base64Source, ContentBlock, HintBlock, ImageBlock, Msg, MsgRole, Source, TextBlock,
    ThinkingBlock, ToolResultBlock, ToolUseBlock, URLSource, VideoBlock,
};
use serde_json::Value;

use crate::model::{Artifact, DataPart, FileContent, FilePart, Message, MessageRole, Part};

/// AgentScope metadata keys carried on A2A parts and message metadata.
///
/// Mirrors `io.agentscope.core.a2a.agent.message.MessageConstants`.
pub struct MessageConstants;

impl MessageConstants {
    /// Source agent name of the originating message.
    pub const SOURCE_NAME_METADATA_KEY: &'static str = "_agentscope_msg_source";
    /// Id of the originating [`Msg`].
    pub const MSG_ID_METADATA_KEY: &'static str = "_agentscope_msg_id";
    /// Role of the originating [`Msg`].
    pub const MSG_ROLE_METADATA_KEY: &'static str = "_agentscope_msg_role";
    /// Content-block discriminator, see [`message_constants::block_content`].
    pub const BLOCK_TYPE_METADATA_KEY: &'static str = "_agentscope_block_type";
    /// `true` when the part is one chunk of a streamed text/thinking block.
    pub const STREAM_CHUNK_METADATA_KEY: &'static str = "_agentscope_stream_chunk";
    /// Id of the originating [`HintBlock`].
    pub const HINT_ID_METADATA_KEY: &'static str = "_agentscope_hint_id";
    /// Source of the originating [`HintBlock`].
    pub const HINT_SOURCE_METADATA_KEY: &'static str = "_agentscope_hint_source";
    /// Tool name of a tool-use / tool-result block.
    pub const TOOL_NAME_METADATA_KEY: &'static str = "_agentscope_tool_name";
    /// Tool call id of a tool-use / tool-result block.
    pub const TOOL_CALL_ID_METADATA_KEY: &'static str = "_agentscope_tool_call_id";
    /// Payload key holding a tool result's output inside its data part.
    pub const TOOL_RESULT_OUTPUT_METADATA_KEY: &'static str = "_agentscope_tool_output";

    /// Builds a metadata map containing only the content-block type discriminator.
    ///
    /// Mirrors Java `MessageConvertUtil#buildTypeMetadata`.
    pub fn build_type_metadata(block_type: &str) -> JsonMap {
        let mut metadata = JsonMap::new();
        metadata.insert(
            Self::BLOCK_TYPE_METADATA_KEY.to_string(),
            Value::String(block_type.to_string()),
        );
        metadata
    }

    /// Reads a metadata entry as a string, mirroring Java `getMetadataValue`
    /// (non-string scalars are rendered via their JSON form).
    pub fn metadata_value(metadata: Option<&JsonMap>, key: &str) -> Option<String> {
        let value = metadata?.get(key)?;
        Some(match value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
    }
}

/// Block-type discriminator values, mirroring Java `MessageConstants.BlockContent`.
pub mod message_constants {
    /// Content-block discriminator values written into part metadata.
    pub mod block_content {
        pub const TYPE_TEXT: &str = "text";
        pub const TYPE_THINKING: &str = "thinking";
        pub const TYPE_HINT: &str = "hint";
        pub const TYPE_IMAGE: &str = "image";
        pub const TYPE_AUDIO: &str = "audio";
        pub const TYPE_VIDEO: &str = "video";
        pub const TYPE_TOOL_USE: &str = "tool_use";
        pub const TYPE_TOOL_RESULT: &str = "tool_result";
    }
}

/// A2A `Part` -> core [`ContentBlock`].
///
/// Mirrors `io.agentscope.core.a2a.agent.message.PartParserRouter` with its
/// `TextPartParser`, `FilePartParser` and `DataPartParser` leaves. Returns `None` for
/// parts that have no AgentScope representation (unknown MIME primary type, unknown data
/// block type).
pub fn part_to_content_block(part: &Part) -> Option<ContentBlock> {
    match part {
        Part::Text(text) => {
            let block_type = MessageConstants::metadata_value(
                text.metadata.as_ref(),
                MessageConstants::BLOCK_TYPE_METADATA_KEY,
            );
            match block_type.as_deref() {
                Some(message_constants::block_content::TYPE_THINKING) => Some(
                    ContentBlock::Thinking(ThinkingBlock::new(text.text.clone())),
                ),
                Some(message_constants::block_content::TYPE_HINT) => {
                    let id = MessageConstants::metadata_value(
                        text.metadata.as_ref(),
                        MessageConstants::HINT_ID_METADATA_KEY,
                    );
                    let source = MessageConstants::metadata_value(
                        text.metadata.as_ref(),
                        MessageConstants::HINT_SOURCE_METADATA_KEY,
                    );
                    Some(ContentBlock::Hint(HintBlock {
                        id,
                        hint: Some(text.text.clone()),
                        source,
                    }))
                }
                _ => Some(ContentBlock::Text(TextBlock::new(text.text.clone()))),
            }
        }
        Part::File(file) => file_part_to_block(&file.file),
        Part::Data(data) => data_part_to_block(data),
    }
}

/// `FilePart` -> media [`ContentBlock`], mirroring Java `FilePartParser`.
fn file_part_to_block(file: &FileContent) -> Option<ContentBlock> {
    let mime = file.mime_type.as_deref().unwrap_or("");
    let primary = mime.split('/').next().unwrap_or("");
    let source = file_to_source(file)?;
    match primary {
        message_constants::block_content::TYPE_IMAGE => {
            Some(ContentBlock::Image(ImageBlock::new(source)))
        }
        message_constants::block_content::TYPE_AUDIO => {
            Some(ContentBlock::Audio(AudioBlock::new(source)))
        }
        message_constants::block_content::TYPE_VIDEO => {
            Some(ContentBlock::Video(VideoBlock::new(source)))
        }
        _ => None,
    }
}

/// `FileContent` -> core [`Source`], preferring inline bytes over a URI.
fn file_to_source(file: &FileContent) -> Option<Source> {
    if let Some(bytes) = &file.bytes {
        return Some(Source::Base64(Base64Source::new(
            file.mime_type.clone().unwrap_or_default(),
            bytes.clone(),
        )));
    }
    if let Some(uri) = &file.uri {
        return Some(Source::Url(URLSource::new(uri.clone())));
    }
    None
}

/// `DataPart` -> tool/text [`ContentBlock`], mirroring Java `DataPartParser`.
fn data_part_to_block(data: &DataPart) -> Option<ContentBlock> {
    let block_type = MessageConstants::metadata_value(
        data.metadata.as_ref(),
        MessageConstants::BLOCK_TYPE_METADATA_KEY,
    );

    // Without a block type the data part degrades to its JSON text form.
    let Some(block_type) = block_type else {
        return Some(ContentBlock::Text(TextBlock::new(data.data.to_string())));
    };

    match block_type.as_str() {
        message_constants::block_content::TYPE_TOOL_USE => {
            let id = MessageConstants::metadata_value(
                data.metadata.as_ref(),
                MessageConstants::TOOL_CALL_ID_METADATA_KEY,
            );
            let name = MessageConstants::metadata_value(
                data.metadata.as_ref(),
                MessageConstants::TOOL_NAME_METADATA_KEY,
            );
            let input = match &data.data {
                Value::Object(map) => map.clone(),
                other => {
                    let mut map = JsonMap::new();
                    map.insert("value".to_string(), other.clone());
                    map
                }
            };
            Some(ContentBlock::ToolUse(ToolUseBlock {
                id: id.unwrap_or_default(),
                name: name.unwrap_or_default(),
                input,
                content: None,
                metadata: original_metadata(data.metadata.as_ref()),
                state: Default::default(),
            }))
        }
        message_constants::block_content::TYPE_TOOL_RESULT => {
            let id = MessageConstants::metadata_value(
                data.metadata.as_ref(),
                MessageConstants::TOOL_CALL_ID_METADATA_KEY,
            );
            let name = MessageConstants::metadata_value(
                data.metadata.as_ref(),
                MessageConstants::TOOL_NAME_METADATA_KEY,
            );
            let output = data
                .data
                .get(MessageConstants::TOOL_RESULT_OUTPUT_METADATA_KEY)
                .map(tool_output_to_blocks)
                .unwrap_or_default();
            Some(ContentBlock::ToolResult(ToolResultBlock {
                id,
                name,
                output,
                metadata: original_metadata(data.metadata.as_ref()),
                state: Default::default(),
            }))
        }
        _ => None,
    }
}

/// Mirrors Java `DataPartParser#parseToToolResultBlock` output decoding: a plain string
/// becomes a single text block, an array is decoded as content blocks, anything else is
/// dropped.
fn tool_output_to_blocks(output: &Value) -> Vec<ContentBlock> {
    match output {
        Value::String(text) => vec![ContentBlock::Text(TextBlock::new(text.clone()))],
        Value::Array(items) => items
            .iter()
            .filter_map(|item| serde_json::from_value::<ContentBlock>(item.clone()).ok())
            .collect(),
        _ => Vec::new(),
    }
}

/// Strips the AgentScope-internal keys, mirroring Java `DataPartParser#getOriginalMetadata`.
fn original_metadata(metadata: Option<&JsonMap>) -> JsonMap {
    let mut result = metadata.cloned().unwrap_or_default();
    result.remove(MessageConstants::TOOL_CALL_ID_METADATA_KEY);
    result.remove(MessageConstants::TOOL_NAME_METADATA_KEY);
    result.remove(MessageConstants::BLOCK_TYPE_METADATA_KEY);
    result
}

/// Core [`ContentBlock`] -> A2A `Part`.
///
/// Mirrors `io.agentscope.core.a2a.agent.message.ContentBlockParserRouter`. Returns `None`
/// for [`ContentBlock::Data`], which Java has no content-block parser for.
pub fn content_block_to_part(block: &ContentBlock) -> Option<Part> {
    match block {
        ContentBlock::Text(text) => Some(Part::Text(crate::model::TextPart {
            text: text.text.clone(),
            metadata: Some(MessageConstants::build_type_metadata(
                message_constants::block_content::TYPE_TEXT,
            )),
        })),
        ContentBlock::Thinking(thinking) => Some(Part::Text(crate::model::TextPart {
            text: thinking.thinking.clone(),
            metadata: Some(MessageConstants::build_type_metadata(
                message_constants::block_content::TYPE_THINKING,
            )),
        })),
        ContentBlock::Hint(hint) => {
            let mut metadata =
                MessageConstants::build_type_metadata(message_constants::block_content::TYPE_HINT);
            if let Some(id) = &hint.id {
                metadata.insert(
                    MessageConstants::HINT_ID_METADATA_KEY.to_string(),
                    Value::String(id.clone()),
                );
            }
            if let Some(source) = &hint.source {
                metadata.insert(
                    MessageConstants::HINT_SOURCE_METADATA_KEY.to_string(),
                    Value::String(source.clone()),
                );
            }
            Some(Part::Text(crate::model::TextPart {
                text: hint.hint.clone().unwrap_or_default(),
                metadata: Some(metadata),
            }))
        }
        ContentBlock::Image(image) => {
            media_part(&image.source, message_constants::block_content::TYPE_IMAGE)
        }
        ContentBlock::Audio(audio) => {
            media_part(&audio.source, message_constants::block_content::TYPE_AUDIO)
        }
        ContentBlock::Video(video) => {
            media_part(&video.source, message_constants::block_content::TYPE_VIDEO)
        }
        ContentBlock::ToolUse(tool) => {
            let mut metadata = MessageConstants::build_type_metadata(
                message_constants::block_content::TYPE_TOOL_USE,
            );
            metadata.insert(
                MessageConstants::TOOL_NAME_METADATA_KEY.to_string(),
                Value::String(tool.name.clone()),
            );
            metadata.insert(
                MessageConstants::TOOL_CALL_ID_METADATA_KEY.to_string(),
                Value::String(tool.id.clone()),
            );
            for (key, value) in &tool.metadata {
                metadata.insert(key.clone(), value.clone());
            }
            Some(Part::Data(
                DataPart::new(Value::Object(tool.input.clone())).with_metadata(metadata),
            ))
        }
        ContentBlock::ToolResult(tool) => {
            let mut metadata = MessageConstants::build_type_metadata(
                message_constants::block_content::TYPE_TOOL_RESULT,
            );
            metadata.insert(
                MessageConstants::TOOL_NAME_METADATA_KEY.to_string(),
                Value::String(tool.name.clone().unwrap_or_default()),
            );
            metadata.insert(
                MessageConstants::TOOL_CALL_ID_METADATA_KEY.to_string(),
                Value::String(tool.id.clone().unwrap_or_default()),
            );
            for (key, value) in &tool.metadata {
                metadata.insert(key.clone(), value.clone());
            }
            let mut data = JsonMap::new();
            data.insert(
                MessageConstants::TOOL_RESULT_OUTPUT_METADATA_KEY.to_string(),
                tool_output_to_value(&tool.output),
            );
            Some(Part::Data(
                DataPart::new(Value::Object(data)).with_metadata(metadata),
            ))
        }
        ContentBlock::Data(_) => None,
        // Documents have no A2A part either: the protocol carries files, not
        // provider-side document references.
        ContentBlock::Document(_) => None,
    }
}

/// Mirrors Java tool-result output serialization: plain text stays a string so the Python
/// adapter interoperates, otherwise the block list is serialized as JSON.
fn tool_output_to_value(output: &[ContentBlock]) -> Value {
    if let [ContentBlock::Text(text)] = output {
        return Value::String(text.text.clone());
    }
    serde_json::to_value(output).unwrap_or(Value::Array(Vec::new()))
}

/// Builds a `FilePart` from a core [`Source`], mirroring Java `BaseMediaBlockParser`.
///
/// A names is not part of [`Source`], so a random UUID is generated, matching Java.
fn media_part(source: &Source, media_type: &str) -> Option<Part> {
    let name = uuid::Uuid::new_v4().to_string();
    let file = match source {
        Source::Base64(base64) => FileContent {
            mime_type: Some(base64.media_type.clone()),
            name: Some(name),
            bytes: Some(base64.data.clone()),
            uri: None,
        },
        Source::Url(url) => FileContent {
            mime_type: Some(mime_type_from_url(&url.url, media_type)),
            name: Some(name),
            bytes: None,
            uri: Some(url.url.clone()),
        },
        Source::FileId(file) => FileContent {
            mime_type: Some(media_type.to_string()),
            name: Some(name),
            bytes: None,
            uri: Some(format!("fileid://{}", file.file_id)),
        },
    };
    Some(Part::File(FilePart::new(file)))
}

/// Mirrors Java `BaseMediaBlockParser#tryToParseMimeTypeFromUrl`: probe the extension,
/// fall back to the block's primary media type when unknown or mismatched.
fn mime_type_from_url(url: &str, media_type: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let extension = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let probed = match extension.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        "mp3" => Some("audio/mpeg"),
        "wav" => Some("audio/wav"),
        "ogg" => Some("audio/ogg"),
        "mp4" => Some("video/mp4"),
        "webm" => Some("video/webm"),
        "mov" => Some("video/quicktime"),
        _ => None,
    };
    match probed {
        Some(mime) if mime.starts_with(media_type) => mime.to_string(),
        _ => media_type.to_string(),
    }
}

/// Bidirectional conversion between AgentScope [`Msg`]s and A2A payloads.
///
/// Mirrors `io.agentscope.core.a2a.agent.utils.MessageConvertUtil`. The conversion is
/// stateless, so the type exists purely for API symmetry with Java and as a namespace.
pub struct MessageConvert;

/// Accumulator used to re-join streamed text/thinking chunks, mirroring Java
/// `MessageConvertUtil.StreamingChunkAccumulator`.
struct StreamingChunkAccumulator {
    msg_id: Option<String>,
    kind: String,
    text: String,
}

impl StreamingChunkAccumulator {
    fn matches(&self, msg_id: Option<&str>, kind: &str) -> bool {
        self.msg_id.as_deref() == msg_id && self.kind == kind
    }

    fn append(&mut self, block: &ContentBlock) {
        match block {
            ContentBlock::Text(text) => self.text.push_str(&text.text),
            ContentBlock::Thinking(thinking) => self.text.push_str(&thinking.thinking),
            _ => {}
        }
    }

    fn build(&self) -> ContentBlock {
        if self.kind == message_constants::block_content::TYPE_THINKING {
            ContentBlock::Thinking(ThinkingBlock::new(self.text.clone()))
        } else {
            ContentBlock::Text(TextBlock::new(self.text.clone()))
        }
    }
}

impl MessageConvert {
    /// Converts a single A2A artifact into a [`Msg`] with role `Assistant`.
    ///
    /// Mirrors Java `MessageConvertUtil#convertFromArtifact(Artifact, String)`.
    pub fn convert_from_artifact(artifact: &Artifact, agent_name: &str) -> Msg {
        Self::convert_from_artifacts(std::slice::from_ref(artifact), agent_name)
    }

    /// Converts A2A artifacts into a single [`Msg`], concatenating their parts.
    ///
    /// Mirrors Java `MessageConvertUtil#convertFromArtifact(List, String)`: the last
    /// artifact's id / name / metadata win and artifacts without parts are skipped.
    pub fn convert_from_artifacts(artifacts: &[Artifact], agent_name: &str) -> Msg {
        let mut builder = Msg::builder();
        let mut parts: Vec<Part> = Vec::new();

        for artifact in artifacts.iter().filter(|a| !a.parts.is_empty()) {
            builder = builder.id(artifact.artifact_id.clone());
            builder = builder.name(if agent_name.is_empty() {
                artifact.name.clone().unwrap_or_default()
            } else {
                agent_name.to_string()
            });
            builder = builder.metadata(artifact.metadata.clone().unwrap_or_default());
            parts.extend(artifact.parts.iter().cloned());
        }

        builder
            .role(MsgRole::Assistant)
            .content(Self::convert_from_parts(&parts))
            .build()
    }

    /// Converts an A2A message into a [`Msg`] with role `Assistant`.
    ///
    /// Mirrors Java `MessageConvertUtil#convertFromMessage(Message, String)`.
    pub fn convert_from_message(message: &Message, agent_name: &str) -> Msg {
        Msg::builder()
            .id(message.message_id.clone())
            .name(agent_name)
            .metadata(message.metadata.clone().unwrap_or_default())
            .role(MsgRole::Assistant)
            .content(Self::convert_from_parts(&message.parts))
            .build()
    }

    /// Converts a batch of [`Msg`]s into one A2A [`Message`].
    ///
    /// Mirrors Java `MessageConvertUtil#convertFromMsg(List)`: per-message metadata is
    /// nested under its message id, each part is stamped with the source message id/name/
    /// role, and the message role is resolved from the batch.
    pub fn convert_from_msgs(msgs: &[Msg]) -> Message {
        let mut nested_metadata = JsonMap::new();
        let mut parts: Vec<Part> = Vec::new();

        for msg in msgs.iter().filter(|m| !m.content.is_empty()) {
            if !msg.metadata.is_empty() {
                nested_metadata.insert(msg.id.clone(), Value::Object(msg.metadata.clone()));
            }
            for block in &msg.content {
                let Some(mut part) = content_block_to_part(block) else {
                    continue;
                };
                let metadata = part_metadata_mut(&mut part);
                metadata.insert(
                    MessageConstants::MSG_ID_METADATA_KEY.to_string(),
                    Value::String(msg.id.clone()),
                );
                metadata.insert(
                    MessageConstants::SOURCE_NAME_METADATA_KEY.to_string(),
                    Value::String(msg.name.clone().unwrap_or_default()),
                );
                metadata.insert(
                    MessageConstants::MSG_ROLE_METADATA_KEY.to_string(),
                    Value::String(msg_role_name(msg.role).to_string()),
                );
                parts.push(part);
            }
        }

        let mut message = Message::new(
            uuid::Uuid::new_v4().to_string(),
            Self::resolve_message_role(msgs),
            parts,
        );
        message.metadata = Some(nested_metadata);
        message
    }

    /// Converts A2A parts into content blocks, re-joining streamed text/thinking chunks.
    ///
    /// Mirrors Java `MessageConvertUtil#convertFromParts`.
    pub fn convert_from_parts(parts: &[Part]) -> Vec<ContentBlock> {
        let mut blocks: Vec<ContentBlock> = Vec::new();
        let mut accumulator: Option<StreamingChunkAccumulator> = None;

        for part in parts {
            let Some(block) = part_to_content_block(part) else {
                continue;
            };

            let kind = if is_streaming_chunk(part) {
                mergeable_chunk_kind(&block)
            } else {
                None
            };

            let Some(kind) = kind else {
                if let Some(pending) = accumulator.take() {
                    blocks.push(pending.build());
                }
                blocks.push(block);
                continue;
            };

            let msg_id = part_metadata(part).and_then(|m| {
                MessageConstants::metadata_value(Some(m), MessageConstants::MSG_ID_METADATA_KEY)
            });

            match accumulator.as_mut() {
                Some(pending) if pending.matches(msg_id.as_deref(), kind) => pending.append(&block),
                _ => {
                    if let Some(finished) = accumulator.take() {
                        blocks.push(finished.build());
                    }
                    let mut started = StreamingChunkAccumulator {
                        msg_id,
                        kind: kind.to_string(),
                        text: String::new(),
                    };
                    started.append(&block);
                    accumulator = Some(started);
                }
            }
        }

        if let Some(pending) = accumulator {
            blocks.push(pending.build());
        }
        blocks
    }

    /// Resolves the A2A role of a batch, mirroring Java `resolveMessageRole`.
    fn resolve_message_role(msgs: &[Msg]) -> MessageRole {
        let mut roles = msgs
            .iter()
            .map(|msg| to_a2a_role(msg.role))
            .collect::<Vec<_>>();
        roles.dedup();
        roles.sort_by_key(|r| match r {
            MessageRole::User => 0,
            MessageRole::Agent => 1,
        });
        roles.dedup();
        if roles.len() == 1 {
            roles[0]
        } else {
            MessageRole::User
        }
    }
}

/// Mirrors Java `MessageConvertUtil#convertRole`: assistant/tool become `agent`.
fn to_a2a_role(role: MsgRole) -> MessageRole {
    match role {
        MsgRole::Assistant | MsgRole::Tool => MessageRole::Agent,
        MsgRole::User | MsgRole::System => MessageRole::User,
    }
}

/// Wire name of a [`MsgRole`], matching its `serde` variant name.
///
/// `agentscope_core::message::MsgRole` does not implement `Display`, and this crate must not
/// add a parallel role enum, so the canonical names are produced here.
pub(crate) fn msg_role_name(role: MsgRole) -> &'static str {
    match role {
        MsgRole::User => "User",
        MsgRole::Assistant => "Assistant",
        MsgRole::System => "System",
        MsgRole::Tool => "Tool",
    }
}

/// Mirrors Java `MessageConvertUtil#isStreamingChunk`.
fn is_streaming_chunk(part: &Part) -> bool {
    MessageConstants::metadata_value(
        part_metadata(part),
        MessageConstants::STREAM_CHUNK_METADATA_KEY,
    )
    .map(|v| v.eq_ignore_ascii_case("true"))
    .unwrap_or(false)
}

/// Mirrors Java `MessageConvertUtil#getMergeableChunkKind`: only text and thinking merge.
fn mergeable_chunk_kind(block: &ContentBlock) -> Option<&'static str> {
    match block {
        ContentBlock::Text(_) => Some(message_constants::block_content::TYPE_TEXT),
        ContentBlock::Thinking(_) => Some(message_constants::block_content::TYPE_THINKING),
        _ => None,
    }
}

/// Read-only metadata of any part variant.
fn part_metadata(part: &Part) -> Option<&JsonMap> {
    match part {
        Part::Text(p) => p.metadata.as_ref(),
        Part::File(p) => p.metadata.as_ref(),
        Part::Data(p) => p.metadata.as_ref(),
    }
}

/// Mutable metadata of any part variant, creating the map when absent.
fn part_metadata_mut(part: &mut Part) -> &mut JsonMap {
    let slot = match part {
        Part::Text(p) => &mut p.metadata,
        Part::File(p) => &mut p.metadata,
        Part::Data(p) => &mut p.metadata,
    };
    slot.get_or_insert_with(JsonMap::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_block_round_trips_through_text_part() {
        let block = ContentBlock::Text(TextBlock::new("hello"));
        let part = content_block_to_part(&block).unwrap();
        assert_eq!(part.kind(), crate::model::PartKind::Text);
        assert_eq!(part_to_content_block(&part), Some(block));
    }

    #[test]
    fn thinking_block_round_trips_via_block_type_metadata() {
        let block = ContentBlock::Thinking(ThinkingBlock::new("let me think"));
        let part = content_block_to_part(&block).unwrap();
        let json = serde_json::to_value(&part).unwrap();
        assert_eq!(json["kind"], "text");
        assert_eq!(json["metadata"]["_agentscope_block_type"], "thinking");
        assert_eq!(part_to_content_block(&part), Some(block));
    }

    #[test]
    fn hint_block_round_trips_with_id_and_source() {
        let block = ContentBlock::Hint(HintBlock {
            id: Some("h1".to_string()),
            hint: Some("did you know".to_string()),
            source: Some("rag".to_string()),
        });
        let part = content_block_to_part(&block).unwrap();
        assert_eq!(part_to_content_block(&part), Some(block));
    }

    #[test]
    fn tool_use_block_round_trips_as_data_part() {
        let mut input = JsonMap::new();
        input.insert("city".to_string(), Value::String("Hangzhou".to_string()));
        let block = ContentBlock::ToolUse(ToolUseBlock::new("call-1", "get_weather", input));
        let part = content_block_to_part(&block).unwrap();
        assert_eq!(part.kind(), crate::model::PartKind::Data);
        let json = serde_json::to_value(&part).unwrap();
        assert_eq!(json["metadata"]["_agentscope_block_type"], "tool_use");
        assert_eq!(json["metadata"]["_agentscope_tool_name"], "get_weather");
        assert_eq!(json["data"]["city"], "Hangzhou");

        let back = part_to_content_block(&part).unwrap();
        match back {
            ContentBlock::ToolUse(tool) => {
                assert_eq!(tool.id, "call-1");
                assert_eq!(tool.name, "get_weather");
                assert_eq!(tool.input["city"], "Hangzhou");
            }
            other => panic!("unexpected block: {other:?}"),
        }
    }

    #[test]
    fn tool_result_round_trips_text_output_as_string() {
        let mut block = ToolResultBlock::new(
            "call-1",
            "get_weather",
            vec![ContentBlock::Text(TextBlock::new("sunny"))],
        );
        block
            .metadata
            .insert("trace".to_string(), Value::String("abc".to_string()));
        let part = content_block_to_part(&ContentBlock::ToolResult(block)).unwrap();
        let json = serde_json::to_value(&part).unwrap();
        // Java keeps a single text block as a JSON string for Python interop.
        assert_eq!(json["data"]["_agentscope_tool_output"], "sunny");
        assert_eq!(json["metadata"]["trace"], "abc");

        let back = part_to_content_block(&part).unwrap();
        match back {
            ContentBlock::ToolResult(result) => {
                assert_eq!(result.id.as_deref(), Some("call-1"));
                assert_eq!(result.name.as_deref(), Some("get_weather"));
                assert_eq!(result.text_content(), "sunny");
                assert_eq!(result.metadata["trace"], "abc");
                // AgentScope-internal keys are stripped from the restored metadata.
                assert!(!result
                    .metadata
                    .contains_key(MessageConstants::BLOCK_TYPE_METADATA_KEY));
            }
            other => panic!("unexpected block: {other:?}"),
        }
    }

    #[test]
    fn image_block_with_url_becomes_file_part_and_back() {
        let block = ContentBlock::Image(ImageBlock::new(Source::Url(URLSource::new(
            "https://example.com/a.png",
        ))));
        let part = content_block_to_part(&block).unwrap();
        let json = serde_json::to_value(&part).unwrap();
        assert_eq!(json["kind"], "file");
        assert_eq!(json["file"]["mimeType"], "image/png");
        assert_eq!(json["file"]["uri"], "https://example.com/a.png");

        match part_to_content_block(&part).unwrap() {
            ContentBlock::Image(image) => match image.source {
                Source::Url(url) => assert_eq!(url.url, "https://example.com/a.png"),
                other => panic!("unexpected source: {other:?}"),
            },
            other => panic!("unexpected block: {other:?}"),
        }
    }

    #[test]
    fn media_part_mime_falls_back_to_primary_type_for_unknown_extension() {
        let block = ContentBlock::Audio(AudioBlock::new(Source::Url(URLSource::new(
            "https://example.com/sound.unknownext",
        ))));
        let part = content_block_to_part(&block).unwrap();
        let json = serde_json::to_value(&part).unwrap();
        assert_eq!(json["file"]["mimeType"], "audio");
        // Java's FilePartParser#getPrimaryType returns the bare `audio` primary type
        // when the mime type has no '/', and that still routes to an AudioBlock.
        match part_to_content_block(&part).unwrap() {
            ContentBlock::Audio(audio) => match audio.source {
                Source::Url(url) => assert_eq!(url.url, "https://example.com/sound.unknownext"),
                other => panic!("unexpected source: {other:?}"),
            },
            other => panic!("unexpected block: {other:?}"),
        }
    }

    #[test]
    fn convert_from_msgs_resolves_agent_role_and_stamps_metadata() {
        let msgs = vec![Msg::builder()
            .id("m1")
            .name("alice")
            .role(MsgRole::Assistant)
            .text_content("hi")
            .metadata_entry("k", Value::String("v".to_string()))
            .build()];
        let message = MessageConvert::convert_from_msgs(&msgs);
        assert_eq!(message.role, MessageRole::Agent);
        assert_eq!(message.parts.len(), 1);

        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(json["parts"][0]["metadata"]["_agentscope_msg_id"], "m1");
        assert_eq!(
            json["parts"][0]["metadata"]["_agentscope_msg_source"],
            "alice"
        );
        assert_eq!(
            json["parts"][0]["metadata"]["_agentscope_msg_role"],
            "Assistant"
        );
        assert_eq!(json["metadata"]["m1"]["k"], "v");
    }

    #[test]
    fn convert_from_msgs_defaults_to_user_role_when_mixed() {
        let msgs = vec![
            Msg::builder()
                .role(MsgRole::Assistant)
                .text_content("a")
                .build(),
            Msg::builder().role(MsgRole::User).text_content("b").build(),
        ];
        assert_eq!(
            MessageConvert::convert_from_msgs(&msgs).role,
            MessageRole::User
        );
    }

    #[test]
    fn convert_from_message_uses_assistant_role_and_agent_name() {
        let message = Message::new("m-1", MessageRole::User, vec![Part::text("incoming")]);
        let msg = MessageConvert::convert_from_message(&message, "remote-agent");
        assert_eq!(msg.id, "m-1");
        assert_eq!(msg.name.as_deref(), Some("remote-agent"));
        assert_eq!(msg.role, MsgRole::Assistant);
        assert_eq!(
            msg.content,
            vec![ContentBlock::Text(TextBlock::new("incoming"))]
        );
    }

    #[test]
    fn convert_from_artifacts_skips_empty_and_uses_last_identity() {
        let empty = Artifact {
            artifact_id: "skip-me".to_string(),
            name: None,
            description: None,
            parts: Vec::new(),
            metadata: None,
        };
        let kept = Artifact::new("art-2", vec![Part::text("payload")]);
        let msg = MessageConvert::convert_from_artifacts(&[empty, kept], "agent-x");
        assert_eq!(msg.id, "art-2");
        assert_eq!(msg.name.as_deref(), Some("agent-x"));
        assert_eq!(
            msg.content,
            vec![ContentBlock::Text(TextBlock::new("payload"))]
        );
    }

    #[test]
    fn streaming_text_chunks_are_merged_by_msg_id_and_kind() {
        let chunk = |text: &str, msg_id: &str, kind: &str| {
            Part::Text(crate::model::TextPart {
                text: text.to_string(),
                metadata: Some({
                    let mut m = MessageConstants::build_type_metadata(kind);
                    m.insert(
                        MessageConstants::STREAM_CHUNK_METADATA_KEY.to_string(),
                        Value::Bool(true),
                    );
                    m.insert(
                        MessageConstants::MSG_ID_METADATA_KEY.to_string(),
                        Value::String(msg_id.to_string()),
                    );
                    m
                }),
            })
        };

        let parts = vec![
            chunk("Hel", "m1", "text"),
            chunk("lo", "m1", "text"),
            chunk("think", "m1", "thinking"),
            chunk("other", "m2", "text"),
        ];
        let blocks = MessageConvert::convert_from_parts(&parts);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0], ContentBlock::Text(TextBlock::new("Hello")));
        assert_eq!(
            blocks[1],
            ContentBlock::Thinking(ThinkingBlock::new("think"))
        );
        assert_eq!(blocks[2], ContentBlock::Text(TextBlock::new("other")));
    }

    #[test]
    fn non_streaming_text_parts_are_not_merged() {
        let parts = vec![Part::text("a"), Part::text("b")];
        let blocks = MessageConvert::convert_from_parts(&parts);
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn data_part_without_block_type_degrades_to_json_text() {
        let part = Part::Data(DataPart::new(serde_json::json!({"a": 1})));
        let block = part_to_content_block(&part).unwrap();
        assert_eq!(block, ContentBlock::Text(TextBlock::new("{\"a\":1}")));
    }

    #[test]
    fn build_type_metadata_matches_java_key() {
        let metadata = MessageConstants::build_type_metadata("text");
        assert_eq!(metadata.len(), 1);
        assert_eq!(metadata[MessageConstants::BLOCK_TYPE_METADATA_KEY], "text");
    }

    #[test]
    fn convert_from_parts_round_trips_a_mixed_message() {
        let original = vec![
            ContentBlock::Thinking(ThinkingBlock::new("reasoning")),
            ContentBlock::Text(TextBlock::new("answer")),
        ];
        let parts: Vec<Part> = original
            .iter()
            .map(|b| content_block_to_part(b).unwrap())
            .collect();
        assert_eq!(MessageConvert::convert_from_parts(&parts), original);
    }
}
