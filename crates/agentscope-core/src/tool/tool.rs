//! Tool abstraction for agent tools, mirroring the Java `AgentTool`/`ToolBase` hierarchy.
use async_trait::async_trait;

use crate::error::Result;
use crate::message::{JsonMap, Msg, ToolResultBlock, ToolUseBlock};
use crate::model::ToolSchema;
use std::sync::Arc;

use super::emitter::ToolEmitter;

/// Runtime key carrying the conversation history of the current call, as a JSON array of
/// [`Msg`]. The wire name matches the Java `conversationHistory` field the built-in tools
/// read from the agent state.
pub const CONVERSATION_HISTORY_KEY: &str = "conversationHistory";

/// Maximum number of most-recent messages exposed to a single tool call.
///
/// Java hands the whole history to every tool call; the runtime channel caps it here to bound
/// the per-call serialization cost, so tools see the recent tail only.
pub const MAX_CONVERSATION_HISTORY: usize = 32;

/// Execution context passed to a tool invocation.
#[derive(Clone, Default)]
pub struct ToolExecutionContext {
    /// The tool call that triggered this execution.
    pub tool_call: Option<ToolUseBlock>,
    /// Additional runtime context.
    pub runtime: JsonMap,
    /// Optional streaming emitter for intermediate progress chunks. When
    /// absent, [`emit_to`](super::emitter::emit_to) falls back to a no-op.
    pub emitter: Option<std::sync::Arc<dyn ToolEmitter>>,
    /// Per-call tool view (external tools + merge mode). Read from the call's
    /// [`RuntimeContext`], never mutating the shared [`Toolkit`].
    ///
    /// [`RuntimeContext`]: crate::agent::RuntimeContext
    /// [`Toolkit`]: super::toolkit::Toolkit
    pub tool_request_config: Option<Arc<super::tool_request_config::ToolRequestConfig>>,
}

impl ToolExecutionContext {
    /// The conversation messages the agent had accumulated when this call was dispatched.
    ///
    /// Read from the runtime entry named by [`CONVERSATION_HISTORY_KEY`]. Returns an empty vec
    /// when the agent exposes no history (no messages yet, or a hand-built context) and when the
    /// entry is not a readable message list, so a tool never panics on a malformed runtime.
    pub fn conversation_history(&self) -> Vec<Msg> {
        self.runtime
            .get(CONVERSATION_HISTORY_KEY)
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default()
    }

    /// Attaches a per-call tool view.
    pub fn with_tool_request_config(
        mut self,
        config: Arc<super::tool_request_config::ToolRequestConfig>,
    ) -> Self {
        self.tool_request_config = Some(config);
        self
    }
}

impl std::fmt::Debug for ToolExecutionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolExecutionContext")
            .field("tool_call", &self.tool_call)
            .field("runtime", &self.runtime)
            .field("has_emitter", &self.emitter.is_some())
            .field(
                "has_tool_request_config",
                &self.tool_request_config.is_some(),
            )
            .finish()
    }
}

