//! MCP-over-ACP: an MCP server supplied by the ACP client over the existing
//! ACP channel, using stateless `mcp/message` requests.
//!
//! Per RFD *MCP-over-ACP: Stateless MCP Requests over ACP* (schema 1.15, MCP
//! 2026-07-28): the **client** declares `McpServer::Acp { name, serverId }` in a
//! session setup request; the **agent** then drives that server by sending ACP
//! requests to the client. There is **no** connect/disconnect handshake and no
//! inner MCP `initialize`: every call is one `mcp/message` carrying
//! `serverId` + a caller-generated `requestId` + the inner method/params, and is
//! answered with either an inner MCP result or an inner [`McpError`] (an MCP
//! error code, never an ACP one).
//!
//! This module keeps the wire parsing pure and testable, and bridges the
//! discovered tools into a core [`Toolkit`] so the ReAct loop can call them.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use agent_client_protocol::v1::schema::{
    McpRequestId, McpServerAcpId, MessageMcpRequest, MessageMcpResponse,
};
use agent_client_protocol::{v1::ConnectionTo, Client};
use agentscope_core::message::{ContentBlock, TextBlock, ToolResultBlock};
use agentscope_core::tool::{McpTool, Toolkit};

/// A tool advertised by an ACP-transport MCP server (`tools/list`).
#[derive(Debug, Clone, PartialEq)]
pub struct McpAcpTool {
    pub name: String,
    pub description: Option<String>,
    pub parameters: serde_json::Value,
}

/// Parses a `tools/list` result into descriptors.
///
/// The result carries a `tools` array whose entries have `name`, optional
/// `description`, and optional `inputSchema`. Malformed entries are skipped so a
/// single bad tool does not drop the whole server.
pub fn parse_tools_list(result: &serde_json::Value) -> Vec<McpAcpTool> {
    let Some(tools) = result.get("tools").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    tools
        .iter()
        .filter_map(|tool| {
            let name = tool.get("name")?.as_str()?.to_string();
            let description = tool
                .get("description")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let parameters = tool
                .get("inputSchema")
                .or_else(|| tool.get("input_schema"))
                .cloned()
                .unwrap_or_else(|| serde_json::json!({ "type": "object" }));
            Some(McpAcpTool {
                name,
                description,
                parameters,
            })
        })
        .collect()
}

/// Renders a `tools/call` result into a single displayable text block.
///
/// MCP returns `content: [{ type: "text", text: ... }, ...]`; non-text blocks
/// are rendered as their JSON form so nothing is silently dropped.
pub fn render_tool_result(result: &serde_json::Value) -> String {
    let Some(content) = result.get("content").and_then(|v| v.as_array()) else {
        // No structured content: fall back to the whole result.
        return match result {
            serde_json::Value::Null => String::new(),
            other => serde_json::to_string_pretty(other).unwrap_or_default(),
        };
    };
    let mut parts = Vec::new();
    for block in content {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                    parts.push(text.to_string());
                }
            }
            _ => parts.push(serde_json::to_string(block).unwrap_or_default()),
        }
    }
    parts.join("\n")
}

/// Mints the id for one inner MCP request.
///
/// The id addresses a single request (it is what a proxy preserves across hops), so it must be
/// fresh per call — reusing one would make a proxy mistake a later call for an earlier one.
fn next_request_id() -> McpRequestId {
    McpRequestId::new(uuid::Uuid::new_v4().to_string())
}

