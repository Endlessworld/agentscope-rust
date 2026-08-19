//! Immutable generation options for LLM models, mirroring the Java `GenerateOptions`.
use std::collections::HashMap;

use crate::formatter::ResponseFormat;

use super::execution_config::ExecutionConfig;
use super::tool_choice::ToolChoice;

/// Per-request and connection-level generation options.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GenerateOptions {
    // Connection-level configuration
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub endpoint_path: Option<String>,
    pub model_name: Option<String>,
    pub stream: Option<bool>,
    // Generation parameters
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_tokens: Option<u32>,
    pub max_completion_tokens: Option<u32>,
    pub frequency_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub thinking_budget: Option<u32>,
    pub reasoning_effort: Option<String>,
    pub execution_config: Option<ExecutionConfig>,
    pub tool_choice: Option<ToolChoice>,
    pub top_k: Option<u32>,
    pub seed: Option<i64>,
    pub cache_control: Option<bool>,
    pub parallel_tool_calls: Option<bool>,
    pub response_format: Option<ResponseFormat>,
    pub additional_headers: HashMap<String, String>,
    pub additional_body_params: HashMap<String, serde_json::Value>,
    pub additional_query_params: HashMap<String, String>,
}

impl GenerateOptions {
    /// Creates a new builder.
    pub fn builder() -> GenerateOptionsBuilder {
        GenerateOptionsBuilder::default()
    }

    /// Merges two option sets, with `primary` taking precedence over `fallback`.
    pub fn merge_options(primary: &GenerateOptions, fallback: &GenerateOptions) -> GenerateOptions {
        let mut b = GenerateOptions::builder();
        b.api_key = primary.api_key.clone().or_else(|| fallback.api_key.clone());
        b.base_url = primary.base_url.clone().or_else(|| fallback.base_url.clone());
        b.endpoint_path = primary.endpoint_path.clone().or_else(|| fallback.endpoint_path.clone());
        b.model_name = primary.model_name.clone().or_else(|| fallback.model_name.clone());
        b.stream = primary.stream.or(fallback.stream);
        b.temperature = primary.temperature.or(fallback.temperature);
        b.top_p = primary.top_p.or(fallback.top_p);
        b.max_tokens = primary.max_tokens.or(fallback.max_tokens);
        b.max_completion_tokens = primary.max_completion_tokens.or(fallback.max_completion_tokens);
        b.frequency_penalty = primary.frequency_penalty.or(fallback.frequency_penalty);
        b.presence_penalty = primary.presence_penalty.or(fallback.presence_penalty);
        b.thinking_budget = primary.thinking_budget.or(fallback.thinking_budget);
        b.reasoning_effort = primary.reasoning_effort.clone().or_else(|| fallback.reasoning_effort.clone());
        b.execution_config = ExecutionConfig::merge_configs(primary.execution_config.as_ref(), fallback.execution_config.as_ref());
        b.tool_choice = primary.tool_choice.clone().or_else(|| fallback.tool_choice.clone());
        b.top_k = primary.top_k.or(fallback.top_k);
        b.seed = primary.seed.or(fallback.seed);
        b.cache_control = primary.cache_control.or(fallback.cache_control);
        b.parallel_tool_calls = primary.parallel_tool_calls.or(fallback.parallel_tool_calls);
        b.response_format = primary.response_format.clone().or_else(|| fallback.response_format.clone());
        let mut headers = fallback.additional_headers.clone();
        headers.extend(primary.additional_headers.clone());
        b.additional_headers = headers;
        let mut body = fallback.additional_body_params.clone();
        body.extend(primary.additional_body_params.clone());
        b.additional_body_params = body;
        let mut query = fallback.additional_query_params.clone();
        query.extend(primary.additional_query_params.clone());
        b.additional_query_params = query;
        b.build()
    }
}

