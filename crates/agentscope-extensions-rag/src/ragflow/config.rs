//! RAGFlow configuration.
//!
//! Ports `io.agentscope.core.rag.integration.ragflow.RAGFlowConfig` and its builder,
//! including the default values and the validation rules enforced by the Java builder
//! (`topK > 0`, thresholds inside `[0, 1]`, `page >= 1`, `maxRetries >= 0`, and
//! "either dataset ids or document ids must be set").

use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;

use crate::error::{RagError, Result};

/// Default `top_k` (Java `DEFAULT_TOP_K`).
///
/// Note: the Javadoc on `getTopK()` says "5" but the actual Java constant is `1024`;
/// the code is the source of truth here.
pub const DEFAULT_TOP_K: i32 = 1024;
/// Default `similarity_threshold` (Java `DEFAULT_SIMILARITY_THRESHOLD`).
pub const DEFAULT_SIMILARITY_THRESHOLD: f64 = 0.2;
/// Default `vector_similarity_weight` (Java `DEFAULT_VECTOR_SIMILARITY_WEIGHT`).
pub const DEFAULT_VECTOR_SIMILARITY_WEIGHT: f64 = 0.3;
/// Default `page` (Java `DEFAULT_PAGE`).
pub const DEFAULT_PAGE: i32 = 1;
/// Default `page_size` (Java `DEFAULT_PAGE_SIZE`).
pub const DEFAULT_PAGE_SIZE: i32 = 30;
/// Default timeout (Java `DEFAULT_TIMEOUT`).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Default retry budget (Java `DEFAULT_MAX_RETRIES`).
pub const DEFAULT_MAX_RETRIES: u32 = 3;

/// RAGFlow client configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct RAGFlowConfig {
    /// API key sent as `Authorization: Bearer <key>`.
    pub api_key: String,
    /// Base URL, e.g. `http://localhost:9380`.
    pub base_url: String,
    /// Dataset (knowledge base) ids sent as `dataset_ids`.
    pub dataset_ids: Vec<String>,
    /// Optional document ids sent as `document_ids`.
    pub document_ids: Option<Vec<String>>,
    /// `top_k` override; defaults to [`DEFAULT_TOP_K`] in the builder.
    pub top_k: Option<i32>,
    /// `similarity_threshold`; defaults to [`DEFAULT_SIMILARITY_THRESHOLD`].
    pub similarity_threshold: Option<f64>,
    /// `vector_similarity_weight`; defaults to [`DEFAULT_VECTOR_SIMILARITY_WEIGHT`].
    pub vector_similarity_weight: Option<f64>,
    /// `page`; defaults to [`DEFAULT_PAGE`].
    pub page: Option<i32>,
    /// `page_size`; defaults to [`DEFAULT_PAGE_SIZE`].
    pub page_size: Option<i32>,
    /// `use_kg` (knowledge-graph retrieval).
    pub use_kg: Option<bool>,
    /// `toc_enhance`.
    pub toc_enhance: Option<bool>,
    /// `rerank_id` (model name or hex UUID, not numeric).
    pub rerank_id: Option<String>,
    /// `keyword`.
    pub keyword: Option<bool>,
    /// `highlight`.
    pub highlight: Option<bool>,
    /// `cross_languages`.
    pub cross_languages: Option<Vec<String>>,
    /// `metadata_condition` filter map.
    pub metadata_condition: Option<Value>,
    /// HTTP timeout.
    pub timeout: Duration,
    /// Retry budget.
    pub max_retries: u32,
    /// Extra HTTP headers appended to every request.
    pub custom_headers: HashMap<String, String>,
}

impl RAGFlowConfig {
    /// Starts a builder with the Java defaults.
    pub fn builder() -> RAGFlowConfigBuilder {
        RAGFlowConfigBuilder::default()
    }

    /// The full retrieval URL (`baseUrl` + `/api/v1/retrieval`).
    pub fn retrieval_url(&self) -> String {
        format!(
            "{}{}",
            self.base_url.trim_end_matches('/'),
            super::RETRIEVAL_PATH
        )
    }
}

