//! Converts Dify retrieve records into AgentScope [`Document`]s.
//!
//! Ports `io.agentscope.core.rag.integration.dify.DifyDocumentConverter`, including the
//! field-by-field payload assembly of `buildPayloadFromSegment`.

use serde_json::{json, Map, Value};

use crate::model::{Document, DocumentMetadata};

/// Stateless converter between the Dify response model and [`Document`].
pub struct DifyDocumentConverter;

impl DifyDocumentConverter {
    /// Converts every record of a response, skipping invalid ones.
    ///
    /// Mirrors Java `fromDifyResponse`: a `null` response or an empty record list yields an
    /// empty vector; exceptions during a single record are swallowed (the Java version logs a
    /// warning). Here invalid records simply return `None`.
    pub fn from_dify_response(response: &crate::dify::model::DifyResponse) -> Vec<Document> {
        let Some(records) = response.records.as_ref() else {
            return Vec::new();
        };
        records.iter().filter_map(Self::from_dify_record).collect()
    }

    /// Converts one record, returning `None` when required data is missing.
    ///
    /// Mirrors Java `fromDifyRecord`:
    /// * content is required (blank content is skipped),
    /// * `doc_id` prefers `document_id`, then the segment id, then `"unknown"`,
    /// * `chunk_id` uses the segment id, falling back to `"0"`,
    /// * the score comes from the record.
    pub fn from_dify_record(record: &crate::dify::model::Record) -> Option<Document> {
        let segment = record.segment.as_ref()?;

        let content = segment.content.as_deref().unwrap_or("");
        if content.trim().is_empty() {
            return None;
        }

        let doc_id = match segment.document_id.as_deref() {
            Some(id) if !id.trim().is_empty() => id.to_string(),
            _ => match segment.id.as_deref() {
                Some(id) if !id.trim().is_empty() => id.to_string(),
                _ => "unknown".to_string(),
            },
        };

        let chunk_id = match segment.id.as_deref() {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => "0".to_string(),
        };

        let payload = Self::build_payload_from_segment(segment);
        let mut document = Document::new(DocumentMetadata::with_payload(
            agentscope_core::message::ContentBlock::Text(agentscope_core::message::TextBlock::new(
                content,
            )),
            doc_id,
            chunk_id,
            Value::Object(payload),
        ));

        document.score = record.score;
        Some(document)
    }

