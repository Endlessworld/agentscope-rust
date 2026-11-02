//! The `tool_search` middleware, mirroring
//! `io.agentscope.core.tool.search.ToolSearchMiddleware`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use super::{
    describe_tool, tool::ToolSearchTool, KeywordToolSearchStrategy, SearchContext,
    ToolSearchStrategy, METADATA_MODE, METADATA_TOOLS, TOOL_SEARCH_TOOL_NAME,
};
use crate::agent::react_agent::STRUCTURED_OUTPUT_TOOL_NAME;
use crate::agent::RuntimeContext;
use crate::error::{Error, Result};
use crate::message::{ContentBlock, JsonMap, Msg, ToolResultBlock, ToolResultState};
use crate::middleware::{EventStream, ExtensionPoint, Middleware, Next, ReasoningInput};
use crate::model::{ToolChoice, ToolSchema, ToolSearchMode};
use crate::tool::{Tool, Toolkit};

/// Opt-in discovery over the current model-call tool surface.
///
/// Construction registers the read-only `tool_search` tool into the toolkit; every reasoning
/// step then exposes only the always-visible names plus the tools already discovered in this
/// conversation. Tool *execution* is untouched: a discovered tool is called like any other.
///
/// The middleware narrows the surface at the reasoning phase, so the mode is fixed at
/// construction rather than read from the model per call. A host that knows its provider
/// supports deferred or client loading passes that [`ToolSearchMode`] to
/// [`with_options`](Self::with_options).
pub struct ToolSearchMiddleware {
    /// The registered discovery tool; identity decides whether the surface is still ours.
    tool: Arc<dyn Tool>,
    toolkit: Toolkit,
    always_visible: HashSet<String>,
    mode: ToolSearchMode,
    /// Search context shared with [`ToolSearchTool`] for the model call in flight.
    context: Arc<Mutex<Option<SearchContext>>>,
}

impl ToolSearchMiddleware {
    /// Registers keyword search over the toolkit, exposing no tool eagerly beyond the
    /// discovery tool itself and using ordinary function calling.
    pub fn new(toolkit: &Toolkit) -> Result<Self> {
        Self::with_options(
            toolkit,
            Arc::new(KeywordToolSearchStrategy::new()),
            Vec::new(),
            ToolSearchMode::Function,
        )
    }

    /// Creates a middleware with an explicit strategy, always-visible names and loading mode.
    ///
    /// `always_visible` names are exposed without discovery when they are eligible.
    pub fn with_options(
        toolkit: &Toolkit,
        strategy: Arc<dyn ToolSearchStrategy>,
        always_visible: impl IntoIterator<Item = String>,
        mode: ToolSearchMode,
    ) -> Result<Self> {
        if toolkit.get_tool(TOOL_SEARCH_TOOL_NAME).is_some() {
            return Err(Error::Tool(format!(
                "A tool named {TOOL_SEARCH_TOOL_NAME} is already registered"
            )));
        }
        let context = Arc::new(Mutex::new(None));
        let tool: Arc<dyn Tool> = Arc::new(ToolSearchTool::new(strategy, Arc::clone(&context)));
        toolkit.register_tool(Arc::clone(&tool));
        Ok(Self {
            tool,
            toolkit: toolkit.clone(),
            always_visible: always_visible.into_iter().collect(),
            mode,
            context,
        })
    }

