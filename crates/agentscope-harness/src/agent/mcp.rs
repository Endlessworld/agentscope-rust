//! MCP server configuration and registrar, mirroring `io.agentscope.harness.agent.tools`.
//!
//! This module provides the declarative config surface and a registrar implementation
//! that can be wired to `rust-mcp-sdk` for real MCP client management.
use agentscope_core::error::{Error, Result};

// Re-export from mcp_client_manager for backward compatibility
pub use super::mcp_client_manager::{McpClientManager, McpServerConfig, McpToolInfo};

/// Registers an MCP server so its tools become available to the toolkit.
pub trait Registrar: Send + Sync {
    fn register(&self, config: &McpServerConfig) -> Result<()>;
}

/// Default registrar that uses McpClientManager to manage connections.
pub struct McpServerRegistrar {
    client_manager: std::sync::Arc<super::mcp_client_manager::McpClientManager>,
}

impl McpServerRegistrar {
    /// Creates a new registrar with the given client manager.
    pub fn new(client_manager: std::sync::Arc<super::mcp_client_manager::McpClientManager>) -> Self {
        Self { client_manager }
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
        
        // Spawn async task to create and connect client
        tokio::spawn(async move {
            match client_manager.create_client(config.clone()).await {
                Ok(client_id) => {
                    tracing::info!("Created MCP client '{}' for server '{}'", client_id, config.name);
                    if let Err(e) = client_manager.connect(&client_id).await {
                        tracing::error!("Failed to connect MCP client '{}': {}", client_id, e);
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to create MCP client for server '{}': {}", config.name, e);
                }
            }
        });
        
        Ok(())
    }
}
