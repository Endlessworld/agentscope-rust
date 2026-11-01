//! On-demand tool discovery, mirroring `io.agentscope.core.tool.search`.
//!
//! [`ToolSearchMiddleware`] is opt-in: it registers one read-only discovery tool
//! (`tool_search`) into a [`Toolkit`](crate::tool::Toolkit) and, on every reasoning step,
//! narrows the tool surface the model sees to the always-visible names plus whatever has
//! already been discovered. The model names a *capability*, a [`ToolSearchStrategy`] ranks
//! the eligible candidates, and the matches come back as metadata on the tool result so the
//! next request can expose their schemas.
//!
//! The loading protocol ([`ToolSearchMode`]) is owned by the provider, not by this module:
//! the middleware only decides which schemas are exposed and whether they are marked
//! [`defer_loading`](crate::model::ToolSchema::defer_loading).

pub mod middleware;
pub mod tool;

pub use middleware::ToolSearchMiddleware;
pub use tool::ToolSearchTool;

use async_trait::async_trait;

use crate::error::Result;
use crate::message::{JsonMap, Msg};
use crate::model::{ToolSchema, ToolSearchMode};

/// Name of the read-only discovery tool (Java `ToolSearchTool.NAME`).
pub const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";

/// Metadata key carrying the loading protocol of a search result
/// (Java `ToolSearchTool.METADATA_MODE`).
pub const METADATA_MODE: &str = "agentscope.tool_search.mode";

/// Metadata key carrying the discovered tool descriptions
/// (Java `ToolSearchTool.METADATA_TOOLS`).
pub const METADATA_TOOLS: &str = "agentscope.tool_search.tools";

/// Default result limit of `tool_search` (Java `ToolSearchTool.DEFAULT_LIMIT`).
pub const DEFAULT_LIMIT: u32 = 5;

/// Description of the discovery tool (Java `ToolSearchTool`'s builder description).
pub const TOOL_SEARCH_DESCRIPTION: &str =
    "Search for tools by capability. Discover tools before calling them.";

/// Immutable search input; candidates already respect tool groups and request overrides.
#[derive(Debug, Clone)]
pub struct ToolSearchRequest {
    /// The tools eligible for the current request.
    pub candidates: Vec<ToolSchema>,
    /// The recent conversation, for strategies that score by context.
    pub recent_messages: Vec<Msg>,
    /// The capability the model is looking for.
    pub query: String,
    /// Maximum number of matches to return.
    pub limit: usize,
}

/// Search matches in relevance order, with optional display text and strategy metadata.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolSearchResult {
    /// The matched tool names, most relevant first.
    pub tool_names: Vec<String>,
    /// Display text for the model; `None` falls back to the tool's own summary.
    pub result_text: Option<String>,
    /// Extra metadata merged into the tool result.
    pub metadata: JsonMap,
}

/// Provider-independent retrieval over the tools eligible for the current request.
#[async_trait]
pub trait ToolSearchStrategy: Send + Sync {
    /// Ranks `request.candidates` for `request.query`.
    async fn search(&self, request: &ToolSearchRequest) -> Result<ToolSearchResult>;
}

/// Dependency-free, case-insensitive matching of query terms against names and descriptions.
///
/// A term found in the tool name scores 3, in the description 1; matches are ordered by score
/// (descending) and then by name, and truncated to the request limit. Mirrors Java
/// `KeywordToolSearchStrategy`.
#[derive(Debug, Clone, Copy, Default)]
pub struct KeywordToolSearchStrategy;

impl KeywordToolSearchStrategy {
    /// Creates the strategy.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolSearchStrategy for KeywordToolSearchStrategy {
    async fn search(&self, request: &ToolSearchRequest) -> Result<ToolSearchResult> {
        let terms = query_terms(&request.query);
        let mut matches: Vec<(&ToolSchema, u32)> = request
            .candidates
            .iter()
            .map(|tool| (tool, keyword_score(tool, &terms)))
            .filter(|(_, score)| *score > 0)
            .collect();
        matches.sort_by(|(left, left_score), (right, right_score)| {
            right_score.cmp(left_score).then_with(|| left.name.cmp(&right.name))
        });
        Ok(ToolSearchResult {
            tool_names: matches
                .into_iter()
                .take(request.limit)
                .map(|(tool, _)| tool.name.clone())
                .collect(),
            result_text: None,
            metadata: JsonMap::new(),
        })
    }
}