    /// Rewrites one reasoning request: refresh the search history, install the search
    /// context and narrow the exposed tool surface.
    fn prepare(&self, ctx: &RuntimeContext, input: ReasoningInput) -> ReasoningInput {
        // External-only requests and tools shadowing `tool_search` keep their own surface.
        let active = self
            .toolkit
            .get_tool(TOOL_SEARCH_TOOL_NAME)
            .is_some_and(|tool| Arc::ptr_eq(&tool, &self.tool))
            && input.tools.iter().any(|s| s.name == TOOL_SEARCH_TOOL_NAME);
        let (messages, loaded) = refresh_search_results(&input.messages, &input.tools, self.mode);
        if !active {
            *self.context.lock().unwrap() = None;
            return ReasoningInput { messages, ..input };
        }

        if !loaded.is_empty() {
            if let Some(state) = &ctx.state {
                state
                    .lock()
                    .unwrap()
                    .tool_context
                    .add_discovered_tools(loaded.iter().cloned());
            }
        }

        let mut visible = self.always_visible.clone();
        visible.insert(TOOL_SEARCH_TOOL_NAME.to_string());
        visible.insert(STRUCTURED_OUTPUT_TOOL_NAME.to_string());
        let persisted: Vec<String> = ctx
            .state
            .as_ref()
            .map(|state| state.lock().unwrap().tool_context.discovered_tools.clone())
            .unwrap_or_default();
        // Java's ToolSearchTool records discoveries into the call-scoped AgentState
        // itself; Rust tools carry no state handle, so the history is the second source
        // (and the only one for an ad-hoc context that has no state at all).
        for name in persisted.into_iter().chain(loaded.iter().cloned()) {
            // Restore visibility after history compaction (function calling always shows
            // everything already discovered; deferred loading only what is not in history).
            if self.mode == ToolSearchMode::Function || !loaded.contains(&name) {
                visible.insert(name);
            }
        }
        if let Some(ToolChoice::Specific { tool_name }) = &input.options.tool_choice {
            visible.insert(tool_name.clone());
        }

        let candidates: Vec<ToolSchema> = input
            .tools
            .iter()
            .filter(|schema| {
                schema.name != TOOL_SEARCH_TOOL_NAME && schema.name != STRUCTURED_OUTPUT_TOOL_NAME
            })
            .cloned()
            .collect();
        *self.context.lock().unwrap() = Some(SearchContext {
            candidates,
            messages: messages.clone(),
            mode: self.mode,
        });

        let exposed = input
            .tools
            .iter()
            .filter(|schema| {
                self.mode == ToolSearchMode::Deferred || visible.contains(&schema.name)
            })
            .map(|schema| {
                let mut exposed = schema.clone();
                exposed.defer_loading =
                    Some(self.mode == ToolSearchMode::Deferred && !visible.contains(&schema.name));
                exposed
            })
            .collect();
        ReasoningInput {
            messages,
            tools: exposed,
            options: input.options,
        }
    }
}

impl Middleware for ToolSearchMiddleware {
    /// Narrow declaration: only the reasoning phase is rewritten.
    fn active_points(&self) -> Vec<ExtensionPoint> {
        vec![ExtensionPoint::OnReasoning]
    }

    fn on_reasoning(
        &self,
        ctx: &RuntimeContext,
        input: ReasoningInput,
        next: Next<ReasoningInput>,
    ) -> EventStream {
        next(self.prepare(ctx, input))
    }
}

