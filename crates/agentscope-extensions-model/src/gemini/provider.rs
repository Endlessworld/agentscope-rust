//! Gemini [`ModelProvider`] for the core [`ModelRegistry`].
//!
//! Mirrors `GeminiModelProvider`: claims `gemini:<model>` ids (Java's `MODEL_ID`
//! pattern is `gemini:.+`, so bare model ids are **not** claimed), and resolves
//! the API key from the creation context, then `GEMINI_API_KEY`.
//!
//! The Java provider also supports Vertex AI (`project` / `location` /
//! `vertexAI` options + `GoogleCredentials`). Vertex AI uses a completely
//! different auth scheme (OAuth2 service accounts) and endpoint host; it is
//! **out of scope** for this offline adapter and recorded as a gap in
//! `MODEL__GAP_TRACKER.md`.

use std::sync::Arc;

use agentscope_core::{Error, Model, ModelCreationContext, ModelProvider, Result};

use crate::gemini::credential::GeminiCredential;
use crate::gemini::model::{default_transport, GeminiChatModel};

/// Provider id used in `provider:model` ids.
pub const PROVIDER_ID: &str = "gemini";

/// Environment variable consulted when no explicit base URL is supplied.
pub const BASE_URL_ENV: &str = "GEMINI_BASE_URL";

/// Provider plugin that builds [`GeminiChatModel`] instances.
#[derive(Debug, Default, Clone, Copy)]
pub struct GeminiModelProvider;

impl GeminiModelProvider {
    /// Creates the provider.
    pub fn new() -> Self {
        Self
    }

    /// Strips an optional `gemini:` prefix, returning the bare model id.
    pub fn strip_prefix(model: &str) -> &str {
        let trimmed = model.trim();
        trimmed
            .strip_prefix("gemini:")
            .map(str::trim)
            .unwrap_or(trimmed)
    }
}

impl ModelProvider for GeminiModelProvider {
    fn provider_id(&self) -> &str {
        PROVIDER_ID
    }

    /// Claims the explicit `gemini:<model>` form **and** the bare `gemini-*`
    /// model family.
    ///
    /// The bare form is required by [`agentscope_core::ModelRegistry`]: its
    /// `can_resolve` splits `provider:model`, finds the provider by prefix and
    /// then calls `supports(model_without_prefix)`. This matches the existing
    /// DashScope adapter, which likewise claims bare `qwen*` ids.
    fn supports(&self, model: &str) -> bool {
        let trimmed = model.trim();
        if trimmed.is_empty() {
            return false;
        }
        if let Some(rest) = trimmed.strip_prefix("gemini:") {
            return !rest.trim().is_empty();
        }
        trimmed.starts_with("gemini-")
    }

