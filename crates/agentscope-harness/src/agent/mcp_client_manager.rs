//! MCP Client Manager for managing MCP client connections and lifecycle.
//!
//! This module provides a manager for MCP client connections, handling
//! client creation, connection, disconnection, and tool discovery.

use agentscope_core::error::{Error, Result};
use rust_mcp_sdk::error::SdkResult;
use rust_mcp_sdk::mcp_client::{client_runtime, ClientHandler, McpClientOptions};
use rust_mcp_sdk::schema::{
    ClientCapabilities, Implementation, InitializeRequestParams, LATEST_PROTOCOL_VERSION,
};
use rust_mcp_sdk::{McpClient, StdioTransport, ToMcpClientHandler, TransportOptions};
use rust_mcp_transport::{
    ClientSseTransport, ClientSseTransportOptions, ClientStreamableTransport, RequestOptions,
    StreamableTransportOptions,
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
    /// Human-readable display title (`Tool.title`) when the server advertises a
    /// non-blank one. Mirrors Java `McpSchema.Tool.title`.
    pub title: Option<String>,
    /// JSON Schema of the tool's structured output (`Tool.outputSchema`).
    ///
    /// Mirrors Java `McpSchema.Tool.outputSchema`; `None` when the server does not
    /// declare one.
    pub output_schema: Option<serde_json::Value>,
}

impl McpToolInfo {
    /// Creates tool info from the four fields the bridge has always carried.
    ///
    /// [`title`](Self::title) and [`output_schema`](Self::output_schema) default to
    /// `None`. The fields stay public for the discovery path, so out-of-crate call
    /// sites that only know the core four can use this constructor instead of a
    /// struct literal.
    pub fn new(
        name: impl Into<String>,
        description: Option<String>,
        parameters: serde_json::Value,
        server: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description,
            parameters,
            server: server.into(),
            title: None,
            output_schema: None,
        }
    }
}

/// Transport an MCP server connection must use.
///
/// Mirrors the `transport` discriminator of Java `tools/McpServerConfig`
/// (`stdio` / `sse` / `http`). `McpServerConfig::transport` is optional: when absent the
/// manager keeps sniffing the config (`command` -> stdio, `/sse` URL -> SSE, other URL ->
/// Streamable HTTP), which is the historical behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpTransport {
    /// Spawn a child process and speak MCP over stdin/stdout.
    Stdio,
    /// Legacy HTTP+SSE transport.
    Sse,
    /// Streamable HTTP transport (Java `http`).
    StreamableHttp,
}

impl McpTransport {
    /// The wire name used by `tools.json` (`http` for Streamable HTTP, like Java).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Sse => "sse",
            Self::StreamableHttp => "http",
        }
    }

    /// Parses the wire name; accepts `http` (Java) and `streamable_http`.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "stdio" => Some(Self::Stdio),
            "sse" => Some(Self::Sse),
            "http" => Some(Self::StreamableHttp),
            "streamable_http" | "streamable-http" | "streamablehttp" => Some(Self::StreamableHttp),
            _ => None,
        }
    }
}

/// The transport a config resolves to, together with the endpoint it will use.
///
/// Owns its strings so `connect()` can plan first and mutate the client instance after
/// the borrow of the config ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannedTransport {
    /// stdio, using `command` (command line including arguments).
    Stdio { command: String },
    /// Legacy SSE, using `url`.
    Sse { url: String },
    /// Streamable HTTP, using `url`.
    StreamableHttp { url: String },
}

