//! DashScope API data transfer objects, mirroring the Java DTOs under
//! `io.agentscope.extensions.model.dashscope.dto`.
//!
//! `content` is polymorphic in Java (`String` for text-only, `List<ContentPart>`
//! for multimodal); here it is modelled as a Rust enum with `#[serde(untagged)]`
//! so both wire shapes are accepted.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Top-level DashScope request body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DashScopeRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<DashScopeInput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<DashScopeParameters>,
}

/// The `input` container.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DashScopeInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<DashScopeMessage>>,
}

impl DashScopeInput {
    /// Wraps a message list.
    pub fn of(messages: Vec<DashScopeMessage>) -> Self {
        Self { messages: Some(messages) }
    }
}

/// Generation parameters (`result_format: "message"` by default).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DashScopeParameters {
    #[serde(rename = "result_format", default = "default_result_format")]
    pub result_format: String,
    #[serde(rename = "incremental_output", skip_serializing_if = "Option::is_none")]
    pub incremental_output: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(rename = "top_p", skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(rename = "top_k", skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(rename = "max_tokens", skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(rename = "enable_thinking", skip_serializing_if = "Option::is_none")]
    pub enable_thinking: Option<bool>,
    #[serde(rename = "thinking_budget", skip_serializing_if = "Option::is_none")]
    pub thinking_budget: Option<u32>,
    #[serde(rename = "enable_search", skip_serializing_if = "Option::is_none")]
    pub enable_search: Option<bool>,
    #[serde(rename = "search_options", skip_serializing_if = "Option::is_none")]
    pub search_options: Option<DashScopeSearchOptions>,
    #[serde(rename = "enable_code_interpreter", skip_serializing_if = "Option::is_none")]
    pub enable_code_interpreter: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<DashScopeTool>>,
    #[serde(rename = "tool_choice", skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    #[serde(rename = "parallel_tool_calls", skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(rename = "frequency_penalty", skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    #[serde(rename = "presence_penalty", skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    #[serde(rename = "repetition_penalty", skip_serializing_if = "Option::is_none")]
    pub repetition_penalty: Option<f64>,
    #[serde(rename = "response_format", skip_serializing_if = "Option::is_none")]
    pub response_format: Option<serde_json::Value>,
}

fn default_result_format() -> String {
    "message".to_string()
}

impl Default for DashScopeParameters {
    fn default() -> Self {
        Self {
            result_format: default_result_format(),
            incremental_output: None,
            temperature: None,
            top_p: None,
            top_k: None,
            max_tokens: None,
            stop: None,
            enable_thinking: None,
            thinking_budget: None,
            enable_search: None,
            search_options: None,
            enable_code_interpreter: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            seed: None,
            frequency_penalty: None,
            presence_penalty: None,
            repetition_penalty: None,
            response_format: None,
        }
    }
}

/// Web-search strategy options (`search_options`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DashScopeSearchOptions {
    #[serde(rename = "enable_source", skip_serializing_if = "Option::is_none")]
    pub enable_source: Option<bool>,
    #[serde(rename = "enable_citation", skip_serializing_if = "Option::is_none")]
    pub enable_citation: Option<bool>,
    #[serde(rename = "citation_format", skip_serializing_if = "Option::is_none")]
    pub citation_format: Option<String>,
    #[serde(rename = "forced_search", skip_serializing_if = "Option::is_none")]
    pub forced_search: Option<bool>,
    #[serde(rename = "search_strategy", skip_serializing_if = "Option::is_none")]
    pub search_strategy: Option<String>,
    #[serde(rename = "enable_search_extension", skip_serializing_if = "Option::is_none")]
    pub enable_search_extension: Option<bool>,
    #[serde(rename = "prepend_search_result", skip_serializing_if = "Option::is_none")]
    pub prepend_search_result: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turbo: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<bool>,
    #[serde(rename = "agent_max", skip_serializing_if = "Option::is_none")]
    pub agent_max: Option<u32>,
}

/// Message content: a plain string or a multimodal content-part list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DashScopeContent {
    /// Text-only content.
    Text(String),
    /// Multimodal content parts.
    Parts(Vec<DashScopeContentPart>),
}

impl Default for DashScopeContent {
    fn default() -> Self {
        DashScopeContent::Text(String::new())
    }
}

impl DashScopeContent {
    /// Returns the text when this is a plain string, or `None` for parts.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            DashScopeContent::Text(t) => Some(t),
            DashScopeContent::Parts(_) => None,
        }
    }

    /// Returns the concatenation of all `text` parts.
    pub fn text_joined(&self) -> String {
        match self {
            DashScopeContent::Text(t) => t.clone(),
            DashScopeContent::Parts(parts) => parts
                .iter()
                .filter_map(|p| p.text.clone())
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

/// A single conversation message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DashScopeMessage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<DashScopeContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "tool_call_id", skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(rename = "tool_calls", skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<DashScopeToolCall>>,
    #[serde(
        rename = "reasoning_content",
        alias = "reasoning",
        skip_serializing_if = "Option::is_none"
    )]
    pub reasoning_content: Option<String>,
    /// Prompt-cache hint; omitted when `None` or empty (Java `NON_EMPTY`).
    #[serde(rename = "cache_control", skip_serializing_if = "cache_control_is_empty")]
    pub cache_control: Option<CacheControl>,
}

