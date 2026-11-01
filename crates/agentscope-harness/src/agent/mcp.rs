//! MCP server configuration and registrar, mirroring `io.agentscope.harness.agent.tools`.
//!
//! This module provides the declarative config surface and a registrar implementation
//! that can be wired to `rust-mcp-sdk` for real MCP client management.
use agentscope_core::error::Result;

// Re-export from mcp_client_manager for backward compatibility
pub use super::mcp_client_manager::{McpClientManager, McpServerConfig, McpToolInfo};

/// Registers an MCP server so its tools become available to the toolkit.
pub trait Registrar: Send + Sync {
    fn register(&self, config: &McpServerConfig) -> Result<()>;
}

/// Terminal state of an MCP server registration, mirroring the Java
/// `McpServerRegistrationResult.Status` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpServerRegistrationStatus {
    /// The server initialized and its tools were listed successfully.
    Success,
    /// The server could not be initialized or its tools could not be listed.
    Failed,
    /// The registration was skipped (e.g. no transport configured).
    Skipped,
}

impl McpServerRegistrationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "SUCCESS",
            Self::Failed => "FAILED",
            Self::Skipped => "SKIPPED",
        }
    }
}

/// Immutable payload describing the outcome of one MCP server registration.
#[derive(Debug, Clone)]
pub struct McpServerRegistrationResult {
    /// Name of the configured server.
    pub server_name: String,
    /// Terminal outcome.
    pub status: McpServerRegistrationStatus,
    /// Human-readable diagnostic (empty on success).
    pub error: Option<String>,
}

impl McpServerRegistrationResult {
    pub fn success(server_name: impl Into<String>) -> Self {
        Self {
            server_name: server_name.into(),
            status: McpServerRegistrationStatus::Success,
            error: None,
        }
    }
    pub fn failed(server_name: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            server_name: server_name.into(),
            status: McpServerRegistrationStatus::Failed,
            error: Some(error.into()),
        }
    }
    pub fn skipped(server_name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            server_name: server_name.into(),
            status: McpServerRegistrationStatus::Skipped,
            error: Some(reason.into()),
        }
    }
}

/// Optional callback receiving the result of each best-effort MCP server registration,
/// mirroring the Java `McpServerRegistrationListener`.
pub trait McpServerRegistrationListener: Send + Sync {
    fn on_registration_result(&self, result: McpServerRegistrationResult);
}

/// Default registrar that uses McpClientManager to manage connections.
pub struct McpServerRegistrar {
    client_manager: std::sync::Arc<super::mcp_client_manager::McpClientManager>,
    listener: Option<std::sync::Arc<dyn McpServerRegistrationListener>>,
}

impl McpServerRegistrar {
    /// Creates a new registrar with the given client manager.
    pub fn new(
        client_manager: std::sync::Arc<super::mcp_client_manager::McpClientManager>,
    ) -> Self {
        Self {
            client_manager,
            listener: None,
        }
    }

    /// Installs a listener that receives each registration's terminal result.
    pub fn with_listener(
        mut self,
        listener: std::sync::Arc<dyn McpServerRegistrationListener>,
    ) -> Self {
        self.listener = Some(listener);
        self
    }

    /// Gets a reference to the underlying client manager.
    pub fn client_manager(&self) -> &std::sync::Arc<super::mcp_client_manager::McpClientManager> {
        &self.client_manager
    }

    /// Disconnects from all registered MCP servers.
    pub async fn disconnect_all(&self) -> Result<()> {
        let connected_clients = self.client_manager.connected_clients().await;
        for client_id in connected_clients {
            if let Err(e) = self.client_manager.disconnect(&client_id).await {
                tracing::error!("Failed to disconnect client '{}': {}", client_id, e);
            }
        }
        Ok(())
    }

    /// Gets a list of all registered server names.
    pub async fn registered_servers(&self) -> Vec<String> {
        let connected_clients = self.client_manager.connected_clients().await;
        let mut servers = Vec::new();
        for client_id in connected_clients {
            if let Some(client) = self.client_manager.get_client(&client_id).await {
                servers.push(client.config.name.clone());
            }
        }
        servers
    }
}

