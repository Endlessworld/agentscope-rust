//! HayStack configuration and filter policy.
//!
//! Ports `HayStackConfig` (+ builder) and `FilterPolicy`. Java validated `topK > 0` and
//! `maxRetries >= 0` eagerly in the builder setters and `baseUrl` in `build()`; the Rust port
//! keeps the same rule set and messages, validating everything in `build()` to match the other
//! backends in this crate.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;

use crate::error::{RagError, Result};
use crate::haystack::model::{HayStackDocument, SparseEmbedding};

/// Default `top_k` (Java `HayStackConfig.DEFAULT_TOP_K`).
pub const DEFAULT_TOP_K: i32 = 3;
/// Default success code (Java `HayStackConfig.DEFAULT_SUCCESS_CODE`).
pub const DEFAULT_SUCCESS_CODE: i32 = 0;
/// Default timeout (Java `DEFAULT_TIMEOUT`).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Default retry budget (Java `DEFAULT_MAX_RETRIES`).
pub const DEFAULT_MAX_RETRIES: u32 = 3;

/// How retriever filters interact with the pipeline's initial filters.
///
/// Ports `FilterPolicy`; the wire value mirrors Java `getPolicy()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FilterPolicy {
    /// Runtime filters replace the pipeline's initial filters.
    #[default]
    Replace,
    /// Runtime filters are merged into the initial filters, overwriting conflicts.
    Merge,
}

impl FilterPolicy {
    /// The wire value (`replace` / `merge`), matching Java `FilterPolicy.getPolicy()`.
    pub fn policy(&self) -> &'static str {
        match self {
            FilterPolicy::Replace => "replace",
            FilterPolicy::Merge => "merge",
        }
    }

    /// Parses a wire value.
    pub fn from_policy(value: &str) -> Result<Self> {
        match value {
            "replace" => Ok(FilterPolicy::Replace),
            "merge" => Ok(FilterPolicy::Merge),
            other => Err(RagError::InvalidArgument(format!(
                "Unknown filter policy: {other}"
            ))),
        }
    }
}

/// HayStack client configuration.
///
/// Ports `HayStackConfig`. Only `base_url` is required; `success_code`, `top_k`, `timeout` and
/// `max_retries` carry the Java defaults listed in the other constants.
#[derive(Debug, Clone, PartialEq)]
pub struct HayStackConfig {
    /// Full retrieve endpoint URL, e.g. `http://localhost:8080/retrieve`.
    pub base_url: String,
    /// Response `code` that counts as success (default `0`).
    pub success_code: i32,
    /// Maximum number of documents / groups to return (default `3`).
    pub top_k: Option<i32>,
    /// Whether to normalise scores into `0..=1`.
    pub scale_score: Option<bool>,
    /// Whether the server should return embeddings.
    pub return_embedding: Option<bool>,
    /// Minimal score threshold.
    pub score_threshold: Option<f64>,
    /// Payload field to group by.
    pub group_by: Option<String>,
    /// Maximum number of points per group.
    pub group_size: Option<i32>,
    /// Dense query embedding.
    pub query_embedding: Option<Vec<f32>>,
    /// Sparse query embedding.
    pub query_sparse_embedding: Option<SparseEmbedding>,
    /// Documents supplied inline.
    pub documents: Option<Vec<HayStackDocument>>,
    /// Documents to window-expand around.
    pub retrieved_documents: Option<Vec<HayStackDocument>>,
    /// Number of adjacent documents to include on each side.
    pub window_size: Option<i32>,
    /// Document-store filters.
    pub filters: Option<HashMap<String, Value>>,
    /// Filter policy (`replace` / `merge`).
    pub filter_policy: Option<FilterPolicy>,
    /// HTTP timeout.
    pub timeout: Duration,
    /// Retry budget.
    pub max_retries: u32,
    /// Extra HTTP headers appended to every request.
    pub custom_headers: HashMap<String, String>,
}

impl HayStackConfig {
    /// Starts a builder with the Java defaults.
    pub fn builder() -> HayStackConfigBuilder {
        HayStackConfigBuilder::default()
    }
}