/// Unwraps the ACP carrier into the inner MCP result, or into a tool-level failure.
///
/// An inner MCP error arrives as a *successful* ACP response, so it must be surfaced as a tool
/// failure instead of pretending the ACP call itself failed.
fn response_payload(
    server: &str,
    response: MessageMcpResponse,
) -> Result<serde_json::Value, agent_client_protocol::Error> {
    match response {
        MessageMcpResponse::Result { result, .. } => Ok(result),
        MessageMcpResponse::Error { error, .. } => Err(agent_client_protocol::Error::new(
            agent_client_protocol::ErrorCode::INTERNAL_ERROR,
            format!(
                "MCP server `{server}` returned error {}: {}",
                error.code, error.message
            ),
        )),
        // `MessageMcpResponse` is `#[non_exhaustive]`: a future carrier may add shapes we
        // cannot interpret, so treat them as a tool-level failure too.
        _ => Err(agent_client_protocol::Error::new(
            agent_client_protocol::ErrorCode::INTERNAL_ERROR,
            format!("MCP server `{server}` sent an unknown response"),
        )),
    }
}

/// A live MCP-over-ACP handle for a client-declared MCP server.
///
/// Cheaply cloneable: it only holds the ACP connection plus the `serverId` the
/// client declared. Each call is self-contained (stateless protocol), so there
/// is no connection lifecycle to manage.
#[derive(Clone)]
pub struct McpAcpConnection {
    connection: ConnectionTo<Client>,
    server_id: McpServerAcpId,
    /// Human-readable server name (used when bridging tools into a toolkit).
    name: String,
}

impl McpAcpConnection {
    /// Binds a handle to a client-declared ACP MCP server.
    ///
    /// No round-trip happens here: the stateless protocol has no handshake, so
    /// this only records `(connection, serverId)`. Tool discovery is an ordinary
    /// `tools/list` request (see [`Self::list_tools`]).
    pub fn new(
        connection: ConnectionTo<Client>,
        name: impl Into<String>,
        server_id: impl Into<McpServerAcpId>,
    ) -> Self {
        Self {
            connection,
            server_id: server_id.into(),
            name: name.into(),
        }
    }

    /// The server name this connection was opened for.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Sends one wrapped MCP request (`mcp/message`) and returns the inner MCP
    /// result payload.
    ///
    /// Each call gets a fresh [`McpRequestId`]: the id addresses this one inner
    /// request (it is what a proxy preserves), not a connection.
    pub async fn request(
        &self,
        method: &str,
        params: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<serde_json::Value, agent_client_protocol::Error> {
        let request = MessageMcpRequest::new(self.server_id.clone(), next_request_id(), method)
            .params(params);
        let response = self.connection.send_request(request).block_task().await?;
        response_payload(&self.name, response)
    }

    /// Lists the tools the server offers.
    pub async fn list_tools(&self) -> Result<Vec<McpAcpTool>, agent_client_protocol::Error> {
        let result = self.request("tools/list", None).await?;
        Ok(parse_tools_list(&result))
    }

    /// Calls one tool on the server and renders its result.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<String, agent_client_protocol::Error> {
        let mut params = serde_json::Map::new();
        params.insert(
            "name".to_string(),
            serde_json::Value::String(name.to_string()),
        );
        params.insert("arguments".to_string(), arguments);
        let result = self.request("tools/call", Some(params)).await?;
        Ok(render_tool_result(&result))
    }

