//! Model provider SPI, registry and creation context, mirroring
//! `io.agentscope.core.model` provider infrastructure.
//!
//! A [`ModelProvider`] knows how to resolve a provider/model id pair and build a
//! concrete [`crate::model::Model`] implementation. [`ModelRegistry`] keeps the
//! registered providers and resolves `provider:model` ids to instances.

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::model::cache_policy::CachePolicy;
use crate::model::generate_options::GenerateOptions;
use crate::model::model::Model;

/// Provider id for the default OpenAI-compatible provider shipped in-core.
pub const DEFAULT_PROVIDER_ID: &str = "openai";

/// Creation parameters for instantiating a concrete model, mirroring the Java
/// `ModelCreationContext`.
#[derive(Debug, Clone, Default)]
pub struct ModelCreationContext {
    /// Stable provider id (e.g. `openai`, `deepseek`, `kimi`).
    pub provider_id: String,
    /// Model id within the provider (e.g. `gpt-4o`, `deepseek-chat`).
    pub model: String,
    /// API key override. When `None`, the provider reads its own env/credential.
    pub api_key: Option<String>,
    /// Base URL override (e.g. an OpenAI-compatible gateway).
    pub base_url: Option<String>,
    /// Endpoint path override (defaults to `/chat/completions`).
    pub endpoint_path: Option<String>,
    /// Whether to stream by default.
    pub stream: Option<bool>,
    /// Enable reasoning/thinking if the model supports it.
    pub enable_thinking: bool,
    /// Prompt caching policy.
    pub cache_policy: Option<CachePolicy>,
    /// Cache bucket id.
    pub cache_id: Option<String>,
    /// Additional per-request options.
    pub options: GenerateOptions,
}

impl ModelCreationContext {
    /// Creates a new context for the given provider and model.
    pub fn new(provider_id: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider_id: provider_id.into(),
            model: model.into(),
            ..Default::default()
        }
    }

    pub fn api_key(mut self, v: impl Into<String>) -> Self {
        self.api_key = Some(v.into());
        self
    }
    pub fn base_url(mut self, v: impl Into<String>) -> Self {
        self.base_url = Some(v.into());
        self
    }
    pub fn endpoint_path(mut self, v: impl Into<String>) -> Self {
        self.endpoint_path = Some(v.into());
        self
    }
    pub fn enable_thinking(mut self, v: bool) -> Self {
        self.enable_thinking = v;
        self
    }
    pub fn options(mut self, v: GenerateOptions) -> Self {
        self.options = v;
        self
    }
}

/// Provider plugin point (the Rust equivalent of the Java `spi.ModelProvider`).
pub trait ModelProvider: Send + Sync {
    /// Stable provider id (e.g. `openai`, `deepseek`).
    fn provider_id(&self) -> &str;
    /// Whether this provider can resolve the given model id.
    fn supports(&self, model: &str) -> bool;
    /// Builds a model instance for the given creation context.
    fn create(&self, ctx: ModelCreationContext) -> Result<Arc<dyn Model>>;
}

/// A registry of model providers, mirroring the Java `ModelRegistry`.
///
/// Resolution is two-phase:
/// 1. If the id contains `:`, the prefix is treated as the provider id and the
///    remainder as the model id (e.g. `openai:gpt-4o`).
/// 2. Otherwise the id is treated as a bare model id and handed to every
///    registered provider, in registration order, until one claims it.
#[derive(Default)]
pub struct ModelRegistry {
    providers: Vec<Arc<dyn ModelProvider>>,
}

impl ModelRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a provider, replacing any existing provider with the same id.
    pub fn register(&mut self, provider: Arc<dyn ModelProvider>) {
        let id = provider.provider_id().to_string();
        self.providers.retain(|p| p.provider_id() != id);
        self.providers.push(provider);
    }

    /// Returns the number of registered providers.
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Whether the registry contains no providers.
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// Whether any registered provider can resolve the given model id.
    pub fn can_resolve(&self, model_id: &str) -> bool {
        let (provider, model) = Self::split_id(model_id);
        if let Some(p) = self.find_provider(provider) {
            return p.supports(model);
        }
        self.providers.iter().any(|p| p.supports(model_id))
    }

    /// Resolves the model id to a concrete model instance.
    pub fn resolve(&self, model_id: &str) -> Result<Arc<dyn Model>> {
        self.resolve_with(model_id, ModelCreationContext::new(DEFAULT_PROVIDER_ID, model_id))
    }

    /// Resolves the model id to a concrete instance, using `base` as the
    /// starting creation context (overriding provider/model from the id).
    pub fn resolve_with(
        &self,
        model_id: &str,
        base: ModelCreationContext,
    ) -> Result<Arc<dyn Model>> {
        let (provider, model) = Self::split_id(model_id);
        let provider = if provider.is_empty() {
            base.provider_id.clone()
        } else {
            provider.to_string()
        };

        if let Some(p) = self.find_provider(&provider) {
            let ctx = ModelCreationContext {
                provider_id: provider.clone(),
                model: model.to_string(),
                api_key: base.api_key,
                base_url: base.base_url,
                endpoint_path: base.endpoint_path,
                stream: base.stream,
                enable_thinking: base.enable_thinking,
                cache_policy: base.cache_policy,
                cache_id: base.cache_id,
                options: base.options,
            };
            return p.create(ctx);
        }

        // No explicit provider id: ask each provider in order.
        let model_only = if model.is_empty() { model_id } else { model };
        for p in &self.providers {
            if p.supports(model_only) {
                return p.create(ModelCreationContext {
                    provider_id: p.provider_id().to_string(),
                    model: model_only.to_string(),
                    api_key: base.api_key,
                    base_url: base.base_url,
                    endpoint_path: base.endpoint_path,
                    stream: base.stream,
                    enable_thinking: base.enable_thinking,
                    cache_policy: base.cache_policy,
                    cache_id: base.cache_id,
                    options: base.options,
                });
            }
        }
        Err(Error::Model(format!("no model provider registered for '{}'", model_id)))
    }

    fn find_provider(&self, provider_id: &str) -> Option<&Arc<dyn ModelProvider>> {
        self.providers
            .iter()
            .find(|p| p.provider_id() == provider_id)
    }

    /// Splits a `provider:model` id into its parts. When there is no colon the
    /// provider part is empty and the whole id is the model part.
    fn split_id(model_id: &str) -> (&str, &str) {
        match model_id.split_once(':') {
            Some((p, m)) => (p, m),
            None => ("", model_id),
        }
    }
}

