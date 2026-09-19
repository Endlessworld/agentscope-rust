//! Bailian configuration: connection, retrieval, rerank and rewrite settings.
//!
//! Ports `BailianConfig`, `RerankConfig` and `RewriteConfig`. Java validated part of the
//! input eagerly inside the fluent builder setters (throwing `IllegalArgumentException`); the
//! Rust port keeps the values on the builder and validates in `build()`, matching the
//! `dify` / `ragflow` builders already in this crate. The accepted ranges and error messages
//! are otherwise identical.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{RagError, Result};

/// Default Bailian OpenAPI endpoint (Java `BailianConfig.DEFAULT_ENDPOINT`).
pub const DEFAULT_ENDPOINT: &str = "bailian.cn-beijing.aliyuncs.com";
/// Default rerank model (Java `RerankConfig.Builder.modelName`).
pub const DEFAULT_RERANK_MODEL: &str = "gte-rerank-hybrid";
/// Default multi-turn rewrite model (Java `RewriteConfig.Builder.modelName`).
pub const DEFAULT_REWRITE_MODEL: &str = "conv-rewrite-qwen-1.8b";

/// Default HTTP timeout.
///
/// Java used the Tea/OpenAPI default `RuntimeOptions` (no explicit timeout was ever set by
/// `BailianClient`), so 30s matches the sibling backends' default in this crate.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Default retry budget, matching the sibling backends' `RetryInterceptor` default.
pub const DEFAULT_MAX_RETRIES: u32 = 3;

/// Rerank settings for Bailian retrieval.
///
/// Ports `RerankConfig`: `modelName` defaults to [`DEFAULT_RERANK_MODEL`], `rerankMinScore`
/// must be within `[0.01, 1.00]` and `rerankTopN` within `[1, 20]` (both optional).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RerankConfig {
    /// Rerank model name. Java defaulted this to `gte-rerank-hybrid`.
    pub model_name: String,
    /// Minimum score threshold for reranking (`0.01..=1.00`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerank_min_score: Option<f32>,
    /// Top N results after reranking (`1..=20`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerank_top_n: Option<i32>,
}

impl Default for RerankConfig {
    fn default() -> Self {
        Self {
            model_name: DEFAULT_RERANK_MODEL.to_string(),
            rerank_min_score: None,
            rerank_top_n: None,
        }
    }
}

impl RerankConfig {
    /// Starts a builder with the Java defaults.
    pub fn builder() -> RerankConfigBuilder {
        RerankConfigBuilder::default()
    }
}

/// Consuming builder for [`RerankConfig`].
#[derive(Debug, Clone)]
pub struct RerankConfigBuilder {
    model_name: String,
    rerank_min_score: Option<f32>,
    rerank_top_n: Option<i32>,
}

impl Default for RerankConfigBuilder {
    fn default() -> Self {
        Self {
            model_name: DEFAULT_RERANK_MODEL.to_string(),
            rerank_min_score: None,
            rerank_top_n: None,
        }
    }
}

impl RerankConfigBuilder {
    /// Sets the rerank model name.
    pub fn model_name(mut self, model_name: impl Into<String>) -> Self {
        self.model_name = model_name.into();
        self
    }

    /// Sets the minimum score (validated in [`Self::build`]).
    pub fn rerank_min_score(mut self, score: f32) -> Self {
        self.rerank_min_score = Some(score);
        self
    }

    /// Sets the top-N (validated in [`Self::build`]).
    pub fn rerank_top_n(mut self, top_n: i32) -> Self {
        self.rerank_top_n = Some(top_n);
        self
    }

