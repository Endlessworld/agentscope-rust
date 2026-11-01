use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::source::Source;

/// State of a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ToolCallState {
    #[default]
    Pending,
    Asking,
    Allowed,
    Submitted,
    Finished,
}

/// State of a tool execution result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ToolResultState {
    Success,
    Error,
    Interrupted,
    Denied,
    /// The tool call was suspended and awaits execution outside the agent runtime.
    ///
    /// A suspended result carries no output: the real result is supplied by the
    /// caller on resume. Mirrors `ToolResultState.SUSPENDED` in the Java SDK.
    Suspended,
    #[default]
    Running,
}

/// Base type for all content blocks in messages.
///
/// Mirrors the sealed `ContentBlock` hierarchy in the Java SDK: serialized with a `type`
/// discriminator (`text`, `thinking`, `image`, `audio`, `video`, `tool_use`, `tool_result`,
/// `hint`, `data`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text(TextBlock),
    Thinking(ThinkingBlock),
    Image(ImageBlock),
    Audio(AudioBlock),
    Video(VideoBlock),
    #[serde(rename = "tool_use")]
    ToolUse(ToolUseBlock),
    #[serde(rename = "tool_result")]
    ToolResult(ToolResultBlock),
    Hint(HintBlock),
    Data(DataBlock),
}

/// Plain text content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TextBlock {
    /// The text content. Null is normalized to an empty string.
    #[serde(default)]
    pub text: String,
}

impl TextBlock {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }

    pub fn builder() -> TextBlockBuilder {
        TextBlockBuilder::default()
    }
}

impl std::fmt::Display for TextBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

#[derive(Debug, Default)]
pub struct TextBlockBuilder {
    text: Option<String>,
}

impl TextBlockBuilder {
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }

    pub fn build(self) -> TextBlock {
        TextBlock {
            text: self.text.unwrap_or_default(),
        }
    }
}

/// Reasoning / thinking content produced before taking action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ThinkingBlock {
    /// The thinking content.
    #[serde(default)]
    pub thinking: String,
    /// Optional metadata, e.g. OpenRouter `reasoningDetails`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<super::JsonMap>,
}

impl ThinkingBlock {
    /// Metadata key for OpenRouter/Gemini reasoning details list.
    pub const METADATA_REASONING_DETAILS: &'static str = "reasoningDetails";

    pub fn new(thinking: impl Into<String>) -> Self {
        Self {
            thinking: thinking.into(),
            metadata: None,
        }
    }

    pub fn builder() -> ThinkingBlockBuilder {
        ThinkingBlockBuilder::default()
    }
}

#[derive(Debug, Default)]
pub struct ThinkingBlockBuilder {
    thinking: Option<String>,
    metadata: Option<super::JsonMap>,
}

impl ThinkingBlockBuilder {
    pub fn thinking(mut self, thinking: impl Into<String>) -> Self {
        self.thinking = Some(thinking.into());
        self
    }

    pub fn metadata(mut self, metadata: super::JsonMap) -> Self {
        self.metadata = Some(metadata);
        self
    }

    pub fn build(self) -> ThinkingBlock {
        ThinkingBlock {
            thinking: self.thinking.unwrap_or_default(),
            metadata: self.metadata,
        }
    }
}

/// Image content (URL or Base64 source).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageBlock {
    /// The image source.
    pub source: Source,
    /// Minimum pixel threshold for the input image.
    #[serde(rename = "min_pixels", skip_serializing_if = "Option::is_none")]
    pub min_pixels: Option<u32>,
    /// Maximum pixel threshold for the input image.
    #[serde(rename = "max_pixels", skip_serializing_if = "Option::is_none")]
    pub max_pixels: Option<u32>,
}

impl ImageBlock {
    pub fn new(source: Source) -> Self {
        Self {
            source,
            min_pixels: None,
            max_pixels: None,
        }
    }

    pub fn builder() -> ImageBlockBuilder {
        ImageBlockBuilder::default()
    }
}

#[derive(Debug, Default)]
pub struct ImageBlockBuilder {
    source: Option<Source>,
    min_pixels: Option<u32>,
    max_pixels: Option<u32>,
}

impl ImageBlockBuilder {
    pub fn source(mut self, source: Source) -> Self {
        self.source = Some(source);
        self
    }

