//! Anthropic [`ModelProvider`] for the core [`ModelRegistry`].
//!
//! Mirrors `AnthropicModelProvider`: claims `anthropic:<model>` ids (Java's
//! `MODEL_ID` pattern is `anthropic:.+`, so bare model ids are **not** claimed),
//! and resolves credentials from the creation context, the `authToken` option,
//! then `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN`.

use std::sync::Arc;

use agentscope_core::{Error, Model, ModelCreationContext, ModelProvider, Result};

use crate::anthropic::credential::AnthropicCredential;
use crate::anthropic::model::{default_transport, AnthropicChatModel};

/// Provider id used in `provider:model` ids.
pub const PROVIDER_ID: &str = "anthropic";

/// Context option carrying a bearer token for compatible gateways.
pub const OPTION_AUTH_TOKEN: &str = "authToken";

/// Provider plugin that builds [`AnthropicChatModel`] instances.
#[derive(Debug, Default, Clone, Copy)]
pub struct AnthropicModelProvider;

impl AnthropicModelProvider {
    /// Creates the provider.
    pub fn new() -> Self {
        Self
    }

    /// Strips an optional `anthropic:` prefix, returning the bare model id.
    pub fn strip_prefix(model: &str) -> &str {
        let trimmed = model.trim();
        trimmed.strip_prefix("anthropic:").map(str::trim).unwrap_or(trimmed)
    }
}

impl ModelProvider for AnthropicModelProvider {
    fn provider_id(&self) -> &str {
        PROVIDER_ID
    }

    /// Claims the explicit `anthropic:<model>` form **and** the bare `claude*`
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
        if let Some(rest) = trimmed.strip_prefix("anthropic:") {
            return !rest.trim().is_empty();
        }
        trimmed.starts_with("claude")
    }

    fn create(&self, ctx: ModelCreationContext) -> Result<Arc<dyn Model>> {
        let model_name = Self::strip_prefix(&ctx.model);
        if model_name.is_empty() || !self.supports(&ctx.model) {
            return Err(Error::Model(format!(
                "Unsupported Anthropic model id: {}",
                ctx.model
            )));
        }

        let auth_token = ctx
            .options
            .additional_body_params
            .get(OPTION_AUTH_TOKEN)
            .and_then(|v| v.as_str());
        let (auth_mode, secret) =
            AnthropicCredential::resolve(ctx.api_key.as_deref(), auth_token).ok_or_else(|| {
                Error::Model(format!(
                    "ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN is required to create Anthropic model: {}",
                    ctx.model
                ))
            })?;

        let context_window_size = ctx
            .options
            .additional_body_params
            .get("contextWindowSize")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let cache_ttl = ctx
            .options
            .additional_body_params
            .get("cacheTtl")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let mut model = AnthropicChatModel::new(
            model_name,
            auth_mode,
            secret,
            ctx.base_url.clone(),
            default_transport(),
        )
        .with_default_options(ctx.options.clone())
        .with_stream(ctx.stream.unwrap_or(true))
        .with_context_window_size(context_window_size);
        if let Some(ttl) = cache_ttl {
            model = model.with_cache_ttl(ttl);
        }
        Ok(Arc::new(model))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::ModelRegistry;

    #[test]
    fn provider_id_and_supports() {
        let provider = AnthropicModelProvider::new();
        assert_eq!(provider.provider_id(), "anthropic");
        assert!(provider.supports("anthropic:claude-sonnet-4-5-20250929"));
        assert!(provider.supports("  anthropic:claude-3-5-haiku-latest  "));
        // Bare ids are claimed so core's registry can resolve `anthropic:<model>`.
        assert!(provider.supports("claude-3-5-haiku-latest"));
        assert!(!provider.supports("anthropic:"));
        assert!(!provider.supports("gpt-4o"));
        assert!(!provider.supports(""));
    }

    #[test]
    fn strip_prefix_handles_prefixed_and_bare_ids() {
        assert_eq!(
            AnthropicModelProvider::strip_prefix("anthropic:claude-3-5-haiku-latest"),
            "claude-3-5-haiku-latest"
        );
        assert_eq!(
            AnthropicModelProvider::strip_prefix("claude-3-5-haiku-latest"),
            "claude-3-5-haiku-latest"
        );
        assert_eq!(AnthropicModelProvider::strip_prefix("anthropic:"), "");
    }

    #[test]
    fn create_builds_model_from_api_key() {
        let provider = AnthropicModelProvider::new();
        let ctx = ModelCreationContext::new("anthropic", "anthropic:claude-3-5-haiku-latest")
            .api_key("sk-ant")
            .base_url("https://example.test");
        let model = provider.create(ctx).unwrap();
        assert_eq!(model.model_name(), "claude-3-5-haiku-latest");
    }

    #[test]
    fn create_uses_auth_token_option_when_no_api_key() {
        let provider = AnthropicModelProvider::new();
        let mut options = agentscope_core::GenerateOptions::default();
        options
            .additional_body_params
            .insert("authToken".to_string(), serde_json::json!("tok"));
        let ctx = ModelCreationContext::new("anthropic", "anthropic:claude-3-5-haiku-latest")
            .options(options);
        assert!(provider.create(ctx).is_ok());
    }

    #[test]
    fn create_rejects_unsupported_ids() {
        let provider = AnthropicModelProvider::new();
        let ctx = ModelCreationContext::new("anthropic", "gpt-4o").api_key("k");
        assert!(provider.create(ctx).is_err());
    }

    #[test]
    fn registers_and_resolves_in_core_registry() {
        let mut registry = ModelRegistry::new();
        registry.register(Arc::new(AnthropicModelProvider::new()));
        assert_eq!(registry.len(), 1);
        assert!(registry.can_resolve("anthropic:claude-3-5-haiku-latest"));
        assert!(registry.can_resolve("claude-3-5-haiku-latest"));
        assert!(!registry.can_resolve("anthropic:"));
        assert!(!registry.can_resolve("gpt-4o"));
    }

    #[test]
    fn context_window_size_option_is_applied() {
        let provider = AnthropicModelProvider::new();
        let mut options = agentscope_core::GenerateOptions::default();
        options
            .additional_body_params
            .insert("contextWindowSize".to_string(), serde_json::json!(200_000));
        let ctx = ModelCreationContext::new("anthropic", "anthropic:claude-3-5-haiku-latest")
            .api_key("k")
            .options(options);
        let model = provider.create(ctx).unwrap();
        assert_eq!(model.context_window_size(), 200_000);
    }

    #[test]
    fn missing_credential_is_rejected() {
        // Only meaningful when no ANTHROPIC_* env var is set in the test env.
        if std::env::var("ANTHROPIC_API_KEY").is_ok()
            || std::env::var("ANTHROPIC_AUTH_TOKEN").is_ok()
        {
            return;
        }
        let provider = AnthropicModelProvider::new();
        let ctx = ModelCreationContext::new("anthropic", "anthropic:claude-3-5-haiku-latest");
        assert!(provider.create(ctx).is_err());
    }
}
