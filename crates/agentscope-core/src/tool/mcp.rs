//! MCP (Model Context Protocol) tool integration.
//!
//! Full MCP client support is provided by the external `rust-mcp-sdk` crate
//! (a path dependency, see the migration plan). This module defines a
//! self-contained [`McpTool`] that wraps a remote MCP tool definition and
//! dispatches invocations through a caller-supplied handler, so the core crate
//! can represent MCP tools without a hard dependency on the SDK.
use async_trait::async_trait;

use crate::error::{Error, Result};
use crate::message::{ContentBlock, JsonMap, TextBlock, ToolResultBlock};
use crate::tool::ToolExecutionContext;

use super::tool::Tool;

/// A tool backed by a remote MCP server tool definition.
/// An async MCP dispatch handler: given the tool, its argument map and the request
/// metadata to propagate (`None` when propagation is disabled), returns a pinned
/// future that resolves to a [`ToolResultBlock`].
///
/// The metadata argument is the Rust equivalent of Java
/// `McpTool.buildMetaMap(...)`: it carries `io.agentscope/toolCallId` and is
/// `None` — not an empty map — when propagation is off, so the transport omits the
/// `_meta` field entirely.
pub type McpAsyncHandler = Box<
    dyn Fn(
            &McpTool,
            JsonMap,
            Option<JsonMap>,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ToolResultBlock>> + Send>>
        + Send
        + Sync,
>;

pub struct McpTool {
    name: String,
    description: String,
    /// Optional human-readable display title (see [`Tool::title`]).
    title: Option<String>,
    /// Optional JSON Schema for the tool's output (see [`Tool::output_schema`]).
    output_schema: Option<serde_json::Value>,
    parameters: serde_json::Value,
    /// Server name this tool belongs to (used in error messages).
    server: String,
    handler: Box<dyn Fn(&McpTool, JsonMap) -> Result<ToolResultBlock> + Send + Sync>,
    /// Optional async handler, preferred over [`handler`](Self::handler) when
    /// present. Lets MCP tool calls await a real client round-trip without
    /// blocking the tokio runtime.
    async_handler: Option<McpAsyncHandler>,
    /// Per-tool restriction for request metadata propagation.
    ///
    /// Effective propagation is the logical AND of this flag and the
    /// connection-level switch (`McpServerConfig::propagate_meta`, read live on every
    /// call). Mirrors Java `McpTool.propagateMeta`: the value is fixed at
    /// registration time and can only further restrict propagation.
    propagate_meta: bool,
}

impl std::fmt::Debug for McpTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpTool")
            .field("name", &self.name)
            .field("server", &self.server)
            .finish()
    }
}