    pub fn min_pixels(mut self, min_pixels: u32) -> Self {
        self.min_pixels = Some(min_pixels);
        self
    }

    pub fn max_pixels(mut self, max_pixels: u32) -> Self {
        self.max_pixels = Some(max_pixels);
        self
    }

    pub fn build(self) -> ImageBlock {
        ImageBlock {
            source: self.source.expect("ImageBlock requires a source"),
            min_pixels: self.min_pixels,
            max_pixels: self.max_pixels,
        }
    }
}

/// Audio content (URL or Base64 source).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioBlock {
    /// The audio source.
    pub source: Source,
}

impl AudioBlock {
    pub fn new(source: Source) -> Self {
        Self { source }
    }

    pub fn builder() -> AudioBlockBuilder {
        AudioBlockBuilder::default()
    }
}

#[derive(Debug, Default)]
pub struct AudioBlockBuilder {
    source: Option<Source>,
}

impl AudioBlockBuilder {
    pub fn source(mut self, source: Source) -> Self {
        self.source = Some(source);
        self
    }

    pub fn build(self) -> AudioBlock {
        AudioBlock {
            source: self.source.expect("AudioBlock requires a source"),
        }
    }
}

/// Video content (URL or Base64 source) with optional frame settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoBlock {
    /// The video source.
    pub source: Source,
    /// Frames per second in `[0.1, 10]`; default 2.0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fps: Option<f32>,
    /// Maximum number of frames captured.
    #[serde(rename = "max_frames", skip_serializing_if = "Option::is_none")]
    pub max_frames: Option<u32>,
    /// Minimum pixel threshold for input video frames.
    #[serde(rename = "min_pixels", skip_serializing_if = "Option::is_none")]
    pub min_pixels: Option<u32>,
    /// Maximum pixel threshold for input video frames.
    #[serde(rename = "max_pixels", skip_serializing_if = "Option::is_none")]
    pub max_pixels: Option<u32>,
    /// Total pixels of all frames (single image pixels x total frames).
    #[serde(rename = "total_pixels", skip_serializing_if = "Option::is_none")]
    pub total_pixels: Option<u32>,
}

impl VideoBlock {
    pub fn new(source: Source) -> Self {
        Self {
            source,
            fps: None,
            max_frames: None,
            min_pixels: None,
            max_pixels: None,
            total_pixels: None,
        }
    }

    pub fn builder() -> VideoBlockBuilder {
        VideoBlockBuilder::default()
    }
}

#[derive(Debug, Default)]
pub struct VideoBlockBuilder {
    source: Option<Source>,
    fps: Option<f32>,
    max_frames: Option<u32>,
    min_pixels: Option<u32>,
    max_pixels: Option<u32>,
    total_pixels: Option<u32>,
}

impl VideoBlockBuilder {
    pub fn source(mut self, source: Source) -> Self {
        self.source = Some(source);
        self
    }

    pub fn fps(mut self, fps: f32) -> Self {
        self.fps = Some(fps);
        self
    }

    pub fn max_frames(mut self, max_frames: u32) -> Self {
        self.max_frames = Some(max_frames);
        self
    }

    pub fn min_pixels(mut self, min_pixels: u32) -> Self {
        self.min_pixels = Some(min_pixels);
        self
    }

    pub fn max_pixels(mut self, max_pixels: u32) -> Self {
        self.max_pixels = Some(max_pixels);
        self
    }

    pub fn total_pixels(mut self, total_pixels: u32) -> Self {
        self.total_pixels = Some(total_pixels);
        self
    }

    pub fn build(self) -> VideoBlock {
        VideoBlock {
            source: self.source.expect("VideoBlock requires a source"),
            fps: self.fps,
            max_frames: self.max_frames,
            min_pixels: self.min_pixels,
            max_pixels: self.max_pixels,
            total_pixels: self.total_pixels,
        }
    }
}

/// A tool use request within a message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolUseBlock {
    /// Unique identifier for this tool call.
    pub id: String,
    /// Name of the tool to execute.
    pub name: String,
    /// Input parameters for the tool.
    #[serde(default)]
    pub input: super::JsonMap,
    /// Raw content for streaming tool calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Provider-specific metadata.
    #[serde(default)]
    pub metadata: super::JsonMap,
    /// Tool call state; defaults to `Pending`.
    #[serde(default)]
    pub state: ToolCallState,
}

