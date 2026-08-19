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
pub struct McpTool {
    name: String,
    description: String,
    parameters: serde_json::Value,
    /// Server name this tool belongs to (used in error messages).
    server: String,
    handler:
        Box<dyn Fn(&McpTool, JsonMap) -> Result<ToolResultBlock> + Send + Sync>,
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
            parameters,
            server: server.into(),
            handler,
        }
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

    fn is_external(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.parameters.clone()
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        match (self.handler)(self, args) {
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