    /// Validates and builds the configuration.
    pub fn build(self) -> Result<RerankConfig> {
        if let Some(score) = self.rerank_min_score {
            if !(0.01..=1.00).contains(&score) {
                return Err(RagError::InvalidArgument(
                    "rerankMinScore must be between 0.01 and 1.00".to_string(),
                ));
            }
        }
        if let Some(top_n) = self.rerank_top_n {
            if !(1..=20).contains(&top_n) {
                return Err(RagError::InvalidArgument(
                    "rerankTopN must be between 1 and 20".to_string(),
                ));
            }
        }
        Ok(RerankConfig {
            model_name: self.model_name,
            rerank_min_score: self.rerank_min_score,
            rerank_top_n: self.rerank_top_n,
        })
    }
}

/// Multi-turn query rewrite settings.
///
/// Ports `RewriteConfig`: a single `modelName` defaulting to [`DEFAULT_REWRITE_MODEL`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewriteConfig {
    /// Rewrite model name.
    pub model_name: String,
}

impl Default for RewriteConfig {
    fn default() -> Self {
        Self {
            model_name: DEFAULT_REWRITE_MODEL.to_string(),
        }
    }
}

impl RewriteConfig {
    /// Starts a builder with the Java defaults.
    pub fn builder() -> RewriteConfigBuilder {
        RewriteConfigBuilder::default()
    }
}

/// Consuming builder for [`RewriteConfig`].
#[derive(Debug, Clone)]
pub struct RewriteConfigBuilder {
    model_name: String,
}

impl Default for RewriteConfigBuilder {
    fn default() -> Self {
        Self {
            model_name: DEFAULT_REWRITE_MODEL.to_string(),
        }
    }
}

impl RewriteConfigBuilder {
    /// Sets the rewrite model name.
    pub fn model_name(mut self, model_name: impl Into<String>) -> Self {
        self.model_name = model_name.into();
        self
    }

    /// Builds the configuration (Java performed no extra validation here).
    pub fn build(self) -> Result<RewriteConfig> {
        Ok(RewriteConfig {
            model_name: self.model_name,
        })
    }
}

/// Bailian client configuration.
///
/// Ports `BailianConfig`. `access_key_id` / `access_key_secret` / `workspace_id` are required;
/// `index_id` is optional at construction time but required to retrieve.
#[derive(Debug, Clone, PartialEq)]
pub struct BailianConfig {
    /// Alibaba Cloud access key id.
    pub access_key_id: String,
    /// Alibaba Cloud access key secret (used for ACS4 request signing).
    pub access_key_secret: String,
    /// Bailian workspace id; also the first path segment of the retrieve URL.
    pub workspace_id: String,
    /// Knowledge base index id; the `IndexId` request body field.
    pub index_id: Option<String>,
    /// OpenAPI endpoint, without scheme ([`DEFAULT_ENDPOINT`] by default).
    pub endpoint: String,
    /// Vector-retrieval top-K (`0..=100`).
    pub dense_similarity_top_k: Option<i32>,
    /// Keyword-retrieval top-K (`0..=100`).
    pub sparse_similarity_top_k: Option<i32>,
    /// Whether to enable reranking.
    pub enable_reranking: Option<bool>,
    /// Rerank settings; when present a `Rerank` array is sent.
    pub rerank_config: Option<RerankConfig>,
    /// Whether to enable multi-turn query rewrite.
    pub enable_rewrite: Option<bool>,
    /// Rewrite settings; when present a `Rewrite` array is sent.
    pub rewrite_config: Option<RewriteConfig>,
    /// Search filters, each a flat `key -> value` map.
    pub search_filters: Option<Vec<HashMap<String, String>>>,
    /// Whether Bailian should persist the retriever history.
    pub save_retriever_history: Option<bool>,
    /// HTTP timeout.
    pub timeout: Duration,
    /// Retry budget.
    pub max_retries: u32,
}

impl BailianConfig {
    /// Starts a builder with the Java defaults.
    pub fn builder() -> BailianConfigBuilder {
        BailianConfigBuilder::default()
    }

