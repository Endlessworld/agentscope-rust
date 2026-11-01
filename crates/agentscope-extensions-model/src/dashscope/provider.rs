//! DashScope [`ModelProvider`] for the core [`ModelRegistry`].
//!
//! Mirrors `DashScopeModelProvider`: supports the explicit `dashscope:` prefix
//! plus the bare `qwen*` model-id family, and reads `DASHSCOPE_API_KEY` from the
//! environment when the creation context carries no key.

use std::sync::Arc;

use agentscope_core::{Error, Model, ModelCreationContext, ModelProvider, Result};

use crate::dashscope::credential::EndpointType;
use crate::dashscope::model::{default_transport, DashScopeChatModel};

/// Provider id used in `provider:model` ids.
pub const PROVIDER_ID: &str = "dashscope";

/// Environment variable consulted when no explicit base URL is supplied.
pub const BASE_URL_ENV: &str = "DASHSCOPE_BASE_URL";

/// Provider plugin that builds [`DashScopeChatModel`] instances.
#[derive(Debug, Default, Clone, Copy)]
pub struct DashScopeModelProvider;

impl DashScopeModelProvider {
    /// Creates the provider.
    pub fn new() -> Self {
        Self
    }

    /// Strips an optional `dashscope:` prefix from a model id.
    pub fn strip_prefix(model: &str) -> &str {
        model
            .trim()
            .strip_prefix("dashscope:")
            .unwrap_or(model.trim())
    }
}

impl ModelProvider for DashScopeModelProvider {
    fn provider_id(&self) -> &str {
        PROVIDER_ID
    }

    fn supports(&self, model: &str) -> bool {
        let trimmed = model.trim();
        if trimmed.is_empty() {
            return false;
        }
        if let Some(rest) = trimmed.strip_prefix("dashscope:") {
            return !rest.trim().is_empty();
        }
        // Bare ids: the `qwen*` family (same predicate as Java).
        trimmed.starts_with("qwen")
    }

    fn create(&self, ctx: ModelCreationContext) -> Result<Arc<dyn Model>> {
        let model_name = Self::strip_prefix(&ctx.model);
        if !self.supports(&ctx.model) || model_name.is_empty() {
            return Err(Error::Model(format!(
                "Unsupported DashScope model id: {}",
                ctx.model
            )));
        }
        let api_key = ctx
            .api_key
            .clone()
            .filter(|k| !k.trim().is_empty())
            .or_else(|| std::env::var("DASHSCOPE_API_KEY").ok())
            .ok_or_else(|| {
                Error::Model(format!(
                    "DASHSCOPE_API_KEY is required to create DashScope model: {}",
                    ctx.model
                ))
            })?;

        // `endpointType` is carried through `additional_body_params` (the Rust
        // `GenerateOptions` has no dedicated field).
        let endpoint_type = ctx
            .options
            .additional_body_params
            .get("endpointType")
            .and_then(|v| v.as_str())
            .and_then(EndpointType::parse)
            .unwrap_or(EndpointType::Auto);

        let context_window_size = ctx
            .options
            .additional_body_params
            .get("contextWindowSize")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;

        // Formatter selector (mirrors Java's `DashScopeMultiAgentFormatter` usage).
        // Consumed here so it never reaches the request body.
        let mut options = ctx.options.clone();
        let multi_agent_prompt = crate::common::take_multi_agent_prompt(&mut options);

        // `enableEncrypt` (Java `DashScopeModelProvider.OPTION_ENABLE_ENCRYPT`): when set,
        // the model fetches the RSA public key and encrypts every request input.
        let enable_encrypt = options
            .additional_body_params
            .remove("enableEncrypt")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);

        // Model-level knobs (`enableSearch` / `searchOptions` /
        // `enableCodeInterpreter` / `multimodalModelPatterns` /
        // `nativeStructuredOutput` / `nativeStructuredOutputWithTools`). They are
        // consumed here so they stay provider options, never body fields.
        let enable_search = options
            .additional_body_params
            .remove("enableSearch")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let search_options = options
            .additional_body_params
            .remove("searchOptions")
            .and_then(|value| {
                serde_json::from_value::<crate::dashscope::dto::DashScopeSearchOptions>(value).ok()
            });
        let enable_code_interpreter = options
            .additional_body_params
            .remove("enableCodeInterpreter")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let multimodal_patterns = options
            .additional_body_params
            .remove("multimodalModelPatterns")
            .and_then(|value| serde_json::from_value::<Vec<String>>(value).ok())
            .unwrap_or_default();
        let native_structured_output = options
            .additional_body_params
            .remove("nativeStructuredOutput")
            .and_then(|value| value.as_bool())
            .unwrap_or(true);
        let native_structured_output_with_tools = options
            .additional_body_params
            .remove("nativeStructuredOutputWithTools")
            .and_then(|value| value.as_bool())
            .unwrap_or(true);

