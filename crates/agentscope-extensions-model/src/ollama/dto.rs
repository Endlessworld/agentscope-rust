//! Ollama chat API data transfer objects, mirroring the Java DTOs under
//! `io.agentscope.extensions.model.ollama.dto`.
//!
//! Every optional field is `skip_serializing_if = "Option::is_none"`, matching
//! Jackson's `JsonInclude.Include.NON_NULL`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Ollama chat request body (`POST /api/chat`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct OllamaRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<OllamaMessage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// `"json"` or a JSON-schema object for structured output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<serde_json::Value>,
    #[serde(rename = "keep_alive", skip_serializing_if = "Option::is_none")]
    pub keep_alive: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<OllamaTool>>,
    #[serde(rename = "tool_choice", skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    /// `true`/`false` or a level string (`"low"`/`"medium"`/`"high"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub think: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<BTreeMap<String, serde_json::Value>>,
}

/// A single conversation message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct OllamaMessage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<String>>,
    #[serde(rename = "tool_calls", skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OllamaToolCall>>,
    #[serde(rename = "tool_call_id", skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Serialized to `tool_name`; the Java DTO names the field `name`.
    #[serde(rename = "tool_name", skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl OllamaMessage {
    /// Creates a `role`/`content` message.
    pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Some(role.into()),
            content: Some(content.into()),
            ..Default::default()
        }
    }
}

/// A tool definition: `{"type": "function", "function": {...}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OllamaTool {
    #[serde(rename = "type", default = "default_tool_type")]
    pub tool_type: String,
    pub function: OllamaToolFunction,
}

fn default_tool_type() -> String {
    "function".to_string()
}

impl OllamaTool {
    /// Wraps a function definition as a tool.
    pub fn function(function: OllamaToolFunction) -> Self {
        Self {
            tool_type: "function".to_string(),
            function,
        }
    }
}

/// Tool function signature (JSON Schema parameters).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OllamaToolFunction {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
}

/// A tool invocation produced by the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OllamaToolCall {
    pub function: OllamaFunction,
}

/// Execution details of a tool call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OllamaFunction {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<BTreeMap<String, serde_json::Value>>,
}

/// Ollama chat response (one NDJSON frame when streaming).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct OllamaResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(rename = "created_at", skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<OllamaMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub done: Option<bool>,
    #[serde(rename = "total_duration", skip_serializing_if = "Option::is_none")]
    pub total_duration: Option<i64>,
    #[serde(rename = "load_duration", skip_serializing_if = "Option::is_none")]
    pub load_duration: Option<i64>,
    #[serde(rename = "prompt_eval_count", skip_serializing_if = "Option::is_none")]
    pub prompt_eval_count: Option<i64>,
    #[serde(
        rename = "prompt_eval_duration",
        skip_serializing_if = "Option::is_none"
    )]
    pub prompt_eval_duration: Option<i64>,
    #[serde(rename = "eval_count", skip_serializing_if = "Option::is_none")]
    pub eval_count: Option<i64>,
    #[serde(rename = "eval_duration", skip_serializing_if = "Option::is_none")]
    pub eval_duration: Option<i64>,
    #[serde(rename = "done_reason", skip_serializing_if = "Option::is_none")]
    pub done_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Ollama embedding request body (`POST /api/embed`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OllamaEmbeddingRequest {
    pub model: String,
    pub input: Vec<String>,
    #[serde(rename = "keep_alive", skip_serializing_if = "Option::is_none")]
    pub keep_alive: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncate: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<BTreeMap<String, serde_json::Value>>,
}

impl OllamaEmbeddingRequest {
    /// Creates a minimal embedding request.
    pub fn new(model: impl Into<String>, input: Vec<String>) -> Self {
        Self {
            model: model.into(),
            input,
            keep_alive: None,
            truncate: None,
            options: None,
        }
    }
}

/// Ollama embedding response body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct OllamaEmbeddingResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub embeddings: Vec<Vec<f32>>,
    #[serde(rename = "total_duration", skip_serializing_if = "Option::is_none")]
    pub total_duration: Option<i64>,
    #[serde(rename = "load_duration", skip_serializing_if = "Option::is_none")]
    pub load_duration: Option<i64>,
    #[serde(rename = "prompt_eval_count", skip_serializing_if = "Option::is_none")]
    pub prompt_eval_count: Option<i64>,
}