impl ToolUseBlock {
    /// Metadata key for Gemini thought signature.
    pub const METADATA_THOUGHT_SIGNATURE: &'static str = "thoughtSignature";

    /// Metadata key marking a tool call as executed by the provider (server tool).
    /// Mirrors the Java `ToolUseBlock.METADATA_SERVER_TOOL`.
    pub const METADATA_SERVER_TOOL: &'static str = "serverTool";

    /// Metadata key (bool) marking a tool call whose streamed arguments never formed a
    /// valid JSON object — typically truncated by an output-token limit. Such a call must
    /// be rejected before execution instead of being run with `{}`.
    /// Mirrors the Java `ToolUseBlock.METADATA_MALFORMED_ARGUMENTS`.
    pub const METADATA_MALFORMED_ARGUMENTS: &'static str = "malformedArguments";

    pub fn new(id: impl Into<String>, name: impl Into<String>, input: super::JsonMap) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            input,
            content: None,
            metadata: super::JsonMap::new(),
            state: ToolCallState::Pending,
        }
    }

    pub fn builder() -> ToolUseBlockBuilder {
        ToolUseBlockBuilder::default()
    }

    /// Whether the provider executes this tool itself (Google Search, web search, …).
    ///
    /// Server-tool calls are never dispatched to the local toolkit; their results are
    /// expected to arrive inline in the same assistant message.
    pub fn is_server_tool(&self) -> bool {
        self.metadata
            .get(Self::METADATA_SERVER_TOOL)
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// Whether the streamed arguments of this call never formed a valid JSON object.
    ///
    /// Malformed calls are not executable: the acting phase answers them with an ERROR
    /// result carrying the original call id, keeping the tool_use ↔ tool_result pairing
    /// complete without invoking the handler. Mirrors Java
    /// `ToolUseBlock#hasMalformedArguments`.
    pub fn has_malformed_arguments(&self) -> bool {
        self.metadata
            .get(Self::METADATA_MALFORMED_ARGUMENTS)
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }
}

#[derive(Debug, Default)]
pub struct ToolUseBlockBuilder {
    id: Option<String>,
    name: Option<String>,
    input: Option<super::JsonMap>,
    content: Option<String>,
    metadata: Option<super::JsonMap>,
    state: Option<ToolCallState>,
}

impl ToolUseBlockBuilder {
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn input(mut self, input: super::JsonMap) -> Self {
        self.input = Some(input);
        self
    }

    pub fn content(mut self, content: impl Into<String>) -> Self {
        self.content = Some(content.into());
        self
    }

    pub fn metadata(mut self, metadata: super::JsonMap) -> Self {
        self.metadata = Some(metadata);
        self
    }

    pub fn state(mut self, state: ToolCallState) -> Self {
        self.state = Some(state);
        self
    }

    pub fn build(self) -> ToolUseBlock {
        ToolUseBlock {
            id: self.id.unwrap_or_default(),
            name: self.name.unwrap_or_default(),
            input: self.input.unwrap_or_default(),
            content: self.content,
            metadata: self.metadata.unwrap_or_default(),
            state: self.state.unwrap_or_default(),
        }
    }
}

/// The result of a tool execution.
///
/// Serves two purposes:
/// 1. As a return value from tool functions (id and name are empty).
/// 2. As a `ContentBlock` in messages (id and name are required).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultBlock {
    /// Tool call ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Tool name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Output content blocks.
    #[serde(default)]
    pub output: Vec<ContentBlock>,
    /// Additional execution information.
    #[serde(default)]
    pub metadata: super::JsonMap,
    /// Tool result state; defaults to `Running`.
    #[serde(default)]
    pub state: ToolResultState,
}

impl ToolResultBlock {
    /// Legacy metadata key indicating this result is suspended for external execution.
    ///
    /// Superseded by [`ToolResultState::Suspended`]; still written alongside the new
    /// encoding and still recognised by [`ToolResultBlock::is_suspended`] so blocks
    /// serialized by older versions keep working (one transition window).
    pub const METADATA_SUSPENDED: &'static str = "agentscope_suspended";

    /// Metadata key carrying the suspend reason (the `ToolSuspendException` message)
    /// when the tool supplied one.
    ///
    /// Control-plane information; never rendered through `output`.
    pub const METADATA_SUSPEND_REASON: &'static str = "agentscope_suspend_reason";

