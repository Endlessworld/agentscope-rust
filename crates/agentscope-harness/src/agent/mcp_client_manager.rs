//! MCP Client Manager for managing MCP client connections and lifecycle.
//!
//! This module provides a manager for MCP client connections, handling
//! client creation, connection, disconnection, and tool discovery.

use agentscope_core::error::{Error, Result};
use rust_mcp_sdk::error::SdkResult;
use rust_mcp_sdk::mcp_client::{client_runtime, ClientHandler, McpClientOptions};
use rust_mcp_sdk::schema::{
    ClientCapabilities, Implementation, InitializeRequestParams,
    LATEST_PROTOCOL_VERSION,
};
use rust_mcp_sdk::{McpClient, StdioTransport, ToMcpClientHandler, TransportOptions};
use rust_mcp_transport::{
    ClientSseTransport, ClientSseTransportOptions, ClientStreamableTransport,
    RequestOptions, StreamableTransportOptions,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Represents a connected MCP client instance.
#[derive(Clone)]
pub struct McpClientInstance {
    /// Unique identifier for this client instance.
    pub id: String,
    /// Server configuration used to create this client.
    pub config: McpServerConfig,
    /// Whether the client is currently connected.
    pub connected: bool,
    /// Tools discovered from the MCP server.
    pub tools: Vec<McpToolInfo>,
    /// The actual MCP client runtime.
    client: Option<Arc<dyn McpClient>>,
}

/// Information about an MCP tool discovered from a server.
#[derive(Debug, Clone)]
pub struct McpToolInfo {
    /// Tool name.
    pub name: String,
    /// Tool description.
    pub description: Option<String>,
    /// JSON Schema for tool parameters.
    pub parameters: serde_json::Value,
    /// Server name this tool belongs to.
    pub server: String,
}

/// Configuration for an MCP server to register as a tool source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    pub name: String,
    /// stdio launch command (e.g. `npx -y @modelcontextprotocol/server-filesystem`).
    pub command: Option<String>,
    /// SSE/HTTP transport endpoint (alternative to `command`).
    pub url: Option<String>,
    /// Optional HTTP/SSE custom headers (name -> value).
    pub headers: Vec<(String, String)>,
    /// Whether request metadata is propagated to this MCP server.
    ///
    /// Mirrors Java `McpClientWrapper.propagateMeta`: default `true`, ANDed with the
    /// per-tool switch at call time. The value is read live from the stored client
    /// instance on every call, so `set_propagate_meta` takes effect immediately.
    pub propagate_meta: bool,
}

impl McpServerConfig {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            command: None,
            url: None,
            headers: Vec::new(),
            propagate_meta: true,
        }
    }

    pub fn command(mut self, command: impl Into<String>) -> Self {
        self.command = Some(command.into());
        self
    }

    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    pub fn headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.headers = headers;
        self
    }

    /// Configures whether request metadata is propagated to this server.
    ///
    /// Disable it for MCP servers that are not fully trusted so internal metadata
    /// (e.g. trace ids or callback URLs) never leaves the process. Mirrors Java
    /// `McpClientBuilder.propagateMeta`.
    pub fn propagate_meta(mut self, propagate_meta: bool) -> Self {
        self.propagate_meta = propagate_meta;
        self
    }
}

/// Manages MCP client connections and lifecycle.
pub struct McpClientManager {
    /// Map of client ID to client instance.
    clients: Mutex<HashMap<String, McpClientInstance>>,
    /// Counter for generating unique client IDs.
    id_counter: Mutex<u64>,
}