/// Consuming builder for [`RAGFlowConfig`].
#[derive(Debug, Clone)]
pub struct RAGFlowConfigBuilder {
    api_key: Option<String>,
    base_url: Option<String>,
    dataset_ids: Vec<String>,
    document_ids: Option<Vec<String>>,
    top_k: Option<i32>,
    similarity_threshold: Option<f64>,
    vector_similarity_weight: Option<f64>,
    page: Option<i32>,
    page_size: Option<i32>,
    use_kg: Option<bool>,
    toc_enhance: Option<bool>,
    rerank_id: Option<String>,
    keyword: Option<bool>,
    highlight: Option<bool>,
    cross_languages: Option<Vec<String>>,
    metadata_condition: Option<Value>,
    timeout: Duration,
    max_retries: u32,
    custom_headers: HashMap<String, String>,
}

impl Default for RAGFlowConfigBuilder {
    fn default() -> Self {
        Self {
            api_key: None,
            base_url: None,
            dataset_ids: Vec::new(),
            document_ids: None,
            top_k: Some(DEFAULT_TOP_K),
            similarity_threshold: Some(DEFAULT_SIMILARITY_THRESHOLD),
            vector_similarity_weight: Some(DEFAULT_VECTOR_SIMILARITY_WEIGHT),
            page: Some(DEFAULT_PAGE),
            page_size: Some(DEFAULT_PAGE_SIZE),
            use_kg: None,
            toc_enhance: None,
            rerank_id: None,
            keyword: None,
            highlight: None,
            cross_languages: None,
            metadata_condition: None,
            timeout: DEFAULT_TIMEOUT,
            max_retries: DEFAULT_MAX_RETRIES,
            custom_headers: HashMap::new(),
        }
    }
}

impl RAGFlowConfigBuilder {
    /// Sets the API key.
    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Sets the base URL.
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    /// Replaces the dataset id list.
    pub fn dataset_ids<I, S>(mut self, dataset_ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.dataset_ids = dataset_ids.into_iter().map(Into::into).collect();
        self
    }

    /// Appends one dataset id.
    pub fn add_dataset_id(mut self, dataset_id: impl Into<String>) -> Self {
        self.dataset_ids.push(dataset_id.into());
        self
    }

    /// Sets the document id filter.
    pub fn document_ids<I, S>(mut self, document_ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.document_ids = Some(document_ids.into_iter().map(Into::into).collect());
        self
    }

    /// Appends one document id.
    pub fn add_document_id(mut self, document_id: impl Into<String>) -> Self {
        self.document_ids
            .get_or_insert_with(Vec::new)
            .push(document_id.into());
        self
    }

    /// Sets `top_k` (must be `> 0`).
    pub fn top_k(mut self, top_k: i32) -> Self {
        self.top_k = Some(top_k);
        self
    }

    /// Sets `similarity_threshold` (must be within `[0, 1]`).
    pub fn similarity_threshold(mut self, threshold: f64) -> Self {
        self.similarity_threshold = Some(threshold);
        self
    }

    /// Sets `vector_similarity_weight` (must be within `[0, 1]`).
    pub fn vector_similarity_weight(mut self, weight: f64) -> Self {
        self.vector_similarity_weight = Some(weight);
        self
    }

    /// Sets `page` (must be `>= 1`).
    pub fn page(mut self, page: i32) -> Self {
        self.page = Some(page);
        self
    }

    /// Sets `page_size` (must be `>= 1`).
    pub fn page_size(mut self, page_size: i32) -> Self {
        self.page_size = Some(page_size);
        self
    }

    /// Sets `use_kg`.
    pub fn use_kg(mut self, use_kg: bool) -> Self {
        self.use_kg = Some(use_kg);
        self
    }

    /// Sets `toc_enhance`.
    pub fn toc_enhance(mut self, toc_enhance: bool) -> Self {
        self.toc_enhance = Some(toc_enhance);
        self
    }