impl Registrar for McpServerRegistrar {
    fn register(&self, config: &McpServerConfig) -> Result<()> {
        let client_manager = self.client_manager.clone();
        let config = config.clone();
        let listener = self.listener.clone();

        // Spawn async task to create and connect client
        tokio::spawn(async move {
            match client_manager.create_client(config.clone()).await {
                Ok(client_id) => {
                    tracing::info!(
                        "Created MCP client '{}' for server '{}'",
                        client_id,
                        config.name
                    );
                    match client_manager.connect(&client_id).await {
                        Ok(()) => {
                            if let Some(l) = &listener {
                                l.on_registration_result(McpServerRegistrationResult::success(
                                    config.name.clone(),
                                ));
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to connect MCP client '{}': {}", client_id, e);
                            if let Some(l) = &listener {
                                l.on_registration_result(McpServerRegistrationResult::failed(
                                    config.name.clone(),
                                    format!("connect failed: {e}"),
                                ));
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to create MCP client for server '{}': {}",
                        config.name,
                        e
                    );
                    if let Some(l) = &listener {
                        l.on_registration_result(McpServerRegistrationResult::failed(
                            config.name.clone(),
                            format!("create failed: {e}"),
                        ));
                    }
                }
            }
        });

        Ok(())
    }
}

/// Per-registration metadata-propagation options, mirroring Java
/// `Toolkit.ToolRegistration#propagateMeta`.
///
/// Effective propagation for a tool is the logical AND of:
/// this registration-level decision (fixed when the tool is bridged), the tool's own
/// switch, and the connection-level switch read live on every call.
#[derive(Debug, Clone, Default)]
pub struct McpMetaPropagationOptions {
    /// Client-wide default for every tool bridged from a server; `None` leaves the
    /// per-tool default (`true`).
    pub propagate_meta: Option<bool>,
    /// Per-tool overrides keyed by the remote tool name (before any prefix). A later
    /// entry for the same name overwrites the earlier one.
    pub tool_overrides: Vec<(String, bool)>,
}

impl McpMetaPropagationOptions {
    /// Creates empty options: everything defaults to `true`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the registration-wide default.
    pub fn propagate_meta(mut self, propagate_meta: bool) -> Self {
        self.propagate_meta = Some(propagate_meta);
        self
    }

    /// Sets a per-tool override.
    ///
    /// Mirrors Java `ToolRegistration.propagateMeta(String, boolean)`: a blank tool
    /// name is rejected instead of being silently ignored.
    pub fn propagate_meta_for(
        mut self,
        tool_name: impl Into<String>,
        propagate_meta: bool,
    ) -> Result<Self> {
        let tool_name = tool_name.into();
        if tool_name.trim().is_empty() {
            return Err(agentscope_core::error::Error::Other(
                "MCP tool name cannot be null or blank".to_string(),
            ));
        }
        self.tool_overrides.push((tool_name, propagate_meta));
        Ok(self)
    }

    /// The effective per-tool switch for `tool_name`.
    fn effective_for(&self, tool_name: &str) -> bool {
        self.tool_overrides
            .iter()
            .rev()
            .find(|(name, _)| name == tool_name)
            .map(|(_, value)| *value)
            .or(self.propagate_meta)
            .unwrap_or(true)
    }
}

/// Bridges every connected MCP client's tools from [`McpClientManager`] into a
/// [`Toolkit`] as [`McpTool`]s.
///
/// Each discovered MCP tool becomes a core [`McpTool`] whose **async** handler
/// dispatches back through the manager (`call_tool_by_server_with_meta`), so calls
/// await the real client round-trip without blocking the tokio runtime.
///
/// Returns the number of tools registered.
pub async fn bridge_mcp_tools_to_toolkit(
    manager: &std::sync::Arc<McpClientManager>,
    toolkit: &agentscope_core::tool::Toolkit,
) -> usize {
    try_bridge_mcp_tools_to_toolkit(manager, toolkit, &McpMetaPropagationOptions::new())
        .await
        .unwrap_or(0)
}

/// Bridges MCP tools with explicit metadata-propagation options.
///
/// Mirrors Java `Toolkit.ToolRegistration`: per-tool override keys must match a tool
/// that is actually bridged — a typo or a renamed/filtered tool fails the whole
/// registration with the rejected names listed in stable (sorted) order, so a
/// silencing override is never lost silently.
pub async fn try_bridge_mcp_tools_to_toolkit(
    manager: &std::sync::Arc<McpClientManager>,
    toolkit: &agentscope_core::tool::Toolkit,
    options: &McpMetaPropagationOptions,
) -> Result<usize> {
    let client_ids = manager.connected_clients().await;
    let mut registered = 0usize;
    let mut bridged_names: Vec<String> = Vec::new();
    for client_id in client_ids {
        let Ok(tools) = manager.list_tools(&client_id).await else {
            continue;
        };
        let prefix_tool_names = manager
            .get_client(&client_id)
            .await
            .map(|client| client.config.prefix_tool_names)
            .unwrap_or(false);
        let manager = std::sync::Arc::clone(manager);
        for info in tools {
            let server = info.server.clone();
            let tool_name = info.name.clone();
            let description = info.description.clone().unwrap_or_default();
            let propagate_meta = options.effective_for(&tool_name);
            // Overrides are keyed by the remote name, before any prefix.
            bridged_names.push(tool_name.clone());
            // Only the *registered* name is prefixed: the remote call still uses the tool
            // name the server advertised, and error text keeps the bare name.
            let registered_name = if prefix_tool_names {
                format!("{server}__{tool_name}")
            } else {
                tool_name.clone()
            };
            // Clones captured by the (move) async handler closure; the originals
            // stay owned by the loop so we can still build the `McpTool` below.
            let h_server = server.clone();
            let h_tool_name = tool_name.clone();
            let h_manager = std::sync::Arc::clone(&manager);
            let async_handler: Box<
                dyn Fn(
                        &agentscope_core::tool::McpTool,
                        serde_json::Map<String, serde_json::Value>,
                        Option<serde_json::Map<String, serde_json::Value>>,
                    ) -> std::pin::Pin<
                        Box<
                            dyn std::future::Future<
                                    Output = Result<agentscope_core::message::ToolResultBlock>,
                                > + Send,
                        >,
                    > + Send
                    + Sync,
            > = Box::new(move |_tool, args, meta| {
                let manager = std::sync::Arc::clone(&h_manager);
                let server = h_server.clone();
                let tool_name = h_tool_name.clone();
                Box::pin(async move {
                    // The connection-level switch is applied inside the manager
                    // (`call_tool_with_meta`), which reads it live on every call, so
                    // disabling a connection takes effect immediately for
                    // already-bridged tools. The per-tool decision is already ANDed in
                    // by `McpTool::call` via `with_propagate_meta`.
                    let raw = serde_json::Value::Object(args);
                    match manager
                        .call_tool_by_server_with_meta(&server, &tool_name, raw, meta)
                        .await
                    {
                        Ok(value) => Ok(crate::agent::mcp_content::convert_call_tool_result_value(
                            &tool_name, &value,
                        )),
                        Err(e) => Err(e),
                    }
                })
            });
            let mut tool = agentscope_core::tool::McpTool::new(
                server.clone(),
                registered_name,
                description,
                info.parameters,
                Box::new(|_t, _a| {
                    Ok(agentscope_core::message::ToolResultBlock::new(
                        "",
                        "",
                        vec![],
                    ))
                }),
            )
            .with_async_handler(async_handler)
            // Per-tool decision, fixed at registration time and ANDed with the
            // connection-level switch on every call.
            .with_propagate_meta(propagate_meta);
            if let Some(title) = info.title {
                tool = tool.with_title(title);
            }
            if let Some(output_schema) = info.output_schema {
                tool = tool.with_output_schema(output_schema);
            }
            toolkit.register_tool(std::sync::Arc::new(tool));
            registered += 1;
        }
    }

    // Fail-loud: an override that matches no bridged tool is a configuration bug
    // (typo, remote rename, or a tool filtered out upstream).
    let mut rejected: Vec<&str> = options
        .tool_overrides
        .iter()
        .map(|(name, _)| name.as_str())
        .filter(|name| !bridged_names.iter().any(|n| n == *name))
        .collect();
    rejected.sort_unstable();
    rejected.dedup();
    if !rejected.is_empty() {
        return Err(agentscope_core::error::Error::Other(format!(
            "MCP meta-propagation overrides name tool(s) that were not registered: [{}]",
            rejected.join(", ")
        )));
    }

    Ok(registered)
}

// ---------------------------------------------------------------------------
// MCP tool source abstraction (HIGRESS-07)
// ---------------------------------------------------------------------------

/// A source of MCP tools that a toolkit can register.
///
/// Mirrors Java's `McpClientWrapper` base class: `Toolkit.registerMcpClient` is declared
/// against the base type, so *any* MCP client can be registered and only provider-specific
/// subclasses are cached (Java's `instanceof` check). Rust has no client base class, so
/// this narrow port stands in for it. The harness implements it for every server managed
/// by [`McpClientManager`] (see [`McpClientSource`]); extension crates (Higress, …) accept
/// `Arc<dyn McpToolSource>` instead of defining their own MCP-client abstraction.
///
/// Provider-specific downcasts (e.g. "is this a Higress client?") deliberately stay in the
/// extension crate: the harness must not depend on a provider type.
#[async_trait::async_trait]
pub trait McpToolSource: Send + Sync {
    /// The unique client/server name (Java `McpClientWrapper.getName()`).
    fn name(&self) -> &str;

    /// Establishes the connection, if it is not already established.
    ///
    /// A toolkit calls this before listing tools (Java
    /// `Toolkit.registerMcpClient` → `initialize()`), so the tool list is never read from
    /// an unconnected client.
    async fn initialize(&self) -> Result<()>;

    /// Lists the tools this client exposes.
    async fn list_tools(&self) -> Result<Vec<rust_mcp_sdk::schema::Tool>>;

    /// Invokes a tool, propagating `meta` as MCP request metadata.
    ///
    /// `meta` must be forwarded untouched: `None` means "send no `_meta`".
    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        meta: Option<agentscope_core::message::JsonMap>,
    ) -> Result<rust_mcp_sdk::schema::CallToolResult>;
}

/// [`McpToolSource`] backed by one server registered on a [`McpClientManager`].
///
/// The manager owns the connection and the discovery cache; this adapter exposes one of
/// its servers through the abstract tool-source port, so a toolkit (Higress or any other)
/// can register MCP tools without knowing about `rust_mcp_sdk` clients or the manager.
#[derive(Clone)]
pub struct McpClientSource {
    manager: std::sync::Arc<McpClientManager>,
    server: String,
}

impl McpClientSource {
    /// Creates a source for `server` on `manager`.
    ///
    /// The server must already be registered with the manager
    /// ([`McpClientManager::create_client`]); `initialize` connects it on demand.
    pub fn new(manager: std::sync::Arc<McpClientManager>, server: impl Into<String>) -> Self {
        Self {
            manager,
            server: server.into(),
        }
    }

    /// The server name this source exposes.
    pub fn server(&self) -> &str {
        &self.server
    }

    /// The manager this source reads through.
    pub fn manager(&self) -> &std::sync::Arc<McpClientManager> {
        &self.manager
    }

    /// The connected client id for this server, or an error when it is not connected.
    async fn connected_client_id(&self) -> Result<String> {
        self.manager
            .find_client_id_by_server(&self.server)
            .await
            .ok_or_else(|| {
                agentscope_core::error::Error::Other(format!(
                    "MCP server '{}' is not connected; call initialize() first",
                    self.server
                ))
            })
    }
}

/// Converts a harness-discovered [`McpToolInfo`] into an MCP schema [`Tool`].
///
/// [`Tool`]: rust_mcp_sdk::schema::Tool
fn mcp_tool_info_to_schema_tool(info: &McpToolInfo) -> rust_mcp_sdk::schema::Tool {
    rust_mcp_sdk::schema::Tool {
        name: info.name.clone(),
        title: info.title.clone(),
        description: info.description.clone(),
        input_schema: build_tool_input_schema(&info.parameters),
        output_schema: info
            .output_schema
            .as_ref()
            .and_then(|schema| serde_json::from_value(schema.clone()).ok()),
        annotations: None,
        meta: None,
        execution: None,
        icons: Vec::new(),
    }
}

/// Builds an MCP [`ToolInputSchema`] from a raw JSON schema value.
///
/// `rust-mcp-schema` models the input schema as a closed struct (`type: "object"` +
/// `properties` + `required`), so keys such as `$schema` or `additionalProperties` have no
/// representation; `properties`/`required` are preserved and each property keeps its raw
/// schema object. This is the same projection the Higress adapter applies.
///
/// [`ToolInputSchema`]: rust_mcp_sdk::schema::ToolInputSchema
fn build_tool_input_schema(schema: &serde_json::Value) -> rust_mcp_sdk::schema::ToolInputSchema {
    let Some(object) = schema.as_object() else {
        return rust_mcp_sdk::schema::ToolInputSchema::new(Vec::new(), None, None);
    };
    let properties = object
        .get("properties")
        .and_then(|p| p.as_object())
        .map(|p| {
            p.iter()
                .filter_map(|(k, v)| v.as_object().map(|m| (k.clone(), m.clone())))
                .collect::<std::collections::BTreeMap<_, _>>()
        });
    let required = object
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    rust_mcp_sdk::schema::ToolInputSchema::new(required, properties, None)
}

#[async_trait::async_trait]
impl McpToolSource for McpClientSource {
    fn name(&self) -> &str {
        &self.server
    }

    async fn initialize(&self) -> Result<()> {
        if self.manager.find_client_id_by_server(&self.server).await.is_some() {
            return Ok(());
        }
        let client_id = self
            .manager
            .find_any_client_id_by_server(&self.server)
            .await
            .ok_or_else(|| {
                agentscope_core::error::Error::Other(format!(
                    "No MCP client configured for server '{}'",
                    self.server
                ))
            })?;
        self.manager.connect(&client_id).await
    }

    async fn list_tools(&self) -> Result<Vec<rust_mcp_sdk::schema::Tool>> {
        let client_id = self.connected_client_id().await?;
        Ok(self
            .manager
            .list_tools(&client_id)
            .await?
            .iter()
            .map(mcp_tool_info_to_schema_tool)
            .collect())
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        meta: Option<agentscope_core::message::JsonMap>,
    ) -> Result<rust_mcp_sdk::schema::CallToolResult> {
        let client_id = self.connected_client_id().await?;
        let value = self
            .manager
            .call_tool_with_meta(&client_id, tool_name, arguments, meta)
            .await?;
        serde_json::from_value(value).map_err(|e| {
            agentscope_core::error::Error::Other(format!(
                "MCP tool '{tool_name}' returned an unparseable result: {e}"
            ))
        })
    }
}