    fn create(&self, ctx: ModelCreationContext) -> Result<Arc<dyn Model>> {
        let model_name = Self::strip_prefix(&ctx.model);
        if model_name.is_empty() || !self.supports(&ctx.model) {
            return Err(Error::Model(format!(
                "Unsupported Gemini model id: {}",
                ctx.model
            )));
        }

        if ctx
            .options
            .additional_body_params
            .get("vertexAI")
            .and_then(|v| v.as_bool())
            == Some(true)
        {
            return Err(Error::Model(
                "Vertex AI is not supported by the offline Gemini adapter; use the api-key flow"
                    .to_string(),
            ));
        }

        let api_key =
            GeminiCredential::resolve_api_key(ctx.api_key.as_deref()).ok_or_else(|| {
                Error::Model(format!(
                    "GEMINI_API_KEY is required to create Gemini model: {}",
                    ctx.model
                ))
            })?;

        let context_window_size = ctx
            .options
            .additional_body_params
            .get("contextWindowSize")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;

        let base_url = crate::common::resolve_base_url(
            ctx.base_url.clone(),
            BASE_URL_ENV,
            crate::gemini::credential::DEFAULT_BASE_URL,
        );

        // Formatter selector (mirrors Java's `GeminiModelProvider` resolving an
        // assignable `Formatter` component). Consumed here so it never reaches
        // the request body.
        let mut default_options = ctx.options.clone();
        let multi_agent_prompt = crate::common::take_multi_agent_prompt(&mut default_options);

        let model = GeminiChatModel::new(model_name, api_key, Some(base_url), default_transport())
            .with_default_options(default_options)
            .with_stream(ctx.stream.unwrap_or(true))
            .with_context_window_size(context_window_size)
            .with_multi_agent_formatter(multi_agent_prompt);
        Ok(Arc::new(model))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::{GenerateOptions, ModelRegistry};

    #[test]
    fn provider_id_and_supports() {
        let provider = GeminiModelProvider::new();
        assert_eq!(provider.provider_id(), "gemini");
        assert!(provider.supports("gemini:gemini-2.5-flash"));
        assert!(provider.supports("  gemini:gemini-1.5-pro  "));
        // Bare ids are claimed so core's registry can resolve `gemini:<model>`.
        assert!(provider.supports("gemini-2.5-flash"));
        assert!(!provider.supports("gemini:"));
        assert!(!provider.supports("gpt-4o"));
        assert!(!provider.supports(""));
    }

    #[test]
    fn strip_prefix_handles_prefixed_and_bare_ids() {
        assert_eq!(
            GeminiModelProvider::strip_prefix("gemini:gemini-2.5-flash"),
            "gemini-2.5-flash"
        );
        assert_eq!(
            GeminiModelProvider::strip_prefix("gemini-2.5-flash"),
            "gemini-2.5-flash"
        );
        assert_eq!(GeminiModelProvider::strip_prefix("gemini:"), "");
    }

    #[test]
    fn create_builds_model_from_api_key() {
        let provider = GeminiModelProvider::new();
        let ctx = ModelCreationContext::new("gemini", "gemini:gemini-2.5-flash")
            .api_key("gm-key")
            .base_url("https://example.test");
        let model = provider.create(ctx).unwrap();
        assert_eq!(model.model_name(), "gemini-2.5-flash");
    }

    #[test]
    fn create_rejects_unsupported_ids() {
        let provider = GeminiModelProvider::new();
        let ctx = ModelCreationContext::new("gemini", "gpt-4o").api_key("k");
        assert!(provider.create(ctx).is_err());
    }

    #[test]
    fn create_rejects_vertex_ai() {
        let provider = GeminiModelProvider::new();
        let mut options = GenerateOptions::default();
        options
            .additional_body_params
            .insert("vertexAI".to_string(), serde_json::json!(true));
        let ctx = ModelCreationContext::new("gemini", "gemini:gemini-2.5-flash")
            .api_key("k")
            .options(options);
        let err = match provider.create(ctx) {
            Ok(_) => panic!("Vertex AI must be rejected by the offline adapter"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("Vertex AI"));
    }

    #[test]
    fn registers_and_resolves_in_core_registry() {
        let mut registry = ModelRegistry::new();
        registry.register(Arc::new(GeminiModelProvider::new()));
        assert_eq!(registry.len(), 1);
        assert!(registry.can_resolve("gemini:gemini-2.5-flash"));
        assert!(registry.can_resolve("gemini-2.5-flash"));
        assert!(!registry.can_resolve("gemini:"));
        assert!(!registry.can_resolve("gpt-4o"));
    }

    #[test]
    fn context_window_size_option_is_applied() {
        let provider = GeminiModelProvider::new();
        let mut options = GenerateOptions::default();
        options.additional_body_params.insert(
            "contextWindowSize".to_string(),
            serde_json::json!(1_048_576),
        );
        let ctx = ModelCreationContext::new("gemini", "gemini:gemini-2.5-flash")
            .api_key("k")
            .options(options);
        let model = provider.create(ctx).unwrap();
        assert_eq!(model.context_window_size(), 1_048_576);
    }

    #[test]
    fn missing_api_key_is_rejected() {
        // Only meaningful when GEMINI_API_KEY is unset in the test environment.
        if std::env::var("GEMINI_API_KEY").is_ok() {
            return;
        }
        let provider = GeminiModelProvider::new();
        let ctx = ModelCreationContext::new("gemini", "gemini:gemini-2.5-flash");
        assert!(provider.create(ctx).is_err());
    }
}
