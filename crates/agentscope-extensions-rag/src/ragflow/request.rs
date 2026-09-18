//! `POST /api/v1/retrieval` request-body construction.
//!
//! Ports the body-building block of `RAGFlowClient.retrieve`. The Java version mutates a
//! `HashMap` with `if (x != null) put(...)` guards; building a `serde_json::Map` with the
//! same guards preserves exactly which keys are emitted.

use serde_json::{json, Map, Value};

use crate::ragflow::config::RAGFlowConfig;

/// Builds the JSON body for the RAGFlow retrieval endpoint.
///
/// `top_k` and `similarity_threshold` arguments override the configured values when
/// supplied, mirroring `retrieve(question, topK, similarityThreshold, metadataCondition)`.
/// `metadata_condition` overrides the configured condition when it is a non-empty object.
pub fn build_retrieval_request_body(
    config: &RAGFlowConfig,
    question: &str,
    top_k: Option<i32>,
    similarity_threshold: Option<f64>,
    metadata_condition: Option<&Value>,
) -> Value {
    let mut body = Map::new();

    // Required fields.
    body.insert("question".to_string(), Value::String(question.to_string()));
    body.insert("dataset_ids".to_string(), json!(config.dataset_ids));

    if let Some(document_ids) = config.document_ids.as_ref().filter(|ids| !ids.is_empty()) {
        body.insert("document_ids".to_string(), json!(document_ids));
    }

    match top_k.or(config.top_k) {
        Some(value) => {
            body.insert("top_k".to_string(), json!(value));
        }
        None => {}
    }

    if let Some(threshold) = similarity_threshold.or(config.similarity_threshold) {
        body.insert("similarity_threshold".to_string(), json!(threshold));
    }

    if let Some(weight) = config.vector_similarity_weight {
        body.insert("vector_similarity_weight".to_string(), json!(weight));
    }
    if let Some(page) = config.page {
        body.insert("page".to_string(), json!(page));
    }
    if let Some(page_size) = config.page_size {
        body.insert("page_size".to_string(), json!(page_size));
    }
    if let Some(use_kg) = config.use_kg {
        body.insert("use_kg".to_string(), json!(use_kg));
    }
    if let Some(toc_enhance) = config.toc_enhance {
        body.insert("toc_enhance".to_string(), json!(toc_enhance));
    }
    if let Some(rerank_id) = &config.rerank_id {
        body.insert("rerank_id".to_string(), json!(rerank_id));
    }
    if let Some(keyword) = config.keyword {
        body.insert("keyword".to_string(), json!(keyword));
    }
    if let Some(highlight) = config.highlight {
        body.insert("highlight".to_string(), json!(highlight));
    }
    if let Some(languages) = config.cross_languages.as_ref().filter(|l| !l.is_empty()) {
        body.insert("cross_languages".to_string(), json!(languages));
    }

    // Java prefers the call-site condition over the configured one, and skips empty maps.
    let condition = metadata_condition
        .filter(|v| is_non_empty_object(v))
        .or_else(|| config.metadata_condition.as_ref().filter(|v| is_non_empty_object(v)));
    if let Some(condition) = condition {
        body.insert("metadata_condition".to_string(), condition.clone());
    }

    Value::Object(body)
}

/// Whether the value is a JSON object with at least one entry (Java `!map.isEmpty()`).
fn is_non_empty_object(value: &Value) -> bool {
    matches!(value, Value::Object(map) if !map.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ragflow::config::RAGFlowConfig;

    fn config() -> RAGFlowConfig {
        RAGFlowConfig::builder()
            .api_key("k")
            .base_url("http://localhost:9380")
            .add_dataset_id("ds-1")
            .build()
            .unwrap()
    }

    #[test]
    fn minimal_body_has_question_datasets_and_defaults() {
        let body = build_retrieval_request_body(&config(), "What is RAGFlow?", None, None, None);
        assert_eq!(body["question"], json!("What is RAGFlow?"));
        assert_eq!(body["dataset_ids"], json!(["ds-1"]));
        assert_eq!(body["top_k"], json!(1024));
        assert_eq!(body["similarity_threshold"], json!(0.2));
        assert_eq!(body["vector_similarity_weight"], json!(0.3));
        assert_eq!(body["page"], json!(1));
        assert_eq!(body["page_size"], json!(30));
        assert!(body.get("document_ids").is_none());
        assert!(body.get("use_kg").is_none());
        assert!(body.get("rerank_id").is_none());
        assert!(body.get("metadata_condition").is_none());
    }

    #[test]
    fn call_site_overrides_config() {
        let body =
            build_retrieval_request_body(&config(), "q", Some(5), Some(0.75), None);
        assert_eq!(body["top_k"], json!(5));
        assert_eq!(body["similarity_threshold"], json!(0.75));
    }

    #[test]
    fn optional_flags_are_emitted_when_set() {
        let cfg = RAGFlowConfig::builder()
            .api_key("k")
            .base_url("http://x")
            .dataset_ids(["ds-1"])
            .document_ids(["doc-1"])
            .use_kg(true)
            .toc_enhance(false)
            .rerank_id("BAAI/bge-reranker-v2-m3@BAAI")
            .keyword(true)
            .highlight(false)
            .cross_languages(["Chinese"])
            .build()
            .unwrap();
        let body = build_retrieval_request_body(&cfg, "q", None, None, None);
        assert_eq!(body["document_ids"], json!(["doc-1"]));
        assert_eq!(body["use_kg"], json!(true));
        assert_eq!(body["toc_enhance"], json!(false));
        assert_eq!(body["rerank_id"], json!("BAAI/bge-reranker-v2-m3@BAAI"));
        assert_eq!(body["keyword"], json!(true));
        assert_eq!(body["highlight"], json!(false));
        assert_eq!(body["cross_languages"], json!(["Chinese"]));
    }

    #[test]
    fn metadata_condition_prefers_call_site() {
        let cfg = RAGFlowConfig::builder()
            .api_key("k")
            .base_url("http://x")
            .add_dataset_id("d")
            .metadata_condition(json!({"author": "Toby"}))
            .build()
            .unwrap();

        let from_config = build_retrieval_request_body(&cfg, "q", None, None, None);
        assert_eq!(from_config["metadata_condition"], json!({"author": "Toby"}));

        let call_site = json!({"logic": "and", "conditions": []});
        let from_call =
            build_retrieval_request_body(&cfg, "q", None, None, Some(&call_site));
        assert_eq!(from_call["metadata_condition"], call_site);
    }

    #[test]
    fn empty_metadata_condition_is_skipped() {
        let cfg = RAGFlowConfig::builder()
            .api_key("k")
            .base_url("http://x")
            .add_dataset_id("d")
            .metadata_condition(json!({}))
            .build()
            .unwrap();
        let body = build_retrieval_request_body(&cfg, "q", None, None, Some(&json!({})));
        assert!(body.get("metadata_condition").is_none());
    }

    #[test]
    fn empty_document_ids_are_skipped() {
        let cfg = RAGFlowConfig::builder()
            .api_key("k")
            .base_url("http://x")
            .add_dataset_id("d")
            .document_ids(Vec::<String>::new())
            .build();
        // Java only validates document ids when non-empty, so building succeeds here too.
        let body = build_retrieval_request_body(&cfg.unwrap(), "q", None, None, None);
        assert!(body.get("document_ids").is_none());
    }
}