    /// The retrieve URL: `https://{endpoint}/{urlEncodedWorkspaceId}/index/retrieve`.
    ///
    /// The SDK built the path as `/` + `getEncodeParam(workspaceId)` + `/index/retrieve`; the
    /// workspace segment is percent-encoded (verified: a space becomes `%20`, `/` becomes
    /// `%2F`). The `host` header is the bare endpoint, without scheme.
    pub fn retrieve_url(&self) -> String {
        format!(
            "https://{}/{}{}",
            self.endpoint,
            super::signing::percent_encode(&self.workspace_id),
            super::RETRIEVE_PATH
        )
    }
}

/// Consuming builder for [`BailianConfig`].
#[derive(Debug, Clone, Default)]
pub struct BailianConfigBuilder {
    access_key_id: Option<String>,
    access_key_secret: Option<String>,
    workspace_id: Option<String>,
    index_id: Option<String>,
    endpoint: Option<String>,
    dense_similarity_top_k: Option<i32>,
    sparse_similarity_top_k: Option<i32>,
    enable_reranking: Option<bool>,
    rerank_config: Option<RerankConfig>,
    enable_rewrite: Option<bool>,
    rewrite_config: Option<RewriteConfig>,
    search_filters: Option<Vec<HashMap<String, String>>>,
    save_retriever_history: Option<bool>,
    timeout: Option<Duration>,
    max_retries: Option<u32>,
}

impl BailianConfigBuilder {
    /// Sets the access key id (required).
    pub fn access_key_id(mut self, value: impl Into<String>) -> Self {
        self.access_key_id = Some(value.into());
        self
    }

    /// Sets the access key secret (required).
    pub fn access_key_secret(mut self, value: impl Into<String>) -> Self {
        self.access_key_secret = Some(value.into());
        self
    }

    /// Sets the workspace id (required).
    pub fn workspace_id(mut self, value: impl Into<String>) -> Self {
        self.workspace_id = Some(value.into());
        self
    }

    /// Sets the knowledge base index id (optional here; required to retrieve).
    pub fn index_id(mut self, value: impl Into<String>) -> Self {
        self.index_id = Some(value.into());
        self
    }

    /// Sets the endpoint (defaults to [`DEFAULT_ENDPOINT`]).
    pub fn endpoint(mut self, value: impl Into<String>) -> Self {
        self.endpoint = Some(value.into());
        self
    }

    /// Sets the dense (vector) similarity top-K; must be within `[0, 100]`.
    pub fn dense_similarity_top_k(mut self, value: i32) -> Self {
        self.dense_similarity_top_k = Some(value);
        self
    }

    /// Sets the sparse (keyword) similarity top-K; must be within `[0, 100]`.
    pub fn sparse_similarity_top_k(mut self, value: i32) -> Self {
        self.sparse_similarity_top_k = Some(value);
        self
    }

    /// Enables or disables reranking.
    pub fn enable_reranking(mut self, value: bool) -> Self {
        self.enable_reranking = Some(value);
        self
    }

    /// Sets the rerank configuration.
    pub fn rerank_config(mut self, value: RerankConfig) -> Self {
        self.rerank_config = Some(value);
        self
    }

    /// Enables or disables multi-turn query rewrite.
    pub fn enable_rewrite(mut self, value: bool) -> Self {
        self.enable_rewrite = Some(value);
        self
    }

    /// Sets the rewrite configuration.
    pub fn rewrite_config(mut self, value: RewriteConfig) -> Self {
        self.rewrite_config = Some(value);
        self
    }

    /// Replaces the search filters.
    pub fn search_filters(mut self, filters: Vec<HashMap<String, String>>) -> Self {
        self.search_filters = Some(filters);
        self
    }

    /// Appends one search filter entry.
    pub fn add_search_filter(mut self, filter: HashMap<String, String>) -> Self {
        self.search_filters.get_or_insert_with(Vec::new).push(filter);
        self
    }

    /// Sets whether Bailian should persist the retriever history.
    pub fn save_retriever_history(mut self, value: bool) -> Self {
        self.save_retriever_history = Some(value);
        self
    }