    /// Metadata key marking a tool result produced by a provider-side (server) tool.
    /// Mirrors the Java `ToolResultBlock.METADATA_SERVER_TOOL`.
    pub const METADATA_SERVER_TOOL: &'static str = "serverTool";

    /// Metadata key carrying the serialized [`ToolExecutionDetails`], named after the Java
    /// `ToolResultBlock.executionDetails` property.
    ///
    /// The facts live in `metadata` rather than in their own field so every existing
    /// `ToolResultBlock { .. }` construction keeps compiling; [`Self::execution_details`]
    /// and [`Self::with_execution_details`] are the typed accessors.
    ///
    /// [`ToolExecutionDetails`]: super::ToolExecutionDetails
    pub const METADATA_EXECUTION_DETAILS: &'static str = "executionDetails";

    pub fn new(id: &str, name: &str, output: Vec<ContentBlock>) -> Self {
        Self {
            id: Some(id.to_string()),
            name: Some(name.to_string()),
            output,
            metadata: super::JsonMap::new(),
            state: ToolResultState::Running,
        }
    }

    /// Creates a simple text result (for tool function return values).
    pub fn text(text: impl Into<String>) -> Self {
        Self::of(ContentBlock::Text(TextBlock::new(text)))
    }

    /// Creates an error result (for tool function return values).
    pub fn error(error_message: impl Into<String>) -> Self {
        Self {
            id: None,
            name: None,
            output: vec![ContentBlock::Text(TextBlock::new(format!(
                "Error: {}",
                error_message.into()
            )))],
            metadata: super::JsonMap::new(),
            state: ToolResultState::Error,
        }
    }

    /// Creates an explicit success result (for tool function return values).
    ///
    /// Mirrors Java `ToolResultBlock.success(String)`: a successful tool call
    /// carries an explicit terminal state instead of relying on an inferrer that
    /// misreads content merely starting with `Error:`/`[ERROR]`.
    pub fn success(text: impl Into<String>) -> Self {
        Self {
            id: None,
            name: None,
            output: vec![ContentBlock::Text(TextBlock::new(text))],
            metadata: super::JsonMap::new(),
            state: ToolResultState::Success,
        }
    }

    /// Resolves the terminal state of a tool result, mirroring Java
    /// `ReActAgent.determineToolResultState`.
    ///
    /// Order: a suspended result is [`ToolResultState::Suspended`]; an explicit
    /// terminal state (anything other than [`ToolResultState::Running`]) is kept
    /// verbatim; a `[ERROR]`-prefixed text output is [`ToolResultState::Error`];
    /// everything else is [`ToolResultState::Success`].
    ///
    /// Why this exists: tools that return a plain block leave `state` at its
    /// `Running` default, so history queries would otherwise always observe
    /// `RUNNING` even after the call finished (Java #2745).
    pub fn final_state(&self) -> ToolResultState {
        if self.is_suspended() {
            return ToolResultState::Suspended;
        }
        if self.state != ToolResultState::Running {
            return self.state;
        }
        let has_error_prefix = self.output.iter().any(|b| match b {
            ContentBlock::Text(t) => t.text.starts_with("[ERROR]"),
            _ => false,
        });
        if has_error_prefix {
            ToolResultState::Error
        } else {
            ToolResultState::Success
        }
    }

    /// Creates a result with a single output block.
    pub fn of(output: ContentBlock) -> Self {
        Self {
            id: None,
            name: None,
            output: vec![output],
            metadata: super::JsonMap::new(),
            state: ToolResultState::Running,
        }
    }

    /// Creates a result with an output list.
    pub fn of_blocks(output: Vec<ContentBlock>) -> Self {
        Self {
            id: None,
            name: None,
            output,
            metadata: super::JsonMap::new(),
            state: ToolResultState::Running,
        }
    }

    /// Creates a result for use in messages by setting id and name.
    pub fn of_message(id: &str, name: &str, output: ContentBlock) -> Self {
        Self::new(id, name, vec![output])
    }

    /// Returns a copy of this block with the given state.
    pub fn with_state(mut self, state: ToolResultState) -> Self {
        self.state = state;
        self
    }