/// Revalidates native search history against the current catalog without changing the
/// stored messages, and returns the tools already discovered for `mode`.
fn refresh_search_results(
    messages: &[Msg],
    tools: &[ToolSchema],
    mode: ToolSearchMode,
) -> (Vec<Msg>, HashSet<String>) {
    let eligible: HashMap<&str, &ToolSchema> = tools
        .iter()
        .map(|tool| (tool.name.as_str(), tool))
        .collect();
    let mut loaded = HashSet::new();
    let mut changed = false;
    let mut refreshed: Vec<Msg> = Vec::with_capacity(messages.len());
    for message in messages {
        let mut content: Vec<ContentBlock> = Vec::with_capacity(message.content.len());
        let mut message_changed = false;
        for block in &message.content {
            let mut next = block.clone();
            if let ContentBlock::ToolResult(result) = block {
                if let Some(serde_json::Value::Array(matches)) = result.metadata.get(METADATA_TOOLS)
                {
                    let current: Vec<serde_json::Value> = matches
                        .iter()
                        .filter_map(|entry| entry.get("name").and_then(|name| name.as_str()))
                        .filter_map(|name| eligible.get(name).map(|tool| describe_tool(tool)))
                        .collect();
                    if current.as_slice() != matches.as_slice() {
                        let mut metadata: JsonMap = result.metadata.clone();
                        metadata.insert(
                            METADATA_TOOLS.to_string(),
                            serde_json::Value::Array(current.clone()),
                        );
                        next = ContentBlock::ToolResult(ToolResultBlock {
                            id: result.id.clone(),
                            name: result.name.clone(),
                            output: result.output.clone(),
                            metadata,
                            state: result.state,
                        });
                        message_changed = true;
                    }
                    if result.state == ToolResultState::Success
                        && result.metadata.get(METADATA_MODE).and_then(|v| v.as_str())
                            == Some(mode.as_str())
                    {
                        for entry in &current {
                            if let Some(name) = entry.get("name").and_then(|v| v.as_str()) {
                                loaded.insert(name.to_string());
                            }
                        }
                    }
                }
            }
            content.push(next);
        }
        if message_changed {
            let mut message = message.clone();
            message.content = content;
            refreshed.push(message);
            changed = true;
        } else {
            refreshed.push(message.clone());
        }
    }
    (
        if changed {
            refreshed
        } else {
            messages.to_vec()
        },
        loaded,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::TextBlock;
    use crate::model::GenerateOptions;
    use crate::state::AgentState;

    fn tool(name: &str, description: &str) -> ToolSchema {
        ToolSchema::new(name, description)
    }

    fn schema_names(request: &ReasoningInput) -> Vec<String> {
        request.tools.iter().map(|s| s.name.clone()).collect()
    }

    /// Runs the middleware's reasoning hook and returns the rewritten request.
    fn rewrite(
        middleware: &ToolSearchMiddleware,
        ctx: &RuntimeContext,
        input: ReasoningInput,
    ) -> ReasoningInput {
        let captured: Arc<Mutex<Option<ReasoningInput>>> = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&captured);
        let _stream = middleware.on_reasoning(
            ctx,
            input,
            Box::new(move |input: ReasoningInput| {
                *sink.lock().unwrap() = Some(input);
                Box::pin(futures::stream::empty()) as EventStream
            }),
        );
        let rewritten = captured.lock().unwrap().take();
        rewritten.expect("the middleware must delegate to the next layer")
    }

    fn search_result_message(names: &[&str], mode: ToolSearchMode) -> Msg {
        let tools: Vec<serde_json::Value> = names
            .iter()
            .map(|name| describe_tool(&tool(name, "searchable capability")))
            .collect();
        let mut metadata = JsonMap::new();
        metadata.insert(METADATA_MODE.to_string(), serde_json::json!(mode.as_str()));
        metadata.insert(METADATA_TOOLS.to_string(), serde_json::Value::Array(tools));
        Msg::builder()
            .role(crate::message::MsgRole::Tool)
            .content(vec![ContentBlock::ToolResult(ToolResultBlock {
                id: Some("c1".to_string()),
                name: Some(TOOL_SEARCH_TOOL_NAME.to_string()),
                output: vec![ContentBlock::Text(TextBlock::new("found"))],
                metadata,
                state: ToolResultState::Success,
            })])
            .build()
    }

    fn request_with_tools(tools: Vec<ToolSchema>) -> ReasoningInput {
        ReasoningInput::new(Vec::new(), tools, GenerateOptions::default())
    }

    #[test]
    fn only_always_visible_tools_survive_the_initial_narrowing() {
        let toolkit = Toolkit::new();
        let middleware = ToolSearchMiddleware::with_options(
            &toolkit,
            Arc::new(KeywordToolSearchStrategy::new()),
            vec!["plan_write".to_string()],
            ToolSearchMode::Function,
        )
        .unwrap();
        let input = request_with_tools(vec![
            tool(TOOL_SEARCH_TOOL_NAME, "search"),
            tool("plan_write", "writes a plan"),
            tool("read_file", "reads a file"),
        ]);

        let rewritten = rewrite(&middleware, &RuntimeContext::default(), input);

        assert_eq!(
            schema_names(&rewritten),
            vec![TOOL_SEARCH_TOOL_NAME, "plan_write"]
        );
    }

    #[test]
    fn a_discovered_tool_joins_the_visible_set() {
        let toolkit = Toolkit::new();
        let middleware = ToolSearchMiddleware::new(&toolkit).unwrap();
        let mut input = request_with_tools(vec![
            tool(TOOL_SEARCH_TOOL_NAME, "search"),
            tool("read_file", "reads a file"),
        ]);
        input.messages = vec![search_result_message(
            &["read_file"],
            ToolSearchMode::Function,
        )];

        let rewritten = rewrite(&middleware, &RuntimeContext::default(), input);

        assert_eq!(
            schema_names(&rewritten),
            vec![TOOL_SEARCH_TOOL_NAME, "read_file"]
        );
    }

    #[test]
    fn deferred_mode_exposes_the_catalog_with_deferred_loading() {
        let toolkit = Toolkit::new();
        let middleware = ToolSearchMiddleware::with_options(
            &toolkit,
            Arc::new(KeywordToolSearchStrategy::new()),
            Vec::new(),
            ToolSearchMode::Deferred,
        )
        .unwrap();
        let input = request_with_tools(vec![
            tool(TOOL_SEARCH_TOOL_NAME, "search"),
            tool("read_file", "reads a file"),
        ]);

        let rewritten = rewrite(&middleware, &RuntimeContext::default(), input);

        assert_eq!(
            schema_names(&rewritten),
            vec![TOOL_SEARCH_TOOL_NAME, "read_file"]
        );
        assert_eq!(rewritten.tools[0].defer_loading, Some(false));
        assert_eq!(rewritten.tools[1].defer_loading, Some(true));
    }

    #[test]
    fn persisted_discoveries_are_restored_into_the_visible_set() {
        // History compaction drops the search result itself; the state keeps the discovery.
        let toolkit = Toolkit::new();
        let middleware = ToolSearchMiddleware::new(&toolkit).unwrap();
        let mut state = AgentState::default();
        state
            .tool_context
            .add_discovered_tools(["read_file".to_string()]);
        let ctx = RuntimeContext {
            state: Some(Arc::new(Mutex::new(state))),
            ..RuntimeContext::default()
        };
        let input = request_with_tools(vec![
            tool(TOOL_SEARCH_TOOL_NAME, "search"),
            tool("read_file", "reads a file"),
        ]);

        let rewritten = rewrite(&middleware, &ctx, input);

        assert_eq!(
            schema_names(&rewritten),
            vec![TOOL_SEARCH_TOOL_NAME, "read_file"]
        );
    }

    #[test]
    fn a_shadowed_search_tool_leaves_the_surface_untouched() {
        let toolkit = Toolkit::new();
        let middleware = ToolSearchMiddleware::new(&toolkit).unwrap();
        // Something else re-registered `tool_search`: the surface is no longer ours.
        toolkit.register_tool(Arc::new(crate::tool::SchemaOnlyTool::new(tool(
            TOOL_SEARCH_TOOL_NAME,
            "someone else's search",
        ))));
        let input = request_with_tools(vec![
            tool(TOOL_SEARCH_TOOL_NAME, "search"),
            tool("read_file", "reads a file"),
        ]);

        let rewritten = rewrite(&middleware, &RuntimeContext::default(), input);

        assert_eq!(
            schema_names(&rewritten),
            vec![TOOL_SEARCH_TOOL_NAME, "read_file"]
        );
    }

    #[test]
    fn a_tool_named_tool_search_cannot_be_registered_twice() {
        let toolkit = Toolkit::new();
        ToolSearchMiddleware::new(&toolkit).unwrap();

        let error = ToolSearchMiddleware::new(&toolkit)
            .err()
            .expect("a second registration must fail");

        assert!(error.to_string().contains("tool_search"), "{error}");
    }
}