    /// Sets the HTTP timeout.
    pub fn timeout(mut self, value: Duration) -> Self {
        self.timeout = Some(value);
        self
    }

    /// Sets the retry budget.
    pub fn max_retries(mut self, value: u32) -> Self {
        self.max_retries = Some(value);
        self
    }

    /// Validates and builds the configuration.
    ///
    /// Reproduces the Java checks: the three credentials must be non-blank, each top-K must be
    /// within `[0, 100]`, and `denseSimilarityTopK + sparseSimilarityTopK <= 200` when both are
    /// set. Error messages match the Java text modulo `IllegalArgumentException` →
    /// [`RagError::InvalidArgument`].
    pub fn build(self) -> Result<BailianConfig> {
        let access_key_id = required(self.access_key_id, "AccessKeyId")?;
        let access_key_secret = required(self.access_key_secret, "AccessKeySecret")?;
        let workspace_id = required(self.workspace_id, "WorkspaceId")?;

        if let Some(top_k) = self.dense_similarity_top_k {
            if !(0..=100).contains(&top_k) {
                return Err(RagError::InvalidArgument(
                    "denseSimilarityTopK must be between 0 and 100".to_string(),
                ));
            }
        }
        if let Some(top_k) = self.sparse_similarity_top_k {
            if !(0..=100).contains(&top_k) {
                return Err(RagError::InvalidArgument(
                    "sparseSimilarityTopK must be between 0 and 100".to_string(),
                ));
            }
        }
        if let (Some(dense), Some(sparse)) =
            (self.dense_similarity_top_k, self.sparse_similarity_top_k)
        {
            if dense + sparse > 200 {
                return Err(RagError::InvalidArgument(
                    "denseSimilarityTopK + sparseSimilarityTopK must be <= 200".to_string(),
                ));
            }
        }

        Ok(BailianConfig {
            access_key_id,
            access_key_secret,
            workspace_id,
            index_id: self.index_id,
            endpoint: self.endpoint.unwrap_or_else(|| DEFAULT_ENDPOINT.to_string()),
            dense_similarity_top_k: self.dense_similarity_top_k,
            sparse_similarity_top_k: self.sparse_similarity_top_k,
            enable_reranking: self.enable_reranking,
            rerank_config: self.rerank_config,
            enable_rewrite: self.enable_rewrite,
            rewrite_config: self.rewrite_config,
            search_filters: self.search_filters,
            save_retriever_history: self.save_retriever_history,
            timeout: self.timeout.unwrap_or(DEFAULT_TIMEOUT),
            max_retries: self.max_retries.unwrap_or(DEFAULT_MAX_RETRIES),
        })
    }
}

