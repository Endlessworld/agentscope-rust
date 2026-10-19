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
/// An async MCP dispatch handler: given the tool and its argument map, returns
/// a pinned future that resolves to a [`ToolResultBlock`].
pub type McpAsyncHandler = Box<
    dyn Fn(&McpTool, JsonMap) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ToolResultBlock>> + Send>,
    > + Send
        + Sync,
>;

pub struct McpTool {
    name: String,
    description: String,
    /// Optional human-readable display title (see [`Tool::title`]).
    title: Option<String>,
    parameters: serde_json::Value,
    /// Server name this tool belongs to (used in error messages).
    server: String,
    handler:
        Box<dyn Fn(&McpTool, JsonMap) -> Result<ToolResultBlock> + Send + Sync>,
    /// Optional async handler, preferred over [`handler`](Self::handler) when
    /// present. Lets MCP tool calls await a real client round-trip without
    /// blocking the tokio runtime.
    async_handler: Option<McpAsyncHandler>,
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
            parameters,
            server: server.into(),
            handler,
            async_handler: None,
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
        non_blank(definition.get("title")).or_else(|| {
            non_blank(definition.get("annotations").and_then(|a| a.get("title")))
        })
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

    fn is_external(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.parameters.clone()
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let outcome = if let Some(async_handler) = &self.async_handler {
            async_handler(self, args).await
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
    fn title_defaults_to_none() {
        assert_eq!(bare_tool().title(), None);
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
