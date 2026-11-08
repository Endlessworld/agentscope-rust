//! Citation metadata for Anthropic document-grounded answers.
//!
//! Mirrors `AnthropicCitationConverter`: the provider attaches citations to the
//! text block they belong to, and the response keeps them as provider-specific
//! metadata under [`METADATA_CITATIONS`] so a host can render the provenance
//! without the adapter inventing a neutral citation model.

use agentscope_core::JsonMap;

use super::dto::AnthropicCitation;

/// Response metadata key carrying the converted citation list.
pub const METADATA_CITATIONS: &str = "anthropic.citations";

/// Converts the citations of one text block (`block_index` is its position in
/// the response content) into metadata entries.
///
/// Citation variants Anthropic defines for web search results have no document
/// location to report and are skipped, matching the Java converter.
pub fn to_metadata(citations: &[AnthropicCitation], block_index: usize) -> Vec<serde_json::Value> {
    citations
        .iter()
        .filter_map(|citation| convert(citation, block_index))
        .collect()
}

/// Builds the response metadata holding `citations`, or `None` when empty.
pub fn metadata(citations: Vec<serde_json::Value>) -> Option<JsonMap> {
    if citations.is_empty() {
        return None;
    }
    let mut metadata = JsonMap::new();
    metadata.insert(
        METADATA_CITATIONS.to_string(),
        serde_json::Value::Array(citations),
    );
    Some(metadata)
}

fn convert(citation: &AnthropicCitation, block_index: usize) -> Option<serde_json::Value> {
    let mut entry = serde_json::Map::new();
    entry.insert("type".to_string(), serde_json::json!(citation.kind));
    entry.insert("block_index".to_string(), serde_json::json!(block_index));
    match citation.kind.as_str() {
        "char_location" => {
            entry.insert(
                "start_char_index".to_string(),
                serde_json::json!(citation.start_char_index.unwrap_or(0)),
            );
            entry.insert(
                "end_char_index".to_string(),
                serde_json::json!(citation.end_char_index.unwrap_or(0)),
            );
        }
        "page_location" => {
            entry.insert(
                "start_page_number".to_string(),
                serde_json::json!(citation.start_page_number.unwrap_or(0)),
            );
            entry.insert(
                "end_page_number".to_string(),
                serde_json::json!(citation.end_page_number.unwrap_or(0)),
            );
        }
        "content_block_location" => {
            entry.insert(
                "start_block_index".to_string(),
                serde_json::json!(citation.start_block_index.unwrap_or(0)),
            );
            entry.insert(
                "end_block_index".to_string(),
                serde_json::json!(citation.end_block_index.unwrap_or(0)),
            );
        }
        // `web_search_result_location` / `search_result_location` and unknown
        // variants point at search results, not at a document location.
        _ => return None,
    }
    entry.insert(
        "cited_text".to_string(),
        serde_json::json!(citation.cited_text),
    );
    entry.insert(
        "document_index".to_string(),
        serde_json::json!(citation.document_index),
    );
    if let Some(title) = &citation.document_title {
        entry.insert("document_title".to_string(), serde_json::json!(title));
    }
    if let Some(file_id) = &citation.file_id {
        entry.insert("file_id".to_string(), serde_json::json!(file_id));
    }
    Some(serde_json::Value::Object(entry))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn citation(kind: &str) -> AnthropicCitation {
        AnthropicCitation {
            kind: kind.to_string(),
            cited_text: "quoted".to_string(),
            document_index: 1,
            document_title: Some("report.pdf".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn document_locations_are_flattened_with_the_block_position() {
        let mut char_location = citation("char_location");
        char_location.start_char_index = Some(3);
        char_location.end_char_index = Some(7);
        let entries = to_metadata(&[char_location], 2);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["type"], serde_json::json!("char_location"));
        assert_eq!(entries[0]["block_index"], serde_json::json!(2));
        assert_eq!(entries[0]["start_char_index"], serde_json::json!(3));
        assert_eq!(entries[0]["end_char_index"], serde_json::json!(7));
        assert_eq!(entries[0]["cited_text"], serde_json::json!("quoted"));
        assert_eq!(entries[0]["document_index"], serde_json::json!(1));
        assert_eq!(
            entries[0]["document_title"],
            serde_json::json!("report.pdf")
        );

        let mut page = citation("page_location");
        page.start_page_number = Some(4);
        page.end_page_number = Some(5);
        let entries = to_metadata(&[page], 0);
        assert_eq!(entries[0]["start_page_number"], serde_json::json!(4));
        assert_eq!(entries[0]["end_page_number"], serde_json::json!(5));

        let mut block = citation("content_block_location");
        block.start_block_index = Some(1);
        block.end_block_index = Some(2);
        block.file_id = Some("file-1".to_string());
        let entries = to_metadata(&[block], 0);
        assert_eq!(entries[0]["start_block_index"], serde_json::json!(1));
        assert_eq!(entries[0]["file_id"], serde_json::json!("file-1"));
    }

    #[test]
    fn search_result_citations_are_skipped() {
        assert!(to_metadata(&[citation("web_search_result_location")], 0).is_empty());
        assert!(to_metadata(&[citation("search_result_location")], 0).is_empty());
        assert!(to_metadata(&[citation("something_new")], 0).is_empty());
    }

    #[test]
    fn metadata_is_absent_when_nothing_was_cited() {
        assert!(metadata(Vec::new()).is_none());
        let metadata = metadata(to_metadata(&[citation("page_location")], 0)).unwrap();
        assert!(metadata.contains_key(METADATA_CITATIONS));
    }
}
