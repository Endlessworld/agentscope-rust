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
use crate::common::SharedTransport;

/// Provider id used in `provider:model` ids.
pub const PROVIDER_ID: &str = "anthropic";

/// Environment variable consulted when no explicit base URL is supplied.
pub const BASE_URL_ENV: &str = "ANTHROPIC_BASE_URL";

/// Context option carrying a bearer token for compatible gateways.
pub const OPTION_AUTH_TOKEN: &str = "authToken";

/// Context option overriding the reported context window size.
pub const OPTION_CONTEXT_WINDOW_SIZE: &str = "contextWindowSize";

/// Context option carrying the prompt-cache TTL (e.g. `"1h"`).
pub const OPTION_CACHE_TTL: &str = "cacheTtl";

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
        trimmed
            .strip_prefix("anthropic:")
            .map(str::trim)
            .unwrap_or(trimmed)
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
        let model = self.create_with_transport(ctx, default_transport())?;
        Ok(Arc::new(model))
    }
}

impl AnthropicModelProvider {
    /// Builds the concrete [`AnthropicChatModel`] for `ctx` on top of `transport`.
    ///
    /// Unlike [`ModelProvider::create`] this does **not** re-check the model-id
    /// claim: it is the construction entry used by hosts that dispatch on the
    /// wire protocol (see [`crate::assembly`]), where the protocol — not the
    /// model id — already selected this implementation. That matters for
    /// Anthropic-compatible gateways whose model ids are not `claude*`.
    pub fn create_with_transport(
        &self,
        ctx: ModelCreationContext,
        transport: SharedTransport,
    ) -> Result<AnthropicChatModel> {
        let model_name = Self::strip_prefix(&ctx.model).to_string();
        if model_name.is_empty() {
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
            .get(OPTION_CONTEXT_WINDOW_SIZE)
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let cache_ttl = ctx
            .options
            .additional_body_params
            .get(OPTION_CACHE_TTL)
            .and_then(|v| v.as_str())
            .map(str::to_string);
        // Provider knobs are consumed here and must not be replayed as wire
        // fields: Anthropic rejects unknown top-level body keys with HTTP 400
        // (MODEL-09).
        let mut default_options = ctx.options.clone();
        for key in [
            OPTION_AUTH_TOKEN,
            OPTION_CACHE_TTL,
            OPTION_CONTEXT_WINDOW_SIZE,
        ] {
            default_options.additional_body_params.remove(key);
        }
        // Formatter selector (mirrors Java's `AnthropicModelProvider` picking an
        // `AnthropicBaseFormatter` component). Consumed here so it never reaches
        // the request body.
        let multi_agent_prompt = crate::common::take_multi_agent_prompt(&mut default_options);

        let base_url = crate::common::resolve_base_url(
            ctx.base_url.clone(),
            BASE_URL_ENV,
            crate::anthropic::credential::DEFAULT_BASE_URL,
        );
        let mut model =
            AnthropicChatModel::new(model_name, auth_mode, secret, Some(base_url), transport)
                .with_default_options(default_options)
                .with_stream(ctx.stream.unwrap_or(true))
                .with_context_window_size(context_window_size)
                .with_multi_agent_formatter(multi_agent_prompt);
        if let Some(ttl) = cache_ttl {
            model = model.with_cache_ttl(ttl);
        }
        Ok(model)
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
    fn multi_agent_formatter_option_is_consumed_by_the_provider() {
        let provider = AnthropicModelProvider::new();
        let mut options = agentscope_core::GenerateOptions::default();
        options
            .additional_body_params
            .insert("multiAgentFormatter".to_string(), serde_json::json!(true));
        let ctx = ModelCreationContext::new("anthropic", "anthropic:claude-3-5-haiku-latest")
            .api_key("k")
            .options(options);
        let model = provider
            .create_with_transport(ctx, Arc::new(crate::common::MemoryTransport::default()))
            .unwrap();
        assert!(model.is_multi_agent());
        // The selector is a provider knob and must never reach the wire.
        let spec = model
            .build_http_request(
                vec![agentscope_core::Msg::user("hi")],
                Vec::new(),
                &agentscope_core::GenerateOptions::default(),
                false,
            )
            .unwrap();
        assert!(spec.body.get("multiAgentFormatter").is_none());
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
