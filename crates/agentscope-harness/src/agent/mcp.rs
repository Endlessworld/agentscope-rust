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
        Self { server_name: server_name.into(), status: McpServerRegistrationStatus::Success, error: None }
    }
    pub fn failed(server_name: impl Into<String>, error: impl Into<String>) -> Self {
        Self { server_name: server_name.into(), status: McpServerRegistrationStatus::Failed, error: Some(error.into()) }
    }
    pub fn skipped(server_name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self { server_name: server_name.into(), status: McpServerRegistrationStatus::Skipped, error: Some(reason.into()) }
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
    pub fn new(client_manager: std::sync::Arc<super::mcp_client_manager::McpClientManager>) -> Self {
        Self { client_manager, listener: None }
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
                    tracing::info!("Created MCP client '{}' for server '{}'", client_id, config.name);
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
                    tracing::error!("Failed to create MCP client for server '{}': {}", config.name, e);
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