impl PlannedTransport {
    /// The transport kind that was planned.
    pub fn kind(&self) -> McpTransport {
        match self {
            Self::Stdio { .. } => McpTransport::Stdio,
            Self::Sse { .. } => McpTransport::Sse,
            Self::StreamableHttp { .. } => McpTransport::StreamableHttp,
        }
    }
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
    /// Explicit transport selection. `None` keeps the sniffing behaviour (see
    /// [`McpTransport`]): `command` -> stdio, an `/sse` URL -> SSE, any other URL ->
    /// Streamable HTTP. Mirrors Java `tools/McpServerConfig.transport`.
    pub transport: Option<McpTransport>,
    /// Registers bridged tools as `<server>__<tool>` instead of the bare remote name, so two
    /// servers advertising the same tool name cannot overwrite each other. Mirrors Java
    /// `McpServerConfig.prefixToolNames` (default `false`).
    pub prefix_tool_names: bool,
    /// Whether connecting opens the streamable-HTTP server-to-client stream (a
    /// standalone `GET` to the MCP endpoint) up front.
    ///
    /// The transport used to open that stream unconditionally. A server that does not
    /// implement it answers `405`, and the failure happened *before* the initialization
    /// handshake, so a perfectly usable server could not be registered at all (#2003).
    /// With the default `false` messages travel over the `POST` responses, which every
    /// streamable-HTTP server supports; enable it for servers that push notifications
    /// between calls (`notifications/tools/list_changed`, resource updates, ...).
    /// Ignored by the stdio and SSE transports.
    pub open_connection_on_startup: bool,
}

impl McpServerConfig {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            command: None,
            url: None,
            headers: Vec::new(),
            propagate_meta: true,
            transport: None,
            prefix_tool_names: false,
            open_connection_on_startup: false,
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

