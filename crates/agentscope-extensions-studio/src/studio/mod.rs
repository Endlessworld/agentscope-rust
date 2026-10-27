//! Central manager for AgentScope Studio integration, mirroring `StudioManager`.
//!
//! Java uses static volatile fields; Rust uses a process-global
//! [`OnceLock`] holding an `RwLock<Option<...>>` so it can be (re)initialized
//! and shut down safely across threads.

pub mod client;
pub mod config;
pub mod message_hook;
pub mod pojo;
pub mod socket_io;
pub mod user_agent;
pub mod websocket;

pub use client::StudioClient;
pub use config::{StudioConfig, StudioConfigBuilder};
pub use message_hook::StudioMessageHook;
pub use socket_io::{
    EngineIoHandshake, ProtocolAction, SocketIoEvent, SocketIoProtocol, SocketIoTransport,
    DEFAULT_NAMESPACE, ENGINE_IO_VERSION,
};
pub use user_agent::StudioUserAgent;
pub use websocket::{
    normalize_tool_result, parse_content_blocks, parse_user_input, preprocess_tool_result_block,
    NoopStudioTransport, PendingInputRegistry, StudioTransport, StudioWebSocketClient,
    UserInputData,
};

use std::sync::{Arc, OnceLock, RwLock};

use crate::error::{StudioError, StudioResult};

/// Snapshot of the currently-installed Studio integration.
#[derive(Clone)]
struct Installed {
    config: StudioConfig,
    client: Arc<StudioClient>,
    ws_client: Arc<StudioWebSocketClient>,
}

fn slot() -> &'static RwLock<Option<Installed>> {
    static SLOT: OnceLock<RwLock<Option<Installed>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

/// Central manager providing a simplified API for initializing Studio
/// integration and accessing its clients.
pub struct StudioManager;

impl StudioManager {
    /// Creates a new initialization builder.
    pub fn init() -> StudioManagerBuilder {
        StudioManagerBuilder::new()
    }

    /// Gets the Studio HTTP client, or `None` if not initialized.
    pub fn client() -> Option<Arc<StudioClient>> {
        slot().read().ok()?.as_ref().map(|i| i.client.clone())
    }

    /// Gets the Studio WebSocket client, or `None` if not initialized.
    pub fn web_socket_client() -> Option<Arc<StudioWebSocketClient>> {
        slot().read().ok()?.as_ref().map(|i| i.ws_client.clone())
    }

    /// Gets the Studio configuration, or `None` if not initialized.
    pub fn config() -> Option<StudioConfig> {
        slot().read().ok()?.as_ref().map(|i| i.config.clone())
    }

    /// Whether Studio integration is initialized.
    pub fn is_initialized() -> bool {
        slot().read().map(|g| g.is_some()).unwrap_or(false)
    }

    /// Shuts down Studio integration and releases resources.
    pub fn shutdown() {
        if let Ok(mut guard) = slot().write() {
            if let Some(installed) = guard.take() {
                installed.client.shutdown();
                // WebSocket close is async; best-effort fire-and-forget.
                let ws = installed.ws_client.clone();
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        let _ = ws.close().await;
                    });
                }
            }
        }
    }

    /// Runs initialization directly (non-builder convenience).
    pub async fn initialize_with(config: StudioConfig) -> StudioResult<()> {
        Self::initialize_with_transport(config, Arc::new(NoopStudioTransport)).await
    }

    /// Runs initialization with an explicit transport.
    ///
    /// Hosts that want the real Socket.IO connection pass a
    /// [`SocketIoTransport`](super::socket_io::SocketIoTransport) built over a shared
    /// [`PendingInputRegistry`]; the default is a no-op transport so tests stay offline.
    pub async fn initialize_with_transport(
        config: StudioConfig,
        transport: Arc<dyn StudioTransport>,
    ) -> StudioResult<()> {
        let client = Arc::new(StudioClient::new(config.clone()));
        let ws_client = Arc::new(StudioWebSocketClient::with_transport(
            config.clone(),
            transport,
        ));

        // Register the run and (best-effort) connect the socket.
        client.register_run().await?;
        if let Err(err) = ws_client.connect().await {
            tracing::warn!(error = %err, "studio websocket connect failed (continuing)");
        }

        let mut guard = slot().write().map_err(|_| StudioError::NotInitialized)?;
        *guard = Some(Installed {
            config,
            client,
            ws_client,
        });
        Ok(())
    }
}

/// Builder for configuring and initializing Studio integration.
#[derive(Debug, Clone, Default)]
pub struct StudioManagerBuilder {
    config: StudioConfigBuilder,
}

impl StudioManagerBuilder {
    fn new() -> Self {
        Self {
            config: StudioConfig::builder(),
        }
    }

    /// Sets the Studio URL.
    pub fn studio_url(mut self, studio_url: impl Into<String>) -> Self {
        self.config = self.config.studio_url(studio_url);
        self
    }

    /// Sets the tracing URL (defaults to `{studioUrl}/v1/traces`).
    pub fn tracing_url(mut self, tracing_url: impl Into<String>) -> Self {
        self.config = self.config.tracing_url(tracing_url);
        self
    }

    /// Sets the project name.
    pub fn project(mut self, project: impl Into<String>) -> Self {
        self.config = self.config.project(project);
        self
    }

    /// Sets the run name.
    pub fn run_name(mut self, run_name: impl Into<String>) -> Self {
        self.config = self.config.run_name(run_name);
        self
    }

    /// Sets the run id.
    pub fn run_id(mut self, run_id: impl Into<String>) -> Self {
        self.config = self.config.run_id(run_id);
        self
    }

    /// Sets the maximum number of HTTP retries.
    pub fn max_retries(mut self, max_retries: u32) -> Self {
        self.config = self.config.max_retries(max_retries);
        self
    }

    /// Sets the number of WebSocket reconnection attempts.
    pub fn reconnect_attempts(mut self, reconnect_attempts: u32) -> Self {
        self.config = self.config.reconnect_attempts(reconnect_attempts);
        self
    }

    /// Builds the configuration without initializing.
    pub fn build_config(self) -> StudioConfig {
        self.config.build()
    }

    /// Initializes Studio integration: builds config, creates clients, registers
    /// the run, and (best-effort) connects the WebSocket.
    pub async fn initialize(self) -> StudioResult<()> {
        let config = self.config.build();
        StudioManager::initialize_with(config).await
    }
}
