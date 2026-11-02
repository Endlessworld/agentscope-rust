//! [`ModelProvider`] registration for the `openai-official` provider.
//!
//! Mirrors `OpenAIOfficialModelProvider`: claims `openai-official:<model>` ids
//! (the Java `MODEL_ID` regex is `openai-official:.+`), resolves
//! `OPENAI_API_KEY` / `OPENAI_BASE_URL` fallbacks, and applies the builder-level
//! options (`strictTools`, `strictJsonSchema`, `contextWindowSize`, formatter and
//! server tools).

use std::sync::Arc;

use agentscope_core::error::{Error, Result};
use agentscope_core::model::{
    GenerateOptions, Model, ModelCreationContext, ModelProvider, ModelRegistry,
};
use serde_json::Value;

use super::constants::{BASE_URL_ENV, DEFAULT_BASE_URL, MODEL_ID_PREFIX, PROVIDER_ID};
use super::credential::OpenAIOfficialCredential;
use super::formatter::ResponsesMultiAgentFormatter;
use super::model::{default_transport, OpenAIResponsesChatModel};
use super::server_tool::OpenAIServerTool;
use crate::common::SharedTransport;

/// Provider plugin that builds [`OpenAIResponsesChatModel`] instances.
#[derive(Debug, Default, Clone, Copy)]
pub struct OpenAIOfficialModelProvider;

impl OpenAIOfficialModelProvider {
    /// Creates the provider.
    pub fn new() -> Self {
        Self
    }

    /// Strips an optional `openai-official:` prefix.
    pub fn strip_prefix(model: &str) -> &str {
        let trimmed = model.trim();
        trimmed
            .strip_prefix(MODEL_ID_PREFIX)
            .map(str::trim)
            .unwrap_or(trimmed)
    }

    /// Whether a bare model id belongs to the OpenAI official family.
    ///
    /// Required because [`ModelRegistry`] splits `openai-official:<model>` and
    /// calls `supports(<model>)` with the prefix already removed.
    fn is_openai_family(model: &str) -> bool {
        let lower = model.trim().to_ascii_lowercase();
        (lower.starts_with("gpt-") && lower.len() > "gpt-".len())
            || (lower.starts_with("chatgpt") && lower.len() > "chatgpt".len())
            || ["o1", "o3", "o4"]
                .iter()
                .any(|prefix| lower.starts_with(prefix) && lower.len() > prefix.len())
            || (lower.starts_with("codex") && lower.len() > "codex".len())
    }
}

impl ModelProvider for OpenAIOfficialModelProvider {
    fn provider_id(&self) -> &str {
        PROVIDER_ID
    }

    fn supports(&self, model: &str) -> bool {
        let trimmed = model.trim();
        if trimmed.is_empty() {
            return false;
        }
        if let Some(rest) = trimmed.strip_prefix(MODEL_ID_PREFIX) {
            return !rest.trim().is_empty();
        }
        Self::is_openai_family(trimmed)
    }

    fn create(&self, ctx: ModelCreationContext) -> Result<Arc<dyn Model>> {
        if !self.supports(&ctx.model) {
            return Err(Error::Model(format!(
                "Unsupported OpenAI model id: {}",
                ctx.model
            )));
        }
        let model = self.create_with_transport(ctx, default_transport())?;
        Ok(Arc::new(model))
    }
}