/// Builder for `GenerateOptions`.
#[derive(Debug, Default, Clone)]
pub struct GenerateOptionsBuilder {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub endpoint_path: Option<String>,
    pub model_name: Option<String>,
    pub stream: Option<bool>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_tokens: Option<u32>,
    pub max_completion_tokens: Option<u32>,
    pub frequency_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub thinking_budget: Option<u32>,
    pub reasoning_effort: Option<String>,
    pub execution_config: Option<ExecutionConfig>,
    pub tool_choice: Option<ToolChoice>,
    pub top_k: Option<u32>,
    pub seed: Option<i64>,
    pub cache_control: Option<bool>,
    pub parallel_tool_calls: Option<bool>,
    pub response_format: Option<ResponseFormat>,
    pub additional_headers: HashMap<String, String>,
    pub additional_body_params: HashMap<String, serde_json::Value>,
    pub additional_query_params: HashMap<String, String>,
}

impl GenerateOptionsBuilder {
    pub fn api_key(mut self, v: impl Into<String>) -> Self { self.api_key = Some(v.into()); self }
    pub fn base_url(mut self, v: impl Into<String>) -> Self { self.base_url = Some(v.into()); self }
    pub fn endpoint_path(mut self, v: impl Into<String>) -> Self { self.endpoint_path = Some(v.into()); self }
    pub fn model_name(mut self, v: impl Into<String>) -> Self { self.model_name = Some(v.into()); self }
    pub fn stream(mut self, v: bool) -> Self { self.stream = Some(v); self }
    pub fn temperature(mut self, v: f64) -> Self { self.temperature = Some(v); self }
    pub fn top_p(mut self, v: f64) -> Self { self.top_p = Some(v); self }
    pub fn max_tokens(mut self, v: u32) -> Self { self.max_tokens = Some(v); self }
    pub fn max_completion_tokens(mut self, v: u32) -> Self { self.max_completion_tokens = Some(v); self }
    pub fn frequency_penalty(mut self, v: f64) -> Self { self.frequency_penalty = Some(v); self }
    pub fn presence_penalty(mut self, v: f64) -> Self { self.presence_penalty = Some(v); self }
    pub fn thinking_budget(mut self, v: u32) -> Self { self.thinking_budget = Some(v); self }
    pub fn reasoning_effort(mut self, v: impl Into<String>) -> Self { self.reasoning_effort = Some(v.into()); self }
    pub fn execution_config(mut self, v: ExecutionConfig) -> Self { self.execution_config = Some(v); self }
    pub fn tool_choice(mut self, v: ToolChoice) -> Self { self.tool_choice = Some(v); self }
    pub fn top_k(mut self, v: u32) -> Self { self.top_k = Some(v); self }
    pub fn seed(mut self, v: i64) -> Self { self.seed = Some(v); self }
    pub fn cache_control(mut self, v: bool) -> Self { self.cache_control = Some(v); self }
    pub fn parallel_tool_calls(mut self, v: bool) -> Self { self.parallel_tool_calls = Some(v); self }
    pub fn response_format(mut self, v: ResponseFormat) -> Self { self.response_format = Some(v); self }
    pub fn additional_header(mut self, k: impl Into<String>, v: impl Into<String>) -> Self { self.additional_headers.insert(k.into(), v.into()); self }
    pub fn additional_body_param(mut self, k: impl Into<String>, v: serde_json::Value) -> Self { self.additional_body_params.insert(k.into(), v); self }
    pub fn additional_query_param(mut self, k: impl Into<String>, v: impl Into<String>) -> Self { self.additional_query_params.insert(k.into(), v.into()); self }

    pub fn build(self) -> GenerateOptions {
        GenerateOptions {
            api_key: self.api_key,
            base_url: self.base_url,
            endpoint_path: self.endpoint_path,
            model_name: self.model_name,
            stream: self.stream,
            temperature: self.temperature,
            top_p: self.top_p,
            max_tokens: self.max_tokens,
            max_completion_tokens: self.max_completion_tokens,
            frequency_penalty: self.frequency_penalty,
            presence_penalty: self.presence_penalty,
            thinking_budget: self.thinking_budget,
            reasoning_effort: self.reasoning_effort,
            execution_config: self.execution_config,
            tool_choice: self.tool_choice,
            top_k: self.top_k,
            seed: self.seed,
            cache_control: self.cache_control,
            parallel_tool_calls: self.parallel_tool_calls,
            response_format: self.response_format,
            additional_headers: self.additional_headers,
            additional_body_params: self.additional_body_params,
            additional_query_params: self.additional_query_params,
        }
    }
}