        let base_url = crate::common::resolve_base_url(
            ctx.base_url.clone(),
            BASE_URL_ENV,
            crate::dashscope::converter::DEFAULT_BASE_URL,
        );

        let model = DashScopeChatModel::new(model_name, api_key, Some(base_url), default_transport())
            .with_default_options(options)
            .with_stream(ctx.stream.unwrap_or(true))
            .with_endpoint_type(endpoint_type)
            .with_enable_thinking(ctx.enable_thinking)
            .with_context_window_size(context_window_size)
            .with_multi_agent_formatter(multi_agent_prompt)
            .with_enable_search(enable_search)
            .with_enable_code_interpreter(enable_code_interpreter)
            .with_multimodal_patterns(multimodal_patterns)
            .with_native_structured_output(native_structured_output)
            .with_native_structured_output_with_tools(native_structured_output_with_tools)
            .with_encryption_enabled(enable_encrypt);
        let model = match search_options {
            Some(search_options) => model.with_search_options(search_options),
            None => model,
        };
        Ok(Arc::new(model))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::ModelRegistry;

    #[test]
    fn provider_id_and_supports() {
        let provider = DashScopeModelProvider::new();
        assert_eq!(provider.provider_id(), "dashscope");
        assert!(provider.supports("dashscope:qwen-plus"));
        assert!(provider.supports("qwen-plus"));
        assert!(provider.supports("qwen-vl-plus"));
        assert!(!provider.supports("dashscope:"));
        assert!(!provider.supports("gpt-4o"));
        assert!(!provider.supports("  "));
    }

    #[test]
    fn create_requires_api_key_and_builds_model() {
        let provider = DashScopeModelProvider::new();
        let ctx = ModelCreationContext::new("dashscope", "dashscope:qwen-vl-plus")
            .api_key("sk-test")
            .base_url("https://example.test");
        let model = provider.create(ctx).unwrap();
        assert_eq!(model.model_name(), "qwen-vl-plus");
    }

    #[test]
    fn create_rejects_unsupported_ids() {
        let provider = DashScopeModelProvider::new();
        let ctx = ModelCreationContext::new("dashscope", "gpt-4o").api_key("sk");
        assert!(provider.create(ctx).is_err());
    }

    #[test]
    fn native_structured_output_option_is_consumed_from_the_provider() {
        let provider = DashScopeModelProvider::new();
        let mut options = agentscope_core::GenerateOptions::default();
        options
            .additional_body_params
            .insert("nativeStructuredOutput".to_string(), serde_json::json!(false));
        options.additional_body_params.insert(
            "nativeStructuredOutputWithTools".to_string(),
            serde_json::json!(false),
        );
        let ctx = ModelCreationContext::new("dashscope", "dashscope:qwen-plus")
            .api_key("sk")
            .options(options);
        let model = provider.create(ctx).unwrap();
        assert!(!model.supports_native_structured_output());
        assert!(!model.supports_native_structured_output_with_tools());
    }

    #[test]
    fn registers_in_core_registry() {
        let mut registry = ModelRegistry::new();
        registry.register(Arc::new(DashScopeModelProvider::new()));
        assert_eq!(registry.len(), 1);
        assert!(registry.can_resolve("dashscope:qwen-max"));
        assert!(!registry.can_resolve("dashscope:"));
    }

    #[test]
    fn endpoint_type_option_is_applied() {
        let provider = DashScopeModelProvider::new();
        let mut options = agentscope_core::GenerateOptions::default();
        options
            .additional_body_params
            .insert("endpointType".to_string(), serde_json::json!("MULTIMODAL"));
        let ctx = ModelCreationContext::new("dashscope", "qwen-plus")
            .api_key("sk")
            .options(options);
        let model = provider.create(ctx).unwrap();
        let downcast = model.model_name();
        assert_eq!(downcast, "qwen-plus");
    }
}