impl OpenAIOfficialModelProvider {
    /// Builds the concrete [`OpenAIResponsesChatModel`] for `ctx` on top of
    /// `transport`.
    ///
    /// Unlike [`ModelProvider::create`] this does **not** re-check the model-id
    /// claim: it is the construction entry used by hosts that dispatch on the
    /// wire protocol (see [`crate::assembly`]), e.g. an OpenAI-compatible
    /// gateway serving `/responses` under a non-`gpt*` model id.
    pub fn create_with_transport(
        &self,
        ctx: ModelCreationContext,
        transport: SharedTransport,
    ) -> Result<OpenAIResponsesChatModel> {
        let model_name = Self::strip_prefix(&ctx.model).to_string();
        if model_name.is_empty() {
            return Err(Error::Model(format!(
                "Unsupported OpenAI model id: {}",
                ctx.model
            )));
        }

        let api_key = OpenAIOfficialCredential::resolve_api_key(ctx.api_key.as_deref())
            .ok_or_else(|| {
                Error::Model(format!(
                    "Environment variable OPENAI_API_KEY is required to auto-create model: {}",
                    ctx.model
                ))
            })?;
        let base_url = OpenAIOfficialCredential::resolve_base_url(ctx.base_url.as_deref())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        let mut options = ctx.options.clone();
        let strict_tools = take_bool(&mut options, "strictTools");
        let strict_json_schema = take_bool(&mut options, "strictJsonSchema");
        let context_window_size = take_u64(&mut options, "contextWindowSize").map(|v| v as usize);
        let formatter = take_formatter(&mut options);
        let server_tools = take_server_tools(&mut options)?;
        let proxy = take_proxy(&mut options)?;

        let mut model =
            OpenAIResponsesChatModel::new(model_name, api_key, Some(base_url), transport)
                .with_default_options(options)
                .with_stream(ctx.stream.unwrap_or(true))
                .with_strict_tools(strict_tools)
                .with_strict_json_schema(strict_json_schema)
                .with_server_tools(server_tools);
        if let Some(formatter) = formatter {
            model = model.with_formatter(formatter);
        }
        if let Some(proxy) = proxy {
            model = model.with_proxy_config(proxy)?;
        }
        if let Some(size) = context_window_size {
            model = model.with_context_window_size(size);
        }
        Ok(model)
    }
}

/// Builds a registry pre-populated with the `openai-official` provider.
pub fn default_openai_official_registry() -> ModelRegistry {
    let mut registry = ModelRegistry::new();
    registry.register(Arc::new(OpenAIOfficialModelProvider::new()));
    registry
}

fn take_option(options: &mut GenerateOptions, key: &str) -> Option<Value> {
    options.additional_body_params.remove(key)
}

fn take_bool(options: &mut GenerateOptions, key: &str) -> Option<bool> {
    take_option(options, key).and_then(|value| match value {
        Value::Bool(value) => Some(value),
        Value::String(value) if value.eq_ignore_ascii_case("true") => Some(true),
        Value::String(value) if value.eq_ignore_ascii_case("false") => Some(false),
        _ => None,
    })
}

