//! HayStack retrieve request-body construction.
//!
//! Ports the body-building block of `HayStackClient.retrieve`. Java assembled a
//! `HashMap<String, Object>` with `if (x != null)` guards; building a [`HayStackRequest`] with
//! the same guards emits exactly the same key set.
//!
//! The only precedence rule is for the score threshold: the per-call value wins over the
//! configured one, which is why it is resolved before the request is built.

use crate::haystack::config::HayStackConfig;
use crate::haystack::model::HayStackRequest;

/// Builds the JSON body for the Haystack retrieval endpoint.
///
/// `top_k` and `score_threshold` override the configured values when supplied, mirroring
/// `retrieve(query, topK, scoreThreshold)`.
pub fn build_retrieve_request_body(
    config: &HayStackConfig,
    query: &str,
    top_k: Option<i32>,
    score_threshold: Option<f64>,
) -> HayStackRequest {
    HayStackRequest {
        query: query.to_string(),
        top_k: top_k.or(config.top_k),
        scale_score: config.scale_score,
        return_embedding: config.return_embedding,
        score_threshold: score_threshold.or(config.score_threshold),
        group_by: config.group_by.clone(),
        group_size: config.group_size,
        filters: config.filters.clone(),
        query_embedding: config.query_embedding.clone(),
        query_sparse_embedding: config.query_sparse_embedding.clone(),
        documents: config.documents.clone(),
        retrieved_documents: config.retrieved_documents.clone(),
        window_size: config.window_size,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::haystack::model::{HayStackDocument, SparseEmbedding};
    use std::collections::HashMap;

    fn config() -> HayStackConfig {
        HayStackConfig::builder()
            .base_url("http://localhost:8080/retrieve")
            .build()
            .unwrap()
    }

    #[test]
    fn default_top_k_is_forwarded_from_the_config() {
        let request = build_retrieve_request_body(&config(), "test query", None, None);
        assert_eq!(request.query, "test query");
        assert_eq!(request.top_k, Some(3));
    }

    #[test]
    fn call_site_top_k_overrides_the_config() {
        let request = build_retrieve_request_body(&config(), "q", Some(10), None);
        assert_eq!(request.top_k, Some(10));
    }

    #[test]
    fn optional_knobs_are_forwarded_from_the_config() {
        let config = HayStackConfig::builder()
            .base_url("http://x")
            .top_k(5)
            .scale_score(true)
            .return_embedding(true)
            .window_size(2)
            .build()
            .unwrap();
        let request = build_retrieve_request_body(&config, "q", None, None);
        assert_eq!(request.scale_score, Some(true));
        assert_eq!(request.return_embedding, Some(true));
        assert_eq!(request.window_size, Some(2));

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["scale_score"], true);
        assert_eq!(json["return_embedding"], true);
        assert_eq!(json["window_size"], 2);
    }

    #[test]
    fn score_threshold_prefers_the_call_site_value() {
        let config = HayStackConfig::builder()
            .base_url("http://x")
            .score_threshold(0.3)
            .build()
            .unwrap();
        assert_eq!(
            build_retrieve_request_body(&config, "q", None, None).score_threshold,
            Some(0.3)
        );
        assert_eq!(
            build_retrieve_request_body(&config, "q", None, Some(0.9)).score_threshold,
            Some(0.9)
        );
    }

    #[test]
    fn filters_and_group_knobs_are_forwarded() {
        let mut filters = HashMap::new();
        filters.insert("field".to_string(), serde_json::json!("value"));
        let config = HayStackConfig::builder()
            .base_url("http://x")
            .group_by("meta.type")
            .group_size(2)
            .filters(filters.clone())
            .build()
            .unwrap();
        let request = build_retrieve_request_body(&config, "q", None, None);
        assert_eq!(request.group_by.as_deref(), Some("meta.type"));
        assert_eq!(request.group_size, Some(2));
        assert_eq!(request.filters, Some(filters));

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["group_by"], "meta.type");
        assert_eq!(json["group_size"], 2);
        assert_eq!(json["filters"]["field"], "value");
    }

    #[test]
    fn embeddings_and_documents_are_forwarded_when_present() {
        let sparse = SparseEmbedding::builder()
            .indices(vec![1])
            .values(vec![0.5])
            .build();
        let document = HayStackDocument {
            id: Some("d1".to_string()),
            ..Default::default()
        };
        let config = HayStackConfig::builder()
            .base_url("http://x")
            .query_embedding(vec![0.1, 0.2])
            .query_sparse_embedding(sparse.clone())
            .documents(vec![document.clone()])
            .retrieved_documents(vec![document.clone()])
            .build()
            .unwrap();
        let request = build_retrieve_request_body(&config, "q", None, None);
        assert_eq!(request.query_embedding.as_ref().unwrap().len(), 2);
        assert_eq!(request.query_sparse_embedding, Some(sparse));
        assert_eq!(request.documents.as_ref().unwrap().len(), 1);
        assert_eq!(request.retrieved_documents.as_ref().unwrap().len(), 1);

        let json = serde_json::to_value(&request).unwrap();
        // `query_embedding` is `Vec<f32>`; `json!([0.1, 0.2])` would build `f64` literals, so the
        // expected value is produced from `f32` to keep the comparison exact.
        assert_eq!(
            json["query_embedding"],
            serde_json::to_value(vec![0.1f32, 0.2f32]).unwrap()
        );
        assert_eq!(
            json["query_sparse_embedding"]["indices"],
            serde_json::json!([1])
        );
        assert_eq!(json["documents"][0]["id"], "d1");
        assert_eq!(json["retrieved_documents"][0]["id"], "d1");
    }

    #[test]
    fn unset_knobs_are_omitted_from_the_body() {
        let request = build_retrieve_request_body(&config(), "q", None, None);
        let json = serde_json::to_value(&request).unwrap();
        let object = json.as_object().unwrap();
        assert_eq!(object.len(), 2);
        assert!(object.contains_key("query"));
        assert!(object.contains_key("top_k"));
        for key in [
            "scale_score",
            "return_embedding",
            "score_threshold",
            "group_by",
            "filters",
            "query_embedding",
            "window_size",
        ] {
            assert!(!object.contains_key(key), "{key} should be omitted");
        }
    }
}
