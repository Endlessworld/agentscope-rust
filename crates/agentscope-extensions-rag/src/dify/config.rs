//! Dify configuration: connection, retrieval, rerank and metadata-filter settings.
//!
//! Ports `DifyRAGConfig`, `RetrievalMode`, `RerankConfig`, `MetadataFilter` and
//! `MetadataFilterCondition`. Java's fluent builder becomes a consuming Rust builder with a
//! fallible [`DifyRAGConfigBuilder::build`] (required key/dataset validation).

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{RagError, Result};

/// Default Dify API base URL (Java `DifyRAGConfig.DEFAULT_API_BASE_URL`).
pub const DEFAULT_API_BASE_URL: &str = "https://api.dify.ai/v1";
/// Default `top_k` (Java `DEFAULT_TOP_K`).
pub const DEFAULT_TOP_K: i32 = 10;
/// Default score threshold (Java `DEFAULT_SCORE_THRESHOLD`).
pub const DEFAULT_SCORE_THRESHOLD: f64 = 0.0;
/// Default connect timeout (Java `DEFAULT_CONNECT_TIMEOUT`).
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Default read timeout (Java `DEFAULT_READ_TIMEOUT`).
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(60);
/// Default maximum retries (Java `DEFAULT_MAX_RETRIES`).
pub const DEFAULT_MAX_RETRIES: u32 = 3;

/// Dify retrieval strategy.
///
/// The wire value mirrors Java `RetrievalMode.getValue()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalMode {
    /// BM25-style keyword search (`keyword_search`).
    #[serde(rename = "keyword_search")]
    KeywordSearch,
    /// Vector similarity search (`semantic_search`).
    #[serde(rename = "semantic_search")]
    SemanticSearch,
    /// Combined keyword + semantic search (`hybrid_search`, the Java default).
    #[serde(rename = "hybrid_search")]
    #[default]
    HybridSearch,
    /// Full-text search (`full_text_search`).
    #[serde(rename = "full_text_search")]
    FullTextSearch,
}

impl RetrievalMode {
    /// The API string value, matching Java `RetrievalMode.getValue()`.
    pub fn value(&self) -> &'static str {
        match self {
            RetrievalMode::KeywordSearch => "keyword_search",
            RetrievalMode::SemanticSearch => "semantic_search",
            RetrievalMode::HybridSearch => "hybrid_search",
            RetrievalMode::FullTextSearch => "full_text_search",
        }
    }

    /// Parses an API value, mirroring Java `RetrievalMode.fromValue`.
    pub fn from_value(value: &str) -> Result<Self> {
        match value {
            "keyword_search" => Ok(RetrievalMode::KeywordSearch),
            "semantic_search" => Ok(RetrievalMode::SemanticSearch),
            "hybrid_search" => Ok(RetrievalMode::HybridSearch),
            "full_text_search" => Ok(RetrievalMode::FullTextSearch),
            other => Err(RagError::InvalidArgument(format!(
                "Unknown retrieval mode: {other}"
            ))),
        }
    }
}

/// Reranking model selection.
///
/// Ports `RerankConfig`. Java's Javadoc mentions `topN`, but the class only stores
/// `providerName` / `modelName`; this port mirrors the actual fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RerankConfig {
    /// Reranking provider (e.g. `cohere`, `jina`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_name: Option<String>,
    /// Reranking model name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
}

impl RerankConfig {
    /// Creates an empty rerank configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the provider name.
    pub fn with_provider_name(mut self, provider: impl Into<String>) -> Self {
        self.provider_name = Some(provider.into());
        self
    }

    /// Sets the model name.
    pub fn with_model_name(mut self, model: impl Into<String>) -> Self {
        self.model_name = Some(model.into());
        self
    }

    /// Whether neither provider nor model is set (Java skips the `reranking_model` object).
    pub fn is_empty(&self) -> bool {
        self.provider_name.is_none() && self.model_name.is_none()
    }
}

/// One metadata filter condition.
///
/// Ports `MetadataFilterCondition` (`name`, `comparisonOperator`, `value`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataFilterCondition {
    /// Metadata field name.
    pub name: String,
    /// Comparison operator (e.g. `equals`, `contains`, `not_equals`).
    pub comparison_operator: String,
    /// Value to compare against.
    pub value: String,
}

impl MetadataFilterCondition {
    /// Creates a condition.
    pub fn new(
        name: impl Into<String>,
        comparison_operator: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            comparison_operator: comparison_operator.into(),
            value: value.into(),
        }
    }
}

/// Metadata filter: a logical operator plus a list of conditions.
///
/// Ports `MetadataFilter`; the default logical operator is `and`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataFilter {
    /// `and` or `or`.
    pub logical_operator: String,
    /// Filter conditions.
    pub conditions: Vec<MetadataFilterCondition>,
}