/// Consuming builder for [`HayStackConfig`].
#[derive(Debug, Clone)]
pub struct HayStackConfigBuilder {
    base_url: Option<String>,
    success_code: i32,
    top_k: Option<i32>,
    scale_score: Option<bool>,
    return_embedding: Option<bool>,
    score_threshold: Option<f64>,
    group_by: Option<String>,
    group_size: Option<i32>,
    query_embedding: Option<Vec<f32>>,
    query_sparse_embedding: Option<SparseEmbedding>,
    documents: Option<Vec<HayStackDocument>>,
    retrieved_documents: Option<Vec<HayStackDocument>>,
    window_size: Option<i32>,
    filters: Option<HashMap<String, Value>>,
    filter_policy: Option<FilterPolicy>,
    timeout: Duration,
    max_retries: u32,
    custom_headers: HashMap<String, String>,
}

impl Default for HayStackConfigBuilder {
    fn default() -> Self {
        Self {
            base_url: None,
            success_code: DEFAULT_SUCCESS_CODE,
            top_k: Some(DEFAULT_TOP_K),
            scale_score: None,
            return_embedding: None,
            score_threshold: None,
            group_by: None,
            group_size: None,
            query_embedding: None,
            query_sparse_embedding: None,
            documents: None,
            retrieved_documents: None,
            window_size: None,
            filters: None,
            filter_policy: None,
            timeout: DEFAULT_TIMEOUT,
            max_retries: DEFAULT_MAX_RETRIES,
            custom_headers: HashMap::new(),
        }
    }
}

impl HayStackConfigBuilder {
    /// Sets the retrieve endpoint URL (required).
    pub fn base_url(mut self, value: impl Into<String>) -> Self {
        self.base_url = Some(value.into());
        self
    }

    /// Sets the success code (default [`DEFAULT_SUCCESS_CODE`]).
    pub fn success_code(mut self, value: i32) -> Self {
        self.success_code = value;
        self
    }

    /// Sets `top_k`; must be `> 0` (validated in [`Self::build`]).
    pub fn top_k(mut self, value: i32) -> Self {
        self.top_k = Some(value);
        self
    }

    /// Sets `scale_score`.
    pub fn scale_score(mut self, value: bool) -> Self {
        self.scale_score = Some(value);
        self
    }

    /// Sets `return_embedding`.
    pub fn return_embedding(mut self, value: bool) -> Self {
        self.return_embedding = Some(value);
        self
    }

    /// Sets `score_threshold`.
    pub fn score_threshold(mut self, value: f64) -> Self {
        self.score_threshold = Some(value);
        self
    }

    /// Sets `group_by`.
    pub fn group_by(mut self, value: impl Into<String>) -> Self {
        self.group_by = Some(value.into());
        self
    }

    /// Sets `group_size`.
    pub fn group_size(mut self, value: i32) -> Self {
        self.group_size = Some(value);
        self
    }

    /// Sets the dense query embedding.
    pub fn query_embedding(mut self, value: Vec<f32>) -> Self {
        self.query_embedding = Some(value);
        self
    }

    /// Sets the sparse query embedding.
    pub fn query_sparse_embedding(mut self, value: SparseEmbedding) -> Self {
        self.query_sparse_embedding = Some(value);
        self
    }

    /// Sets the inline documents.
    pub fn documents(mut self, value: Vec<HayStackDocument>) -> Self {
        self.documents = Some(value);
        self
    }

    /// Sets the documents to window-expand around.
    pub fn retrieved_documents(mut self, value: Vec<HayStackDocument>) -> Self {
        self.retrieved_documents = Some(value);
        self
    }

    /// Sets `window_size`.
    pub fn window_size(mut self, value: i32) -> Self {
        self.window_size = Some(value);
        self
    }

    /// Sets the document-store filters.
    pub fn filters(mut self, value: HashMap<String, Value>) -> Self {
        self.filters = Some(value);
        self
    }

    /// Sets the filter policy.
    pub fn filter_policy(mut self, value: FilterPolicy) -> Self {
        self.filter_policy = Some(value);
        self
    }