    /// Sets `rerank_id`.
    pub fn rerank_id(mut self, rerank_id: impl Into<String>) -> Self {
        self.rerank_id = Some(rerank_id.into());
        self
    }

    /// Sets `keyword`.
    pub fn keyword(mut self, keyword: bool) -> Self {
        self.keyword = Some(keyword);
        self
    }

    /// Sets `highlight`.
    pub fn highlight(mut self, highlight: bool) -> Self {
        self.highlight = Some(highlight);
        self
    }

    /// Replaces `cross_languages`.
    pub fn cross_languages<I, S>(mut self, languages: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.cross_languages = Some(languages.into_iter().map(Into::into).collect());
        self
    }

    /// Appends one cross-language entry.
    pub fn add_cross_language(mut self, language: impl Into<String>) -> Self {
        self.cross_languages
            .get_or_insert_with(Vec::new)
            .push(language.into());
        self
    }

    /// Sets the metadata condition filter.
    pub fn metadata_condition(mut self, condition: Value) -> Self {
        self.metadata_condition = Some(condition);
        self
    }

    /// Sets the HTTP timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets the retry budget.
    pub fn max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Adds one custom header.
    pub fn add_custom_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.custom_headers.insert(key.into(), value.into());
        self
    }

    /// Validates and builds the configuration.
    ///
    /// Mirrors the Java `RAGFlowConfig.Builder.build()` checks (error messages are the
    /// same modulo `IllegalArgumentException` → [`RagError::InvalidArgument`]).
    pub fn build(self) -> Result<RAGFlowConfig> {
        let base_url = self
            .base_url
            .filter(|u| !u.trim().is_empty())
            .ok_or_else(|| RagError::InvalidArgument("BaseUrl key is required".to_string()))?;
        let api_key = self
            .api_key
            .filter(|k| !k.trim().is_empty())
            .ok_or_else(|| RagError::InvalidArgument("API key is required".to_string()))?;

        if let Some(top_k) = self.top_k {
            if top_k <= 0 {
                return Err(RagError::InvalidArgument(
                    "topK must be greater than 0".to_string(),
                ));
            }
        }
        if let Some(threshold) = self.similarity_threshold {
            if !(0.0..=1.0).contains(&threshold) {
                return Err(RagError::InvalidArgument(
                    "similarityThreshold must be between 0.0 and 1.0".to_string(),
                ));
            }
        }
        if let Some(weight) = self.vector_similarity_weight {
            if !(0.0..=1.0).contains(&weight) {
                return Err(RagError::InvalidArgument(
                    "vectorSimilarityWeight must be between 0.0 and 1.0".to_string(),
                ));
            }
        }
        if let Some(page) = self.page {
            if page < 1 {
                return Err(RagError::InvalidArgument("page must be >= 1".to_string()));
            }
        }
        if let Some(page_size) = self.page_size {
            if page_size < 1 {
                return Err(RagError::InvalidArgument(
                    "pageSize must be >= 1".to_string(),
                ));
            }
        }

        let has_dataset_ids = !self.dataset_ids.is_empty();
        let has_document_ids = self
            .document_ids
            .as_ref()
            .is_some_and(|ids| !ids.is_empty());
        if !has_dataset_ids && !has_document_ids {
            return Err(RagError::InvalidArgument(
                "Either dataset_ids or document_ids must be set. Use \
                 addDatasetId()/datasetIds() or addDocumentId()/documentIds()"
                    .to_string(),
            ));
        }

        Ok(RAGFlowConfig {
            api_key,
            base_url,
            dataset_ids: self.dataset_ids,
            document_ids: self.document_ids,
            top_k: self.top_k,
            similarity_threshold: self.similarity_threshold,
            vector_similarity_weight: self.vector_similarity_weight,
            page: self.page,
            page_size: self.page_size,
            use_kg: self.use_kg,
            toc_enhance: self.toc_enhance,
            rerank_id: self.rerank_id,
            keyword: self.keyword,
            highlight: self.highlight,
            cross_languages: self.cross_languages,
            metadata_condition: self.metadata_condition,
            timeout: self.timeout,
            max_retries: self.max_retries,
            custom_headers: self.custom_headers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base_builder() -> RAGFlowConfigBuilder {
        RAGFlowConfig::builder()
            .api_key("ragflow-key")
            .base_url("http://localhost:9380")
            .add_dataset_id("ds-1")
    }

    #[test]
    fn builder_applies_java_defaults() {
        let cfg = base_builder().build().unwrap();
        assert_eq!(cfg.top_k, Some(1024));
        assert_eq!(cfg.similarity_threshold, Some(0.2));
        assert_eq!(cfg.vector_similarity_weight, Some(0.3));
        assert_eq!(cfg.page, Some(1));
        assert_eq!(cfg.page_size, Some(30));
        assert_eq!(cfg.timeout, Duration::from_secs(30));
        assert_eq!(cfg.max_retries, 3);
        assert!(cfg.use_kg.is_none());
    }

    #[test]
    fn retrieval_url_trims_trailing_slash() {
        let cfg = base_builder()
            .base_url("http://localhost:9380/")
            .build()
            .unwrap();
        assert_eq!(
            cfg.retrieval_url(),
            "http://localhost:9380/api/v1/retrieval"
        );
    }

    #[test]
    fn dataset_id_required_unless_document_ids_present() {
        let err = RAGFlowConfig::builder()
            .api_key("k")
            .base_url("http://x")
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("dataset_ids or document_ids"));

        let ok = RAGFlowConfig::builder()
            .api_key("k")
            .base_url("http://x")
            .add_document_id("doc-1")
            .build();
        assert!(ok.is_ok());
    }

    #[test]
    fn api_key_and_base_url_are_required() {
        let missing_key = RAGFlowConfig::builder()
            .base_url("http://x")
            .add_dataset_id("d")
            .build()
            .unwrap_err();
        assert!(missing_key.to_string().contains("API key"));

        let missing_url = RAGFlowConfig::builder()
            .api_key("k")
            .add_dataset_id("d")
            .build()
            .unwrap_err();
        assert!(missing_url.to_string().contains("BaseUrl"));
    }

    #[test]
    fn numeric_validation_matches_java() {
        assert!(base_builder().top_k(0).build().is_err());
        assert!(base_builder().similarity_threshold(1.5).build().is_err());
        assert!(base_builder().similarity_threshold(-0.1).build().is_err());
        assert!(base_builder()
            .vector_similarity_weight(2.0)
            .build()
            .is_err());
        assert!(base_builder().page(0).build().is_err());
        assert!(base_builder().page_size(0).build().is_err());
        assert!(base_builder().similarity_threshold(1.0).build().is_ok());
        assert!(base_builder().similarity_threshold(0.0).build().is_ok());
    }

    #[test]
    fn optional_flags_round_trip() {
        let cfg = base_builder()
            .use_kg(true)
            .toc_enhance(true)
            .keyword(true)
            .highlight(true)
            .rerank_id("BAAI/bge-reranker-v2-m3@BAAI")
            .cross_languages(vec!["Chinese"])
            .metadata_condition(json!({"author": "Toby"}))
            .max_retries(5)
            .add_custom_header("X-Trace", "1")
            .build()
            .unwrap();
        assert_eq!(cfg.use_kg, Some(true));
        assert_eq!(
            cfg.rerank_id.as_deref(),
            Some("BAAI/bge-reranker-v2-m3@BAAI")
        );
        assert_eq!(
            cfg.cross_languages.as_deref(),
            Some(&["Chinese".to_string()][..])
        );
        assert_eq!(cfg.max_retries, 5);
        assert_eq!(
            cfg.custom_headers.get("X-Trace").map(String::as_str),
            Some("1")
        );
    }
}