impl Default for MetadataFilter {
    fn default() -> Self {
        Self {
            logical_operator: "and".to_string(),
            conditions: Vec::new(),
        }
    }
}

impl MetadataFilter {
    /// Creates an empty filter with the default `and` operator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the logical operator.
    pub fn with_logical_operator(mut self, op: impl Into<String>) -> Self {
        self.logical_operator = op.into();
        self
    }

    /// Appends a condition.
    pub fn add_condition(mut self, condition: MetadataFilterCondition) -> Self {
        self.conditions.push(condition);
        self
    }
}

/// Configuration for the Dify Knowledge Base integration.
///
/// Ports `DifyRAGConfig`. Use [`DifyRAGConfig::builder`] to construct it.
#[derive(Debug, Clone, PartialEq)]
pub struct DifyRAGConfig {
    /// API key (required).
    pub api_key: String,
    /// API base URL (defaults to [`DEFAULT_API_BASE_URL`]).
    pub api_base_url: String,
    /// Dataset / knowledge base id (required).
    pub dataset_id: String,
    /// Retrieval mode.
    pub retrieval_mode: RetrievalMode,
    /// `top_k` used when the caller does not override the limit.
    pub top_k: i32,
    /// Score threshold; `score_threshold_enabled` is set only when `> 0`.
    pub score_threshold: f64,
    /// Whether reranking is enabled.
    pub enable_rerank: bool,
    /// Optional rerank model selection.
    pub rerank_config: Option<RerankConfig>,
    /// Hybrid-search weight (0-1), only used for `hybrid_search`.
    pub weights: Option<f64>,
    /// Optional metadata filtering.
    pub metadata_filter: Option<MetadataFilter>,
    /// HTTP connect timeout.
    pub connect_timeout: Duration,
    /// HTTP read/write timeout.
    pub read_timeout: Duration,
    /// Maximum number of retries.
    pub max_retries: u32,
    /// Extra HTTP headers.
    pub custom_headers: HashMap<String, String>,
}

impl DifyRAGConfig {
    /// Starts building a configuration.
    pub fn builder() -> DifyRAGConfigBuilder {
        DifyRAGConfigBuilder::default()
    }
}

/// Builder for [`DifyRAGConfig`].
#[derive(Debug, Clone, Default)]
pub struct DifyRAGConfigBuilder {
    api_key: Option<String>,
    api_base_url: Option<String>,
    dataset_id: Option<String>,
    retrieval_mode: Option<RetrievalMode>,
    top_k: Option<i32>,
    score_threshold: Option<f64>,
    enable_rerank: Option<bool>,
    rerank_config: Option<RerankConfig>,
    weights: Option<f64>,
    metadata_filter: Option<MetadataFilter>,
    connect_timeout: Option<Duration>,
    read_timeout: Option<Duration>,
    max_retries: Option<u32>,
    custom_headers: HashMap<String, String>,
}