    /// Returns a copy of this block with the id and name set to the given values.
    pub fn with_id_and_name(&self, id: &str, name: &str) -> Self {
        let mut b = self.clone();
        b.id = Some(id.to_string());
        b.name = Some(name.to_string());
        b
    }

    /// Creates a suspended result for external execution.
    ///
    /// Mirrors Java `ToolResultBlock.suspended(...)`: the tool has not executed, so the
    /// result carries **no output** (the real result arrives from the caller on resume),
    /// the state is [`ToolResultState::Suspended`] and, when a reason was supplied, it
    /// travels in [`Self::METADATA_SUSPEND_REASON`]. The legacy `METADATA_SUSPENDED`
    /// marker is dual-written so older readers keep detecting the suspension.
    pub fn suspended(reason: impl Into<String>, id: &str, name: &str) -> Self {
        let reason = reason.into();
        let mut metadata = super::JsonMap::new();
        metadata.insert(
            Self::METADATA_SUSPENDED.to_string(),
            serde_json::Value::Bool(true),
        );
        if !reason.is_empty() {
            metadata.insert(
                Self::METADATA_SUSPEND_REASON.to_string(),
                serde_json::Value::String(reason),
            );
        }
        Self {
            id: Some(id.to_string()),
            name: Some(name.to_string()),
            output: Vec::new(),
            metadata,
            state: ToolResultState::Suspended,
        }
    }

    /// Extracts plain text content from the output blocks.
    pub fn text_content(&self) -> String {
        self.output
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Whether this result is suspended for external execution.
    ///
    /// Recognises the canonical encoding ([`ToolResultState::Suspended`]) and the
    /// legacy wire marker ([`Self::METADATA_SUSPENDED`]), so blocks serialized by
    /// older versions keep working.
    pub fn is_suspended(&self) -> bool {
        self.state == ToolResultState::Suspended
            || self
                .metadata
                .get(Self::METADATA_SUSPENDED)
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
    }

    /// The suspend reason when this result is suspended and the tool supplied one.
    ///
    /// Returns `None` when the result is not suspended or when no reason was recorded.
    pub fn suspend_reason(&self) -> Option<&str> {
        if !self.is_suspended() {
            return None;
        }
        self.metadata
            .get(Self::METADATA_SUSPEND_REASON)
            .and_then(|v| v.as_str())
    }

    /// Attaches producer-supplied execution facts.
    ///
    /// Mirrors Java `ToolResultBlock.withExecutionDetails`.
    pub fn with_execution_details(mut self, details: super::ToolExecutionDetails) -> Self {
        if let Ok(value) = serde_json::to_value(&details) {
            self.metadata
                .insert(Self::METADATA_EXECUTION_DETAILS.to_string(), value);
        }
        self
    }

    /// The producer-supplied execution facts, when the tool supplied any.
    ///
    /// Mirrors Java `ToolResultBlock.getExecutionDetails`.
    pub fn execution_details(&self) -> Option<super::ToolExecutionDetails> {
        self.metadata
            .get(Self::METADATA_EXECUTION_DETAILS)
            .and_then(|value| serde_json::from_value(value.clone()).ok())
    }

    /// Whether this result was produced by a provider-side (server) tool.
    pub fn is_server_tool(&self) -> bool {
        self.metadata
            .get(Self::METADATA_SERVER_TOOL)
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    pub fn builder() -> ToolResultBlockBuilder {
        ToolResultBlockBuilder::default()
    }
}

#[derive(Debug, Default)]
pub struct ToolResultBlockBuilder {
    id: Option<String>,
    name: Option<String>,
    output: Option<Vec<ContentBlock>>,
    metadata: Option<super::JsonMap>,
    state: Option<ToolResultState>,
}

impl ToolResultBlockBuilder {
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn output(mut self, output: ContentBlock) -> Self {
        self.output = Some(vec![output]);
        self
    }

    pub fn outputs(mut self, output: Vec<ContentBlock>) -> Self {
        self.output = Some(output);
        self
    }

    pub fn metadata(mut self, metadata: super::JsonMap) -> Self {
        self.metadata = Some(metadata);
        self
    }

    pub fn state(mut self, state: ToolResultState) -> Self {
        self.state = Some(state);
        self
    }

    pub fn build(self) -> ToolResultBlock {
        ToolResultBlock {
            id: self.id,
            name: self.name,
            output: self.output.unwrap_or_default(),
            metadata: self.metadata.unwrap_or_default(),
            state: self.state.unwrap_or_default(),
        }
    }
}

/// A hint to the LLM during reasoning (e.g. from RAG).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HintBlock {
    /// Unique identifier of this hint block.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The hint text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Sender or origin of the hint (e.g. `"alice"`, `"system"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl HintBlock {
    pub fn new(id: &str, hint: &str) -> Self {
        Self {
            id: Some(id.to_string()),
            hint: Some(hint.to_string()),
            source: None,
        }
    }

    pub fn builder() -> HintBlockBuilder {
        HintBlockBuilder::default()
    }
}

#[derive(Debug, Default)]
pub struct HintBlockBuilder {
    id: Option<String>,
    hint: Option<String>,
    source: Option<String>,
}

impl HintBlockBuilder {
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    pub fn build(self) -> HintBlock {
        HintBlock {
            id: self.id,
            hint: self.hint,
            source: self.source,
        }
    }
}

fn new_data_id() -> String {
    Uuid::new_v4().to_string().replace('-', "")
}

/// Unified data block for arbitrary binary media (image / audio / video / file).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataBlock {
    /// The data source (URL or Base64); required.
    pub source: Source,
    /// Stable identifier; generated if absent.
    #[serde(default = "new_data_id")]
    pub id: String,
    /// Optional human-readable name (e.g. file name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl DataBlock {
    pub fn new(source: Source) -> Self {
        Self {
            source,
            id: new_data_id(),
            name: None,
        }
    }