/// A tool that can be invoked by an AI agent.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The tool name (snake_case).
    fn name(&self) -> &str;
    /// The tool description shown to the model.
    fn description(&self) -> &str;
    /// Human-readable title meant **for display purposes** (e.g. rendering a
    /// permission confirmation to an end user), as opposed to [`name`](Self::name)
    /// (a programmatic identifier such as `repair__create_ticket`) and
    /// [`description`](Self::description) (model-facing guidance).
    ///
    /// Mirrors Java `AgentTool.getTitle()`, including its additive shape: the
    /// default is `None`, so no implementer must change. MCP servers may supply it
    /// as `Tool.title` or `ToolAnnotations.title`.
    fn title(&self) -> Option<&str> {
        None
    }
    /// The JSON Schema describing this tool's output, when the tool declares one.
    ///
    /// Mirrors Java `AgentTool.getOutputSchema()`, including its additive shape: the
    /// default is `None`, so no implementer must change. Unlike
    /// [`parameters_schema`](Self::parameters_schema) it is not part of the tool-call
    /// contract; providers that support structured tool output forward it as
    /// `outputSchema`.
    fn output_schema(&self) -> Option<&serde_json::Value> {
        None
    }
    /// Whether this tool performs no observable side effects.
    fn is_read_only(&self) -> bool {
        false
    }
    /// Whether this tool is executed outside the framework.
    fn is_external(&self) -> bool {
        false
    }
    /// Whether multiple invocations may run in parallel.
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    /// Whether this tool may legitimately run longer than the configured
    /// execution timeout (e.g. a blocking `task_output` / `wait_async_results`
    /// barrier that waits for background work). The executor then skips its
    /// timeout wrapper for the tool, so a slow call completes instead of being
    /// cancelled mid-run. Retry policy is unaffected. Defaults to `false`.
    fn is_long_running(&self) -> bool {
        false
    }
    /// Whether this tool runs in **strict** schema mode, mirroring Java
    /// `Tool.strict()`.
    ///
    /// When enabled, [`Tool::schema`] recursively injects
    /// `additionalProperties: false` into every object node of the parameter
    /// schema and marks the schema as strict. The injected keyword makes the
    /// tool-argument validation reject parameters that are not declared in the
    /// schema (on any model provider) and satisfies OpenAI's strict mode,
    /// which rejects schemas without it.
    ///
    /// Defaults to `false`, so every existing tool keeps its current
    /// behaviour.
    fn is_strict(&self) -> bool {
        false
    }
    /// Whether this tool's result should be returned **directly to the caller as
    /// the turn's final answer**, skipping the next reasoning iteration.
    ///
    /// Defaults to `false`. Mirrors Java `AgentTool.isReturnDirect()`:
    /// - the flag is invisible to the model (it is never part of the tool schema),
    ///   so it cannot influence the model's decision to call the tool — it only
    ///   controls what happens after a *successful* call;
    /// - it only applies when **every** executed tool in the round is
    ///   `returnDirect` **and** every result is successful; otherwise all results
    ///   are fed back to the model;
    /// - a result that is denied / errored / interrupted must never be presented
    ///   as the final answer.
    ///
    /// Implementers declaring `true` should therefore always produce presentable
    /// (non-empty) content blocks on success.
    fn is_return_direct(&self) -> bool {
        false
    }
    /// Whether this tool's full definition may be **deferred** until the model needs
    /// it, mirroring Java `AgentTool.getDeferLoading()`.
    ///
    /// Providers that support tool search (e.g. the OpenAI Responses `tool_search`
    /// server tool) may then load the parameter schema lazily; the name and
    /// description stay visible to the model. Defaults to `false`, so any unmarked
    /// tool is sent eagerly loaded — deferral is a per-tool opt-in. Providers without
    /// deferred loading support ignore the flag.
    fn defer_loading(&self) -> bool {
        false
    }
    /// The JSON Schema for the tool parameters.
    fn parameters_schema(&self) -> serde_json::Value;
    /// Executes the tool with the given input arguments.
    async fn call(&self, args: JsonMap, ctx: ToolExecutionContext) -> Result<ToolResultBlock>;
    /// Returns the full tool schema.
    fn schema(&self) -> ToolSchema {
        let mut parameters = match self.parameters_schema() {
            serde_json::Value::Object(map) => map,
            other => {
                let mut m = serde_json::Map::new();
                m.insert("type".into(), serde_json::Value::String("object".into()));
                m.insert("properties".into(), other);
                m
            }
        };
        if !parameters.contains_key("type") {
            parameters.insert("type".into(), serde_json::Value::String("object".into()));
        }
        // strict 模式：递归注入 `additionalProperties: false`（对齐 Java
        // `ToolSchemaGenerator.generateParameterSchema(..., allowAdditionalProperties)`）。
        // 注入后 `ToolValidator` 用的是同一份 schema，因此未声明的入参会被拒绝。
        let strict = self.is_strict();
        if strict {
            let mut parameters_value = serde_json::Value::Object(parameters);
            super::schema::apply_additional_properties_false(&mut parameters_value);
            parameters = match parameters_value {
                serde_json::Value::Object(map) => map,
                other => {
                    let mut map = serde_json::Map::new();
                    map.insert("type".into(), serde_json::Value::String("object".into()));
                    map.insert("properties".into(), other);
                    map
                }
            };
        }
        let schema = ToolSchema::new(self.name(), self.description()).parameters(parameters);
        let schema = if strict { schema.strict(true) } else { schema };
        // Java only sends `defer_loading: true` for explicitly marked tools; an
        // unmarked tool carries `null` and stays eagerly loaded.
        if self.defer_loading() {
            schema.defer_loading(Some(true))
        } else {
            schema
        }
    }
    /// Toolkit-aware deep-copy rebinding hook, mirroring Java `ToolkitAware`.
    /// When a [`Toolkit`](super::toolkit::Toolkit) is copied, aware tools are
    /// asked to rebind themselves to the new copy. Returns `None` by default,
    /// meaning the tool is not toolkit-aware and is shared as-is.
    fn rebind_toolkit(
        &self,
        _toolkit: &super::toolkit::Toolkit,
    ) -> Option<std::sync::Arc<dyn Tool>> {
        None
    }
}