    /// Pins the transport instead of letting `connect` sniff it from `command`/`url`.
    pub fn transport(mut self, transport: McpTransport) -> Self {
        self.transport = Some(transport);
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

    /// Registers bridged tools with a `<server>__` prefix. Mirrors Java
    /// `McpServerConfig.setPrefixToolNames`.
    pub fn prefix_tool_names(mut self, prefix: bool) -> Self {
        self.prefix_tool_names = prefix;
        self
    }

    /// Opens the streamable-HTTP server-to-client stream at connect time.
    ///
    /// Off by default so servers without a standalone stream (they answer `405` to the
    /// `GET`) can still be registered; see the field documentation above.
    pub fn open_connection_on_startup(mut self, open: bool) -> Self {
        self.open_connection_on_startup = open;
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
        // Snapshot the configuration, then drop the registry lock: the handshake and the
        // tool discovery await the network, and holding the lock across them stalled
        // every other operation on every other server (a slow `initialize` on one server
        // blocked `list_tools`/`call_tool` for all of them).
        let config = {
            let clients = self.clients.lock().await;
            let client = clients
                .get(client_id)
                .ok_or_else(|| Error::Other(format!("Client '{}' not found", client_id)))?;

            if client.connected {
                return Ok(());
            }
            client.config.clone()
        };

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

        // Choose the transport: an explicit `transport` pin wins, otherwise sniff the
        // config (`command` -> stdio, `/sse` URL -> SSE, other URL -> Streamable HTTP).
        let planned = plan_transport(&config)?;
        let headers_map = if config.headers.is_empty() {
            None
        } else {
            Some(
                config
                    .headers
                    .iter()
                    .cloned()
                    .collect::<std::collections::HashMap<_, _>>(),
            )
        };
        let mcp_client = match &planned {
            PlannedTransport::Stdio { command } => {
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
            }
            PlannedTransport::Sse { url } => {
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
            }
            PlannedTransport::StreamableHttp { url } => {
                // Streamable HTTP transport.
                let transport_options = StreamableTransportOptions {
                    mcp_url: url.clone(),
                    request_options: RequestOptions {
                        custom_headers: headers_map,
                        ..RequestOptions::default()
                    },
                };
                // `standalone` makes the transport open the server-to-client SSE stream
                // with a `GET` as soon as it starts. A server that does not implement it
                // answers `405`, which fails the whole connection *before* the
                // initialization handshake even though plain `POST` would have worked, so
                // it is opt-in rather than hardcoded (#2003).
                let transport = ClientStreamableTransport::new(
                    &transport_options,
                    None,
                    config.open_connection_on_startup,
                )
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
        };

        // Start the client
        mcp_client
            .clone()
            .start()
            .await
            .map_err(|e| Error::Other(format!("Failed to start MCP client: {}", e)))?;

        // Discover the server's tools so they can be bridged into a Toolkit.
        // Without this, `list_tools`/`call_tool` would see an empty tool list.
        let server_name = config.name.clone();
        let discovered = match mcp_client.request_tool_list(None).await {
            Ok(result) => result
                .tools
                .into_iter()
                .map(|tool| tool_info_from_schema(&server_name, tool))
                .collect::<Vec<_>>(),
            Err(e) => {
                tracing::warn!("Failed to list tools for MCP client '{}': {}", client_id, e);
                Vec::new()
            }
        };

        // Publish the connection. `connect` may have run concurrently for this id, in
        // which case the first one wins and this transport is shut down outside the lock.
        let (published, known, tool_count) = {
            let mut clients = self.clients.lock().await;
            let published = match clients.get_mut(client_id) {
                Some(client) if !client.connected => {
                    client.connected = true;
                    client.client = Some(mcp_client.clone());
                    client.tools = discovered;
                    true
                }
                _ => false,
            };
            let known = clients.contains_key(client_id);
            let tool_count = clients
                .get(client_id)
                .map(|client| client.tools.len())
                .unwrap_or(0);
            (published, known, tool_count)
        };
        if !known || !published {
            if let Err(error) = mcp_client.shut_down().await {
                tracing::warn!("Failed to shut down a duplicate MCP client: {}", error);
            }
            if !known {
                return Err(Error::Other(format!("Client '{}' not found", client_id)));
            }
            return Ok(());
        }

        tracing::info!(
            "MCP client '{}' connected to server '{}' ({} tools)",
            client_id,
            config.name,
            tool_count
        );

        Ok(())
    }

    /// Disconnects from an MCP server.
    pub async fn disconnect(&self, client_id: &str) -> Result<()> {
        // Take the handle out of the registry first so the shutdown (which awaits the
        // server) does not hold the registry lock and block the other servers.
        let (mcp_client, server_name) = {
            let mut clients = self.clients.lock().await;
            let client = clients
                .get_mut(client_id)
                .ok_or_else(|| Error::Other(format!("Client '{}' not found", client_id)))?;

            if !client.connected {
                return Ok(());
            }
            let server_name = client.config.name.clone();
            client.connected = false;
            client.tools.clear();
            (client.client.take(), server_name)
        };

        // Shutdown the client
        if let Some(mcp_client) = mcp_client {
            if let Err(error) = mcp_client.shut_down().await {
                // The local state is already cleared: a failed shutdown must not leave the
                // connection marked as live, otherwise every later call targets a dead one.
                tracing::warn!("Failed to shut down MCP client '{}': {}", client_id, error);
            }
        }

        tracing::info!(
            "MCP client '{}' disconnected from server '{}'",
            client_id,
            server_name
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
        // Snapshot the connection and its metadata switch, then release the registry
        // lock: the tool call awaits the remote server, and a slow call on one server
        // used to block every other server's operations (#665).
        let (mcp_client, propagate_meta) = {
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

            match &client.client {
                Some(mcp_client) => (mcp_client.clone(), client.config.propagate_meta),
                None => return Err(Error::Other("Client not initialized".to_string())),
            }
        };

        // The connection-level switch is authoritative and read live on every call,
        // so a caller cannot leak metadata to a server configured as untrusted: the
        // meta is dropped here, before it reaches the SDK. `None` omits `_meta`.
        let meta = if propagate_meta { meta } else { None };
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
        serde_json::to_value(result)
            .map_err(|e| Error::Other(format!("Failed to serialize result: {}", e)))
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

    /// Finds any client id registered for the given server name, connected or not.
    ///
    /// A [`McpClientSource`](super::mcp::McpClientSource) uses this to connect a
    /// configured-but-idle server on demand; `find_client_id_by_server` only reports
    /// live connections.
    pub async fn find_any_client_id_by_server(&self, server: &str) -> Option<String> {
        let clients = self.clients.lock().await;
        clients.iter().find_map(|(id, inst)| {
            if inst.config.name == server {
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
        let client_id = self.find_client_id_by_server(server).await.ok_or_else(|| {
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

/// Projects a discovered MCP schema [`Tool`] into [`McpToolInfo`].
///
/// A blank `title` is dropped instead of being carried as `Some("")`, and the output
/// schema is kept as raw JSON so the bridge can hand it to a provider verbatim.
///
/// [`Tool`]: rust_mcp_sdk::schema::Tool
fn tool_info_from_schema(server: &str, tool: rust_mcp_sdk::schema::Tool) -> McpToolInfo {
    let title = tool
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string);
    let output_schema = tool
        .output_schema
        .as_ref()
        .and_then(|schema| serde_json::to_value(schema).ok());
    McpToolInfo {
        name: tool.name,
        description: tool.description.clone(),
        parameters: serde_json::to_value(tool.input_schema)
            .unwrap_or_else(|_| serde_json::json!({ "type": "object" })),
        server: server.to_string(),
        title,
        output_schema,
    }
}

/// Returns `true` when `url` should use the legacy SSE transport (path contains
/// `/sse`), otherwise the URL is treated as a Streamable HTTP endpoint.
fn is_sse_url(url: &str) -> bool {
    url.contains("/sse")
}

/// Resolves the transport a config must connect with.
///
/// An explicit [`McpServerConfig::transport`] is authoritative — the caller pinned it, so
/// a mismatch (e.g. `stdio` without a command) is a configuration error rather than a
/// silent fallback. Without a pin the historical sniffing applies: `command` -> stdio,
/// `/sse` URL -> legacy SSE, any other URL -> Streamable HTTP.
pub fn plan_transport(config: &McpServerConfig) -> Result<PlannedTransport> {
    let command = config
        .command
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty());
    let url = config
        .url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty());

    match config.transport {
        Some(McpTransport::Stdio) => match command {
            Some(command) => Ok(PlannedTransport::Stdio {
                command: command.to_string(),
            }),
            None => Err(Error::Other(format!(
                "MCP server '{}' declares transport 'stdio' but has no command",
                config.name
            ))),
        },
        Some(McpTransport::Sse) => match url {
            Some(url) => Ok(PlannedTransport::Sse {
                url: url.to_string(),
            }),
            None => Err(Error::Other(format!(
                "MCP server '{}' declares transport 'sse' but has no url",
                config.name
            ))),
        },
        Some(McpTransport::StreamableHttp) => match url {
            Some(url) => Ok(PlannedTransport::StreamableHttp {
                url: url.to_string(),
            }),
            None => Err(Error::Other(format!(
                "MCP server '{}' declares transport 'http' but has no url",
                config.name
            ))),
        },
        None => {
            if let Some(command) = command {
                return Ok(PlannedTransport::Stdio {
                    command: command.to_string(),
                });
            }
            match url {
                Some(url) if is_sse_url(url) => Ok(PlannedTransport::Sse {
                    url: url.to_string(),
                }),
                Some(url) => Ok(PlannedTransport::StreamableHttp {
                    url: url.to_string(),
                }),
                None => Err(Error::Other("No command or URL specified".to_string())),
            }
        }
    }
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
        let cfg = McpServerConfig::new("s")
            .url("http://x/sse")
            .headers(vec![("Authorization".into(), "Bearer t".into())]);
        assert_eq!(cfg.url.as_deref(), Some("http://x/sse"));
        assert_eq!(cfg.headers.len(), 1);
        assert_eq!(
            cfg.headers[0],
            ("Authorization".to_string(), "Bearer t".to_string())
        );
    }

    // ---- Explicit transport selection (`tools.json` `transport`) ----

    #[test]
    fn transport_pin_overrides_sniffing() {
        // No `/sse` in the path, but the pin says SSE: the pin wins.
        let cfg = McpServerConfig::new("srv")
            .url("http://mcp.example.com/mcp")
            .transport(McpTransport::Sse);
        assert_eq!(
            plan_transport(&cfg).unwrap(),
            PlannedTransport::Sse {
                url: "http://mcp.example.com/mcp".to_string()
            }
        );

        // A `/sse` path, but the pin says streamable HTTP: the pin wins.
        let cfg = McpServerConfig::new("srv")
            .url("http://mcp.example.com/sse")
            .transport(McpTransport::StreamableHttp);
        assert_eq!(
            plan_transport(&cfg).unwrap(),
            PlannedTransport::StreamableHttp {
                url: "http://mcp.example.com/sse".to_string()
            }
        );

        // Unpinned configs keep sniffing.
        let sniffed = plan_transport(&McpServerConfig::new("srv").url("http://x/sse")).unwrap();
        assert_eq!(sniffed.kind(), McpTransport::Sse);
        let sniffed = plan_transport(&McpServerConfig::new("srv").url("http://x/mcp")).unwrap();
        assert_eq!(sniffed.kind(), McpTransport::StreamableHttp);
        let sniffed =
            plan_transport(&McpServerConfig::new("srv").command("npx -y server")).unwrap();
        assert_eq!(sniffed.kind(), McpTransport::Stdio);
    }

    #[test]
    fn transport_pin_without_the_matching_endpoint_is_rejected() {
        // Before the pin existed this config silently took the URL/no-transport path, so
        // a `stdio` declaration without a command looked like "nothing configured".
        let error = plan_transport(&McpServerConfig::new("files").transport(McpTransport::Stdio))
            .expect_err("a stdio pin without a command must be rejected");
        let text = format!("{error}");
        assert!(text.contains("stdio"), "{text}");
        assert!(text.contains("files"), "the error names the server: {text}");

        let error = plan_transport(
            &McpServerConfig::new("gateway")
                .command("npx -y gateway")
                .transport(McpTransport::StreamableHttp),
        )
        .expect_err("an http pin without a url must be rejected");
        assert!(format!("{error}").contains("http"), "{error}");
    }

    #[test]
    fn transport_wire_names_round_trip() {
        assert_eq!(McpTransport::Stdio.as_str(), "stdio");
        assert_eq!(McpTransport::Sse.as_str(), "sse");
        assert_eq!(McpTransport::StreamableHttp.as_str(), "http");
        for transport in [
            McpTransport::Stdio,
            McpTransport::Sse,
            McpTransport::StreamableHttp,
        ] {
            assert_eq!(McpTransport::parse(transport.as_str()), Some(transport));
        }
        assert_eq!(
            McpTransport::parse("streamable_http"),
            Some(McpTransport::StreamableHttp)
        );
        assert_eq!(McpTransport::parse("carrier-pigeon"), None);
    }

    // ---- Title / output schema discovery (HIGRESS-05) ----

    #[test]
    fn discovered_tool_keeps_title_and_output_schema() {
        // Round trip through the wire form, because discovery receives what the server
        // serialized: a field the SDK drops on the way in would never reach the bridge.
        let wire = serde_json::json!({
            "name": "weather",
            "title": "Weather lookup",
            "description": "Looks up the weather",
            "inputSchema": { "type": "object", "properties": {} },
            "outputSchema": {
                "type": "object",
                "properties": { "temp": { "type": "number" } },
                "required": ["temp"]
            }
        });
        let tool: rust_mcp_sdk::schema::Tool =
            serde_json::from_value(wire).expect("valid MCP tool");
        let info = tool_info_from_schema("srv", tool);
        assert_eq!(info.name, "weather");
        assert_eq!(info.title.as_deref(), Some("Weather lookup"));
        let schema = info
            .output_schema
            .expect("output schema survives discovery");
        assert_eq!(schema["properties"]["temp"]["type"], "number");
        assert_eq!(info.parameters["type"], "object");
    }

    #[test]
    fn blank_title_and_missing_output_schema_are_none() {
        let tool: rust_mcp_sdk::schema::Tool = serde_json::from_value(serde_json::json!({
            "name": "bare",
            "title": "   ",
            "inputSchema": { "type": "object" }
        }))
        .expect("valid MCP tool");
        let info = tool_info_from_schema("srv", tool);
        assert!(info.title.is_none(), "a blank title must not be carried");
        assert!(info.output_schema.is_none());
    }

    #[test]
    fn constructor_defaults_the_new_fields() {
        let info = McpToolInfo::new(
            "legacy",
            Some("desc".to_string()),
            serde_json::json!({ "type": "object" }),
            "srv",
        );
        assert!(info.title.is_none());
        assert!(info.output_schema.is_none());
        assert_eq!(info.server, "srv");
    }

    // ---- Connection-level meta gate ----

    /// A stand-in MCP client that records every tool call it would have sent.
    struct RecordingClient {
        client_info: InitializeRequestParams,
        calls: std::sync::Mutex<Vec<rust_mcp_sdk::schema::CallToolRequestParams>>,
        /// How long a `request_tool_call` pretends to take, for concurrency tests.
        delay: Option<std::time::Duration>,
    }

    impl RecordingClient {
        fn new() -> Self {
            Self {
                client_info: InitializeRequestParams {
                    capabilities: ClientCapabilities::default(),
                    client_info: Implementation {
                        name: "recording".into(),
                        version: "0.1.0".into(),
                        title: None,
                        description: None,
                        icons: vec![],
                        website_url: None,
                    },
                    protocol_version: LATEST_PROTOCOL_VERSION.into(),
                    meta: None,
                },
                calls: std::sync::Mutex::new(Vec::new()),
                delay: None,
            }
        }

        /// Makes every recorded call sleep first, standing in for a slow server.
        fn with_delay(mut self, delay: std::time::Duration) -> Self {
            self.delay = Some(delay);
            self
        }

        fn last_meta(&self) -> Option<rust_mcp_sdk::schema::CallToolMeta> {
            self.calls
                .lock()
                .unwrap()
                .last()
                .and_then(|p| p.meta.clone())
        }

        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    #[async_trait::async_trait]
    impl McpClient for RecordingClient {
        async fn start(self: Arc<Self>) -> SdkResult<()> {
            Ok(())
        }

        fn set_server_details(
            &self,
            _server_details: rust_mcp_sdk::schema::InitializeResult,
        ) -> SdkResult<()> {
            Ok(())
        }

        async fn terminate_session(&self) {}

        fn task_store(&self) -> Option<Arc<rust_mcp_sdk::task_store::ClientTaskStore>> {
            None
        }

        fn server_task_store(&self) -> Option<Arc<rust_mcp_sdk::task_store::ServerTaskStore>> {
            None
        }

        async fn shut_down(&self) -> SdkResult<()> {
            Ok(())
        }

        async fn is_shut_down(&self) -> bool {
            true
        }

        fn client_info(&self) -> &InitializeRequestParams {
            &self.client_info
        }

        fn server_info(&self) -> Option<rust_mcp_sdk::schema::InitializeResult> {
            None
        }

        async fn session_id(&self) -> Option<rust_mcp_transport::SessionId> {
            None
        }

        async fn send(
            &self,
            _message: rust_mcp_sdk::schema::MessageFromClient,
            _request_id: Option<rust_mcp_sdk::schema::RequestId>,
            _request_timeout: Option<std::time::Duration>,
        ) -> SdkResult<Option<rust_mcp_sdk::schema::ServerMessage>> {
            Ok(None)
        }

        async fn send_batch(
            &self,
            _messages: Vec<rust_mcp_sdk::schema::ClientMessage>,
            _timeout: Option<std::time::Duration>,
        ) -> SdkResult<Option<Vec<rust_mcp_sdk::schema::ServerMessage>>> {
            Ok(None)
        }

        async fn request_tool_call(
            &self,
            params: rust_mcp_sdk::schema::CallToolRequestParams,
        ) -> SdkResult<rust_mcp_sdk::schema::CallToolResult> {
            // The real call awaits the server; the registry lock must not be held meanwhile.
            if let Some(delay) = self.delay {
                tokio::time::sleep(delay).await;
            }
            self.calls.lock().unwrap().push(params);
            Ok(rust_mcp_sdk::schema::CallToolResult {
                content: Vec::new(),
                is_error: None,
                meta: None,
                structured_content: None,
            })
        }
    }

    /// A manager holding one "connected" server backed by [`RecordingClient`].
    async fn manager_with_recording_client(
        propagate_meta: bool,
    ) -> (McpClientManager, String, Arc<RecordingClient>) {
        let manager = McpClientManager::new();
        let client_id = manager
            .create_client(
                McpServerConfig::new("srv")
                    .url("http://mcp.example.com/mcp")
                    .propagate_meta(propagate_meta),
            )
            .await
            .unwrap();
        let recorder = Arc::new(RecordingClient::new());
        {
            let mut clients = manager.clients.lock().await;
            let instance = clients.get_mut(&client_id).unwrap();
            instance.connected = true;
            instance.client = Some(recorder.clone());
        }
        (manager, client_id, recorder)
    }

    fn meta_map() -> serde_json::Map<String, serde_json::Value> {
        let mut meta = serde_json::Map::new();
        meta.insert(
            "io.agentscope/toolCallId".to_string(),
            serde_json::json!("call-1"),
        );
        meta
    }

    /// Registers a client for `server`, marked connected and backed by `client`.
    async fn register_connected(
        manager: &McpClientManager,
        server: &str,
        client: Arc<RecordingClient>,
    ) -> String {
        let client_id = manager
            .create_client(McpServerConfig::new(server).url("http://mcp.example.com/mcp"))
            .await
            .unwrap();
        let mut clients = manager.clients.lock().await;
        let instance = clients.get_mut(&client_id).unwrap();
        instance.connected = true;
        instance.client = Some(client);
        client_id
    }

    #[tokio::test]
    async fn a_slow_tool_call_does_not_block_the_other_servers() {
        // A slow `tools/call` on one server used to hold the registry lock for the whole
        // round trip, freezing every other server (#665).
        let manager = Arc::new(McpClientManager::new());
        let slow =
            Arc::new(RecordingClient::new().with_delay(std::time::Duration::from_millis(250)));
        let slow_id = register_connected(&manager, "slow", slow.clone()).await;
        let fast_id = register_connected(&manager, "fast", Arc::new(RecordingClient::new())).await;

        let in_flight = {
            let manager = Arc::clone(&manager);
            let slow_id = slow_id.clone();
            tokio::spawn(async move {
                manager
                    .call_tool(&slow_id, "remote", serde_json::json!({}))
                    .await
            })
        };
        // Let the call reach its await.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let started = std::time::Instant::now();
        let tools = manager.list_tools(&fast_id).await.unwrap();
        let elapsed = started.elapsed();
        assert!(tools.is_empty());
        assert!(
            elapsed < std::time::Duration::from_millis(150),
            "listing an idle server waited {elapsed:?} behind another server's call"
        );

        in_flight.await.unwrap().unwrap();
        assert_eq!(slow.call_count(), 1);
    }

    #[test]
    fn the_startup_stream_is_opt_in() {
        // The streamable transport used to open the server-to-client stream
        // unconditionally, which fails `initialize` on servers that answer 405 to it
        // (#2003).
        let config = McpServerConfig::new("srv").url("http://mcp.example.com/mcp");
        assert!(!config.open_connection_on_startup);
        assert!(
            McpServerConfig::new("srv")
                .url("http://mcp.example.com/mcp")
                .open_connection_on_startup(true)
                .open_connection_on_startup
        );
    }

    #[tokio::test]
    async fn disabled_connection_strips_meta_before_it_reaches_the_sdk() {
        let (manager, client_id, recorder) = manager_with_recording_client(false).await;

        manager
            .call_tool_with_meta(
                &client_id,
                "remote",
                serde_json::json!({ "a": 1 }),
                Some(meta_map()),
            )
            .await
            .unwrap();
        assert_eq!(recorder.call_count(), 1);
        assert!(
            recorder.last_meta().is_none(),
            "a disabled connection must send no `_meta`, even when the caller passes one"
        );

        // The server-name entry point inherits the same fail-closed behaviour.
        manager
            .call_tool_by_server_with_meta("srv", "remote", serde_json::json!({}), Some(meta_map()))
            .await
            .unwrap();
        assert_eq!(recorder.call_count(), 2);
        assert!(recorder.last_meta().is_none());
    }

    #[tokio::test]
    async fn enabled_connection_forwards_meta() {
        let (manager, client_id, recorder) = manager_with_recording_client(true).await;

        manager
            .call_tool_with_meta(
                &client_id,
                "remote",
                serde_json::json!({}),
                Some(meta_map()),
            )
            .await
            .unwrap();

        let meta = recorder.last_meta().expect("meta must reach the SDK");
        let extra = meta.extra.expect("caller-provided entries are kept");
        assert_eq!(
            extra["io.agentscope/toolCallId"],
            serde_json::json!("call-1")
        );
    }

    #[tokio::test]
    async fn per_tool_switch_ands_with_the_connection_switch() {
        use agentscope_core::message::ToolUseBlock;
        use agentscope_core::tool::{ToolExecutionContext, Toolkit};

        let (manager, client_id, recorder) = manager_with_recording_client(true).await;
        {
            let mut clients = manager.clients.lock().await;
            let instance = clients.get_mut(&client_id).unwrap();
            // The bridge registers from the discovery cache, not from a live server.
            instance.tools = vec![McpToolInfo::new(
                "remote",
                Some("desc".to_string()),
                serde_json::json!({ "type": "object" }),
                "srv",
            )];
        }
        let manager = Arc::new(manager);
        let toolkit = Toolkit::new();
        let options = crate::agent::mcp::McpMetaPropagationOptions::new()
            .propagate_meta_for("remote", false)
            .expect("valid tool name");
        let registered =
            crate::agent::mcp::try_bridge_mcp_tools_to_toolkit(&manager, &toolkit, &options)
                .await
                .expect("bridge");
        assert_eq!(registered, 1);

        let tool = toolkit.get_tool("remote").expect("tool bridged");
        let ctx = ToolExecutionContext {
            tool_call: Some(ToolUseBlock::new("call-1", "remote", Default::default())),
            ..Default::default()
        };
        tool.call(Default::default(), ctx).await.unwrap();

        assert_eq!(recorder.call_count(), 1);
        assert!(
            recorder.last_meta().is_none(),
            "per-tool off must silence meta even though the connection allows it"
        );
    }

    #[tokio::test]
    async fn bridged_tool_exposes_title_and_output_schema() {
        use agentscope_core::tool::Toolkit;

        let (manager, client_id, _recorder) = manager_with_recording_client(true).await;
        {
            let mut clients = manager.clients.lock().await;
            let instance = clients.get_mut(&client_id).unwrap();
            let mut info = McpToolInfo::new(
                "remote",
                Some("desc".to_string()),
                serde_json::json!({ "type": "object" }),
                "srv",
            );
            info.title = Some("Remote title".to_string());
            info.output_schema = Some(serde_json::json!({
                "type": "object",
                "properties": { "temp": { "type": "number" } }
            }));
            instance.tools = vec![info];
        }
        let manager = Arc::new(manager);
        let toolkit = Toolkit::new();
        let registered = crate::agent::mcp::bridge_mcp_tools_to_toolkit(&manager, &toolkit).await;
        assert_eq!(registered, 1);

        let tool = toolkit.get_tool("remote").expect("tool bridged");
        assert_eq!(tool.title(), Some("Remote title"));
        let schema = tool.output_schema().expect("output schema bridged");
        assert_eq!(schema["properties"]["temp"]["type"], "number");
    }

    #[tokio::test]
    async fn disabling_a_live_connection_takes_effect_on_the_next_call() {
        let (manager, client_id, recorder) = manager_with_recording_client(true).await;
        manager
            .call_tool_with_meta(
                &client_id,
                "remote",
                serde_json::json!({}),
                Some(meta_map()),
            )
            .await
            .unwrap();
        assert!(recorder.last_meta().is_some());

        assert!(manager.set_propagate_meta(&client_id, false).await);
        manager
            .call_tool_with_meta(
                &client_id,
                "remote",
                serde_json::json!({}),
                Some(meta_map()),
            )
            .await
            .unwrap();
        assert!(
            recorder.last_meta().is_none(),
            "the switch is read live, not baked in at connect time"
        );
    }
}
