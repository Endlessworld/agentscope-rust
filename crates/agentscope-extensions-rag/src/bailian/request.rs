//! `RetrieveRequest` assembly for Bailian.
//!
//! Ports the request-building block of `BailianClient.retrieve`. The Java code mutates a
//! `RetrieveRequest` bean with `if (x != null)` guards; building the Rust struct with the same
//! guards preserves exactly which fields reach the wire.
//!
//! Two Java quirks are reproduced deliberately:
//!
//! * `denseSimilarityTopK` falls back to the per-call `limit` only when the config leaves it
//!   unset **and** `limit > 0`;
//! * the `Rerank` / `Rewrite` arrays are always created when the corresponding config object is
//!   present — even if the model name is blank — because Java unconditionally calls
//!   `rerankList.add(rerank)`. A blank model name simply leaves that entry empty.

use crate::bailian::config::BailianConfig;
use crate::bailian::model::{QueryHistorySpec, RerankSpec, RetrieveRequest, RewriteSpec};
use crate::bailian::query_history::QueryHistoryEntry;

/// Builds the retrieve request body.
///
/// `limit` and `conversation_history` come from the call site (Java `RetrieveConfig`), while the
/// remaining retrieval knobs come from the client configuration.
pub fn build_retrieve_request(
    config: &BailianConfig,
    index_id: &str,
    query: &str,
    limit: Option<usize>,
    conversation_history: Option<&[QueryHistoryEntry]>,
) -> RetrieveRequest {
    let mut request = RetrieveRequest {
        index_id: index_id.to_string(),
        query: query.to_string(),
        ..Default::default()
    };

    // Dense top-K: config wins, otherwise fall back to a positive per-call limit.
    request.dense_similarity_top_k = config.dense_similarity_top_k.or_else(|| {
        limit.and_then(|limit| i32::try_from(limit).ok()).filter(|limit| *limit > 0)
    });

    request.sparse_similarity_top_k = config.sparse_similarity_top_k;
    request.enable_reranking = config.enable_reranking;

    if let Some(rerank) = &config.rerank_config {
        request.rerank = Some(vec![RerankSpec {
            // Java used `StringUtils.isEmpty`, i.e. null *or* the empty string is skipped.
            model_name: non_empty(&rerank.model_name),
        }]);
        request.rerank_min_score = rerank.rerank_min_score;
        request.rerank_top_n = rerank.rerank_top_n;
    }

    request.enable_rewrite = config.enable_rewrite;
    if let Some(rewrite) = &config.rewrite_config {
        request.rewrite = Some(vec![RewriteSpec {
            model_name: non_empty(&rewrite.model_name),
        }]);
    }

    if let Some(history) = conversation_history.filter(|history| !history.is_empty()) {
        request.query_history = Some(
            history
                .iter()
                .map(|entry| QueryHistorySpec {
                    role: entry.role,
                    content: entry.content.clone(),
                })
                .collect(),
        );
    }

    if let Some(filters) = config.search_filters.as_ref().filter(|f| !f.is_empty()) {
        request.search_filters = Some(filters.clone());
    }

    request.save_retriever_history = config.save_retriever_history;

    request
}