/// A wrapper for a function-based tool.
pub struct FunctionTool {
    name: String,
    description: String,
    /// Optional display title (see [`Tool::title`]).
    title: Option<String>,
    parameters: serde_json::Value,
    read_only: bool,
    return_direct: bool,
    defer_loading: bool,
    /// Strict schema mode (see [`Tool::is_strict`]).
    strict: bool,
    handler: Box<dyn Fn(JsonMap, ToolExecutionContext) -> Result<ToolResultBlock> + Send + Sync>,
    rebind: Option<Box<dyn Fn(&super::toolkit::Toolkit) -> Arc<dyn Tool> + Send + Sync>>,
}

impl FunctionTool {
    /// Creates a new function tool.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
        handler: Box<
            dyn Fn(JsonMap, ToolExecutionContext) -> Result<ToolResultBlock> + Send + Sync,
        >,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            title: None,
            parameters,
            read_only: false,
            return_direct: false,
            defer_loading: false,
            strict: false,
            handler,
            rebind: None,
        }
    }

    /// Marks this tool as read-only.
    pub fn read_only(mut self, v: bool) -> Self {
        self.read_only = v;
        self
    }

    /// Marks this tool's successful result as the turn's final answer, skipping
    /// the follow-up model call (see [`Tool::is_return_direct`]).
    pub fn return_direct(mut self, v: bool) -> Self {
        self.return_direct = v;
        self
    }

    /// Marks this tool as eligible for deferred definition loading (see
    /// [`Tool::defer_loading`]).
    pub fn defer_loading(mut self, v: bool) -> Self {
        self.defer_loading = v;
        self
    }

    /// Enables strict schema mode (see [`Tool::is_strict`]).
    pub fn strict(mut self, v: bool) -> Self {
        self.strict = v;
        self
    }

    /// Sets the human-readable display title (see [`Tool::title`]). Blank values
    /// are ignored, matching Java `McpTool.resolveDisplayTitle`.
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        let title = title.into();
        if !title.trim().is_empty() {
            self.title = Some(title);
        }
        self
    }

    /// Marks this tool as toolkit-aware: when the owning toolkit is copied,
    /// `factory` builds the rebound instance against the new copy (Java
    /// `ToolkitAware.rebindToolkit` deep-copy semantics).
    pub fn toolkit_aware(
        mut self,
        factory: Box<dyn Fn(&super::toolkit::Toolkit) -> Arc<dyn Tool> + Send + Sync>,
    ) -> Self {
        self.rebind = Some(factory);
        self
    }
}