impl McpClientManager {
    /// Creates a new MCP client manager.
    pub fn new() -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
            id_counter: Mutex::new(0),
        }
    }

    /// Creates a new client instance with the given configuration.
    ///
    /// This method creates a client but does not connect to the server.
    /// Use `connect` to establish the connection.
    pub async fn create_client(&self, config: McpServerConfig) -> Result<String> {
        let mut counter = self.id_counter.lock().await;
        let id = format!("mcp_client_{}", *counter);
        *counter += 1;

        let instance = McpClientInstance {
            id: id.clone(),
            config,
            connected: false,
            tools: Vec::new(),
            client: None,
        };

        let mut clients = self.clients.lock().await;
        clients.insert(id.clone(), instance);

        Ok(id)
    }

    /// Connects to an MCP server using the specified client ID.
    ///
    /// This method establishes the connection to the MCP server and
    /// performs the initialization handshake.
    pub async fn connect(&self, client_id: &str) -> Result<()> {
        let mut clients = self.clients.lock().await;
        let client = clients
            .get_mut(client_id)
            .ok_or_else(|| Error::Other(format!("Client '{}' not found", client_id)))?;

        if client.connected {
            return Ok(());
        }

        // Create client details
        let client_details = InitializeRequestParams {
            capabilities: ClientCapabilities::default(),
            client_info: Implementation {
                name: "agentscope-harness-mcp-client".into(),
                version: "0.1.0".into(),
                title: Some("AgentScope Harness MCP Client".into()),
                description: Some("MCP Client for AgentScope Harness".into()),
                icons: vec![],
                website_url: None,
            },
            protocol_version: LATEST_PROTOCOL_VERSION.into(),
            meta: None,
        };

        // Create handler
        let handler = DefaultClientHandler {};

        // Create the MCP client. The transport type varies by config:
        // - `command`  -> stdio
        // - `url`      -> SSE (path ends with /sse) or Streamable HTTP otherwise
        let headers_map = if client.config.headers.is_empty() {
            None
        } else {
            Some(
                client
                    .config
                    .headers
                    .iter()
                    .cloned()
                    .collect::<std::collections::HashMap<_, _>>(),
            )
        };
        let mcp_client = if let Some(command) = &client.config.command {
            // Parse command and arguments
            let parts: Vec<&str> = command.split_whitespace().collect();
            if parts.is_empty() {
                return Err(Error::Other("Empty command".to_string()));
            }
            let program = parts[0];
            let args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();

            let transport = StdioTransport::create_with_server_launch(
                program,
                args,
                None,
                TransportOptions::default(),
            )
            .map_err(|e| Error::Other(format!("Failed to create stdio transport: {}", e)))?;
            client_runtime::create_client(McpClientOptions {
                client_details,
                transport,
                handler: handler.to_mcp_client_handler(),
                task_store: None,
                server_task_store: None,
                message_observer: None,
            })
        } else if let Some(url) = &client.config.url {
            if is_sse_url(url) {
                // Legacy SSE transport.
                let sse_options = ClientSseTransportOptions {
                    custom_headers: headers_map,
                    ..ClientSseTransportOptions::default()
                };
                let transport = ClientSseTransport::new(url, sse_options)
                    .map_err(|e| Error::Other(format!("Failed to create SSE transport: {}", e)))?;
                client_runtime::create_client(McpClientOptions {
                    client_details,
                    transport,
                    handler: handler.to_mcp_client_handler(),
                    task_store: None,
                    server_task_store: None,
                    message_observer: None,
                })
            } else {
                // Streamable HTTP transport.
                let transport_options = StreamableTransportOptions {
                    mcp_url: url.clone(),
                    request_options: RequestOptions {
                        custom_headers: headers_map,
                        ..RequestOptions::default()
                    },
                };
                let transport = ClientStreamableTransport::new(&transport_options, None, true)
                    .map_err(|e| {
                        Error::Other(format!("Failed to create streamable HTTP transport: {}", e))
                    })?;
                client_runtime::create_client(McpClientOptions {
                    client_details,
                    transport,
                    handler: handler.to_mcp_client_handler(),
                    task_store: None,
                    server_task_store: None,
                    message_observer: None,
                })
            }
        } else {
            return Err(Error::Other("No command or URL specified".to_string()));
        };

        // Start the client
        mcp_client
            .clone()
            .start()
            .await
            .map_err(|e| Error::Other(format!("Failed to start MCP client: {}", e)))?;

        // Update client instance
        client.connected = true;
        client.client = Some(mcp_client);

        // Discover the server's tools so they can be bridged into a Toolkit.
        // Without this, `list_tools`/`call_tool` would see an empty tool list.
        let server_name = client.config.name.clone();
        let discovered = match client
            .client
            .as_ref()
            .unwrap()
            .request_tool_list(None)
            .await
        {
            Ok(result) => result
                .tools
                .into_iter()
                .map(|tool| McpToolInfo {
                    name: tool.name,
                    description: tool.description.clone(),
                    parameters: serde_json::to_value(tool.input_schema)
                        .unwrap_or_else(|_| serde_json::json!({ "type": "object" })),
                    server: server_name.clone(),
                })
                .collect::<Vec<_>>(),
            Err(e) => {
                tracing::warn!(
                    "Failed to list tools for MCP client '{}': {}",
                    client_id, e
                );
                Vec::new()
            }
        };
        client.tools = discovered;

        tracing::info!(
            "MCP client '{}' connected to server '{}' ({} tools)",
            client_id,
            client.config.name,
            client.tools.len()
        );

        Ok(())
    }

    /// Disconnects from an MCP server.
    pub async fn disconnect(&self, client_id: &str) -> Result<()> {
        let mut clients = self.clients.lock().await;
        let client = clients
            .get_mut(client_id)
            .ok_or_else(|| Error::Other(format!("Client '{}' not found", client_id)))?;

        if !client.connected {
            return Ok(());
        }

        // Shutdown the client
        if let Some(mcp_client) = &client.client {
            mcp_client
                .shut_down()
                .await
                .map_err(|e| Error::Other(format!("Failed to shut down MCP client: {}", e)))?;
        }

        client.connected = false;
        client.tools.clear();
        client.client = None;

        tracing::info!(
            "MCP client '{}' disconnected from server '{}'",
            client_id,
            client.config.name
        );

        Ok(())
    }

    /// Lists tools available from the specified client.
    pub async fn list_tools(&self, client_id: &str) -> Result<Vec<McpToolInfo>> {
        let clients = self.clients.lock().await;
        let client = clients
            .get(client_id)
            .ok_or_else(|| Error::Other(format!("Client '{}' not found", client_id)))?;

        if !client.connected {
            return Err(Error::Other(format!(
                "Client '{}' is not connected",
                client_id
            )));
        }

        Ok(client.tools.clone())
    }

    /// Gets a list of all connected clients.
    pub async fn connected_clients(&self) -> Vec<String> {
        let clients = self.clients.lock().await;
        clients
            .iter()
            .filter(|(_, instance)| instance.connected)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Gets client information by ID.
    pub async fn get_client(&self, client_id: &str) -> Option<McpClientInstance> {
        let clients = self.clients.lock().await;
        clients.get(client_id).cloned()
    }

    /// Removes a client instance.
    pub async fn remove_client(&self, client_id: &str) -> Result<()> {
        let mut clients = self.clients.lock().await;
        clients
            .remove(client_id)
            .ok_or_else(|| Error::Other(format!("Client '{}' not found", client_id)))?;

        Ok(())
    }

    /// Calls a tool on the specified client.
    pub async fn call_tool(
        &self,
        client_id: &str,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.call_tool_with_meta(client_id, tool_name, arguments, None)
            .await
    }

    /// Calls a tool on the specified client with request metadata.
    ///
    /// `meta` becomes the MCP request's `_meta` field; `None` omits the field
    /// entirely (fail-closed), mirroring Java `McpClientWrapper.callTool` when
    /// propagation is disabled.
    pub async fn call_tool_with_meta(
        &self,
        client_id: &str,
        tool_name: &str,
        arguments: serde_json::Value,
        meta: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<serde_json::Value> {
        let clients = self.clients.lock().await;
        let client = clients
            .get(client_id)
            .ok_or_else(|| Error::Other(format!("Client '{}' not found", client_id)))?;

        if !client.connected {
            return Err(Error::Other(format!(
                "Client '{}' is not connected",
                client_id
            )));
        }

        if let Some(mcp_client) = &client.client {
            // Convert arguments to MCP tool call params
            let params = rust_mcp_sdk::schema::CallToolRequestParams {
                name: tool_name.to_string(),
                arguments: Some(arguments.as_object().cloned().unwrap_or_default()),
                meta: meta.map(|extra| rust_mcp_sdk::schema::CallToolMeta {
                    progress_token: None,
                    extra: Some(extra),
                }),
                task: None,
            };

            // Call the tool
            let result = mcp_client
                .request_tool_call(params)
                .await
                .map_err(|e| Error::Other(format!("Tool call failed: {}", e)))?;

            // Convert result to serde_json::Value
            Ok(serde_json::to_value(result)
                .map_err(|e| Error::Other(format!("Failed to serialize result: {}", e)))?)
        } else {
            Err(Error::Other("Client not initialized".to_string()))
        }
    }

    /// Finds the connected client id for the given server name.
    pub async fn find_client_id_by_server(&self, server: &str) -> Option<String> {
        let clients = self.clients.lock().await;
        clients.iter().find_map(|(id, inst)| {
            if inst.connected && inst.config.name == server {
                Some(id.clone())
            } else {
                None
            }
        })
    }

    /// Calls a tool on the connected client for the given server name.
    pub async fn call_tool_by_server(
        &self,
        server: &str,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.call_tool_by_server_with_meta(server, tool_name, arguments, None)
            .await
    }

    /// Calls a tool on the connected client for the given server name, propagating
    /// request metadata when supplied.
    pub async fn call_tool_by_server_with_meta(
        &self,
        server: &str,
        tool_name: &str,
        arguments: serde_json::Value,
        meta: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<serde_json::Value> {
        let client_id = self
            .find_client_id_by_server(server)
            .await
            .ok_or_else(|| {
                Error::Other(format!("No connected MCP client for server '{server}'"))
            })?;
        self.call_tool_with_meta(&client_id, tool_name, arguments, meta)
            .await
    }

    /// Returns whether the connection for `client_id` currently propagates metadata.
    ///
    /// `None` when the client id is unknown.
    pub async fn propagate_meta(&self, client_id: &str) -> Option<bool> {
        let clients = self.clients.lock().await;
        clients.get(client_id).map(|c| c.config.propagate_meta)
    }

    /// Turns request-metadata propagation on or off for an existing connection.
    ///
    /// Mirrors Java `McpClientWrapper.setPropagateMeta`: the switch is stored on the
    /// live client instance, so the next tool call sees it. Returns `false` when the
    /// client id is unknown.
    pub async fn set_propagate_meta(&self, client_id: &str, propagate_meta: bool) -> bool {
        let mut clients = self.clients.lock().await;
        match clients.get_mut(client_id) {
            Some(client) => {
                client.config.propagate_meta = propagate_meta;
                true
            }
            None => false,
        }
    }
}

/// Default client handler for MCP client.
struct DefaultClientHandler;

#[async_trait::async_trait]
impl ClientHandler for DefaultClientHandler {
    // Use default implementations for all methods
}

impl std::fmt::Debug for McpClientInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClientInstance")
            .field("id", &self.id)
            .field("config", &self.config)
            .field("connected", &self.connected)
            .field("tools_count", &self.tools.len())
            .finish()
    }
}
impl Default for McpClientManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns `true` when `url` should use the legacy SSE transport (path contains
/// `/sse`), otherwise the URL is treated as a Streamable HTTP endpoint.
fn is_sse_url(url: &str) -> bool {
    url.contains("/sse")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_url_routing() {
        assert!(is_sse_url("http://127.0.0.1:3001/sse"));
        assert!(is_sse_url("https://mcp.example.com/v1/sse"));
        assert!(!is_sse_url("http://127.0.0.1:3001/mcp"));
        assert!(!is_sse_url("https://mcp.example.com/mcp"));
    }

    #[test]
    fn config_builder_sets_url_and_headers() {
        let cfg = McpServerConfig::new("s").url("http://x/sse").headers(vec![
            ("Authorization".into(), "Bearer t".into()),
        ]);
        assert_eq!(cfg.url.as_deref(), Some("http://x/sse"));
        assert_eq!(cfg.headers.len(), 1);
        assert_eq!(cfg.headers[0], ("Authorization".to_string(), "Bearer t".to_string()));
    }
}
