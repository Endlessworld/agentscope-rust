//! A model wrapper that exposes catalog-derived capabilities without changing the
//! concrete adapter.
//!
//! Adapters keep their own protocol/client implementation; the wrapper only overrides
//! window/output/capability accessors and keeps the descriptor available to callers.

use std::sync::Arc;

use futures::stream::BoxStream;

use crate::message::Msg;
use crate::model::catalog::{ModelCatalog, ResolveRequest};
use crate::model::{ChatResponse, GenerateOptions, Model, ModelDescriptor, ToolSchema};

/// Model wrapper carrying a resolved descriptor.
pub struct CatalogAwareModel {
    inner: Arc<dyn Model>,
    descriptor: Arc<ModelDescriptor>,
}

impl CatalogAwareModel {
    /// Wraps a model when a descriptor is available; otherwise the original model is
    /// returned unchanged.
    pub fn wrap(
        inner: Arc<dyn Model>,
        provider_id: Option<&str>,
        model_id: &str,
        base_url: Option<&str>,
    ) -> Arc<dyn Model> {
        match descriptor_for(provider_id, model_id, base_url) {
            Some(descriptor) => Self::from_descriptor(inner, descriptor),
            None => inner,
        }
    }

    /// Wraps with an already-resolved descriptor.
    pub fn from_descriptor(
        inner: Arc<dyn Model>,
        descriptor: Arc<ModelDescriptor>,
    ) -> Arc<dyn Model> {
        if inner.descriptor().is_some() {
            return inner;
        }
        Arc::new(Self { inner, descriptor })
    }

    /// Descriptor used by this wrapper.
    pub fn descriptor(&self) -> &Arc<ModelDescriptor> {
        &self.descriptor
    }
}

#[async_trait::async_trait]
impl Model for CatalogAwareModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: GenerateOptions,
    ) -> BoxStream<'_, crate::Result<ChatResponse>> {
        self.inner.stream(messages, tools, options)
    }

    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    fn supports_native_structured_output(&self) -> bool {
        self.descriptor
            .structured_output
            .unwrap_or_else(|| self.inner.supports_native_structured_output())
    }

    fn supports_native_structured_output_with_tools(&self) -> bool {
        self.descriptor
            .structured_output
            .unwrap_or_else(|| self.inner.supports_native_structured_output_with_tools())
    }

    fn supports_tool_choice_specific(&self) -> bool {
        self.inner.supports_tool_choice_specific()
    }

    fn context_window_size(&self) -> usize {
        if self.descriptor.context_window > 0 {
            self.descriptor.context_window
        } else {
            self.inner.context_window_size()
        }
    }

    fn max_output_tokens(&self) -> usize {
        if self.descriptor.max_output > 0 {
            self.descriptor.max_output
        } else {
            self.inner.max_output_tokens()
        }
    }

    fn descriptor(&self) -> Option<&ModelDescriptor> {
        Some(&self.descriptor)
    }
}

/// Resolves a descriptor through the process-wide catalog.
///
/// Returns `None` when the catalog is disabled/empty or the id cannot be resolved;
/// callers must then keep their existing table fallback.
pub fn descriptor_for(
    provider_id: Option<&str>,
    model_id: &str,
    base_url: Option<&str>,
) -> Option<Arc<ModelDescriptor>> {
    let catalog = ModelCatalog::global();
    let local_models = [model_id.to_string()];
    catalog
        .resolve(&ResolveRequest {
            provider_key: provider_id,
            model_id,
            base_url,
            local_models: &local_models,
        })
        .descriptor
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::catalog::ModelStatus;
    use futures::StreamExt;
    use std::collections::BTreeSet;

    struct FakeModel;

    #[async_trait::async_trait]
    impl Model for FakeModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<ToolSchema>,
            _options: GenerateOptions,
        ) -> BoxStream<'_, crate::Result<ChatResponse>> {
            futures::stream::empty().boxed()
        }

        fn model_name(&self) -> &str {
            "fake"
        }

        fn context_window_size(&self) -> usize {
            1_000
        }
    }

    fn descriptor() -> Arc<ModelDescriptor> {
        Arc::new(ModelDescriptor {
            key: "p/m".into(),
            provider: "p".into(),
            id: "m".into(),
            name: "M".into(),
            description: None,
            family: None,
            kind: crate::model::catalog::ModelKind::Chat,
            context_window: 1_000_000,
            max_output: 384_000,
            input_modalities: BTreeSet::from(["text".into()]),
            output_modalities: BTreeSet::from(["text".into()]),
            attachment: false,
            reasoning: true,
            reasoning_options: Vec::new(),
            interleaved_field: None,
            tool_call: true,
            structured_output: None,
            temperature: true,
            status: ModelStatus::Active,
            open_weights: false,
            cost: None,
            canonical: None,
            use_cases: BTreeSet::new(),
            fingerprint: 0,
        })
    }

    #[test]
    fn wrapper_overrides_window_and_exposes_descriptor() {
        let wrapped = CatalogAwareModel::from_descriptor(Arc::new(FakeModel), descriptor());
        assert_eq!(wrapped.context_window_size(), 1_000_000);
        assert_eq!(wrapped.max_output_tokens(), 384_000);
        assert_eq!(wrapped.descriptor().unwrap().key, "p/m");
    }
}