    pub fn builder() -> DataBlockBuilder {
        DataBlockBuilder::default()
    }
}

#[derive(Debug, Default)]
pub struct DataBlockBuilder {
    source: Option<Source>,
    id: Option<String>,
    name: Option<String>,
}

impl DataBlockBuilder {
    pub fn source(mut self, source: Source) -> Self {
        self.source = Some(source);
        self
    }

    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn build(self) -> DataBlock {
        DataBlock {
            source: self.source.expect("DataBlock requires a source"),
            id: self
                .id
                .filter(|id| !id.is_empty())
                .unwrap_or_else(new_data_id),
            name: self.name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_sets_explicit_success_state() {
        let rb = ToolResultBlock::success("all good");
        assert_eq!(rb.state, ToolResultState::Success);
        assert_eq!(rb.text_content(), "all good");
    }

    #[test]
    fn error_sets_explicit_error_state() {
        let rb = ToolResultBlock::error("boom");
        assert_eq!(rb.state, ToolResultState::Error);
        assert_eq!(rb.text_content(), "Error: boom");
    }

    #[test]
    fn final_state_resolves_running_default_to_success() {
        // `of`/`text` leave the `Running` default; a finished call must report
        // Success rather than staying RUNNING forever (Java #2745).
        assert_eq!(
            ToolResultBlock::text("42").final_state(),
            ToolResultState::Success
        );
        assert_eq!(
            ToolResultBlock::of_blocks(vec![]).final_state(),
            ToolResultState::Success
        );
    }

    #[test]
    fn final_state_infers_error_from_error_prefixed_output() {
        let rb = ToolResultBlock::text("[ERROR] disk full");
        assert_eq!(rb.state, ToolResultState::Running);
        assert_eq!(rb.final_state(), ToolResultState::Error);
    }

    #[test]
    fn final_state_keeps_explicit_terminal_states_verbatim() {
        // A successful literal that merely *starts with* "Error:" is not an error:
        // the explicit state wins over any text heuristic.
        let rb = ToolResultBlock::success("Error: none, everything is fine");
        assert_eq!(rb.final_state(), ToolResultState::Success);

        let denied = ToolResultBlock::text("nope").with_state(ToolResultState::Denied);
        assert_eq!(denied.final_state(), ToolResultState::Denied);

        let interrupted = ToolResultBlock::text("stopped").with_state(ToolResultState::Interrupted);
        assert_eq!(interrupted.final_state(), ToolResultState::Interrupted);
    }

    #[test]
    fn final_state_reports_suspended_first() {
        let rb = ToolResultBlock::suspended("awaiting user", "c1", "external");
        assert_eq!(rb.final_state(), ToolResultState::Suspended);
    }
}