/// `Some(value)` when the string is non-blank, otherwise `None` (Java `StringUtils.isEmpty`).
fn non_empty(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bailian::config::BailianConfig;
    use crate::bailian::query_history::{QueryHistoryEntry, QueryRole};
    use std::collections::HashMap;

    fn config() -> BailianConfig {
        BailianConfig::builder()
            .access_key_id("ak")
            .access_key_secret("sk")
            .workspace_id("w")
            .index_id("idx")
            .build()
            .unwrap()
    }

    #[test]
    fn minimal_request_sends_only_required_fields() {
        let request = build_retrieve_request(&config(), "idx", "q", None, None);
        assert_eq!(request.index_id, "idx");
        assert_eq!(request.query, "q");
        assert_eq!(request.dense_similarity_top_k, None);
        assert_eq!(request.rerank, None);
        assert_eq!(request.rewrite, None);
        assert_eq!(request.query_history, None);
        assert_eq!(request.search_filters, None);

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json.as_object().unwrap().len(), 2);
    }

    #[test]
    fn limit_fills_dense_top_k_only_when_config_is_unset() {
        let request = build_retrieve_request(&config(), "idx", "q", Some(5), None);
        assert_eq!(request.dense_similarity_top_k, Some(5));

        // Non-positive limits are ignored, mirroring `limit != null && limit > 0`.
        let request = build_retrieve_request(&config(), "idx", "q", Some(0), None);
        assert_eq!(request.dense_similarity_top_k, None);

        let configured = BailianConfig::builder()
            .access_key_id("ak")
            .access_key_secret("sk")
            .workspace_id("w")
            .dense_similarity_top_k(50)
            .build()
            .unwrap();
        let request = build_retrieve_request(&configured, "idx", "q", Some(5), None);
        assert_eq!(request.dense_similarity_top_k, Some(50));
    }

    #[test]
    fn rerank_block_is_emitted_with_model_score_and_top_n() {
        let cfg = BailianConfig::builder()
            .access_key_id("ak")
            .access_key_secret("sk")
            .workspace_id("w")
            .enable_reranking(true)
            .rerank_config(
                crate::bailian::config::RerankConfig::builder()
                    .model_name("gte-rerank-hybrid")
                    .rerank_min_score(0.2)
                    .rerank_top_n(5)
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap();
        let request = build_retrieve_request(&cfg, "idx", "q", None, None);
        assert_eq!(request.enable_reranking, Some(true));
        assert_eq!(
            request.rerank.as_ref().unwrap()[0].model_name.as_deref(),
            Some("gte-rerank-hybrid")
        );
        assert_eq!(request.rerank_min_score, Some(0.2));
        assert_eq!(request.rerank_top_n, Some(5));

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["rerank"][0]["modelName"], "gte-rerank-hybrid");
        assert_eq!(json["rerankTopN"], 5);

        // `rerank_min_score` is an f32 (Java `Float`); `to_value` widens it to f64, but the
        // serialized wire form is `0.2`, matching the SDK's `Float` rendering.
        assert!(
            serde_json::to_string(&request)
                .unwrap()
                .contains(r#""rerankMinScore":0.2"#),
            "{}",
            serde_json::to_string(&request).unwrap()
        );
    }

    #[test]
    fn rerank_entry_is_still_emitted_when_model_name_is_blank() {
        // Java unconditionally does `rerankList.add(rerank)`.
        let cfg = BailianConfig::builder()
            .access_key_id("ak")
            .access_key_secret("sk")
            .workspace_id("w")
            .rerank_config(crate::bailian::config::RerankConfig::builder().model_name("").build().unwrap())
            .build()
            .unwrap();
        let request = build_retrieve_request(&cfg, "idx", "q", None, None);
        assert_eq!(request.rerank.as_ref().unwrap()[0].model_name, None);
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["rerank"], serde_json::json!([{}]));
    }

    #[test]
    fn rewrite_block_is_emitted_from_config() {
        let cfg = BailianConfig::builder()
            .access_key_id("ak")
            .access_key_secret("sk")
            .workspace_id("w")
            .enable_rewrite(true)
            .rewrite_config(crate::bailian::config::RewriteConfig::default())
            .build()
            .unwrap();
        let request = build_retrieve_request(&cfg, "idx", "q", None, None);
        assert_eq!(request.enable_rewrite, Some(true));
        assert_eq!(
            request.rewrite.as_ref().unwrap()[0].model_name.as_deref(),
            Some("conv-rewrite-qwen-1.8b")
        );
    }

    #[test]
    fn conversation_history_is_forwarded_when_not_empty() {
        let history = vec![
            QueryHistoryEntry::user("hello"),
            QueryHistoryEntry::assistant("hi"),
        ];
        let request = build_retrieve_request(&config(), "idx", "q", None, Some(&history));
        let sent = request.query_history.as_ref().unwrap();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].role, QueryRole::User);
        assert_eq!(sent[1].content, "hi");

        // An empty history is omitted rather than sent as `[]`.
        let request = build_retrieve_request(&config(), "idx", "q", None, Some(&[]));
        assert_eq!(request.query_history, None);
    }

    #[test]
    fn search_filters_are_forwarded_and_empty_ones_skipped() {
        let mut filter = HashMap::new();
        filter.insert("tag".to_string(), "a".to_string());
        let cfg = BailianConfig::builder()
            .access_key_id("ak")
            .access_key_secret("sk")
            .workspace_id("w")
            .add_search_filter(filter.clone())
            .build()
            .unwrap();
        let request = build_retrieve_request(&cfg, "idx", "q", None, None);
        assert_eq!(request.search_filters.as_ref().unwrap()[0], filter);

        let cfg = BailianConfig::builder()
            .access_key_id("ak")
            .access_key_secret("sk")
            .workspace_id("w")
            .search_filters(vec![])
            .build()
            .unwrap();
        let request = build_retrieve_request(&cfg, "idx", "q", None, None);
        assert_eq!(request.search_filters, None);
    }

    #[test]
    fn save_retriever_history_is_forwarded_including_false() {
        let cfg = BailianConfig::builder()
            .access_key_id("ak")
            .access_key_secret("sk")
            .workspace_id("w")
            .save_retriever_history(false)
            .build()
            .unwrap();
        let request = build_retrieve_request(&cfg, "idx", "q", None, None);
        assert_eq!(request.save_retriever_history, Some(false));
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["saveRetrieverHistory"], false);
    }
}