impl DifyRAGConfigBuilder {
    /// Sets the API key (required).
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Sets the API base URL.
    pub fn api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = Some(url.into());
        self
    }

    /// Sets the dataset id (required).
    pub fn dataset_id(mut self, id: impl Into<String>) -> Self {
        self.dataset_id = Some(id.into());
        self
    }

    /// Sets the retrieval mode.
    pub fn retrieval_mode(mut self, mode: RetrievalMode) -> Self {
        self.retrieval_mode = Some(mode);
        self
    }

    /// Sets `top_k`.
    ///
    /// Java's `Builder.topK` rejects values below `1`; the builder cannot fail, so the
    /// equivalent check lives in [`DifyRAGConfigBuilder::build`].
    pub fn top_k(mut self, top_k: i32) -> Self {
        self.top_k = Some(top_k);
        self
    }

    /// Sets the score threshold (0-1, validated by [`DifyRAGConfigBuilder::build`]).
    pub fn score_threshold(mut self, threshold: f64) -> Self {
        self.score_threshold = Some(threshold);
        self
    }

    /// Enables or disables reranking.
    pub fn enable_rerank(mut self, enable: bool) -> Self {
        self.enable_rerank = Some(enable);
        self
    }

    /// Sets the rerank configuration.
    pub fn rerank_config(mut self, config: RerankConfig) -> Self {
        self.rerank_config = Some(config);
        self
    }

    /// Sets the hybrid-search weight (0-1, validated by [`DifyRAGConfigBuilder::build`]).
    pub fn weights(mut self, weights: f64) -> Self {
        self.weights = Some(weights);
        self
    }

    /// Sets the metadata filter.
    pub fn metadata_filter(mut self, filter: MetadataFilter) -> Self {
        self.metadata_filter = Some(filter);
        self
    }

    /// Sets the connect timeout.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = Some(timeout);
        self
    }

    /// Sets the read timeout.
    pub fn read_timeout(mut self, timeout: Duration) -> Self {
        self.read_timeout = Some(timeout);
        self
    }

    /// Sets the maximum number of retries.
    ///
    /// Java's `Builder.maxRetries` rejects negative values; `u32` makes that unrepresentable,
    /// so no equivalent check exists here.
    pub fn max_retries(mut self, retries: u32) -> Self {
        self.max_retries = Some(retries);
        self
    }

    /// Adds a custom HTTP header.
    pub fn add_custom_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.custom_headers.insert(name.into(), value.into());
        self
    }

    /// Validates required fields and applies defaults.
    ///
    /// Mirrors Java `DifyRAGConfig`'s constructor, which throws when the API key or dataset
    /// id is null/blank, and otherwise applies the `DEFAULT_*` constants.
    ///
    /// Java also validated the numeric knobs inside its builder setters (`topK >= 1`,
    /// `scoreThreshold` / `weights` within `[0, 1]`); Rust setters cannot fail, so the same
    /// checks run here with the Java messages verbatim. The comparisons keep Java's `NaN`
    /// behaviour (a `NaN` threshold/weight passes). Java's `maxRetries cannot be negative`
    /// has no counterpart because the field is a `u32`.
    pub fn build(self) -> Result<DifyRAGConfig> {
        let api_key = self.api_key.unwrap_or_default().trim().to_string();
        if api_key.is_empty() {
            return Err(RagError::InvalidArgument(
                "API key cannot be null or empty".to_string(),
            ));
        }

        let dataset_id = self.dataset_id.unwrap_or_default().trim().to_string();
        if dataset_id.is_empty() {
            return Err(RagError::InvalidArgument(
                "Dataset ID cannot be null or empty".to_string(),
            ));
        }

        if self.top_k.is_some_and(|top_k| top_k < 1) {
            return Err(RagError::InvalidArgument(
                "topK must be larger than 1".to_string(),
            ));
        }

        if self
            .score_threshold
            .is_some_and(|threshold| threshold < 0.0 || threshold > 1.0)
        {
            return Err(RagError::InvalidArgument(
                "scoreThreshold must be between 0.0 and 1.0".to_string(),
            ));
        }

        if self.weights.is_some_and(|w| w < 0.0 || w > 1.0) {
            return Err(RagError::InvalidArgument(
                "weights must be between 0.0 and 1.0".to_string(),
            ));
        }

        let api_base_url = self
            .api_base_url
            .map(|u| u.trim().to_string())
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| DEFAULT_API_BASE_URL.to_string());

        Ok(DifyRAGConfig {
            api_key,
            api_base_url,
            dataset_id,
            retrieval_mode: self.retrieval_mode.unwrap_or_default(),
            top_k: self.top_k.unwrap_or(DEFAULT_TOP_K),
            score_threshold: self.score_threshold.unwrap_or(DEFAULT_SCORE_THRESHOLD),
            enable_rerank: self.enable_rerank.unwrap_or(false),
            rerank_config: self.rerank_config,
            weights: self.weights,
            metadata_filter: self.metadata_filter,
            connect_timeout: self.connect_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT),
            read_timeout: self.read_timeout.unwrap_or(DEFAULT_READ_TIMEOUT),
            max_retries: self.max_retries.unwrap_or(DEFAULT_MAX_RETRIES),
            custom_headers: self.custom_headers,
        })
    }

    /// Builds the configuration or panics when required fields are missing.
    ///
    /// Provided for test/ergonomic use where an invalid configuration is a programming error.
    pub fn build_or_panic(self) -> DifyRAGConfig {
        self.build().expect("invalid DifyRAGConfig")
    }

    /// The retrieve endpoint URL: `{apiBaseUrl}/datasets/{datasetId}/retrieve`.
    pub fn retrieve_url(api_base_url: &str, dataset_id: &str) -> String {
        format!("{api_base_url}/datasets/{dataset_id}/retrieve")
    }
}