/// Splits a query into lower-cased alphanumeric terms, dropping duplicates.
fn query_terms(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for term in query.to_lowercase().split(|c: char| !c.is_alphanumeric()) {
        if !term.is_empty() && !terms.iter().any(|seen| seen == term) {
            terms.push(term.to_string());
        }
    }
    terms
}

/// Scores one tool against the query terms (3 per name hit, 1 per description hit).
fn keyword_score(tool: &ToolSchema, terms: &[String]) -> u32 {
    let name = tool.name.to_lowercase();
    let description = tool.description.to_lowercase();
    terms
        .iter()
        .map(|term| {
            let mut score = 0;
            if name.contains(term.as_str()) {
                score += 3;
            }
            if description.contains(term.as_str()) {
                score += 1;
            }
            score
        })
        .sum()
}

/// Describes an eligible tool for the model, mirroring Java `ToolSearchTool.describe`.
pub(crate) fn describe_tool(schema: &ToolSchema) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert("name".to_string(), serde_json::json!(schema.name));
    map.insert(
        "description".to_string(),
        serde_json::json!(schema.description),
    );
    map.insert(
        "parameters".to_string(),
        serde_json::Value::Object(schema.parameters.clone()),
    );
    if let Some(strict) = schema.strict {
        map.insert("strict".to_string(), serde_json::json!(strict));
    }
    if let Some(output_schema) = &schema.output_schema {
        map.insert(
            "output_schema".to_string(),
            serde_json::Value::Object(output_schema.clone()),
        );
    }
    serde_json::Value::Object(map)
}

/// The search context one model call installs for [`ToolSearchTool`], mirroring Java
/// `ToolSearchTool.SearchContext` (Java keeps it in the call-scoped `RuntimeContext`).
#[derive(Debug, Clone)]
pub(crate) struct SearchContext {
    pub(crate) candidates: Vec<ToolSchema>,
    pub(crate) messages: Vec<Msg>,
    pub(crate) mode: ToolSearchMode,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, description: &str) -> ToolSchema {
        ToolSchema::new(name, description)
    }

    fn request(candidates: Vec<ToolSchema>, query: &str, limit: usize) -> ToolSearchRequest {
        ToolSearchRequest {
            candidates,
            recent_messages: Vec::new(),
            query: query.to_string(),
            limit,
        }
    }

    #[tokio::test]
    async fn keyword_strategy_ranks_name_hits_above_description_hits() {
        let candidates = vec![
            tool("read_file", "reads a file from disk"),
            tool("image_reader", "reads an image into text"),
            tool("weather", "reports the forecast"),
        ];
        let strategy = KeywordToolSearchStrategy::new();
        let result = strategy
            .search(&request(candidates, "read image", 5))
            .await
            .unwrap();
        // `image_reader` scores 8 (name hits "read"/"image" = 6, description hits = 2);
        // `read_file` scores 4 (name "read" = 3, description "reads" = 1); `weather` 0.
        assert_eq!(result.tool_names, vec!["image_reader", "read_file"]);
    }

    #[tokio::test]
    async fn keyword_strategy_returns_nothing_when_no_term_matches() {
        let candidates = vec![tool("read_file", "reads a file from disk")];
        let result = KeywordToolSearchStrategy
            .search(&request(candidates, "kubernetes", 5))
            .await
            .unwrap();
        assert!(result.tool_names.is_empty());
    }

    #[tokio::test]
    async fn keyword_strategy_truncates_to_the_request_limit() {
        let candidates = vec![
            tool("search_a", "search"),
            tool("search_b", "search"),
            tool("search_c", "search"),
        ];
        let result = KeywordToolSearchStrategy
            .search(&request(candidates, "search", 2))
            .await
            .unwrap();
        assert_eq!(result.tool_names, vec!["search_a", "search_b"]);
    }
}