impl McpTool {
    /// Creates a new MCP tool with a dispatch handler.
    pub fn new(
        server: impl Into<String>,
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
        handler: Box<dyn Fn(&McpTool, JsonMap) -> Result<ToolResultBlock> + Send + Sync>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            title: None,
            output_schema: None,
            parameters,
            server: server.into(),
            handler,
            async_handler: None,
            propagate_meta: true,
        }
    }

    /// Resolves the display title from a raw MCP tool definition, preferring the
    /// top-level `title` and falling back to `annotations.title`; blank values are
    /// ignored. Mirrors Java `McpTool.resolveDisplayTitle(McpSchema.Tool)`.
    pub fn resolve_display_title(definition: &serde_json::Value) -> Option<String> {
        let non_blank = |v: Option<&serde_json::Value>| {
            v.and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        non_blank(definition.get("title"))
            .or_else(|| non_blank(definition.get("annotations").and_then(|a| a.get("title"))))
    }

    /// Sets the human-readable display title (see [`Tool::title`]). Blank values
    /// are ignored.
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        let title = title.into();
        if !title.trim().is_empty() {
            self.title = Some(title);
        }
        self
    }

    /// Attaches the display title resolved from a raw MCP tool definition.
    pub fn with_title_from_definition(self, definition: &serde_json::Value) -> Self {
        match Self::resolve_display_title(definition) {
            Some(t) => self.with_title(t),
            None => self,
        }
    }

    /// Sets the JSON Schema describing this tool's output (see
    /// [`Tool::output_schema`]). Non-object and empty-object schemas are ignored, in the
    /// same spirit as the blank-title rule above: a server that sends `{}` declares no
    /// output contract.
    pub fn with_output_schema(mut self, schema: serde_json::Value) -> Self {
        if schema
            .as_object()
            .map(|object| !object.is_empty())
            .unwrap_or(false)
        {
            self.output_schema = Some(schema);
        }
        self
    }

    /// Attaches the output schema resolved from a raw MCP tool definition, preferring
    /// `outputSchema` and falling back to the snake_case `output_schema`.
    pub fn with_output_schema_from_definition(self, definition: &serde_json::Value) -> Self {
        match definition
            .get("outputSchema")
            .or_else(|| definition.get("output_schema"))
        {
            Some(schema) => self.with_output_schema(schema.clone()),
            None => self,
        }
    }

    /// Attaches an async dispatch handler, preferred over the sync one in
    /// [`call`](Self::call). Enables awaiting a real MCP client round-trip.
    pub fn with_async_handler(mut self, handler: McpAsyncHandler) -> Self {
        self.async_handler = Some(handler);
        self
    }

    /// The MCP server this tool belongs to.
    pub fn server(&self) -> &str {
        &self.server
    }

    /// Whether this tool allows request metadata propagation.
    ///
    /// This is a per-tool restriction only: the actual decision also ANDs the
    /// connection-level switch, which is read live on every call.
    pub fn propagate_meta(&self) -> bool {
        self.propagate_meta
    }

    /// Configures whether this tool allows request metadata propagation.
    ///
    /// Setting this to `false` permanently restricts this tool; setting it back to
    /// `true` only re-enables propagation if the connection-level switch also allows
    /// it.
    pub fn with_propagate_meta(mut self, propagate_meta: bool) -> Self {
        self.propagate_meta = propagate_meta;
        self
    }

    /// Builds the request metadata sent to the server (Java `McpTool.buildMetaMap`).
    ///
    /// Today this carries the framework tool-call id under
    /// `io.agentscope/toolCallId`. Java additionally merges user `McpMeta` entries
    /// registered in the runtime context; Rust has no typed context-key mechanism, so
    /// that half is not implemented (see the module report).
    fn build_meta(&self, ctx: &ToolExecutionContext) -> JsonMap {
        let mut meta = JsonMap::new();
        if let Some(call) = &ctx.tool_call {
            meta.insert(
                "io.agentscope/toolCallId".to_string(),
                serde_json::Value::String(call.id.clone()),
            );
        }
        meta
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    fn output_schema(&self) -> Option<&serde_json::Value> {
        self.output_schema.as_ref()
    }

    /// An MCP tool is executed in this process: the handler performs the client
    /// round-trip. Reporting `true` here would make the executor short-circuit every
    /// call with "Tool ... is external and cannot be executed by the framework", which
    /// is only correct for schema-only tools (`register_schema`).
    fn is_external(&self) -> bool {
        false
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.parameters.clone()
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        // Request metadata is propagated only when this tool allows it; `None` omits
        // the MCP `_meta` field entirely so no metadata leaves the process.
        let meta = if self.propagate_meta {
            Some(self.build_meta(&_ctx))
        } else {
            None
        };
        let outcome = if let Some(async_handler) = &self.async_handler {
            async_handler(self, args, meta).await
        } else {
            (self.handler)(self, args)
        };
        match outcome {
            Ok(block) => Ok(block),
            Err(e) => Err(Error::Tool(format!(
                "MCP tool '{}' on server '{}' failed: {}",
                self.name, self.server, e
            ))),
        }
    }
}