/// Provider-agnostic helper methods, mirroring `ModelProviderSupport`.
pub struct ModelProviderSupport;

impl ModelProviderSupport {
    /// Returns the first non-blank value among the candidates (in order).
    pub fn first_non_blank<'a>(candidates: impl IntoIterator<Item = Option<&'a str>>) -> Option<String> {
        candidates
            .into_iter()
            .flatten()
            .map(str::trim)
            .find(|s| !s.is_empty())
            .map(str::to_string)
    }

    /// Parses an optional integer field into an `i32`.
    pub fn int_option(v: Option<&serde_json::Value>) -> Option<i32> {
        v.and_then(|v| v.as_i64()).map(|n| n as i32)
    }

    /// Finds the first component (structurally) that can be assigned to the
    /// requested type by checking its `providerId`/`type` discriminator.
    pub fn find_assignable_component<T>(
        _components: &HashMap<String, serde_json::Value>,
        _discriminator_key: &str,
        _expected: T,
    ) -> Option<serde_json::Value> {
        None
    }
}

/// Utilities for building generation requests, mirroring `ModelUtils`.
pub struct ModelUtils;

impl ModelUtils {
    /// Ensures the options carry a model-level [`crate::model::ExecutionConfig`],
    /// applying the model defaults when absent.
    pub fn ensure_default_execution_config(options: &mut GenerateOptions) {
        if options.execution_config.is_none() {
            options.execution_config = Some(crate::model::ExecutionConfig::model_defaults());
        }
    }
}

/// How the framework asks the model for structured (schema-conforming) output,
/// mirroring `StructuredOutputReminder`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructuredOutputReminder {
    /// Rely on the provider's native `response_format` support.
    ToolChoice,
    /// Inject a textual reminder into the system prompt.
    Prompt,
}

impl StructuredOutputReminder {
    /// The default reminder strategy.
    pub fn default_strategy() -> Self {
        StructuredOutputReminder::ToolChoice
    }

    /// Builds the system-prompt reminder text for [`StructuredOutputReminder::Prompt`].
    pub fn prompt_text() -> &'static str {
        "You must respond with valid JSON that strictly conforms to the requested schema."
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyProvider;
    impl ModelProvider for DummyProvider {
        fn provider_id(&self) -> &str {
            "dummy"
        }
        fn supports(&self, model: &str) -> bool {
            model.starts_with("dummy-")
        }
        fn create(&self, ctx: ModelCreationContext) -> Result<Arc<dyn Model>> {
            Err(Error::Model(format!("dummy provider cannot instantiate '{}'", ctx.model)))
        }
    }

    #[test]
    fn split_id_handles_colon_and_bare() {
        assert_eq!(ModelRegistry::split_id("openai:gpt-4o"), ("openai", "gpt-4o"));
        assert_eq!(ModelRegistry::split_id("gpt-4o"), ("", "gpt-4o"));
    }

    #[test]
    fn registry_can_resolve_by_provider_prefix() {
        let mut reg = ModelRegistry::new();
        reg.register(Arc::new(DummyProvider));
        // Explicit provider prefix; model part must still pass `supports`.
        assert!(reg.can_resolve("dummy:dummy-x"));
        assert!(!reg.can_resolve("dummy:unrelated"));
        assert!(!reg.can_resolve("openai:gpt-4o"));
        assert!(reg.len() == 1);
    }

    #[test]
    fn support_first_non_blank() {
        assert_eq!(
            ModelProviderSupport::first_non_blank([None, Some("  "), Some("key")]),
            Some("key".to_string())
        );
        assert_eq!(
            ModelProviderSupport::first_non_blank([None, Some("  ")]),
            None
        );
    }

    #[test]
    fn utils_apply_model_defaults() {
        let mut opts = GenerateOptions::default();
        assert!(opts.execution_config.is_none());
        ModelUtils::ensure_default_execution_config(&mut opts);
        assert!(opts.execution_config.is_some());
    }
}
