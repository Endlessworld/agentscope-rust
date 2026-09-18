//! Dify retrieve request-body construction.
//!
//! Ports the body-building block of Java `DifyRAGClient.retrieve`: a `query` string plus a
//! `retrieval_model` object assembled from [`DifyRAGConfig`]. Building the body as a plain
//! `serde_json::Value` keeps key order/omission semantics identical to the Java `HashMap`
//! logic (e.g. `score_threshold` is only emitted when `score_threshold_enabled` is true).

use serde_json::{json, Map, Value};

use crate::dify::config::DifyRAGConfig;

/// Builds the JSON request body for `POST /datasets/{dataset_id}/retrieve`.
///
/// `limit` overrides the configured `top_k` when supplied, matching Java's
/// `int topK = limit != null ? limit : config.getTopK()`.
pub fn build_retrieve_request_body(config: &DifyRAGConfig, query: &str, limit: Option<i32>) -> Value {
    let mut retrieval_model = Map::new();

    retrieval_model.insert(
        "search_method".to_string(),
        Value::String(config.retrieval_mode.value().to_string()),
    );
    retrieval_model.insert(
        "top_k".to_string(),
        json!(limit.unwrap_or(config.top_k)),
    );

    // score_threshold is emitted only when a positive threshold is configured.
    if config.score_threshold > 0.0 {
        retrieval_model.insert("score_threshold_enabled".to_string(), json!(true));
        retrieval_model.insert("score_threshold".to_string(), json!(config.score_threshold));
    } else {
        retrieval_model.insert("score_threshold_enabled".to_string(), json!(false));
    }

    if config.enable_rerank {
        retrieval_model.insert("reranking_enable".to_string(), json!(true));
        if let Some(rerank) = &config.rerank_config {
            if !rerank.is_empty() {
                let mut reranking_model = Map::new();
                if let Some(provider) = &rerank.provider_name {
                    reranking_model.insert(
                        "reranking_provider_name".to_string(),
                        Value::String(provider.clone()),
                    );
                }
                if let Some(model) = &rerank.model_name {
                    reranking_model
                        .insert("reranking_model_name".to_string(), Value::String(model.clone()));
                }
                retrieval_model.insert(
                    "reranking_model".to_string(),
                    Value::Object(reranking_model),
                );
            }
        }
    } else {
        retrieval_model.insert("reranking_enable".to_string(), json!(false));
    }

    if let Some(weights) = config.weights {
        retrieval_model.insert("weights".to_string(), json!(weights));
    }

    if let Some(filter) = &config.metadata_filter {
        let conditions: Vec<Value> = filter
            .conditions
            .iter()
            .map(|c| {
                json!({
                    "name": c.name,
                    "comparison_operator": c.comparison_operator,
                    "value": c.value,
                })
            })
            .collect();

        retrieval_model.insert(
            "metadata_filtering_conditions".to_string(),
            json!({
                "logical_operator": filter.logical_operator,
                "conditions": conditions,
            }),
        );
    }

    json!({
        "query": query,
        "retrieval_model": Value::Object(retrieval_model),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dify::config::{MetadataFilter, MetadataFilterCondition, RerankConfig, RetrievalMode};

    fn base_config() -> DifyRAGConfig {
        DifyRAGConfig::builder()
            .api_key("k")
            .dataset_id("ds")
            .build_or_panic()
    }

    #[test]
    fn default_body_matches_java_shape() {
        let body = build_retrieve_request_body(&base_config(), "hello", None);
        assert_eq!(body["query"], "hello");
        let rm = &body["retrieval_model"];
        assert_eq!(rm["search_method"], "hybrid_search");
        assert_eq!(rm["top_k"], 10);
        assert_eq!(rm["score_threshold_enabled"], false);
        assert_eq!(rm["reranking_enable"], false);
        assert!(rm.get("score_threshold").is_none());
        assert!(rm.get("weights").is_none());
        assert!(rm.get("metadata_filtering_conditions").is_none());
    }

    #[test]
    fn limit_overrides_configured_top_k() {
        let cfg = DifyRAGConfig::builder()
            .api_key("k")
            .dataset_id("ds")
            .top_k(20)
            .build_or_panic();
        let body = build_retrieve_request_body(&cfg, "q", Some(3));
        assert_eq!(body["retrieval_model"]["top_k"], 3);

        let body = build_retrieve_request_body(&cfg, "q", None);
        assert_eq!(body["retrieval_model"]["top_k"], 20);
    }

    #[test]
    fn positive_threshold_enables_the_threshold_fields() {
        let cfg = DifyRAGConfig::builder()
            .api_key("k")
            .dataset_id("ds")
            .score_threshold(0.75)
            .build_or_panic();
        let body = build_retrieve_request_body(&cfg, "q", None);
        let rm = &body["retrieval_model"];
        assert_eq!(rm["score_threshold_enabled"], true);
        assert_eq!(rm["score_threshold"], 0.75);
    }

    #[test]
    fn zero_threshold_leaves_the_threshold_disabled() {
        let body = build_retrieve_request_body(&base_config(), "q", None);
        assert_eq!(body["retrieval_model"]["score_threshold_enabled"], false);
        assert!(body["retrieval_model"].get("score_threshold").is_none());
    }

    #[test]
    fn rerank_is_included_only_when_enabled_and_non_empty() {
        let cfg = DifyRAGConfig::builder()
            .api_key("k")
            .dataset_id("ds")
            .enable_rerank(true)
            .rerank_config(RerankConfig::new().with_provider_name("cohere").with_model_name("rerank-v2"))
            .build_or_panic();
        let body = build_retrieve_request_body(&cfg, "q", None);
        let rm = &body["retrieval_model"];
        assert_eq!(rm["reranking_enable"], true);
        assert_eq!(rm["reranking_model"]["reranking_provider_name"], "cohere");
        assert_eq!(rm["reranking_model"]["reranking_model_name"], "rerank-v2");

        // Enabled but empty rerank config -> no `reranking_model` key, like Java.
        let cfg = DifyRAGConfig::builder()
            .api_key("k")
            .dataset_id("ds")
            .enable_rerank(true)
            .rerank_config(RerankConfig::new())
            .build_or_panic();
        let body = build_retrieve_request_body(&cfg, "q", None);
        assert!(body["retrieval_model"].get("reranking_model").is_none());
    }

    #[test]
    fn weights_are_included_when_set() {
        let cfg = DifyRAGConfig::builder()
            .api_key("k")
            .dataset_id("ds")
            .retrieval_mode(RetrievalMode::HybridSearch)
            .weights(0.4)
            .build_or_panic();
        let body = build_retrieve_request_body(&cfg, "q", None);
        assert_eq!(body["retrieval_model"]["weights"], 0.4);
    }

    #[test]
    fn metadata_filter_conditions_are_serialized() {
        let cfg = DifyRAGConfig::builder()
            .api_key("k")
            .dataset_id("ds")
            .metadata_filter(
                MetadataFilter::new()
                    .with_logical_operator("or")
                    .add_condition(MetadataFilterCondition::new("category", "equals", "technical"))
                    .add_condition(MetadataFilterCondition::new("language", "contains", "en")),
            )
            .build_or_panic();
        let body = build_retrieve_request_body(&cfg, "q", None);
        let meta = &body["retrieval_model"]["metadata_filtering_conditions"];
        assert_eq!(meta["logical_operator"], "or");
        assert_eq!(meta["conditions"][0]["name"], "category");
        assert_eq!(meta["conditions"][0]["comparison_operator"], "equals");
        assert_eq!(meta["conditions"][0]["value"], "technical");
        assert_eq!(meta["conditions"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn retrieval_mode_reaches_the_wire_value() {
        let cfg = DifyRAGConfig::builder()
            .api_key("k")
            .dataset_id("ds")
            .retrieval_mode(RetrievalMode::FullTextSearch)
            .build_or_panic();
        let body = build_retrieve_request_body(&cfg, "q", None);
        assert_eq!(body["retrieval_model"]["search_method"], "full_text_search");
    }
}