/// A fallback handler that reports the tool has not been wired to an MCP client.
pub fn not_connected_handler(tool: &McpTool, _args: JsonMap) -> Result<ToolResultBlock> {
    Ok(ToolResultBlock::new(
        "",
        &tool.name,
        vec![ContentBlock::Text(TextBlock::new(format!(
            "Tool '{}' from server '{}' is not connected to an MCP client.",
            tool.name, tool.server
        )))],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolUseBlock;
    use std::sync::{Arc, Mutex};

    /// Builds a tool whose async handler records the propagated metadata.
    fn meta_recording_tool(propagate: bool) -> (McpTool, Arc<Mutex<Option<Option<JsonMap>>>>) {
        let seen: Arc<Mutex<Option<Option<JsonMap>>>> = Arc::new(Mutex::new(None));
        let seen_handler = Arc::clone(&seen);
        let tool = McpTool::new(
            "srv",
            "remote_tool",
            "desc",
            serde_json::json!({"type": "object"}),
            Box::new(|_, _| Ok(ToolResultBlock::text("sync"))),
        )
        .with_async_handler(Box::new(move |_t, _a, meta| {
            let seen = Arc::clone(&seen_handler);
            Box::pin(async move {
                *seen.lock().unwrap() = Some(meta);
                Ok(ToolResultBlock::text("ok"))
            })
        }))
        .with_propagate_meta(propagate);
        (tool, seen)
    }

    fn ctx_with_call(id: &str) -> ToolExecutionContext {
        ToolExecutionContext {
            tool_call: Some(ToolUseBlock::new(id, "remote_tool", JsonMap::new())),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn meta_carries_the_tool_call_id_when_propagation_is_enabled() {
        let (tool, seen) = meta_recording_tool(true);
        assert!(tool.propagate_meta());

        tool.call(JsonMap::new(), ctx_with_call("call-42"))
            .await
            .unwrap();

        let meta = seen
            .lock()
            .unwrap()
            .clone()
            .expect("handler must be called");
        let meta = meta.expect("propagation on -> Some(meta)");
        assert_eq!(
            meta.get("io.agentscope/toolCallId"),
            Some(&serde_json::Value::String("call-42".to_string()))
        );
    }

    #[tokio::test]
    async fn meta_is_none_when_the_tool_disables_propagation() {
        // Fail-closed: the whole `_meta` field is omitted, not sent empty.
        let (tool, seen) = meta_recording_tool(false);
        assert!(!tool.propagate_meta());

        tool.call(JsonMap::new(), ctx_with_call("call-7"))
            .await
            .unwrap();

        let meta = seen
            .lock()
            .unwrap()
            .clone()
            .expect("handler must be called");
        assert!(meta.is_none(), "propagation off -> None, got {meta:?}");
    }

    fn bare_tool() -> McpTool {
        McpTool::new(
            "srv",
            "repair__create_ticket",
            "creates a ticket",
            serde_json::json!({"type": "object"}),
            Box::new(|_, _| Ok(ToolResultBlock::text("ok"))),
        )
    }

    #[test]
    fn propagation_defaults_to_enabled() {
        assert!(bare_tool().propagate_meta());
    }

    #[test]
    fn title_defaults_to_none() {
        assert_eq!(bare_tool().title(), None);
    }

    #[test]
    fn output_schema_defaults_to_none_and_is_reachable_through_dyn_tool() {
        let tool = bare_tool();
        assert_eq!(tool.output_schema(), None);
        // The toolkit and the provider converters hold `dyn Tool`, so the override must
        // be visible there too.
        let as_dyn: &dyn Tool = &tool;
        assert_eq!(as_dyn.output_schema(), None);
    }

    #[test]
    fn with_output_schema_is_carried_on_the_tool() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"ticket_id": {"type": "string"}}
        });
        let tool = bare_tool().with_output_schema(schema.clone());
        let as_dyn: &dyn Tool = &tool;
        assert_eq!(as_dyn.output_schema(), Some(&schema));
    }

    #[test]
    fn empty_or_non_object_output_schemas_are_ignored() {
        // A server that sends `{}` declares no output contract.
        assert_eq!(
            bare_tool()
                .with_output_schema(serde_json::json!({}))
                .output_schema(),
            None
        );
        assert_eq!(
            bare_tool()
                .with_output_schema(serde_json::Value::Null)
                .output_schema(),
            None
        );
        assert_eq!(
            bare_tool()
                .with_output_schema(serde_json::json!("not a schema"))
                .output_schema(),
            None
        );
    }

    #[test]
    fn output_schema_from_definition_reads_both_keys() {
        let camel = serde_json::json!({
            "name": "t",
            "outputSchema": {"type": "object", "properties": {"a": {"type": "string"}}}
        });
        assert!(bare_tool()
            .with_output_schema_from_definition(&camel)
            .output_schema()
            .is_some());

        let snake = serde_json::json!({
            "output_schema": {"type": "object", "properties": {"b": {"type": "boolean"}}}
        });
        assert!(bare_tool()
            .with_output_schema_from_definition(&snake)
            .output_schema()
            .is_some());

        // Absent or empty stays `None`.
        assert_eq!(
            bare_tool()
                .with_output_schema_from_definition(&serde_json::json!({"name": "t"}))
                .output_schema(),
            None
        );
        assert_eq!(
            bare_tool()
                .with_output_schema_from_definition(&serde_json::json!({"outputSchema": {}}))
                .output_schema(),
            None
        );
    }

    #[test]
    fn resolve_display_title_prefers_tool_title() {
        let def = serde_json::json!({
            "title": "Create Ticket",
            "annotations": {"title": "Annotation Title"}
        });
        assert_eq!(
            McpTool::resolve_display_title(&def).as_deref(),
            Some("Create Ticket")
        );
    }

    #[test]
    fn resolve_display_title_falls_back_to_annotations() {
        let def = serde_json::json!({"annotations": {"title": "Annotation Title"}});
        assert_eq!(
            McpTool::resolve_display_title(&def).as_deref(),
            Some("Annotation Title")
        );
    }

    #[test]
    fn resolve_display_title_ignores_blank_values() {
        // A blank top-level title must not mask the annotation fallback.
        let def = serde_json::json!({
            "title": "   ",
            "annotations": {"title": "Annotation Title"}
        });
        assert_eq!(
            McpTool::resolve_display_title(&def).as_deref(),
            Some("Annotation Title")
        );
        assert_eq!(McpTool::resolve_display_title(&serde_json::json!({})), None);
    }

    #[test]
    fn title_is_reachable_through_the_tool_trait() {
        let tool = bare_tool().with_title_from_definition(&serde_json::json!({
            "annotations": {"title": "Create Ticket"}
        }));
        // Access through `dyn Tool`, as Toolkit consumers do.
        let dynamic: &dyn Tool = &tool;
        assert_eq!(dynamic.title(), Some("Create Ticket"));
        assert_eq!(dynamic.name(), "repair__create_ticket");
    }
}