/// Pulls a required, non-blank value out of an optional builder field.
fn required(value: Option<String>, name: &str) -> Result<String> {
    match value {
        Some(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(RagError::InvalidArgument(format!(
            "{name} cannot be null or empty"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_builder() -> BailianConfigBuilder {
        BailianConfig::builder()
            .access_key_id("ak")
            .access_key_secret("sk")
            .workspace_id("llm-abc")
    }

    #[test]
    fn builder_applies_java_defaults() {
        let cfg = base_builder().build().unwrap();
        assert_eq!(cfg.endpoint, DEFAULT_ENDPOINT);
        assert_eq!(cfg.index_id, None);
        assert_eq!(cfg.dense_similarity_top_k, None);
        assert_eq!(cfg.enable_reranking, None);
        assert_eq!(cfg.save_retriever_history, None);
        assert_eq!(cfg.timeout, DEFAULT_TIMEOUT);
        assert_eq!(cfg.max_retries, DEFAULT_MAX_RETRIES);
    }

    #[test]
    fn rerank_and_rewrite_defaults_match_java() {
        assert_eq!(RerankConfig::default().model_name, "gte-rerank-hybrid");
        assert_eq!(RewriteConfig::default().model_name, "conv-rewrite-qwen-1.8b");
        assert_eq!(RerankConfig::builder().build().unwrap().rerank_top_n, None);
        assert_eq!(
            RewriteConfig::builder().build().unwrap().model_name,
            "conv-rewrite-qwen-1.8b"
        );
    }

    #[test]
    fn required_credentials_are_validated() {
        assert!(BailianConfig::builder().build().is_err());
        assert!(BailianConfig::builder()
            .access_key_id("  ")
            .access_key_secret("sk")
            .workspace_id("w")
            .build()
            .is_err());
        assert!(BailianConfig::builder()
            .access_key_id("ak")
            .access_key_secret("")
            .workspace_id("w")
            .build()
            .is_err());
        assert!(BailianConfig::builder()
            .access_key_id("ak")
            .access_key_secret("sk")
            .workspace_id(" ")
            .build()
            .is_err());
    }

    #[test]
    fn index_id_is_optional_at_config_time() {
        let cfg = base_builder().build().unwrap();
        assert!(cfg.index_id.is_none());
        let cfg = base_builder().index_id("idx").build().unwrap();
        assert_eq!(cfg.index_id.as_deref(), Some("idx"));
    }

    #[test]
    fn top_k_ranges_are_enforced() {
        assert!(base_builder().dense_similarity_top_k(101).build().is_err());
        assert!(base_builder().dense_similarity_top_k(-1).build().is_err());
        assert!(base_builder().sparse_similarity_top_k(101).build().is_err());
        assert!(base_builder().sparse_similarity_top_k(0).build().is_ok());
        assert!(base_builder().dense_similarity_top_k(100).build().is_ok());
    }

    #[test]
    fn combined_top_k_check_is_unreachable_behind_the_range_guards() {
        // Java validated each top-K in the builder setter ([0, 100]) *and* the sum in the
        // constructor (<= 200). Since both caps are 100, the sum can never exceed 200, so the
        // range guard always fires first — this port keeps both checks and the same precedence.
        let out_of_range = base_builder()
            .dense_similarity_top_k(100)
            .sparse_similarity_top_k(101)
            .build()
            .unwrap_err();
        assert!(
            out_of_range
                .to_string()
                .contains("sparseSimilarityTopK must be between 0 and 100"),
            "{out_of_range}"
        );

        // The maximum representable combination is exactly at the boundary and is accepted.
        let boundary = base_builder()
            .dense_similarity_top_k(100)
            .sparse_similarity_top_k(100)
            .build()
            .unwrap();
        assert_eq!(boundary.dense_similarity_top_k, Some(100));
        assert_eq!(boundary.sparse_similarity_top_k, Some(100));
    }

    #[test]
    fn rerank_ranges_are_enforced() {
        assert!(RerankConfig::builder().rerank_min_score(0.001).build().is_err());
        assert!(RerankConfig::builder().rerank_min_score(1.5).build().is_err());
        assert!(RerankConfig::builder().rerank_min_score(0.01).build().is_ok());
        assert!(RerankConfig::builder().rerank_top_n(0).build().is_err());
        assert!(RerankConfig::builder().rerank_top_n(21).build().is_err());
        assert!(RerankConfig::builder().rerank_top_n(20).build().is_ok());
    }

    #[test]
    fn retrieve_url_percent_encodes_the_workspace_segment() {
        let cfg = base_builder().build().unwrap();
        assert_eq!(
            cfg.retrieve_url(),
            "https://bailian.cn-beijing.aliyuncs.com/llm-abc/index/retrieve"
        );

        let cfg = BailianConfig::builder()
            .access_key_id("ak")
            .access_key_secret("sk")
            .workspace_id("a b/c")
            .endpoint("bailian-vpc.cn-beijing.aliyuncs.com")
            .build()
            .unwrap();
        assert_eq!(
            cfg.retrieve_url(),
            "https://bailian-vpc.cn-beijing.aliyuncs.com/a%20b%2Fc/index/retrieve"
        );
    }
}
