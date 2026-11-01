//! The read-only `tool_search` discovery tool, mirroring
//! `io.agentscope.core.tool.search.ToolSearchTool`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;

use super::{
    describe_tool, KeywordToolSearchStrategy, SearchContext, ToolSearchRequest,
    ToolSearchStrategy, DEFAULT_LIMIT, METADATA_MODE, METADATA_TOOLS, TOOL_SEARCH_DESCRIPTION,
    TOOL_SEARCH_TOOL_NAME,
};
use crate::error::Result;
use crate::message::{ContentBlock, JsonMap, TextBlock, ToolResultBlock, ToolResultState};
use crate::tool::{Tool, ToolExecutionContext};

/// Read-only discovery tool. Register it through
/// [`ToolSearchMiddleware`](super::ToolSearchMiddleware), which also installs the per-call
/// search context the tool reads.
pub struct ToolSearchTool {
    strategy: Arc<dyn ToolSearchStrategy>,
    /// The context the middleware installs for the model call currently in flight.
    context: Arc<Mutex<Option<SearchContext>>>,
}

impl ToolSearchTool {
    pub(crate) fn new(
        strategy: Arc<dyn ToolSearchStrategy>,
        context: Arc<Mutex<Option<SearchContext>>>,
    ) -> Self {
        Self { strategy, context }
    }

    /// Creates a standalone tool bound to its own (never populated) context.
    ///
    /// Only useful to inspect the tool's declaration; a search needs the middleware.
    pub fn with_keyword_search() -> Self {
        Self::new(Arc::new(KeywordToolSearchStrategy::new()), Arc::new(Mutex::new(None)))
    }
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        TOOL_SEARCH_TOOL_NAME
    }

    fn description(&self) -> &str {
        TOOL_SEARCH_DESCRIPTION
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "The capability to find"},
                "limit": {"type": "integer", "minimum": 1, "description": "Maximum results (default 5)"}
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let search = self.context.lock().unwrap().clone();
        let Some(search) = search else {
            return Ok(ToolResultBlock::error(
                "Tool search requires ToolSearchMiddleware and call-scoped agent state",
            ));
        };
        let Some(query) = args.get("query").and_then(Value::as_str) else {
            return Ok(ToolResultBlock::error("query must be a non-blank string"));
        };
        if query.trim().is_empty() {
            return Ok(ToolResultBlock::error("query must be a non-blank string"));
        }
        let Some(limit) = parse_limit(args.get("limit")) else {
            return Ok(ToolResultBlock::error("limit must be a positive integer"));
        };

        let request = ToolSearchRequest {
            candidates: search.candidates.clone(),
            recent_messages: search.messages.clone(),
            query: query.to_string(),
            limit: limit as usize,
        };
        let result = self.strategy.search(&request).await?;

        let eligible: HashMap<&str, &crate::model::ToolSchema> = search
            .candidates
            .iter()
            .map(|schema| (schema.name.as_str(), schema))
            .collect();
        let mut names: Vec<String> = Vec::new();
        for name in &result.tool_names {
            if !eligible.contains_key(name.as_str()) {
                return Ok(ToolResultBlock::error(format!(
                    "Search returned an ineligible tool: {name}"
                )));
            }
            if names.len() < request.limit && !names.contains(name) {
                names.push(name.clone());
            }
        }

        let mut metadata = result.metadata.clone();
        metadata.insert(
            METADATA_MODE.to_string(),
            Value::String(search.mode.as_str().to_string()),
        );
        metadata.insert(
            METADATA_TOOLS.to_string(),
            Value::Array(
                names
                    .iter()
                    .map(|name| describe_tool(eligible[name.as_str()]))
                    .collect(),
            ),
        );
        let text = result.result_text.unwrap_or_else(|| {
            if names.is_empty() {
                "No matching tools found.".to_string()
            } else {
                format!("Discovered tools: {}", names.join(", "))
            }
        });
        Ok(ToolResultBlock {
            id: None,
            name: Some(TOOL_SEARCH_TOOL_NAME.to_string()),
            output: vec![ContentBlock::Text(TextBlock::new(text))],
            metadata,
            state: ToolResultState::Success,
        })
    }
}

/// Parses the optional `limit` argument, mirroring Java's “positive integer” check
/// (a whole-number JSON float such as `2.0` is accepted, `1.5` is not).
fn parse_limit(value: Option<&Value>) -> Option<u32> {
    match value {
        None => Some(DEFAULT_LIMIT),
        Some(Value::Number(number)) => {
            let value = number.as_f64()?;
            let valid = value >= 1.0 && value <= u32::MAX as f64 && value.fract() == 0.0;
            valid.then_some(value as u32)
        }
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ToolSearchMode;

    fn tool(name: &str, description: &str) -> crate::model::ToolSchema {
        crate::model::ToolSchema::new(name, description)
    }

    fn searchable(names: &[&str]) -> ToolSearchTool {
        let candidates = names
            .iter()
            .map(|name| tool(name, "searchable capability"))
            .collect();
        let context = SearchContext {
            candidates,
            messages: Vec::new(),
            mode: ToolSearchMode::Function,
        };
        ToolSearchTool::new(
            Arc::new(KeywordToolSearchStrategy::new()),
            Arc::new(Mutex::new(Some(context))),
        )
    }

    fn discovered_names(block: &ToolResultBlock) -> Vec<String> {
        block.metadata[METADATA_TOOLS]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn a_search_without_limit_returns_at_most_the_default_limit() {
        let names = [
            "search_a", "search_b", "search_c", "search_d", "search_e", "search_f",
            "search_g",
        ];
        let tool = searchable(&names);
        let mut args = JsonMap::new();
        args.insert("query".to_string(), serde_json::json!("search"));

        let block = tool.call(args, ToolExecutionContext::default()).await.unwrap();

        assert_eq!(block.state, ToolResultState::Success);
        assert_eq!(
            discovered_names(&block).len(),
            DEFAULT_LIMIT as usize,
            "the default limit must truncate the match list"
        );
        assert_eq!(
            block.metadata[METADATA_MODE],
            serde_json::json!(ToolSearchMode::Function.as_str())
        );
        assert!(block
            .text_content()
            .starts_with("Discovered tools: search_a, search_b"));
    }

    #[tokio::test]
    async fn a_search_without_a_middleware_context_reports_an_error_result() {
        let tool = ToolSearchTool::with_keyword_search();
        let mut args = JsonMap::new();
        args.insert("query".to_string(), serde_json::json!("search"));

        let block = tool.call(args, ToolExecutionContext::default()).await.unwrap();

        assert_eq!(block.state, ToolResultState::Error);
    }
}