    /// Discovers the server's tools and registers each as a core [`McpTool`]
    /// whose call dispatches back through this connection.
    ///
    /// Returns the number of tools registered.
    pub async fn bridge_tools(
        &self,
        toolkit: &Toolkit,
    ) -> Result<usize, agent_client_protocol::Error> {
        let server = self.name.clone();
        let tools = self.list_tools().await?;
        let mut registered = 0usize;
        for tool in tools {
            let connection = self.clone();
            let tool_name = tool.name.clone();
            let handler: Box<
                dyn Fn(
                        &McpTool,
                        serde_json::Map<String, serde_json::Value>,
                        Option<serde_json::Map<String, serde_json::Value>>,
                    ) -> Pin<
                        Box<
                            dyn Future<Output = agentscope_core::error::Result<ToolResultBlock>>
                                + Send,
                        >,
                    > + Send
                    + Sync,
            > = Box::new(move |_tool, args, _meta| {
                // MCP-over-ACP has no `_meta` channel on the ACP request, so the
                // propagated metadata is deliberately dropped here rather than being
                // smuggled into the tool arguments.
                let connection = connection.clone();
                let tool_name = tool_name.clone();
                Box::pin(async move {
                    let arguments = serde_json::Value::Object(args);
                    match connection.call_tool(&tool_name, arguments).await {
                        Ok(text) => Ok(ToolResultBlock::new(
                            "",
                            &tool_name,
                            vec![ContentBlock::Text(TextBlock::new(text))],
                        )),
                        Err(e) => Err(agentscope_core::error::Error::Other(format!(
                            "MCP-over-ACP tool call failed: {e}"
                        ))),
                    }
                })
            });
            let tool = McpTool::new(
                server.clone(),
                tool.name.clone(),
                tool.description.clone().unwrap_or_default(),
                tool.parameters.clone(),
                Box::new(|_t, _a| Ok(ToolResultBlock::new("", "", vec![]))),
            )
            .with_async_handler(handler);
            toolkit.register_tool(Arc::new(tool));
            registered += 1;
        }
        Ok(registered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tools_list_and_skips_malformed_entries() {
        let result = serde_json::json!({
            "tools": [
                {"name": "echo", "description": "Echo", "inputSchema": {"type": "object"}},
                {"description": "no name"},
                {"name": "no_schema"}
            ]
        });
        let tools = parse_tools_list(&result);
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].description.as_deref(), Some("Echo"));
        assert_eq!(tools[0].parameters, serde_json::json!({"type": "object"}));
        assert_eq!(tools[1].name, "no_schema");
        assert_eq!(tools[1].parameters, serde_json::json!({"type": "object"}));
    }

    #[test]
    fn missing_tools_array_yields_no_tools() {
        assert!(parse_tools_list(&serde_json::json!({})).is_empty());
        assert!(parse_tools_list(&serde_json::json!(null)).is_empty());
    }

    #[test]
    fn renders_text_content_and_jsonifies_other_blocks() {
        let result = serde_json::json!({
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "image", "data": "abc"}
            ]
        });
        let rendered = render_tool_result(&result);
        assert!(rendered.starts_with("hello"));
        assert!(
            rendered.contains("\"type\":\"image\"") || rendered.contains("\"type\": \"image\"")
        );
    }

    #[test]
    fn renders_bare_result_when_no_content_array() {
        assert_eq!(render_tool_result(&serde_json::json!(null)), "");
        let rendered = render_tool_result(&serde_json::json!({ "ok": true }));
        assert!(rendered.contains("ok"));
    }

    /// ACP-34 相关批次的 ACP-U04：`mcp/message` 的两种载体形态都要按「工具结果」处理。
    #[test]
    fn unwraps_a_result_and_surfaces_an_inner_error_as_a_tool_failure() {
        use agent_client_protocol::v1::schema::{McpError, MessageMcpResponse};

        let payload = response_payload(
            "srv",
            MessageMcpResponse::success(serde_json::json!({ "tools": [] })),
        )
        .expect("an inner result is returned verbatim");
        assert_eq!(payload, serde_json::json!({ "tools": [] }));

        // The inner MCP error is a successful ACP response, so it must not look like a
        // transport failure -- but it must still fail the tool call, with a readable message.
        let error = response_payload(
            "srv",
            MessageMcpResponse::error(McpError::new(-32601, "no such tool")),
        )
        .expect_err("an inner MCP error fails the tool call");
        assert_eq!(error.code, agent_client_protocol::ErrorCode::INTERNAL_ERROR);
        assert!(error.message.contains("no such tool"), "{}", error.message);
        assert!(error.message.contains("srv"), "{}", error.message);
    }

    #[test]
    fn every_request_gets_its_own_id() {
        let first = next_request_id();
        let second = next_request_id();
        assert_ne!(first.0.as_ref(), second.0.as_ref());
    }
}