#[async_trait]
impl Tool for FunctionTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }
    fn is_read_only(&self) -> bool {
        self.read_only
    }
    fn is_return_direct(&self) -> bool {
        self.return_direct
    }
    fn defer_loading(&self) -> bool {
        self.defer_loading
    }
    fn is_strict(&self) -> bool {
        self.strict
    }
    fn parameters_schema(&self) -> serde_json::Value {
        self.parameters.clone()
    }
    async fn call(&self, args: JsonMap, ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        (self.handler)(args, ctx)
    }
    fn rebind_toolkit(&self, toolkit: &super::toolkit::Toolkit) -> Option<Arc<dyn Tool>> {
        self.rebind.as_ref().map(|f| f(toolkit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fn_tool() -> FunctionTool {
        FunctionTool::new(
            "lookup",
            "looks things up",
            serde_json::json!({"type": "object"}),
            Box::new(|_, _| Ok(ToolResultBlock::text("ok"))),
        )
    }

    #[test]
    fn conversation_history_is_read_from_the_runtime_in_order() {
        let messages = vec![Msg::user("first"), Msg::user("second")];
        let mut runtime = JsonMap::new();
        runtime.insert(
            CONVERSATION_HISTORY_KEY.to_string(),
            serde_json::to_value(&messages).unwrap(),
        );
        let ctx = ToolExecutionContext {
            runtime,
            ..Default::default()
        };
        let seen: Vec<String> = ctx
            .conversation_history()
            .iter()
            .map(Msg::text_content)
            .collect();
        assert_eq!(seen, vec!["first".to_string(), "second".to_string()]);
    }

    #[test]
    fn a_context_without_the_history_key_has_no_history() {
        assert!(ToolExecutionContext::default()
            .conversation_history()
            .is_empty());
    }

    #[test]
    fn an_unreadable_conversation_history_is_treated_as_absent() {
        let mut runtime = JsonMap::new();
        runtime.insert(
            CONVERSATION_HISTORY_KEY.to_string(),
            serde_json::json!({"not": "a message list"}),
        );
        let ctx = ToolExecutionContext {
            runtime,
            ..Default::default()
        };
        assert!(ctx.conversation_history().is_empty());
    }

    #[test]
    fn title_is_additive_and_defaults_to_none() {
        assert_eq!(fn_tool().title(), None);
        assert_eq!(fn_tool().with_title("Lookup").title(), Some("Lookup"));
    }

    #[test]
    fn blank_title_is_ignored() {
        assert_eq!(fn_tool().with_title("  ").title(), None);
        assert_eq!(fn_tool().with_title("").title(), None);
    }

    #[test]
    fn output_schema_is_additive_and_defaults_to_none() {
        let tool = fn_tool();
        assert_eq!(tool.output_schema(), None);
        // The trait default must also be reachable through a trait object, which is how
        // the toolkit and the provider converters hold tools.
        let as_dyn: &dyn Tool = &tool;
        assert_eq!(as_dyn.output_schema(), None);
        assert_eq!(as_dyn.schema().output_schema, None);
    }

    #[test]
    fn defer_loading_is_opt_in_and_defaults_to_false() {
        // The trait default keeps every existing tool eagerly loaded.
        struct Bare;
        #[async_trait]
        impl Tool for Bare {
            fn name(&self) -> &str {
                "bare"
            }
            fn description(&self) -> &str {
                "b"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn call(
                &self,
                _args: JsonMap,
                _ctx: ToolExecutionContext,
            ) -> Result<ToolResultBlock> {
                Ok(ToolResultBlock::text("ok"))
            }
        }
        let bare = Bare;
        assert!(!Tool::defer_loading(&bare));
        assert_eq!(bare.schema().defer_loading, None);

        // `FunctionTool` has both an inherent setter (`defer_loading(bool)`) and the trait
        // getter, so the trait method must be called explicitly here.
        let eager = fn_tool();
        assert!(!Tool::defer_loading(&eager));
        assert_eq!(eager.schema().defer_loading, None);

        // Marked tools carry `defer_loading: true` on their schema.
        let deferred = fn_tool().defer_loading(true);
        assert!(Tool::defer_loading(&deferred));
        assert_eq!(deferred.schema().defer_loading, Some(true));
    }

    #[test]
    fn strict_is_opt_in_and_injects_additional_properties_false() {
        // Default: no strict flag, parameters untouched.
        let lax = fn_tool();
        assert!(!Tool::is_strict(&lax));
        let lax_schema = lax.schema();
        assert_eq!(lax_schema.strict, None);
        assert!(lax_schema.parameters.get("additionalProperties").is_none());

        let strict = FunctionTool::new(
            "lookup",
            "looks things up",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "nested": { "type": "object", "properties": {
                        "x": { "type": "string" }
                    } }
                }
            }),
            Box::new(|_, _| Ok(ToolResultBlock::text("ok"))),
        )
        .strict(true);
        assert!(Tool::is_strict(&strict));
        let schema = strict.schema();
        assert_eq!(schema.strict, Some(true));
        assert_eq!(
            schema.parameters.get("additionalProperties"),
            Some(&serde_json::Value::Bool(false))
        );
        let nested = &schema.parameters["properties"]["nested"];
        assert_eq!(
            nested.get("additionalProperties"),
            Some(&serde_json::Value::Bool(false))
        );
    }
}