impl DifyRAGConfig {
    /// The retrieve endpoint URL for this configuration.
    pub fn retrieve_url(&self) -> String {
        DifyRAGConfigBuilder::retrieve_url(&self.api_base_url, &self.dataset_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_requires_api_key_and_dataset_id() {
        assert!(DifyRAGConfig::builder().build().is_err());
        assert!(DifyRAGConfig::builder().api_key("k").build().is_err());
        assert!(DifyRAGConfig::builder()
            .api_key("  ")
            .dataset_id("d")
            .build()
            .is_err());
        assert!(DifyRAGConfig::builder()
            .api_key("k")
            .dataset_id("")
            .build()
            .is_err());
    }

    fn invalid_argument_message(err: RagError) -> String {
        match err {
            RagError::InvalidArgument(message) => message,
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[test]
    fn builder_rejects_top_k_below_one() {
        let err = DifyRAGConfig::builder()
            .api_key("k")
            .dataset_id("d")
            .top_k(0)
            .build()
            .unwrap_err();
        assert_eq!(invalid_argument_message(err), "topK must be larger than 1");
    }

    #[test]
    fn builder_rejects_a_score_threshold_outside_the_unit_interval() {
        for threshold in [-0.1, 1.1] {
            let err = DifyRAGConfig::builder()
                .api_key("k")
                .dataset_id("d")
                .score_threshold(threshold)
                .build()
                .unwrap_err();
            assert_eq!(
                invalid_argument_message(err),
                "scoreThreshold must be between 0.0 and 1.0"
            );
        }
    }

    #[test]
    fn builder_rejects_weights_outside_the_unit_interval() {
        for weights in [-0.1, 1.1] {
            let err = DifyRAGConfig::builder()
                .api_key("k")
                .dataset_id("d")
                .weights(weights)
                .build()
                .unwrap_err();
            assert_eq!(
                invalid_argument_message(err),
                "weights must be between 0.0 and 1.0"
            );
        }
    }

    #[test]
    fn builder_accepts_the_documented_boundaries() {
        let cfg = DifyRAGConfig::builder()
            .api_key("k")
            .dataset_id("d")
            .top_k(1)
            .score_threshold(1.0)
            .weights(0.0)
            .build()
            .unwrap();
        assert_eq!(cfg.top_k, 1);
        assert_eq!(cfg.score_threshold, 1.0);
        assert_eq!(cfg.weights, Some(0.0));
    }

    #[test]
    fn builder_applies_java_defaults() {
        let cfg = DifyRAGConfig::builder()
            .api_key("  key  ")
            .dataset_id("  ds  ")
            .build()
            .unwrap();
        assert_eq!(cfg.api_key, "key");
        assert_eq!(cfg.dataset_id, "ds");
        assert_eq!(cfg.api_base_url, DEFAULT_API_BASE_URL);
        assert_eq!(cfg.retrieval_mode, RetrievalMode::HybridSearch);
        assert_eq!(cfg.top_k, 10);
        assert_eq!(cfg.score_threshold, 0.0);
        assert!(!cfg.enable_rerank);
        assert_eq!(cfg.connect_timeout, Duration::from_secs(30));
        assert_eq!(cfg.read_timeout, Duration::from_secs(60));
        assert_eq!(cfg.max_retries, 3);
    }

    #[test]
    fn blank_base_url_falls_back_to_default() {
        let cfg = DifyRAGConfig::builder()
            .api_key("k")
            .dataset_id("d")
            .api_base_url("   ")
            .build()
            .unwrap();
        assert_eq!(cfg.api_base_url, DEFAULT_API_BASE_URL);
    }

    #[test]
    fn retrieve_url_is_built_from_base_and_dataset() {
        let cfg = DifyRAGConfig::builder()
            .api_key("k")
            .dataset_id("ds-1")
            .api_base_url("https://self.hosted/v1")
            .build()
            .unwrap();
        assert_eq!(
            cfg.retrieve_url(),
            "https://self.hosted/v1/datasets/ds-1/retrieve"
        );
    }

    #[test]
    fn retrieval_mode_wire_values_match_java() {
        assert_eq!(RetrievalMode::KeywordSearch.value(), "keyword_search");
        assert_eq!(RetrievalMode::SemanticSearch.value(), "semantic_search");
        assert_eq!(RetrievalMode::HybridSearch.value(), "hybrid_search");
        assert_eq!(RetrievalMode::FullTextSearch.value(), "full_text_search");

        assert_eq!(
            RetrievalMode::from_value("hybrid_search").unwrap(),
            RetrievalMode::HybridSearch
        );
        assert!(RetrievalMode::from_value("nope").is_err());
    }

    #[test]
    fn retrieval_mode_serializes_to_wire_value() {
        let json = serde_json::to_string(&RetrievalMode::FullTextSearch).unwrap();
        assert_eq!(json, "\"full_text_search\"");
        let back: RetrievalMode = serde_json::from_str("\"keyword_search\"").unwrap();
        assert_eq!(back, RetrievalMode::KeywordSearch);
    }

    #[test]
    fn metadata_filter_defaults_to_and() {
        let f = MetadataFilter::new().add_condition(MetadataFilterCondition::new(
            "category",
            "equals",
            "technical",
        ));
        assert_eq!(f.logical_operator, "and");
        assert_eq!(f.conditions.len(), 1);
        assert_eq!(f.conditions[0].name, "category");
    }

    #[test]
    fn rerank_config_emptiness_is_reported() {
        assert!(RerankConfig::new().is_empty());
        assert!(!RerankConfig::new().with_provider_name("cohere").is_empty());
        assert!(!RerankConfig::new().with_model_name("m").is_empty());
    }
}