fn take_u64(options: &mut GenerateOptions, key: &str) -> Option<u64> {
    take_option(options, key).and_then(|value| {
        value
            .as_u64()
            .or_else(|| value.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
    })
}

fn take_formatter(options: &mut GenerateOptions) -> Option<ResponsesMultiAgentFormatter> {
    match take_option(options, "multiAgentFormatter")? {
        Value::Bool(true) => Some(ResponsesMultiAgentFormatter::new()),
        Value::Bool(false) => None,
        Value::String(prompt) => Some(ResponsesMultiAgentFormatter::with_prompt(prompt)),
        _ => None,
    }
}

fn take_server_tools(options: &mut GenerateOptions) -> Result<Vec<OpenAIServerTool>> {
    let Some(value) = take_option(options, "serverTools") else {
        return Ok(Vec::new());
    };
    let array = value.as_array().ok_or_else(|| {
        Error::Model("serverTools must be a JSON array of server tool definitions".to_string())
    })?;
    array
        .iter()
        .cloned()
        .map(|tool| OpenAIServerTool::from_json(tool).map_err(Error::Model))
        .collect()
}

/// Reads the `proxy` option object (`{type, host, port, username, password,
/// nonProxyHosts}`) and turns it into a [`super::proxy::ProxyConfig`].
fn take_proxy(options: &mut GenerateOptions) -> Result<Option<super::proxy::ProxyConfig>> {
    let Some(value) = take_option(options, "proxy") else {
        return Ok(None);
    };
    super::proxy::ProxyConfig::from_json(&value).map(Some)
}

// Keep the env name referenced by public docs/tests.
#[allow(dead_code)]
const _BASE_URL_ENV: &str = BASE_URL_ENV;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_id_and_prefix_handling() {
        let provider = OpenAIOfficialModelProvider::new();
        assert_eq!(provider.provider_id(), "openai-official");
        assert!(provider.supports("openai-official:gpt-5"));
        assert!(provider.supports("  openai-official:o3  "));
        assert!(provider.supports("gpt-4o"));
        assert!(provider.supports("o3-mini"));
        assert!(!provider.supports("openai-official:"));
        assert!(!provider.supports("gpt-"));
        assert!(!provider.supports("claude-3"));
        assert!(!provider.supports(""));
        assert_eq!(
            OpenAIOfficialModelProvider::strip_prefix("openai-official:gpt-5"),
            "gpt-5"
        );
        assert_eq!(OpenAIOfficialModelProvider::strip_prefix("gpt-5"), "gpt-5");
    }

    #[test]
    fn create_builds_model_from_explicit_credentials() {
        let provider = OpenAIOfficialModelProvider::new();
        let ctx = ModelCreationContext::new("openai-official", "openai-official:gpt-5")
            .api_key("sk-key")
            .base_url("https://example.test/v1");
        let model = provider.create(ctx).unwrap();
        assert_eq!(model.model_name(), "gpt-5");
    }

    #[test]
    fn create_rejects_unsupported_ids() {
        let provider = OpenAIOfficialModelProvider::new();
        let ctx = ModelCreationContext::new("openai-official", "claude-3").api_key("sk");
        assert!(provider.create(ctx).is_err());
    }

    #[test]
    fn create_applies_builder_options_and_strips_them() {
        let provider = OpenAIOfficialModelProvider::new();
        let mut options = GenerateOptions::default();
        options
            .additional_body_params
            .insert("strictTools".to_string(), Value::Bool(true));
        options
            .additional_body_params
            .insert("strictJsonSchema".to_string(), Value::Bool(true));
        options
            .additional_body_params
            .insert("contextWindowSize".to_string(), serde_json::json!(123_456));
        options
            .additional_body_params
            .insert("multiAgentFormatter".to_string(), Value::Bool(true));
        options.additional_body_params.insert(
            "serverTools".to_string(),
            serde_json::json!([{ "type": "web_search" }]),
        );
        let ctx = ModelCreationContext::new("openai-official", "openai-official:gpt-5")
            .api_key("sk")
            .options(options);
        let model = provider.create(ctx).unwrap();
        assert_eq!(model.context_window_size(), 123_456);
        assert!(model.supports_native_structured_output());
    }

    #[test]
    fn proxy_option_is_consumed_and_not_rejected_by_the_whitelist() {
        let provider = OpenAIOfficialModelProvider::new();
        let mut options = GenerateOptions::default();
        options.additional_body_params.insert(
            "proxy".to_string(),
            serde_json::json!({
                "type": "http",
                "host": "127.0.0.1",
                "port": 3128,
                "nonProxyHosts": ["api.openai.com"]
            }),
        );
        let ctx = ModelCreationContext::new("openai-official", "openai-official:gpt-5")
            .api_key("sk")
            .options(options);
        // `proxy` is consumed before the additionalBodyParams whitelist runs, so
        // creation succeeds (a leftover key would be rejected as non-whitelisted).
        assert!(provider.create(ctx).is_ok());
    }

    #[test]
    fn unsupported_proxy_fails_fast_at_creation() {
        let provider = OpenAIOfficialModelProvider::new();
        let mut options = GenerateOptions::default();
        options.additional_body_params.insert(
            "proxy".to_string(),
            serde_json::json!({"type": "socks5", "host": "127.0.0.1", "port": 1080}),
        );
        let ctx = ModelCreationContext::new("openai-official", "openai-official:gpt-5")
            .api_key("sk")
            .options(options);
        let err = match provider.create(ctx) {
            Ok(_) => panic!("SOCKS proxies must fail fast"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("socks"), "{err}");
    }

    #[test]
    fn registers_and_resolves_in_core_registry() {
        let registry = default_openai_official_registry();
        assert_eq!(registry.len(), 1);
        assert!(registry.can_resolve("openai-official:gpt-5"));
        assert!(registry.can_resolve("gpt-5"));
        assert!(!registry.can_resolve("openai-official:"));
        assert!(!registry.can_resolve("claude-3"));
    }

    #[test]
    fn registry_resolution_creates_a_model() {
        let registry = default_openai_official_registry();
        let base =
            ModelCreationContext::new("openai-official", "openai-official:gpt-5").api_key("sk");
        let model = registry
            .resolve_with("openai-official:gpt-5", base)
            .unwrap();
        assert_eq!(model.model_name(), "gpt-5");
    }

    #[test]
    fn missing_api_key_is_rejected() {
        // Only meaningful when OPENAI_API_KEY is unset in the test environment.
        if crate::openai_official::credential::OpenAIOfficialCredential::resolve_api_key(None)
            .is_some()
        {
            return;
        }
        let provider = OpenAIOfficialModelProvider::new();
        let ctx = ModelCreationContext::new("openai-official", "openai-official:gpt-5");
        let error = match provider.create(ctx) {
            Ok(_) => panic!("missing API key must be rejected"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("OPENAI_API_KEY"), "{error}");
    }
}
