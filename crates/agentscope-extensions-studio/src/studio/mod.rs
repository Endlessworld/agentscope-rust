//! Central manager for AgentScope Studio integration, mirroring `StudioManager`.
//!
//! Java uses static volatile fields; Rust uses a process-global
//! [`OnceLock`] holding an `RwLock<Option<...>>` so it can be (re)initialized
//! and shut down safely across threads.

pub mod client;
pub mod config;
pub mod http;
pub mod message_hook;
pub mod pojo;
pub mod socket_io;
pub mod user_agent;
pub mod websocket;

pub use client::StudioClient;
pub use config::{StudioConfig, StudioConfigBuilder};
pub use http::{
    ReqwestStudioTransport, StudioHttpRequest, StudioHttpResponse, StudioHttpTransport,
};
pub use message_hook::StudioMessageHook;
pub use socket_io::{
    ConnectionState, EngineIoHandshake, ProtocolAction, SocketIoEvent, SocketIoProtocol,
    SocketIoTransport, DEFAULT_CONNECT_TIMEOUT, DEFAULT_NAMESPACE, ENGINE_IO_VERSION,
};
pub use user_agent::{StdTerminalIo, StudioUserAgent, TerminalIo};
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

    /// Runs initialization directly (non-builder convenience), with a no-op WebSocket transport.
    pub async fn initialize_with(config: StudioConfig) -> StudioResult<()> {
        Self::initialize_with_transport(config, Arc::new(NoopStudioTransport)).await
    }

    /// Runs initialization with an explicit WebSocket transport.
    ///
    /// Hosts that want the real Socket.IO connection pass a
    /// [`SocketIoTransport`](super::socket_io::SocketIoTransport); the [`PendingInputRegistry`] it
    /// owns is reused by the installed client automatically. The default is a no-op transport so
    /// tests stay offline. The HTTP side uses the `reqwest` transport.
    pub async fn initialize_with_transport(
        config: StudioConfig,
        transport: Arc<dyn StudioTransport>,
    ) -> StudioResult<()> {
        Self::initialize_with_transports(config, Arc::new(ReqwestStudioTransport::new()), transport)
            .await
    }

    /// Runs initialization with explicit HTTP and WebSocket transports.
    ///
    /// Both seams are injectable so the whole startup path (`registerRun` + socket connect) can
    /// run offline; production hosts pass [`ReqwestStudioTransport`] and a
    /// [`SocketIoTransport`](super::socket_io::SocketIoTransport).
    ///
    /// When the transport owns a [`PendingInputRegistry`]
    /// ([`StudioTransport::input_registry`]), the WebSocket client is built over that same
    /// registry, so `wait_for_input` via [`StudioManager::web_socket_client`] sees the events
    /// the transport decodes (STUDIO-01).
    pub async fn initialize_with_transports(
        config: StudioConfig,
        http_transport: Arc<dyn StudioHttpTransport>,
        transport: Arc<dyn StudioTransport>,
    ) -> StudioResult<()> {
        let client = Arc::new(StudioClient::with_transport(config.clone(), http_transport));
        // The transport resolves `forwardUserInput` into its own registry; reuse that exact
        // registry for the client so `wait_for_input` observes those events (STUDIO-01).
        let shared_registry = transport.input_registry();
        let ws_client = match shared_registry {
            Some(registry) => Arc::new(StudioWebSocketClient::with_transport_and_registry(
                config.clone(),
                transport,
                registry,
            )),
            None => Arc::new(StudioWebSocketClient::with_transport(
                config.clone(),
                transport,
            )),
        };

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
///
/// [`Self::initialize`] keeps [`NoopStudioTransport`] as the WebSocket default so offline hosts
/// and tests never dial `localhost:3000`; production hosts pass the real socket with
/// [`Self::transport`] — typically
/// [`SocketIoTransport`](super::socket_io::SocketIoTransport), whose own
/// [`PendingInputRegistry`] the installed client then reuses (STUDIO-09).
#[derive(Clone)]
pub struct StudioManagerBuilder {
    config: StudioConfigBuilder,
    http_transport: Option<Arc<dyn StudioHttpTransport>>,
    transport: Option<Arc<dyn StudioTransport>>,
}

impl std::fmt::Debug for StudioManagerBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The transports are trait objects without `Debug`; report them opaquely.
        f.debug_struct("StudioManagerBuilder")
            .field("config", &self.config)
            .field("http_transport", &self.http_transport.is_some())
            .field("transport", &self.transport.is_some())
            .finish()
    }
}

impl Default for StudioManagerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl StudioManagerBuilder {
    fn new() -> Self {
        Self {
            config: StudioConfig::builder(),
            http_transport: None,
            transport: None,
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

    /// Sets the REST transport (defaults to the `reqwest` one with 10s/30s timeouts).
    pub fn http_transport(mut self, http_transport: Arc<dyn StudioHttpTransport>) -> Self {
        self.http_transport = Some(http_transport);
        self
    }

    /// Sets the WebSocket transport used to receive `forwardUserInput` events.
    ///
    /// Defaults to [`NoopStudioTransport`]: initialization never connects to a server the host
    /// did not configure. Pass a real transport (e.g.
    /// [`SocketIoTransport`](super::socket_io::SocketIoTransport)) in production.
    pub fn transport(mut self, transport: Arc<dyn StudioTransport>) -> Self {
        self.transport = Some(transport);
        self
    }

    /// Initializes Studio integration: builds config, creates clients, registers
    /// the run, and (best-effort) connects the WebSocket.
    pub async fn initialize(self) -> StudioResult<()> {
        let config = self.config.build();
        let http_transport = self
            .http_transport
            .unwrap_or_else(|| Arc::new(ReqwestStudioTransport::new()));
        // A no-op transport stays the default: tests and offline hosts must never dial Studio.
        let transport = self
            .transport
            .unwrap_or_else(|| Arc::new(NoopStudioTransport));
        StudioManager::initialize_with_transports(config, http_transport, transport).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;
    use crate::studio::http::testing::{Outcome, ScriptedTransport};

    /// A transport that records connects and always reports a ready connection.
    #[derive(Debug, Default)]
    struct ReadyTransport {
        connects: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl StudioTransport for ReadyTransport {
        async fn connect(&self) -> StudioResult<()> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn close(&self) -> StudioResult<()> {
            Ok(())
        }

        fn is_connected(&self) -> bool {
            true
        }
    }

    /// STUDIO-09: the builder's main entry point installs the transports it was handed, so a
    /// production host does not have to reach for `initialize_with_transports`.
    #[tokio::test]
    async fn the_builder_installs_the_injected_transports() {
        let scripted = Arc::new(ScriptedTransport::always(Outcome::ok(200, "")));
        let http: Arc<dyn StudioHttpTransport> = scripted.clone();
        let transport = Arc::new(ReadyTransport::default());

        StudioManager::init()
            .studio_url("http://127.0.0.1:3000")
            .run_id("run-9")
            .http_transport(http)
            .transport(transport.clone())
            .initialize()
            .await
            .expect("initializes offline");

        assert_eq!(transport.connects.load(Ordering::SeqCst), 1);
        assert_eq!(
            scripted.attempt_count(),
            1,
            "registerRun must use the injected REST transport"
        );
        let client = StudioManager::web_socket_client().expect("installed by the builder");
        assert!(
            client.is_connected(),
            "the injected transport is the installed one"
        );
        StudioManager::shutdown();
    }
}