    /// Builds the payload map from the scattered Dify segment fields.
    ///
    /// Mirrors Java `buildPayloadFromSegment`. Keys that are absent in the segment are
    /// omitted, exactly like the Java `if (x != null)` guards.
    fn build_payload_from_segment(segment: &crate::dify::model::Segment) -> Map<String, Value> {
        let mut payload = Map::new();

        if let Some(document) = &segment.document {
            let mut document_info = Map::new();
            if let Some(id) = &document.id {
                document_info.insert("id".to_string(), json!(id));
            }
            if let Some(name) = &document.name {
                document_info.insert("name".to_string(), json!(name));
            }
            if let Some(data_source_type) = &document.data_source_type {
                document_info.insert("data_source_type".to_string(), json!(data_source_type));
            }
            if !document_info.is_empty() {
                payload.insert("document".to_string(), Value::Object(document_info));
            }
        }

        if let Some(position) = segment.position {
            payload.insert("position".to_string(), json!(position));
        }
        if let Some(word_count) = segment.word_count {
            payload.insert("word_count".to_string(), json!(word_count));
        }
        if let Some(tokens) = segment.tokens {
            payload.insert("tokens".to_string(), json!(tokens));
        }
        if let Some(answer) = segment.answer.as_ref().filter(|a| !a.is_empty()) {
            payload.insert("answer".to_string(), json!(answer));
        }

        if let Some(index_node_id) = &segment.index_node_id {
            payload.insert("index_node_id".to_string(), json!(index_node_id));
        }
        if let Some(index_node_hash) = &segment.index_node_hash {
            payload.insert("index_node_hash".to_string(), json!(index_node_hash));
        }

        if let Some(status) = &segment.status {
            payload.insert("status".to_string(), json!(status));
        }
        if let Some(enabled) = segment.enabled {
            payload.insert("enabled".to_string(), json!(enabled));
        }
        if let Some(disabled_at) = segment.disabled_at {
            payload.insert("disabled_at".to_string(), json!(disabled_at));
        }
        if let Some(disabled_by) = &segment.disabled_by {
            payload.insert("disabled_by".to_string(), json!(disabled_by));
        }
        if let Some(hit_count) = segment.hit_count {
            payload.insert("hit_count".to_string(), json!(hit_count));
        }

        if let Some(created_at) = segment.created_at {
            payload.insert("created_at".to_string(), json!(created_at));
        }
        if let Some(created_by) = &segment.created_by {
            payload.insert("created_by".to_string(), json!(created_by));
        }
        if let Some(indexing_at) = segment.indexing_at {
            payload.insert("indexing_at".to_string(), json!(indexing_at));
        }
        if let Some(completed_at) = segment.completed_at {
            payload.insert("completed_at".to_string(), json!(completed_at));
        }
        if let Some(stopped_at) = segment.stopped_at {
            payload.insert("stopped_at".to_string(), json!(stopped_at));
        }

        if let Some(keywords) = segment.keywords.as_ref().filter(|k| !k.is_empty()) {
            payload.insert("keywords".to_string(), json!(keywords));
        }

        if let Some(error) = segment.error.as_ref().filter(|e| !e.is_empty()) {
            payload.insert("error".to_string(), json!(error));
        }

        payload
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dify::model::{DifyResponse, DocumentInfo, Query, Record, Segment};

    fn record(
        content: Option<&str>,
        doc_id: Option<&str>,
        segment_id: Option<&str>,
        score: Option<f64>,
    ) -> Record {
        Record {
            segment: Some(Segment {
                id: segment_id.map(str::to_string),
                document_id: doc_id.map(str::to_string),
                content: content.map(str::to_string),
                ..Default::default()
            }),
            score,
        }
    }

    #[test]
    fn null_or_empty_response_yields_no_documents() {
        assert!(DifyDocumentConverter::from_dify_response(&DifyResponse::empty()).is_empty());
        let response = DifyResponse {
            query: None,
            records: Some(vec![]),
        };
        assert!(DifyDocumentConverter::from_dify_response(&response).is_empty());
    }

    #[test]
    fn blank_content_is_skipped() {
        assert!(DifyDocumentConverter::from_dify_record(&record(
            Some("   "),
            Some("d"),
            Some("s"),
            None
        ))
        .is_none());
        assert!(
            DifyDocumentConverter::from_dify_record(&record(None, Some("d"), Some("s"), None))
                .is_none()
        );
    }

    #[test]
    fn missing_segment_is_skipped() {
        assert!(DifyDocumentConverter::from_dify_record(&Record {
            segment: None,
            score: Some(1.0),
        })
        .is_none());
    }

    #[test]
    fn document_id_prefers_document_id_then_segment_id_the_unknown() {
        let d = DifyDocumentConverter::from_dify_record(&record(
            Some("x"),
            Some("doc-1"),
            Some("seg-1"),
            None,
        ))
        .unwrap();
        assert_eq!(d.metadata.doc_id, "doc-1");
        assert_eq!(d.metadata.chunk_id, "seg-1");

        let d =
            DifyDocumentConverter::from_dify_record(&record(Some("x"), None, Some("seg-1"), None))
                .unwrap();
        assert_eq!(d.metadata.doc_id, "seg-1");

        let d =
            DifyDocumentConverter::from_dify_record(&record(Some("x"), None, None, None)).unwrap();
        assert_eq!(d.metadata.doc_id, "unknown");
        assert_eq!(d.metadata.chunk_id, "0");
    }

    #[test]
    fn score_is_taken_from_the_record() {
        let d = DifyDocumentConverter::from_dify_record(&record(
            Some("x"),
            Some("d"),
            Some("s"),
            Some(0.87),
        ))
        .unwrap();
        assert_eq!(d.score, Some(0.87));

        let d =
            DifyDocumentConverter::from_dify_record(&record(Some("x"), Some("d"), Some("s"), None))
                .unwrap();
        assert_eq!(d.score, None);
    }

    #[test]
    fn payload_collects_scattered_segment_metadata() {
        let record = Record {
            segment: Some(Segment {
                id: Some("seg-1".into()),
                document_id: Some("doc-1".into()),
                content: Some("body".into()),
                answer: Some("an answer".into()),
                position: Some(2),
                word_count: Some(10),
                tokens: Some(4),
                keywords: Some(vec!["a".into(), "b".into()]),
                index_node_id: Some("node".into()),
                index_node_hash: Some("hash".into()),
                hit_count: Some(5),
                enabled: Some(true),
                status: Some("completed".into()),
                created_at: Some(1),
                created_by: Some("me".into()),
                indexing_at: Some(2),
                completed_at: Some(3),
                stopped_at: Some(4),
                error: Some("boom".into()),
                document: Some(DocumentInfo {
                    id: Some("doc-1".into()),
                    name: Some("intro.md".into()),
                    data_source_type: Some("upload_file".into()),
                }),
                ..Default::default()
            }),
            score: Some(0.5),
        };

        let d = DifyDocumentConverter::from_dify_record(&record).unwrap();
        assert_eq!(d.metadata.content_text(), "body");
        assert_eq!(d.payload_value("position"), Some(&json!(2)));
        assert_eq!(d.payload_value("word_count"), Some(&json!(10)));
        assert_eq!(d.payload_value("tokens"), Some(&json!(4)));
        assert_eq!(d.payload_value("answer"), Some(&json!("an answer")));
        assert_eq!(d.payload_value("index_node_id"), Some(&json!("node")));
        assert_eq!(d.payload_value("index_node_hash"), Some(&json!("hash")));
        assert_eq!(d.payload_value("status"), Some(&json!("completed")));
        assert_eq!(d.payload_value("enabled"), Some(&json!(true)));
        assert_eq!(d.payload_value("hit_count"), Some(&json!(5)));
        assert_eq!(d.payload_value("created_at"), Some(&json!(1)));
        assert_eq!(d.payload_value("created_by"), Some(&json!("me")));
        assert_eq!(d.payload_value("stopped_at"), Some(&json!(4)));
        assert_eq!(d.payload_value("keywords"), Some(&json!(["a", "b"])));
        assert_eq!(d.payload_value("error"), Some(&json!("boom")));
        assert_eq!(
            d.payload_value("document"),
            Some(&json!({"id": "doc-1", "name": "intro.md", "data_source_type": "upload_file"}))
        );
    }

    #[test]
    fn absent_payload_keys_are_omitted() {
        let d =
            DifyDocumentConverter::from_dify_record(&record(Some("x"), Some("d"), Some("s"), None))
                .unwrap();
        assert!(d.payload().is_empty());
    }

    #[test]
    fn response_level_conversion_keeps_valid_records_in_order() {
        let response = DifyResponse {
            query: Some(Query {
                content: Some("q".into()),
            }),
            records: Some(vec![
                record(Some("first"), Some("d1"), Some("s1"), Some(0.9)),
                record(Some(""), Some("d2"), Some("s2"), Some(0.1)),
                record(Some("third"), Some("d3"), Some("s3"), None),
            ]),
        };
        let docs = DifyDocumentConverter::from_dify_response(&response);
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].metadata.content_text(), "first");
        assert_eq!(docs[1].metadata.content_text(), "third");
    }

    #[test]
    fn deterministic_ids_are_derived_from_doc_chunk_and_content() {
        let a = DifyDocumentConverter::from_dify_record(&record(
            Some("same"),
            Some("d"),
            Some("s"),
            Some(0.1),
        ))
        .unwrap();
        let b = DifyDocumentConverter::from_dify_record(&record(
            Some("same"),
            Some("d"),
            Some("s"),
            Some(0.9),
        ))
        .unwrap();
        assert_eq!(a.id, b.id);
    }
}