impl DashScopeMessage {
    /// Attaches a `cache_control` hint.
    pub fn with_cache_control(mut self, cache_control: CacheControl) -> Self {
        self.cache_control = Some(cache_control);
        self
    }
}

/// One multimodal content part; omitted fields are not serialized, matching
/// `JsonInclude.Include.NON_NULL` plus the "has content" helper in Java.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DashScopeContentPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fps: Option<f32>,
    #[serde(rename = "max_frames", skip_serializing_if = "Option::is_none")]
    pub max_frames: Option<u32>,
    #[serde(rename = "min_pixels", skip_serializing_if = "Option::is_none")]
    pub min_pixels: Option<u32>,
    #[serde(rename = "max_pixels", skip_serializing_if = "Option::is_none")]
    pub max_pixels: Option<u32>,
    #[serde(rename = "total_pixels", skip_serializing_if = "Option::is_none")]
    pub total_pixels: Option<u32>,
}

impl DashScopeContentPart {
    /// Creates a `text` part.
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: Some(text.into()), ..Default::default() }
    }

    /// Creates an `image` part.
    pub fn image(url: impl Into<String>) -> Self {
        Self { image: Some(url.into()), ..Default::default() }
    }

    /// Creates an `audio` part.
    pub fn audio(url: impl Into<String>) -> Self {
        Self { audio: Some(url.into()), ..Default::default() }
    }

    /// Creates a `video` part.
    pub fn video(url: impl Into<String>) -> Self {
        Self { video: Some(serde_json::Value::String(url.into())), ..Default::default() }
    }

    /// Whether this part carries any content.
    pub fn has_content(&self) -> bool {
        self.text.as_deref().map(|t| !t.is_empty()).unwrap_or(false)
            || self.image.is_some()
            || self.audio.is_some()
            || self.video.is_some()
    }
}

/// A tool definition (`{"type": "function", "function": {...}}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DashScopeTool {
    #[serde(rename = "type", default = "default_tool_type")]
    pub tool_type: String,
    pub function: DashScopeToolFunction,
}

fn default_tool_type() -> String {
    "function".to_string()
}

impl DashScopeTool {
    /// Wraps a function definition.
    pub fn function(function: DashScopeToolFunction) -> Self {
        Self { tool_type: "function".to_string(), function }
    }
}

/// Tool function signature.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DashScopeToolFunction {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
}

/// A tool invocation produced by the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DashScopeToolCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub tool_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<DashScopeFunction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<i64>,
}

/// Tool-call execution payload; `arguments` is a JSON **string** in DashScope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DashScopeFunction {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

impl DashScopeFunction {
    /// Creates a function payload from a name and raw argument JSON string.
    pub fn of(name: impl Into<String>, arguments: impl Into<String>) -> Self {
        Self { name: name.into(), arguments: Some(arguments.into()) }
    }
}

/// Top-level DashScope response body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DashScopeResponse {
    #[serde(rename = "request_id", skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<DashScopeOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<DashScopeUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// The `output` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DashScopeOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub choices: Option<Vec<DashScopeChoice>>,
    #[serde(rename = "finish_reason", skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl DashScopeOutput {
    /// Returns the first choice, if any.
    pub fn first_choice(&self) -> Option<&DashScopeChoice> {
        self.choices.as_ref().and_then(|c| c.first())
    }
}

/// A single choice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DashScopeChoice {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<DashScopeMessage>,
    #[serde(rename = "finish_reason", skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<i64>,
}

/// Token usage statistics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DashScopeUsage {
    #[serde(rename = "input_tokens", skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,
    #[serde(rename = "output_tokens", skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,
    #[serde(rename = "total_tokens", skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<i64>,
    #[serde(rename = "image_tokens", skip_serializing_if = "Option::is_none")]
    pub image_tokens: Option<i64>,
    #[serde(rename = "video_tokens", skip_serializing_if = "Option::is_none")]
    pub video_tokens: Option<i64>,
    #[serde(rename = "audio_tokens", skip_serializing_if = "Option::is_none")]
    pub audio_tokens: Option<i64>,
}

/// A cache-control hint attached to a message (empty map == `NON_EMPTY` skip).
pub type CacheControl = BTreeMap<String, String>;

/// Returns `true` when the optional cache-control map is absent or empty; used
/// by `#[serde(skip_serializing_if = ...)]` to mirror Jackson's `NON_EMPTY`.
#[allow(clippy::ptr_arg)]
pub fn cache_control_is_empty(value: &Option<CacheControl>) -> bool {
    match value {
        None => true,
        Some(map) => map.is_empty(),
    }
}

/// Response of `GET /api/v1/public-keys/latest` (RSA request encryption).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DashScopePublicKeyResponse {
    #[serde(rename = "request_id", skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<PublicKeyData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl DashScopePublicKeyResponse {
    /// Whether the response carries an error code.
    pub fn is_error(&self) -> bool {
        self.code.as_deref().map(|c| !c.is_empty()).unwrap_or(false)
    }
}

/// The `data` object of [`DashScopePublicKeyResponse`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PublicKeyData {
    #[serde(rename = "public_key", skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    #[serde(rename = "public_key_id", skip_serializing_if = "Option::is_none")]
    pub public_key_id: Option<String>,
}