    /// Sets the HTTP timeout.
    pub fn timeout(mut self, value: Duration) -> Self {
        self.timeout = value;
        self
    }

    /// Sets the retry budget; must be `>= 0` (validated in [`Self::build`]).
    pub fn max_retries(mut self, value: u32) -> Self {
        self.max_retries = value;
        self
    }

    /// Replaces the custom headers.
    pub fn custom_headers(mut self, value: HashMap<String, String>) -> Self {
        self.custom_headers = value;
        self
    }

    /// Adds one custom header.
    pub fn add_custom_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.custom_headers.insert(key.into(), value.into());
        self
    }

    /// Validates and builds the configuration.
    ///
    /// `baseUrl` is required and `topK` must be `> 0` (`maxRetries` is unsigned in Rust, so the
    /// Java `>= 0` check is enforced by the type).
    pub fn build(self) -> Result<HayStackConfig> {
        let base_url = self
            .base_url
            .filter(|url| !url.trim().is_empty())
            .ok_or_else(|| RagError::InvalidArgument("BaseUrl key is required".to_string()))?;

        if let Some(top_k) = self.top_k {
            if top_k <= 0 {
                return Err(RagError::InvalidArgument(
                    "topK must be greater than 0".to_string(),
                ));
            }
        }

        Ok(HayStackConfig {
            base_url,
            success_code: self.success_code,
            top_k: self.top_k,
            scale_score: self.scale_score,
            return_embedding: self.return_embedding,
            score_threshold: self.score_threshold,
            group_by: self.group_by,
            group_size: self.group_size,
            query_embedding: self.query_embedding,
            query_sparse_embedding: self.query_sparse_embedding,
            documents: self.documents,
            retrieved_documents: self.retrieved_documents,
            window_size: self.window_size,
            filters: self.filters,
            filter_policy: self.filter_policy,
            timeout: self.timeout,
            max_retries: self.max_retries,
            custom_headers: self.custom_headers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_applies_java_defaults() {
        let config = HayStackConfig::builder()
            .base_url("http://localhost:8080/retrieve")
            .build()
            .unwrap();
        assert_eq!(config.success_code, 0);
        assert_eq!(config.top_k, Some(3));
        assert_eq!(config.timeout, DEFAULT_TIMEOUT);
        assert_eq!(config.max_retries, DEFAULT_MAX_RETRIES);
        assert!(config.scale_score.is_none());
        assert!(config.return_embedding.is_none());
        assert!(config.score_threshold.is_none());
        assert!(config.filter_policy.is_none());
        assert!(config.custom_headers.is_empty());
    }

    #[test]
    fn base_url_is_required() {
        assert!(HayStackConfig::builder().build().is_err());
        assert!(HayStackConfig::builder().base_url("   ").build().is_err());
    }

    #[test]
    fn top_k_must_be_positive() {
        assert!(HayStackConfig::builder()
            .base_url("http://x")
            .top_k(0)
            .build()
            .is_err());
        assert!(HayStackConfig::builder()
            .base_url("http://x")
            .top_k(-5)
            .build()
            .is_err());
        assert!(HayStackConfig::builder()
            .base_url("http://x")
            .top_k(1)
            .build()
            .is_ok());
    }

    #[test]
    fn custom_headers_accumulate() {
        let config = HayStackConfig::builder()
            .base_url("http://x")
            .add_custom_header("X-Request-ID", "req-123")
            .add_custom_header("X-Test", "test")
            .build()
            .unwrap();
        assert_eq!(config.custom_headers.get("X-Request-ID").unwrap(), "req-123");
        assert_eq!(config.custom_headers.get("X-Test").unwrap(), "test");
    }

    #[test]
    fn filter_policy_wire_values_match_java() {
        assert_eq!(FilterPolicy::Replace.policy(), "replace");
        assert_eq!(FilterPolicy::Merge.policy(), "merge");
        assert_eq!(FilterPolicy::default(), FilterPolicy::Replace);
        assert_eq!(FilterPolicy::from_policy("merge").unwrap(), FilterPolicy::Merge);
        assert!(FilterPolicy::from_policy("unknown").is_err());
    }
}
