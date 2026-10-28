//! Ollama [`ModelProvider`] for the core [`ModelRegistry`].
//!
//! Mirrors `OllamaModelProvider`, including env-based base URL resolution
//! (`OLLAMA_BASE_URL`).

use std::sync::Arc;

use agentscope_core::{Model, ModelCreationContext, ModelProvider, Result};

use crate::ollama::model::{default_transport, OllamaChatModel, DEFAULT_BASE_URL};
use crate::ollama::options::OllamaOptions;

/// Provider id used in `provider:model` ids.
pub const PROVIDER_ID: &str = "ollama";

/// Provider plugin that builds [`OllamaChatModel`] instances.
#[derive(Debug, Default, Clone, Copy)]
pub struct OllamaModelProvider;

impl OllamaModelProvider {
    /// Creates the provider.
    pub fn new() -> Self {
        Self
    }

    /// Strips an optional `ollama:` prefix from a model id.
    pub fn strip_prefix(model: &str) -> &str {
        model.trim().strip_prefix("ollama:").unwrap_or(model.trim())
    }
}

impl ModelProvider for OllamaModelProvider {
    fn provider_id(&self) -> &str {
        PROVIDER_ID
    }

    fn supports(&self, model: &str) -> bool {
        let trimmed = model.trim();
        if trimmed.is_empty() {
            return false;
        }
        if let Some(rest) = trimmed.strip_prefix("ollama:") {
            return !rest.trim().is_empty();
        }
        // A bare id (no `provider:` prefix) is claimable by Ollama. Register
        // this provider *after* the OpenAI-compatible one so that it only acts
        // as the fallback for ids no one else claims.
        !trimmed.contains(':')
    }

    fn create(&self, ctx: ModelCreationContext) -> Result<Arc<dyn Model>> {
        let model_name = Self::strip_prefix(&ctx.model);
        if model_name.is_empty() {
            return Err(agentscope_core::Error::Model(
                "Ollama model id must not be empty".into(),
            ));
        }
        let base_url = ctx
            .base_url
            .clone()
            .filter(|u| !u.trim().is_empty())
            .or_else(|| std::env::var("OLLAMA_BASE_URL").ok())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        let mut default_options = OllamaOptions::from_generate_options(&ctx.options);
        if ctx.enable_thinking {
            default_options.think = Some(crate::ollama::options::ThinkOption::Boolean(true));
        }
        if default_options.model.is_none() {
            default_options.model = Some(model_name.to_string());
        }

        // Provider-level formatter selectors (mirrors Java's
        // `OllamaModelProvider` + `OllamaMultiAgentFormatter` wiring). Consumed
        // here so they never leak into the request body.
        let mut selector_options = ctx.options.clone();
        let multi_agent_prompt = crate::common::take_multi_agent_prompt(&mut selector_options);
        let promote_tool_result_images =
            crate::common::take_bool_option(&mut selector_options, "promoteToolResultImages")
                .unwrap_or(false);

        let model = OllamaChatModel::new(model_name, Some(base_url), default_transport())
            .with_default_options(default_options)
            .with_stream(ctx.stream.unwrap_or(true))
            .with_multi_agent_formatter(multi_agent_prompt)
            .with_promote_tool_result_images(promote_tool_result_images);
        Ok(Arc::new(model))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_id_and_supports() {
        let provider = OllamaModelProvider::new();
        assert_eq!(provider.provider_id(), "ollama");
        assert!(provider.supports("ollama:llama3"));
        assert!(provider.supports("llama3"));
        assert!(!provider.supports("ollama:"));
        assert!(!provider.supports("openai:gpt-4o"));
        assert!(!provider.supports("  "));
    }

    #[test]
    fn create_builds_model_with_explicit_base_url() {
        let provider = OllamaModelProvider::new();
        let ctx = ModelCreationContext::new("ollama", "ollama:qwen2")
            .base_url("http://example.test:1234");
        let model = provider.create(ctx).unwrap();
        assert_eq!(model.model_name(), "qwen2");
    }

    #[test]
    fn create_rejects_empty_model() {
        let provider = OllamaModelProvider::new();
        let ctx = ModelCreationContext::new("ollama", "ollama:");
        assert!(provider.create(ctx).is_err());
    }
}
