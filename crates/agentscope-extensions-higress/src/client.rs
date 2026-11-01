//! Higress MCP client builder and wrapper.
//!
//! Mirrors `io.agentscope.extensions.higress.HigressMcpClientBuilder` and
//! `HigressMcpClientWrapper`.
//!
//! Rather than re-implementing the MCP client base class (Java's
//! `McpClientWrapper`), this module delegates protocol work to the harness
//! [`McpClientManager`], which owns the real `rust-mcp-sdk` client lifecycle.
//! The wrapper only adds Higress-specific behaviour: the
//! `x_higress_tool_search` semantic tool-selection shortcut and a local tool
//! cache.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentscope_core::error::{Error, Result};
use agentscope_harness::agent::{McpClientManager, McpServerConfig, McpToolInfo, McpTransport};
use async_trait::async_trait;
use rust_mcp_sdk::schema::{CallToolResult, Tool, ToolInputSchema};
use serde_json::{Map, Value};

use crate::search_result::{HigressToolInfo, HigressToolSearchResult};

/// The name of the Higress tool search tool.
pub const TOOL_SEARCH_NAME: &str = "x_higress_tool_search";

/// Default per-request timeout (Java `DEFAULT_TIMEOUT`).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
/// Default initialization timeout (Java `DEFAULT_INIT_TIMEOUT`).
pub const DEFAULT_INITIALIZATION_TIMEOUT: Duration = Duration::from_secs(30);
/// Default number of tools returned by tool search.
pub const DEFAULT_TOOL_SEARCH_TOP_K: usize = 10;

/// Transport used to reach the Higress MCP gateway.
///
/// Mirrors the Java private `HigressMcpClientBuilder.TransportType` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportType {
    /// Server-Sent Events transport for stateful connections.
    Sse,
    /// Streamable HTTP transport for stateless connections.
    StreamableHttp,
}

impl TransportType {
    /// A stable lowercase identifier for logging / diagnostics.
    pub fn as_str(&self) -> &'static str {
        match self {
            TransportType::Sse => "sse",
            TransportType::StreamableHttp => "streamable-http",
        }
    }

    /// The harness transport this selection pins.
    ///
    /// Mirrors the explicit `sseTransport` / `streamableHttpTransport` branches of
    /// Java `HigressMcpClientBuilder#createMcpClientBuilder`: the choice is
    /// authoritative instead of being sniffed from the endpoint URL.
    pub fn to_mcp_transport(self) -> McpTransport {
        match self {
            TransportType::Sse => McpTransport::Sse,
            TransportType::StreamableHttp => McpTransport::StreamableHttp,
        }
    }
}

/// Immutable configuration produced by [`HigressMcpClientBuilder`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HigressClientConfig {
    /// Unique name for the MCP client.
    pub client_name: String,
    /// Gateway endpoint URL.
    pub endpoint: String,
    /// Selected transport.
    pub transport_type: TransportType,
    /// HTTP headers sent with each request (name -> value).
    pub headers: Vec<(String, String)>,
    /// Query parameters merged into the endpoint URL (name -> value).
    pub query_params: Vec<(String, String)>,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Initialization timeout.
    pub initialization_timeout: Duration,
    /// Whether the `x_higress_tool_search` shortcut is enabled.
    pub enable_tool_search: bool,
    /// Query used by `x_higress_tool_search` when enabled.
    pub tool_search_query: Option<String>,
    /// Maximum number of tools returned by tool search.
    pub tool_search_top_k: usize,
    /// Whether request metadata (`_meta`) is propagated to this server.
    ///
    /// Mirrors Java `McpClientWrapper.propagateMeta` (default `true`); projected
    /// into [`McpServerConfig::propagate_meta`] so the harness honours it.
    pub propagate_meta: bool,
}

impl HigressClientConfig {
    /// Builds the endpoint with any configured query parameters appended.
    ///
    /// Mirrors Java `McpClientBuilder.extractEndpoint`
    /// (`agentscope-core/.../McpClientBuilder.java:740-795`): the endpoint's own
    /// query is URL-decoded, the configured parameters are merged on top (a
    /// configured value wins over a duplicate key coming from the URL), and the
    /// result is URL-encoded again with `URLEncoder` semantics (`+` for spaces).
    ///
    /// Rust keeps the scheme/host in the returned value (the harness needs a full
    /// URL), whereas Java only returns `path?query` for its transport builder.
    pub fn endpoint_with_query(&self) -> String {
        let (base, existing) = match self.endpoint.split_once('?') {
            Some((base, query)) => (base, query),
            None => (self.endpoint.as_str(), ""),
        };
        let mut merged: Vec<(String, String)> = Vec::new();
        for param in existing.split('&') {
            if param.is_empty() {
                continue;
            }
            let (key, value) = match param.split_once('=') {
                Some((key, value)) => (key, value),
                None => (param, ""),
            };
            let key = decode_query_component(key);
            let value = decode_query_component(value);
            // A duplicate key in the URL keeps the last value, like Java's map put.
            match merged.iter_mut().find(|(existing, _)| *existing == key) {
                Some(slot) => slot.1 = value,
                None => merged.push((key, value)),
            }
        }
        for (key, value) in &self.query_params {
            match merged.iter_mut().find(|(existing, _)| existing == key) {
                Some(slot) => slot.1 = value.clone(),
                None => merged.push((key.clone(), value.clone())),
            }
        }
        if merged.is_empty() {
            return base.to_string();
        }
        let query = merged
            .iter()
            .map(|(k, v)| {
                format!(
                    "{}={}",
                    encode_query_component(k),
                    encode_query_component(v)
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        format!("{base}?{query}")
    }

    /// Projects this configuration into a harness [`McpServerConfig`] so the
    /// caller can establish a real connection through [`McpClientManager`].
    ///
    /// The selected `transport_type` is pinned through
    /// [`McpServerConfig::transport`], so the harness never has to sniff `/sse`
    /// from the URL and an SSE endpoint that does not contain `/sse` still
    /// connects over SSE (HIGRESS-06).
    pub fn to_server_config(&self) -> McpServerConfig {
        McpServerConfig::new(self.client_name.clone())
            .url(self.endpoint_with_query())
            .headers(self.headers.clone())
            .transport(self.transport_type.to_mcp_transport())
            .propagate_meta(self.propagate_meta)
    }
}

/// Everything the wrapper needs to hand to the connection layer when it
/// establishes the real MCP connection.
///
/// [`McpServerConfig`] carries the transport choice, but not the two timeouts,
/// so they travel alongside it. Keeping them in one value makes it possible to
/// assert (in tests and in diagnostics) that no configured knob is dropped.
#[derive(Debug, Clone)]
pub struct McpConnectRequest {
    /// Server projection consumed by [`McpClientManager`].
    pub server: McpServerConfig,
    /// Caller-selected transport.
    pub transport: TransportType,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Connection/initialization timeout.
    pub initialization_timeout: Duration,
}

/// Narrow port over the harness MCP connection lifecycle.
///
/// [`McpClientManager`] is a concrete type that opens real sockets, which makes
/// it impossible to observe or fail deterministically in unit tests. This trait
/// exists only as a seam for the Higress wrapper: the real manager implements it
/// below, and callers may inject their own implementation through
/// [`HigressMcpClientWrapper::with_transport`]. It deliberately covers nothing
/// but the operations the wrapper uses — it is not a parallel MCP abstraction.
#[async_trait]
pub trait HigressMcpTransport: Send + Sync {
    /// Creates and connects a client for `request`, propagating any failure.
    async fn connect_server(&self, request: McpConnectRequest) -> Result<()>;
    /// Returns the connected client id for `server`, if any.
    async fn find_client_id_by_server(&self, server: &str) -> Option<String>;
    /// Lists the tools discovered for `client_id`.
    async fn list_tools(&self, client_id: &str) -> Result<Vec<McpToolInfo>>;
    /// Calls a tool on the connected client for `server`.
    ///
    /// `meta` is the MCP request metadata, forwarded untouched: `None` must reach
    /// the wire as an absent `_meta` field (Java
    /// `McpClientWrapper.callTool(name, args, meta)` passes `null` through).
    async fn call_tool_by_server(
        &self,
        server: &str,
        tool_name: &str,
        arguments: Value,
        meta: Option<Map<String, Value>>,
    ) -> Result<Value>;
    /// Disconnects the client for `server`, if one is connected.
    async fn disconnect_by_server(&self, server: &str) -> Result<()>;
}

#[async_trait]
impl HigressMcpTransport for McpClientManager {
    async fn connect_server(&self, request: McpConnectRequest) -> Result<()> {
        tracing::debug!(
            server = %request.server.name,
            transport = request.transport.as_str(),
            timeout = ?request.timeout,
            initialization_timeout = ?request.initialization_timeout,
            "Connecting Higress MCP server"
        );
        // Another wrapper may already own a connection for this server.
        if self
            .find_client_id_by_server(&request.server.name)
            .await
            .is_some()
        {
            return Ok(());
        }
        let client_id = self.create_client(request.server).await?;
        if let Err(error) = self.connect(&client_id).await {
            // Do not leave a half-registered client behind, otherwise retries
            // accumulate dead instances in the manager.
            if let Err(cleanup_error) = self.remove_client(&client_id).await {
                tracing::warn!(
                    "Failed to remove MCP client '{}' after connect error: {}",
                    client_id,
                    cleanup_error
                );
            }
            return Err(error);
        }
        Ok(())
    }

    async fn find_client_id_by_server(&self, server: &str) -> Option<String> {
        McpClientManager::find_client_id_by_server(self, server).await
    }

    async fn list_tools(&self, client_id: &str) -> Result<Vec<McpToolInfo>> {
        McpClientManager::list_tools(self, client_id).await
    }

    async fn call_tool_by_server(
        &self,
        server: &str,
        tool_name: &str,
        arguments: Value,
        meta: Option<Map<String, Value>>,
    ) -> Result<Value> {
        // `call_tool_by_server_with_meta` is the manager's metadata-aware entry
        // point; passing `None` keeps the historic (meta-less) behaviour.
        McpClientManager::call_tool_by_server_with_meta(self, server, tool_name, arguments, meta)
            .await
    }

    async fn disconnect_by_server(&self, server: &str) -> Result<()> {
        match McpClientManager::find_client_id_by_server(self, server).await {
            Some(client_id) => McpClientManager::disconnect(self, &client_id).await,
            None => Ok(()),
        }
    }
}

/// Fluent builder for [`HigressMcpClientWrapper`].
///
/// Mirrors `io.agentscope.extensions.higress.HigressMcpClientBuilder`.
#[derive(Debug, Clone)]
pub struct HigressMcpClientBuilder {
    client_name: String,
    endpoint: Option<String>,
    transport_type: Option<TransportType>,
    headers: HashMap<String, String>,
    query_params: HashMap<String, String>,
    timeout: Duration,
    initialization_timeout: Duration,
    enable_tool_search: bool,
    tool_search_query: Option<String>,
    tool_search_top_k: usize,
    propagate_meta: bool,
}

impl HigressMcpClientBuilder {
    /// Creates a new builder for the given client name.
    ///
    /// Returns an error when the name is missing or blank, mirroring the Java
    /// `IllegalArgumentException` on `create(null | "" | "   ")`.
    pub fn create(client_name: impl AsRef<str>) -> Result<Self> {
        let client_name = client_name.as_ref();
        if client_name.trim().is_empty() {
            return Err(Error::Other(
                "clientName must not be null or empty".to_string(),
            ));
        }
        Ok(Self {
            client_name: client_name.to_string(),
            endpoint: None,
            transport_type: None,
            headers: HashMap::new(),
            query_params: HashMap::new(),
            timeout: DEFAULT_TIMEOUT,
            initialization_timeout: DEFAULT_INITIALIZATION_TIMEOUT,
            enable_tool_search: false,
            tool_search_query: None,
            tool_search_top_k: DEFAULT_TOOL_SEARCH_TOP_K,
            propagate_meta: true,
        })
    }

    /// Configures the SSE transport endpoint.
    pub fn sse_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self.transport_type = Some(TransportType::Sse);
        self
    }

    /// Configures the StreamableHTTP transport endpoint.
    pub fn streamable_http_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self.transport_type = Some(TransportType::StreamableHttp);
        self
    }

    /// Adds a single HTTP header.
    pub fn header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(key.into(), value.into());
        self
    }

    /// Adds multiple HTTP headers at once.
    pub fn headers<I, K, V>(mut self, headers: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        for (k, v) in headers {
            self.headers.insert(k.into(), v.into());
        }
        self
    }

    /// Adds a single query parameter.
    pub fn query_param(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.query_params.insert(key.into(), value.into());
        self
    }

    /// Adds multiple query parameters at once.
    pub fn query_params<I, K, V>(mut self, query_params: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        for (k, v) in query_params {
            self.query_params.insert(k.into(), v.into());
        }
        self
    }

    /// Sets the per-request timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets the initialization timeout.
    pub fn initialization_timeout(mut self, timeout: Duration) -> Self {
        self.initialization_timeout = timeout;
        self
    }

    /// Enables tool search with the specified query.
    pub fn tool_search(mut self, query: impl Into<String>) -> Self {
        self.enable_tool_search = true;
        self.tool_search_query = Some(query.into());
        self
    }

    /// Enables tool search with the specified query and `top_k`.
    ///
    /// Returns an error when `top_k` is zero, mirroring the Java
    /// `IllegalArgumentException`.
    pub fn tool_search_with_top_k(
        mut self,
        query: impl Into<String>,
        top_k: usize,
    ) -> Result<Self> {
        if top_k == 0 {
            return Err(Error::Other(format!(
                "topK must be a positive integer, got: {top_k}"
            )));
        }
        self.enable_tool_search = true;
        self.tool_search_query = Some(query.into());
        self.tool_search_top_k = top_k;
        Ok(self)
    }

    /// Sets whether request metadata (`_meta`) is propagated to the gateway.
    ///
    /// Mirrors Java `McpClientWrapper.setPropagateMeta`: the default is `true`,
    /// and `false` keeps internal metadata (trace ids, callback URLs) inside the
    /// process. The value is projected into the harness `McpServerConfig`, which
    /// reads it live on every call.
    pub fn propagate_meta(mut self, propagate_meta: bool) -> Self {
        self.propagate_meta = propagate_meta;
        self
    }

    /// Validates the configuration, mirroring the Java `validateConfiguration`.
    fn validate(&self) -> Result<()> {
        match &self.endpoint {
            Some(e) if !e.trim().is_empty() => {}
            _ => {
                return Err(Error::Other(
                    "Endpoint must be configured via sseEndpoint() or streamableHttpEndpoint()"
                        .to_string(),
                ))
            }
        }
        if self.transport_type.is_none() {
            return Err(Error::Other(
                "Transport type must be configured via sseEndpoint() or streamableHttpEndpoint()"
                    .to_string(),
            ));
        }
        if self.enable_tool_search
            && self
                .tool_search_query
                .as_deref()
                .map(|q| q.trim().is_empty())
                .unwrap_or(true)
        {
            return Err(Error::Other(
                "Query is required for tool search. Use toolSearch(query) to configure."
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Validates the builder and materialises an immutable [`HigressClientConfig`].
    pub fn build_config(&self) -> Result<HigressClientConfig> {
        self.validate()?;
        let mut headers: Vec<(String, String)> = self
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        headers.sort();
        let mut query_params: Vec<(String, String)> = self
            .query_params
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        query_params.sort();
        Ok(HigressClientConfig {
            client_name: self.client_name.clone(),
            endpoint: self.endpoint.clone().unwrap_or_default(),
            transport_type: self.transport_type.unwrap_or(TransportType::Sse),
            headers,
            query_params,
            timeout: self.timeout,
            initialization_timeout: self.initialization_timeout,
            enable_tool_search: self.enable_tool_search,
            tool_search_query: self.tool_search_query.clone(),
            tool_search_top_k: self.tool_search_top_k,
            propagate_meta: self.propagate_meta,
        })
    }

    /// Validates the configuration and builds a wrapper backed by `manager`.
    ///
    /// Mirrors the Java `buildSync`, which validates, builds the delegate and
    /// initializes before returning.
    pub async fn build(&self, manager: Arc<McpClientManager>) -> Result<HigressMcpClientWrapper> {
        let config = self.build_config()?;
        let wrapper = HigressMcpClientWrapper::new(config, manager);
        wrapper.initialize().await?;
        Ok(wrapper)
    }

    /// Async alias of [`build`](Self::build), mirroring the Java `buildAsync`.
    pub async fn build_async(
        &self,
        manager: Arc<McpClientManager>,
    ) -> Result<HigressMcpClientWrapper> {
        self.build(manager).await
    }
}

/// Higress MCP client wrapper.
///
/// Mirrors `io.agentscope.extensions.higress.HigressMcpClientWrapper`, delegating
/// MCP protocol operations to a harness [`McpClientManager`].
pub struct HigressMcpClientWrapper {
    name: String,
    connection: Arc<dyn HigressMcpTransport>,
    /// Server projection handed to the connection layer on `initialize`.
    server_config: McpServerConfig,
    transport_type: TransportType,
    timeout: Duration,
    initialization_timeout: Duration,
    enable_tool_search: bool,
    tool_search_query: Option<String>,
    tool_search_top_k: usize,
    /// Connection-level `_meta` switch (Java `McpClientWrapper.propagateMeta`).
    /// Atomic so the Java "live switch" semantics survive `&self` accessors.
    propagate_meta: std::sync::atomic::AtomicBool,
    initialized: Mutex<bool>,
    /// Serialises `initialize` so concurrent callers connect at most once.
    init_guard: tokio::sync::Mutex<()>,
    cached_tools: Mutex<HashMap<String, Tool>>,
}

impl std::fmt::Debug for HigressMcpClientWrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HigressMcpClientWrapper")
            .field("name", &self.name)
            .field("enable_tool_search", &self.enable_tool_search)
            .field("initialized", &self.is_initialized())
            .finish()
    }
}

impl HigressMcpClientWrapper {
    /// Creates a wrapper from an already-validated [`HigressClientConfig`].
    pub fn new(config: HigressClientConfig, manager: Arc<McpClientManager>) -> Self {
        Self::with_transport(config, manager)
    }

    /// Creates a wrapper backed by an arbitrary [`HigressMcpTransport`].
    ///
    /// This is the seam used by tests and by hosts that own their own MCP
    /// connection layer; production code normally goes through
    /// [`HigressMcpClientBuilder::build`].
    pub fn with_transport(
        config: HigressClientConfig,
        transport: Arc<dyn HigressMcpTransport>,
    ) -> Self {
        let server_config = config.to_server_config();
        Self {
            name: config.client_name,
            connection: transport,
            server_config,
            transport_type: config.transport_type,
            timeout: config.timeout,
            initialization_timeout: config.initialization_timeout,
            enable_tool_search: config.enable_tool_search,
            tool_search_query: config.tool_search_query,
            tool_search_top_k: config.tool_search_top_k,
            propagate_meta: std::sync::atomic::AtomicBool::new(config.propagate_meta),
            initialized: Mutex::new(false),
            init_guard: tokio::sync::Mutex::new(()),
            cached_tools: Mutex::new(HashMap::new()),
        }
    }

    /// Creates a wrapper directly, primarily for tests and advanced wiring.
    ///
    /// No endpoint is configured here, so [`initialize`](Self::initialize) only
    /// succeeds if `manager` already holds a connection for `name`.
    pub fn with_options(
        name: impl Into<String>,
        manager: Arc<McpClientManager>,
        enable_tool_search: bool,
        tool_search_query: Option<String>,
        tool_search_top_k: usize,
    ) -> Self {
        let name = name.into();
        Self {
            server_config: McpServerConfig::new(name.clone()),
            name,
            connection: manager,
            transport_type: TransportType::StreamableHttp,
            timeout: DEFAULT_TIMEOUT,
            initialization_timeout: DEFAULT_INITIALIZATION_TIMEOUT,
            enable_tool_search,
            tool_search_query,
            tool_search_top_k,
            propagate_meta: std::sync::atomic::AtomicBool::new(true),
            initialized: Mutex::new(false),
            init_guard: tokio::sync::Mutex::new(()),
            cached_tools: Mutex::new(HashMap::new()),
        }
    }

    /// The client name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the `x_higress_tool_search` shortcut is enabled.
    pub fn is_tool_search_enabled(&self) -> bool {
        self.enable_tool_search
    }

    /// Whether the client has been initialized.
    pub fn is_initialized(&self) -> bool {
        *self.initialized.lock().unwrap()
    }

    /// Whether request metadata (`_meta`) is propagated to the gateway.
    ///
    /// Mirrors Java `McpClientWrapper.getPropagateMeta()`.
    pub fn propagate_meta(&self) -> bool {
        self.propagate_meta.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Toggles request-metadata propagation.
    ///
    /// Mirrors Java `McpClientWrapper.setPropagateMeta(boolean)`: the switch is
    /// live (no reconnect needed). It is applied on every call by
    /// [`call_tool_with_meta`](Self::call_tool_with_meta) — the Java
    /// `propagateMeta && clientWrapper.isPropagateMeta()` conjunction — and is
    /// also re-applied to the harness config on the next
    /// [`initialize`](Self::initialize).
    pub fn set_propagate_meta(&self, propagate_meta: bool) {
        self.propagate_meta
            .store(propagate_meta, std::sync::atomic::Ordering::Relaxed);
    }

    /// Establishes the real MCP connection, mirroring Java's
    /// `HigressMcpClientWrapper.initialize()`, which delegates to the underlying
    /// MCP client.
    ///
    /// The connection is created through [`McpClientManager`] and is bounded by
    /// the configured `initialization_timeout`. Any failure is returned to the
    /// caller and leaves the wrapper uninitialized, so `build()` can never hand
    /// back an object that merely claims to be connected.
    ///
    /// Idempotent: a second call (concurrent or sequential) is a no-op.
    pub async fn initialize(&self) -> Result<()> {
        if self.is_initialized() {
            tracing::debug!("Higress MCP client '{}' already initialized", self.name);
            return Ok(());
        }

        // Serialise the check-then-connect sequence; without this, two
        // concurrent callers would each create a client instance.
        let _guard = self.init_guard.lock().await;
        if self.is_initialized() {
            return Ok(());
        }

        if self.server_config.url.is_none() && self.server_config.command.is_none() {
            return Err(Error::Other(format!(
                "Higress MCP client '{}' has no endpoint configured; build it via \
                 HigressMcpClientBuilder::build()",
                self.name
            )));
        }

        // A connection for this server may already exist on the shared manager
        // (another wrapper, or a previous initialize). Reuse it rather than
        // opening a second one.
        if self
            .connection
            .find_client_id_by_server(&self.name)
            .await
            .is_some()
        {
            tracing::debug!(
                "Higress MCP client '{}' reusing existing connection",
                self.name
            );
            *self.initialized.lock().unwrap() = true;
            return Ok(());
        }

        tracing::info!("Initializing Higress MCP client: {}", self.name);
        let mut server = self.server_config.clone();
        server.propagate_meta = self.propagate_meta();
        let request = McpConnectRequest {
            server,
            transport: self.transport_type,
            timeout: self.timeout,
            initialization_timeout: self.initialization_timeout,
        };

        match tokio::time::timeout(
            self.initialization_timeout,
            self.connection.connect_server(request),
        )
        .await
        {
            Ok(Ok(())) => {
                *self.initialized.lock().unwrap() = true;
                tracing::info!("Higress MCP client '{}' initialized", self.name);
                Ok(())
            }
            Ok(Err(error)) => {
                tracing::error!(
                    "Failed to initialize Higress MCP client '{}': {}",
                    self.name,
                    error
                );
                Err(error)
            }
            Err(_) => Err(Error::Other(format!(
                "Timed out after {:?} initializing Higress MCP client '{}'",
                self.initialization_timeout, self.name
            ))),
        }
    }

    /// Applies the configured per-request `timeout` to one MCP operation.
    async fn with_request_timeout<F, T>(&self, operation: &str, future: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        match tokio::time::timeout(self.timeout, future).await {
            Ok(result) => result,
            Err(_) => Err(Error::Other(format!(
                "Higress MCP client '{}' timed out after {:?} during {}",
                self.name, self.timeout, operation
            ))),
        }
    }

    /// Lists the tools exposed by the gateway.
    ///
    /// When tool search is enabled, calls `x_higress_tool_search` with the
    /// configured query and returns only the semantically relevant tools;
    /// otherwise delegates to the underlying MCP server's full tool list.
    pub async fn list_tools(&self) -> Result<Vec<Tool>> {
        if self.enable_tool_search {
            let query = self.tool_search_query.clone().unwrap_or_default();
            tracing::info!(
                "Tool search enabled, calling {} with query: '{}'",
                TOOL_SEARCH_NAME,
                query
            );
            let result = self
                .search_tools_with_top_k(&query, self.tool_search_top_k)
                .await?;
            if !result.is_success() {
                let msg = result
                    .error_message()
                    .unwrap_or("unknown error")
                    .to_string();
                tracing::error!("Tool search failed: {}", msg);
                return Err(Error::Other(format!("Tool search failed: {}", msg)));
            }
            let tools: Vec<Tool> = result.tools().iter().map(convert_to_mcp_tool).collect();
            tracing::info!(
                "Tool search returned {} tools: {:?}",
                tools.len(),
                result.tool_names()
            );
            self.cache_tools(&tools);
            Ok(tools)
        } else {
            let client_id = self
                .with_request_timeout("find_client", async {
                    self.connection
                        .find_client_id_by_server(&self.name)
                        .await
                        .ok_or_else(|| {
                            Error::Other(format!(
                                "No connected MCP client for server '{}'",
                                self.name
                            ))
                        })
                })
                .await?;
            let infos = self
                .with_request_timeout("list_tools", self.connection.list_tools(&client_id))
                .await?;
            let tools: Vec<Tool> = infos.iter().map(mcp_tool_info_to_tool).collect();
            self.cache_tools(&tools);
            tracing::debug!(
                "Higress MCP client '{}' discovered {} tools",
                self.name,
                tools.len()
            );
            Ok(tools)
        }
    }

    /// Invokes a tool on the gateway.
    ///
    /// `arguments` may be any JSON value; object values are passed through as-is.
    /// No request metadata is propagated — use
    /// [`call_tool_with_meta`](Self::call_tool_with_meta) for that.
    pub async fn call_tool(&self, tool_name: &str, arguments: Value) -> Result<CallToolResult> {
        self.call_tool_with_meta(tool_name, arguments, None).await
    }

    /// Invokes a tool on the gateway, propagating `meta` as MCP request metadata.
    ///
    /// Mirrors the Java three-argument overload
    /// `HigressMcpClientWrapper.callTool(String, Map, Map)`, which hands `meta`
    /// to the delegate **untouched** (including `null`/`None`, meaning "send no
    /// `_meta` field"). Java's `testCallTool_PassesMetaStraightThrough` pins
    /// exactly that, so no merging, filtering or defaulting happens here either.
    ///
    /// The connection-level switch still applies: Java `McpTool` sends metadata
    /// only when `propagateMeta && clientWrapper.isPropagateMeta()`, so after
    /// [`set_propagate_meta(false)`](Self::set_propagate_meta) the metadata is
    /// dropped here (HIGRESS-02). The switch is read on every call, so flipping
    /// it takes effect immediately, without a reconnect.
    pub async fn call_tool_with_meta(
        &self,
        tool_name: &str,
        arguments: Value,
        meta: Option<Map<String, Value>>,
    ) -> Result<CallToolResult> {
        let meta = if self.propagate_meta() { meta } else { None };
        tracing::debug!(
            "Calling tool '{}' on Higress MCP client '{}' with arguments: {}",
            tool_name,
            self.name,
            arguments
        );
        let raw = self
            .with_request_timeout(
                "call_tool",
                self.connection
                    .call_tool_by_server(&self.name, tool_name, arguments, meta),
            )
            .await?;
        let result: CallToolResult = serde_json::from_value(raw)
            .map_err(|e| Error::Other(format!("Failed to decode tool call result: {}", e)))?;
        if result.is_error == Some(true) {
            tracing::warn!(
                "Higress tool '{}' returned error: {:?}",
                tool_name,
                result.content
            );
        } else {
            tracing::debug!("Higress tool '{}' completed successfully", tool_name);
        }
        Ok(result)
    }

    /// Calls `x_higress_tool_search` with `query` and parses the result.
    ///
    /// Mirrors the Java single-argument overload `searchTools(String query)`,
    /// whose request body is exactly `{"query": q}` — no `topK`
    /// (`HigressMcpClientWrapper.java:317-319`).
    pub async fn search_tools(&self, query: &str) -> Result<HigressToolSearchResult> {
        let mut args = Map::new();
        args.insert("query".to_string(), Value::String(query.to_string()));
        let result = self
            .call_tool(TOOL_SEARCH_NAME, Value::Object(args))
            .await?;
        Ok(HigressToolSearchResult::parse(&result))
    }

    /// Calls `x_higress_tool_search` with `query` and explicit `top_k`.
    ///
    /// Mirrors the Java two-argument overload `searchTools(String query, int topK)`.
    pub async fn search_tools_with_top_k(
        &self,
        query: &str,
        top_k: usize,
    ) -> Result<HigressToolSearchResult> {
        let mut args = Map::new();
        args.insert("query".to_string(), Value::String(query.to_string()));
        args.insert("topK".to_string(), Value::Number(top_k.into()));
        let result = self
            .call_tool(TOOL_SEARCH_NAME, Value::Object(args))
            .await?;
        Ok(HigressToolSearchResult::parse(&result))
    }

    /// Returns a previously discovered/cached tool by name.
    pub fn get_cached_tool(&self, name: &str) -> Option<Tool> {
        self.cached_tools.lock().unwrap().get(name).cloned()
    }

    /// Closes the client.
    ///
    /// Disconnects the underlying harness connection (if this wrapper started
    /// one) and clears cached data, mirroring Java's `close()`.
    pub async fn close(&self) {
        tracing::info!("Closing Higress MCP client: {}", self.name);
        if self.is_initialized() {
            if let Err(error) = self
                .with_request_timeout(
                    "disconnect",
                    self.connection.disconnect_by_server(&self.name),
                )
                .await
            {
                tracing::warn!(
                    "Failed to disconnect Higress MCP client '{}': {}",
                    self.name,
                    error
                );
            }
        }
        *self.initialized.lock().unwrap() = false;
        self.cached_tools.lock().unwrap().clear();
    }

    fn cache_tools(&self, tools: &[Tool]) {
        let mut cache = self.cached_tools.lock().unwrap();
        for tool in tools {
            cache.insert(tool.name.clone(), tool.clone());
        }
    }
}

/// Converts a parsed Higress [`HigressToolInfo`] into an MCP [`Tool`].
///
/// Mirrors the Java `convertToMcpTool`.
pub fn convert_to_mcp_tool(tool_info: &HigressToolInfo) -> Tool {
    let input_schema = tool_info
        .input_schema
        .as_ref()
        .and_then(build_input_schema)
        .unwrap_or_else(|| ToolInputSchema::new(Vec::new(), None, None));
    // Java `HigressMcpClientWrapper.java:223-225` forwards `outputSchema` whenever
    // the gateway supplied one.
    let output_schema = tool_info
        .output_schema
        .as_ref()
        .and_then(build_output_schema);

    Tool {
        name: tool_info.name.clone(),
        title: tool_info.title.clone(),
        description: tool_info.description.clone(),
        input_schema,
        output_schema,
        annotations: None,
        meta: None,
        execution: None,
        icons: Vec::new(),
    }
}

/// Decodes an `application/x-www-form-urlencoded` query component the way Java's
/// `URLDecoder.decode(..., UTF_8)` does: `+` becomes a space and `%XX` escapes are
/// resolved. Malformed escapes are kept verbatim rather than rejected.
fn decode_query_component(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                match (hex_value(bytes[index + 1]), hex_value(bytes[index + 2])) {
                    (Some(high), Some(low)) => {
                        out.push(high * 16 + low);
                        index += 3;
                    }
                    _ => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Encodes a query component the way Java's `URLEncoder.encode(..., UTF_8)` does:
/// alphanumerics plus `.-*_` stay as-is, a space becomes `+`, everything else is
/// `%XX`.
fn encode_query_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'*' | b'_' => {
                out.push(*byte as char)
            }
            b' ' => out.push('+'),
            byte => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Builds a [`ToolInputSchema`] from a raw JSON schema value.
///
/// DESIGN: `rust-mcp-schema` 0.10.3 models the MCP input schema as a closed
/// struct (`type: "object"` + `properties` + `required` only,
/// `rust-mcp-schema-0.10.3/src/generated_schema/2025_06_18/mcp_schema.rs`
/// `pub struct ToolInputSchema`), so keys such as `$schema`,
/// `additionalProperties` or `$defs` have no representation and cannot be
/// forwarded. Java passes the gateway map through verbatim because its
/// `McpSchema.Tool` keeps the schema as an untyped map. This is a type-system
/// limitation, not a dropped conversion: `properties`/`required` are preserved
/// and each property is kept as its raw schema object.
fn build_input_schema(schema: &Value) -> Option<ToolInputSchema> {
    let object = schema.as_object()?;
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
    Some(ToolInputSchema::new(required, properties, None))
}

/// Builds a [`ToolOutputSchema`] from a raw JSON schema value.
///
/// Java passes the gateway's `outputSchema` map through verbatim. Rust's typed
/// [`ToolOutputSchema`] can only describe `type: "object"` results, so a
/// non-object schema (for example a top-level array) cannot be represented and
/// degrades to `None` instead of silently claiming an object shape.
fn build_output_schema(schema: &Value) -> Option<rust_mcp_sdk::schema::ToolOutputSchema> {
    let object = schema.as_object()?;
    // `ToolOutputSchema` hard-codes `type: "object"`, so only object (or
    // untyped) results can be represented faithfully.
    match object.get("type").and_then(|t| t.as_str()) {
        None | Some("object") => {}
        Some(_) => return None,
    }
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
    Some(rust_mcp_sdk::schema::ToolOutputSchema::new(
        required, properties, None,
    ))
}

/// Converts a harness-discovered [`McpToolInfo`] into an MCP [`Tool`].
///
/// The gateway's human-readable `title` and its `outputSchema` are both carried
/// through (HIGRESS-05); dropping them here made the non-tool-search path lose
/// metadata the tool-search path (`convert_to_mcp_tool`) already kept.
///
/// [`McpToolInfo`]: agentscope_harness::agent::McpToolInfo
fn mcp_tool_info_to_tool(info: &agentscope_harness::agent::McpToolInfo) -> Tool {
    let input_schema = build_input_schema(&info.parameters)
        .unwrap_or_else(|| ToolInputSchema::new(Vec::new(), None, None));
    Tool {
        name: info.name.clone(),
        title: info.title.clone(),
        description: info.description.clone(),
        input_schema,
        output_schema: info.output_schema.as_ref().and_then(build_output_schema),
        annotations: None,
        meta: None,
        execution: None,
        icons: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_harness::agent::plan_transport;

    /// Minimal recording double for [`HigressMcpTransport`].
    ///
    /// The harness [`McpClientManager`] is concrete and opens real sockets, so
    /// it cannot be observed from a unit test. This double records every
    /// connect request and tool call and simulates a live server, which is
    /// exactly the seam `initialize`/`list_tools`/`call_tool` talk to.
    #[derive(Default)]
    struct RecordingConnection {
        requests: Mutex<Vec<McpConnectRequest>>,
        connected: Mutex<HashMap<String, Vec<McpToolInfo>>>,
        tools: Vec<McpToolInfo>,
        connect_error: Mutex<Option<String>>,
        connect_delay: Mutex<Option<Duration>>,
        tool_calls: Mutex<Vec<RecordedCall>>,
        /// Overrides the canned tool-call response, so tests can drive the
        /// tool-search path with a realistic gateway payload.
        call_response: Mutex<Option<Value>>,
    }

    /// One recorded [`HigressMcpTransport::call_tool_by_server`] invocation.
    #[derive(Debug, Clone)]
    struct RecordedCall {
        server: String,
        tool_name: String,
        arguments: Value,
        meta: Option<Map<String, Value>>,
    }

    impl RecordingConnection {
        fn with_tools(tools: Vec<McpToolInfo>) -> Self {
            Self {
                tools,
                ..Self::default()
            }
        }

        fn requests(&self) -> Vec<McpConnectRequest> {
            self.requests.lock().unwrap().clone()
        }

        fn calls(&self) -> Vec<RecordedCall> {
            self.tool_calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl HigressMcpTransport for RecordingConnection {
        async fn connect_server(&self, request: McpConnectRequest) -> Result<()> {
            self.requests.lock().unwrap().push(request.clone());
            if let Some(error) = self.connect_error.lock().unwrap().clone() {
                return Err(Error::Other(error));
            }
            let delay = *self.connect_delay.lock().unwrap();
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            self.connected
                .lock()
                .unwrap()
                .insert(request.server.name, self.tools.clone());
            Ok(())
        }

        async fn find_client_id_by_server(&self, server: &str) -> Option<String> {
            self.connected
                .lock()
                .unwrap()
                .contains_key(server)
                .then(|| server.to_string())
        }

        async fn list_tools(&self, client_id: &str) -> Result<Vec<McpToolInfo>> {
            self.connected
                .lock()
                .unwrap()
                .get(client_id)
                .cloned()
                .ok_or_else(|| Error::Other(format!("Client '{}' is not connected", client_id)))
        }

        async fn call_tool_by_server(
            &self,
            server: &str,
            tool_name: &str,
            arguments: Value,
            meta: Option<Map<String, Value>>,
        ) -> Result<Value> {
            self.tool_calls.lock().unwrap().push(RecordedCall {
                server: server.to_string(),
                tool_name: tool_name.to_string(),
                arguments,
                meta,
            });
            if let Some(response) = self.call_response.lock().unwrap().clone() {
                return Ok(response);
            }
            Ok(serde_json::json!({
                "content": [{ "type": "text", "text": "ok" }],
                "isError": false
            }))
        }

        async fn disconnect_by_server(&self, server: &str) -> Result<()> {
            self.connected.lock().unwrap().remove(server);
            Ok(())
        }
    }

    fn test_tool(name: &str) -> McpToolInfo {
        McpToolInfo {
            name: name.to_string(),
            description: Some(format!("{name} tool")),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "q": { "type": "string" } },
                "required": ["q"]
            }),
            server: "higress".to_string(),
            title: None,
            output_schema: None,
        }
    }

    fn test_config() -> HigressClientConfig {
        HigressMcpClientBuilder::create("higress")
            .unwrap()
            .streamable_http_endpoint("https://gw.example.com/mcp")
            .header("Authorization", "Bearer t")
            .query_param("x", "1")
            .timeout(Duration::from_secs(5))
            .initialization_timeout(Duration::from_secs(7))
            .build_config()
            .unwrap()
    }

    #[tokio::test]
    async fn initialize_connects_and_records_full_config() {
        let recording = Arc::new(RecordingConnection::default());
        let wrapper = HigressMcpClientWrapper::with_transport(test_config(), recording.clone());

        wrapper.initialize().await.unwrap();

        assert!(wrapper.is_initialized());
        // The regression: before the fix nothing registered a client, so this
        // lookup always returned None.
        assert!(recording
            .find_client_id_by_server("higress")
            .await
            .is_some());

        let requests = recording.requests();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.server.name, "higress");
        assert_eq!(
            request.server.url.as_deref(),
            Some("https://gw.example.com/mcp?x=1")
        );
        assert_eq!(
            request.server.headers,
            vec![("Authorization".to_string(), "Bearer t".to_string())]
        );
        assert_eq!(request.transport, TransportType::StreamableHttp);
        assert_eq!(request.timeout, Duration::from_secs(5));
        assert_eq!(request.initialization_timeout, Duration::from_secs(7));
    }

    #[tokio::test]
    async fn initialize_is_idempotent_and_only_connects_once() {
        let recording = Arc::new(RecordingConnection::default());
        let wrapper = HigressMcpClientWrapper::with_transport(test_config(), recording.clone());

        wrapper.initialize().await.unwrap();
        wrapper.initialize().await.unwrap();

        assert_eq!(recording.requests().len(), 1);
    }

    #[tokio::test]
    async fn initialize_propagates_connection_failure() {
        let recording = Arc::new(RecordingConnection::default());
        *recording.connect_error.lock().unwrap() = Some("connection refused".to_string());
        let wrapper = HigressMcpClientWrapper::with_transport(test_config(), recording.clone());

        let error = wrapper.initialize().await.unwrap_err();
        assert!(error.to_string().contains("connection refused"));
        assert!(!wrapper.is_initialized());
    }

    #[tokio::test]
    async fn initialize_applies_initialization_timeout() {
        let recording = Arc::new(RecordingConnection::default());
        *recording.connect_delay.lock().unwrap() = Some(Duration::from_millis(500));
        let config = HigressMcpClientBuilder::create("higress")
            .unwrap()
            .streamable_http_endpoint("https://gw.example.com/mcp")
            .initialization_timeout(Duration::from_millis(10))
            .build_config()
            .unwrap();
        let wrapper = HigressMcpClientWrapper::with_transport(config, recording);

        let error = wrapper.initialize().await.unwrap_err();
        assert!(error.to_string().contains("Timed out"));
        assert!(!wrapper.is_initialized());
    }

    #[tokio::test]
    async fn initialize_pins_sse_for_an_endpoint_without_a_sse_suffix() {
        // Before HIGRESS-06 this configuration was rejected: the harness sniffed
        // `/sse` from the URL and would have silently used Streamable HTTP. The
        // transport is pinned on `McpServerConfig` now, so the caller's choice
        // is authoritative and the connection goes through SSE.
        let recording = Arc::new(RecordingConnection::with_tools(vec![test_tool("echo")]));
        let config = HigressMcpClientBuilder::create("higress")
            .unwrap()
            .sse_endpoint("https://gw.example.com/mcp")
            .build_config()
            .unwrap();
        let wrapper = HigressMcpClientWrapper::with_transport(config, recording.clone());

        wrapper.initialize().await.unwrap();

        let requests = recording.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].server.transport, Some(McpTransport::Sse));
        assert_eq!(requests[0].transport, TransportType::Sse);
        // The projection the harness actually plans with agrees (no sniffing).
        assert_eq!(
            plan_transport(&requests[0].server).unwrap().kind(),
            McpTransport::Sse
        );
        assert_eq!(wrapper.list_tools().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn list_tools_after_initialize_does_not_report_missing_client() {
        let recording = Arc::new(RecordingConnection::with_tools(vec![
            test_tool("echo"),
            test_tool("add"),
        ]));
        let wrapper = HigressMcpClientWrapper::with_transport(test_config(), recording);

        wrapper.initialize().await.unwrap();
        let tools = wrapper.list_tools().await.unwrap();

        assert_eq!(
            tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["echo", "add"]
        );
        assert_eq!(tools[0].description.as_deref(), Some("echo tool"));
        assert_eq!(tools[0].input_schema.required, vec!["q".to_string()]);
        assert!(wrapper.get_cached_tool("echo").is_some());
    }

    #[test]
    fn create_rejects_blank_client_name() {
        assert!(HigressMcpClientBuilder::create("").is_err());
        assert!(HigressMcpClientBuilder::create("   ").is_err());
        assert!(HigressMcpClientBuilder::create("ok").is_ok());
    }

    #[test]
    fn build_requires_endpoint() {
        let err = HigressMcpClientBuilder::create("c")
            .unwrap()
            .build_config()
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("Endpoint must be configured via sseEndpoint() or streamableHttpEndpoint()"));
    }

    #[test]
    fn sse_endpoint_sets_transport() {
        let config = HigressMcpClientBuilder::create("c")
            .unwrap()
            .sse_endpoint("https://gw.example.com/sse")
            .build_config()
            .unwrap();
        assert_eq!(config.transport_type, TransportType::Sse);
        assert_eq!(config.transport_type.as_str(), "sse");
        assert_eq!(config.endpoint, "https://gw.example.com/sse");
        assert_eq!(config.timeout, DEFAULT_TIMEOUT);
        assert_eq!(
            config.initialization_timeout,
            DEFAULT_INITIALIZATION_TIMEOUT
        );
        assert_eq!(config.tool_search_top_k, DEFAULT_TOOL_SEARCH_TOP_K);
    }

    #[test]
    fn streamable_http_endpoint_sets_transport() {
        let config = HigressMcpClientBuilder::create("c")
            .unwrap()
            .streamable_http_endpoint("https://gw.example.com/mcp")
            .build_config()
            .unwrap();
        assert_eq!(config.transport_type, TransportType::StreamableHttp);
        assert_eq!(config.transport_type.as_str(), "streamable-http");
    }

    #[test]
    fn tool_search_requires_non_blank_query() {
        let err = HigressMcpClientBuilder::create("c")
            .unwrap()
            .sse_endpoint("https://gw.example.com/sse")
            .tool_search("  ")
            .build_config()
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("Query is required for tool search"));
    }

    #[test]
    fn tool_search_with_top_k_rejects_zero() {
        let err = HigressMcpClientBuilder::create("c")
            .unwrap()
            .sse_endpoint("https://gw.example.com/sse")
            .tool_search_with_top_k("weather", 0)
            .unwrap_err();
        assert!(err.to_string().contains("topK must be a positive integer"));
    }

    #[test]
    fn tool_search_with_top_k_keeps_top_k() {
        let config = HigressMcpClientBuilder::create("c")
            .unwrap()
            .sse_endpoint("https://gw.example.com/sse")
            .tool_search_with_top_k("weather", 3)
            .unwrap()
            .build_config()
            .unwrap();
        assert!(config.enable_tool_search);
        assert_eq!(config.tool_search_query.as_deref(), Some("weather"));
        assert_eq!(config.tool_search_top_k, 3);
    }

    #[test]
    fn build_config_sorts_headers_and_query_params() {
        let config = HigressMcpClientBuilder::create("c")
            .unwrap()
            .sse_endpoint("https://gw.example.com/sse")
            .header("b", "2")
            .header("a", "1")
            .query_param("y", "2")
            .query_param("x", "1")
            .build_config()
            .unwrap();
        assert_eq!(
            config.headers,
            vec![
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "2".to_string())
            ]
        );
        assert_eq!(
            config.query_params,
            vec![
                ("x".to_string(), "1".to_string()),
                ("y".to_string(), "2".to_string())
            ]
        );
    }

    #[test]
    fn headers_and_query_params_bulk_setters_merge() {
        let config = HigressMcpClientBuilder::create("c")
            .unwrap()
            .sse_endpoint("https://gw.example.com/sse")
            .headers(vec![("a", "1"), ("b", "2")])
            .header("b", "3")
            .query_params(vec![("x", "1"), ("y", "2")])
            .build_config()
            .unwrap();
        assert!(config.headers.contains(&("b".to_string(), "3".to_string())));
        assert_eq!(config.headers.len(), 2);
        assert_eq!(config.query_params.len(), 2);
    }

    #[test]
    fn endpoint_with_query_appends_using_ampersand_when_query_present() {
        let config = HigressMcpClientBuilder::create("c")
            .unwrap()
            .sse_endpoint("https://gw.example.com/sse?token=t")
            .query_param("x", "1")
            .query_param("y", "2")
            .build_config()
            .unwrap();
        assert_eq!(
            config.endpoint_with_query(),
            "https://gw.example.com/sse?token=t&x=1&y=2"
        );
    }

    #[test]
    fn endpoint_with_query_is_identity_when_no_params() {
        let config = HigressMcpClientBuilder::create("c")
            .unwrap()
            .sse_endpoint("https://gw.example.com/sse")
            .build_config()
            .unwrap();
        assert_eq!(config.endpoint_with_query(), "https://gw.example.com/sse");
    }

    #[test]
    fn to_server_config_projects_name_url_and_headers() {
        let config = HigressMcpClientBuilder::create("higress")
            .unwrap()
            .streamable_http_endpoint("https://gw.example.com/mcp")
            .header("Authorization", "Bearer t")
            .query_param("x", "1")
            .build_config()
            .unwrap();
        let server = config.to_server_config();
        assert_eq!(server.name, "higress");
        assert_eq!(
            server.url.as_deref(),
            Some("https://gw.example.com/mcp?x=1")
        );
        assert_eq!(
            server.headers,
            vec![("Authorization".to_string(), "Bearer t".to_string())]
        );
        assert!(server.command.is_none());
    }

    #[test]
    fn to_server_config_pins_the_selected_transport() {
        // HIGRESS-06: the selection is projected instead of being sniffed from
        // the URL, so an SSE endpoint without a `/sse` suffix still uses SSE.
        let sse = HigressMcpClientBuilder::create("c")
            .unwrap()
            .sse_endpoint("https://gw.example.com/mcp")
            .build_config()
            .unwrap();
        assert_eq!(sse.to_server_config().transport, Some(McpTransport::Sse));
        assert_eq!(
            plan_transport(&sse.to_server_config()).unwrap().kind(),
            McpTransport::Sse
        );

        // …and the reverse: an explicit Streamable HTTP pin wins over a `/sse`
        // path that would otherwise be sniffed as SSE.
        let http = HigressMcpClientBuilder::create("c")
            .unwrap()
            .streamable_http_endpoint("https://gw.example.com/sse")
            .build_config()
            .unwrap();
        assert_eq!(
            http.to_server_config().transport,
            Some(McpTransport::StreamableHttp)
        );
        assert_eq!(
            plan_transport(&http.to_server_config()).unwrap().kind(),
            McpTransport::StreamableHttp
        );
    }

    #[test]
    fn endpoint_with_query_merges_duplicate_keys_and_encodes_like_java() {
        let config = HigressMcpClientBuilder::create("c")
            .unwrap()
            .sse_endpoint("https://gw.example.com/sse?token=a%20b&token=c+d&keep=1")
            .query_param("q", "a&b")
            .query_param("token", "x y")
            .build_config()
            .unwrap();
        assert_eq!(
            config.endpoint_with_query(),
            "https://gw.example.com/sse?token=x+y&keep=1&q=a%26b"
        );
    }

    #[test]
    fn propagate_meta_defaults_true_and_projects_into_server_config() {
        let config = HigressMcpClientBuilder::create("c")
            .unwrap()
            .streamable_http_endpoint("https://gw.example.com/mcp")
            .build_config()
            .unwrap();
        assert!(config.propagate_meta);
        assert!(config.to_server_config().propagate_meta);

        let config = HigressMcpClientBuilder::create("c")
            .unwrap()
            .streamable_http_endpoint("https://gw.example.com/mcp")
            .propagate_meta(false)
            .build_config()
            .unwrap();
        assert!(!config.propagate_meta);
        assert!(!config.to_server_config().propagate_meta);
    }

    #[tokio::test]
    async fn initialize_applies_propagate_meta_switch() {
        let recording = Arc::new(RecordingConnection::default());
        let config = HigressMcpClientBuilder::create("higress")
            .unwrap()
            .streamable_http_endpoint("https://gw.example.com/mcp")
            .propagate_meta(false)
            .build_config()
            .unwrap();
        let wrapper = HigressMcpClientWrapper::with_transport(config, recording.clone());
        assert!(!wrapper.propagate_meta());

        wrapper.initialize().await.unwrap();
        assert!(!recording.requests()[0].server.propagate_meta);

        // The switch stays live after construction, like Java's setter.
        wrapper.set_propagate_meta(true);
        assert!(wrapper.propagate_meta());
    }

    #[tokio::test]
    async fn set_propagate_meta_false_strips_metadata_from_calls() {
        let recording = Arc::new(RecordingConnection::default());
        let wrapper = HigressMcpClientWrapper::with_transport(test_config(), recording.clone());
        wrapper.initialize().await.unwrap();

        let mut meta = Map::new();
        meta.insert(
            "io.agentscope/toolCallId".to_string(),
            Value::String("call-1".to_string()),
        );

        wrapper
            .call_tool_with_meta("echo", serde_json::json!({ "q": "x" }), Some(meta.clone()))
            .await
            .unwrap();
        wrapper.set_propagate_meta(false);
        wrapper
            .call_tool_with_meta("echo", serde_json::json!({ "q": "x" }), Some(meta.clone()))
            .await
            .unwrap();

        let calls = recording.calls();
        assert_eq!(calls.len(), 2);
        // Default (`true`) forwards the caller's metadata untouched.
        assert_eq!(calls[0].meta.as_ref(), Some(&meta));
        // The connection-level switch wins over the caller's map (Java
        // `propagateMeta && clientWrapper.isPropagateMeta()`), and it is live:
        // no reconnect happened.
        assert!(calls[1].meta.is_none());
        assert_eq!(recording.requests().len(), 1);

        wrapper.set_propagate_meta(true);
        wrapper
            .call_tool_with_meta("echo", serde_json::json!({ "q": "x" }), Some(meta.clone()))
            .await
            .unwrap();
        assert_eq!(recording.calls().last().unwrap().meta.as_ref(), Some(&meta));
    }

    #[test]
    fn timeout_setters_are_respected() {
        let config = HigressMcpClientBuilder::create("c")
            .unwrap()
            .sse_endpoint("https://gw.example.com/sse")
            .timeout(Duration::from_secs(5))
            .initialization_timeout(Duration::from_secs(7))
            .build_config()
            .unwrap();
        assert_eq!(config.timeout, Duration::from_secs(5));
        assert_eq!(config.initialization_timeout, Duration::from_secs(7));
    }

    #[test]
    fn convert_to_mcp_tool_maps_name_title_description_and_schema() {
        let info = HigressToolInfo {
            name: "n".into(),
            description: Some("d".into()),
            title: Some("t".into()),
            input_schema: Some(serde_json::json!({
                "type": "object",
                "properties": { "a": { "type": "string" } },
                "required": ["a"]
            })),
            output_schema: None,
        };
        let tool = convert_to_mcp_tool(&info);
        assert_eq!(tool.name, "n");
        assert_eq!(tool.title.as_deref(), Some("t"));
        assert_eq!(tool.description.as_deref(), Some("d"));
        assert_eq!(tool.input_schema.required, vec!["a".to_string()]);
        let props = tool.input_schema.properties.as_ref().unwrap();
        assert!(props.contains_key("a"));
    }

    #[test]
    fn convert_to_mcp_tool_without_schema_yields_empty_schema() {
        let tool = convert_to_mcp_tool(&HigressToolInfo::new("only"));
        assert_eq!(tool.name, "only");
        assert!(tool.input_schema.required.is_empty());
    }

    #[test]
    fn convert_to_mcp_tool_ignores_non_object_schema() {
        let info = HigressToolInfo {
            name: "n".into(),
            description: None,
            title: None,
            input_schema: Some(serde_json::json!("not-an-object")),
            output_schema: None,
        };
        let tool = convert_to_mcp_tool(&info);
        assert!(tool.input_schema.required.is_empty());
        assert!(tool.input_schema.properties.is_none());
    }

    #[test]
    fn tool_info_new_sets_only_name() {
        let info = HigressToolInfo::new("alpha");
        assert_eq!(info.name, "alpha");
        assert!(info.description.is_none());
        assert!(info.title.is_none());
        assert!(info.input_schema.is_none());
        assert!(info.output_schema.is_none());
    }

    #[test]
    fn with_options_exposes_state_without_initializing() {
        let manager = Arc::new(McpClientManager::new());
        let wrapper = HigressMcpClientWrapper::with_options(
            "higress",
            manager,
            true,
            Some("weather".to_string()),
            5,
        );
        assert_eq!(wrapper.name(), "higress");
        assert!(wrapper.is_tool_search_enabled());
        assert!(!wrapper.is_initialized());
        assert!(wrapper.get_cached_tool("anything").is_none());
        assert!(format!("{:?}", wrapper).contains("HigressMcpClientWrapper"));
    }

    #[tokio::test]
    async fn close_disconnects_and_resets_state() {
        let recording = Arc::new(RecordingConnection::default());
        let wrapper = HigressMcpClientWrapper::with_transport(test_config(), recording.clone());
        wrapper.initialize().await.unwrap();
        assert!(wrapper.is_initialized());

        wrapper.close().await;

        assert!(!wrapper.is_initialized());
        assert!(recording
            .find_client_id_by_server("higress")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn list_tools_without_connection_reports_missing_client() {
        let manager = Arc::new(McpClientManager::new());
        let wrapper = HigressMcpClientWrapper::with_options("absent", manager, false, None, 10);
        let err = wrapper.list_tools().await.unwrap_err();
        assert!(err
            .to_string()
            .contains("No connected MCP client for server 'absent'"));
    }

    #[tokio::test]
    async fn search_tools_without_connection_fails_to_reach_gateway() {
        let manager = Arc::new(McpClientManager::new());
        let wrapper = HigressMcpClientWrapper::with_options("absent", manager, true, None, 10);
        let err = wrapper.search_tools("weather").await.unwrap_err();
        assert!(err.to_string().contains("absent"));
    }

    // ------------------------------------------------------------------
    // P1-4: tool-search main path, caching and metadata passthrough
    // ------------------------------------------------------------------

    /// A gateway payload shaped like a real `x_higress_tool_search` response:
    /// tools live in `structuredContent`, each with input *and* output schema.
    fn tool_search_response() -> Value {
        serde_json::json!({
            "content": [],
            "structuredContent": {
                "tools": [{
                    "name": "map___maps_weather",
                    "title": "maps_weather",
                    "description": "Weather by city",
                    "inputSchema": {
                        "type": "object",
                        "properties": { "city": { "type": "string" } },
                        "required": ["city"]
                    },
                    "outputSchema": {
                        "type": "object",
                        "properties": { "temp": { "type": "number" } },
                        "required": ["temp"]
                    }
                }]
            },
            "isError": false
        })
    }

    fn tool_search_config(query: &str, top_k: usize) -> HigressClientConfig {
        HigressMcpClientBuilder::create("higress")
            .unwrap()
            .streamable_http_endpoint("https://gw.example.com/mcp")
            .tool_search_with_top_k(query, top_k)
            .unwrap()
            .build_config()
            .unwrap()
    }

    #[tokio::test]
    async fn tool_search_lists_converted_tools_and_sends_query_with_top_k() {
        let recording = Arc::new(RecordingConnection::default());
        *recording.call_response.lock().unwrap() = Some(tool_search_response());
        let wrapper = HigressMcpClientWrapper::with_transport(
            tool_search_config("weather", 3),
            recording.clone(),
        );

        let tools = wrapper.list_tools().await.unwrap();

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "map___maps_weather");
        assert_eq!(tools[0].title.as_deref(), Some("maps_weather"));
        assert_eq!(tools[0].description.as_deref(), Some("Weather by city"));
        assert_eq!(tools[0].input_schema.required, vec!["city".to_string()]);
        // P1-2: `outputSchema` must survive the ToolInfo -> Tool conversion
        // (Java `HigressMcpClientWrapper.java:223-225`).
        let output = tools[0].output_schema.as_ref().unwrap();
        assert_eq!(output.required, vec!["temp".to_string()]);
        assert!(output.properties.as_ref().unwrap().contains_key("temp"));

        // Request body: exactly one call to the tool-search tool, carrying the
        // configured query and `topK` (Java `HigressMcpClientWrapper.java:164`
        // passes both).
        let calls = recording.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool_name, TOOL_SEARCH_NAME);
        assert_eq!(calls[0].arguments["query"], "weather");
        assert_eq!(calls[0].arguments["topK"], 3);
        assert!(calls[0].meta.is_none());
    }

    #[tokio::test]
    async fn tool_search_results_are_cached_and_served_without_a_second_call() {
        let recording = Arc::new(RecordingConnection::default());
        *recording.call_response.lock().unwrap() = Some(tool_search_response());
        let wrapper = HigressMcpClientWrapper::with_transport(
            tool_search_config("weather", 5),
            recording.clone(),
        );

        let first = wrapper.list_tools().await.unwrap();
        assert_eq!(first.len(), 1);

        // Cache hit: the tool is available by name without another gateway
        // round-trip.
        let cached = wrapper.get_cached_tool("map___maps_weather").unwrap();
        assert_eq!(cached.name, "map___maps_weather");
        assert!(wrapper.get_cached_tool("map___maps_weather").is_some());
        assert_eq!(recording.calls().len(), 1);
    }

    #[tokio::test]
    async fn list_tools_without_tool_search_caches_every_tool() {
        let recording = Arc::new(RecordingConnection::with_tools(vec![
            test_tool("echo"),
            test_tool("add"),
        ]));
        let wrapper = HigressMcpClientWrapper::with_transport(test_config(), recording.clone());
        wrapper.initialize().await.unwrap();

        let tools = wrapper.list_tools().await.unwrap();

        assert_eq!(tools.len(), 2);
        for name in ["echo", "add"] {
            assert!(
                wrapper.get_cached_tool(name).is_some(),
                "tool '{name}' should be cached"
            );
        }
        // The plain path never touches tool search.
        assert!(recording
            .calls()
            .iter()
            .all(|call| call.tool_name != TOOL_SEARCH_NAME));
    }

    #[tokio::test]
    async fn list_tools_without_tool_search_keeps_title_and_output_schema() {
        // HIGRESS-05: the plain path used to drop the gateway's `title` and
        // `outputSchema` even though the tool-search path kept them.
        let recording = Arc::new(RecordingConnection::with_tools(vec![McpToolInfo {
            name: "weather".to_string(),
            description: Some("Weather".to_string()),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
            server: "higress".to_string(),
            title: Some("Weather Lookup".to_string()),
            output_schema: Some(serde_json::json!({
                "type": "object",
                "properties": { "temp": { "type": "number" } },
                "required": ["temp"]
            })),
        }]));
        let wrapper = HigressMcpClientWrapper::with_transport(test_config(), recording.clone());
        wrapper.initialize().await.unwrap();

        let tools = wrapper.list_tools().await.unwrap();

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].title.as_deref(), Some("Weather Lookup"));
        let output_schema = tools[0]
            .output_schema
            .as_ref()
            .expect("outputSchema must survive the plain list path");
        assert_eq!(output_schema.required, vec!["temp".to_string()]);
        assert!(output_schema
            .properties
            .as_ref()
            .unwrap()
            .contains_key("temp"));
        // The cache stores the same metadata, not a stripped copy.
        let cached = wrapper.get_cached_tool("weather").unwrap();
        assert_eq!(cached.title.as_deref(), Some("Weather Lookup"));
        assert!(cached.output_schema.is_some());
    }

    #[tokio::test]
    async fn tool_search_failure_surfaces_as_list_tools_error() {
        let recording = Arc::new(RecordingConnection::default());
        *recording.call_response.lock().unwrap() = Some(serde_json::json!({
            "content": [],
            "isError": true
        }));
        let wrapper = HigressMcpClientWrapper::with_transport(
            tool_search_config("weather", 3),
            recording.clone(),
        );

        let err = wrapper.list_tools().await.unwrap_err();

        assert!(err.to_string().contains("Tool search failed"));
        assert!(wrapper.get_cached_tool("map___maps_weather").is_none());
    }

    #[tokio::test]
    async fn tool_search_falls_back_to_text_content_payload() {
        // Gateways that do not emit `structuredContent` send the same JSON as a
        // text block; `list_tools` must still return the tools.
        let recording = Arc::new(RecordingConnection::default());
        *recording.call_response.lock().unwrap() = Some(serde_json::json!({
            "content": [{
                "type": "text",
                "text": serde_json::json!({
                    "tools": [{ "name": "text_tool", "description": "from text" }]
                }).to_string()
            }],
            "isError": false
        }));
        let wrapper = HigressMcpClientWrapper::with_transport(
            tool_search_config("weather", 2),
            recording.clone(),
        );

        let tools = wrapper.list_tools().await.unwrap();

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "text_tool");
        assert_eq!(tools[0].description.as_deref(), Some("from text"));
    }

    #[tokio::test]
    async fn call_tool_with_meta_passes_meta_straight_through() {
        let recording = Arc::new(RecordingConnection::default());
        let wrapper = HigressMcpClientWrapper::with_transport(test_config(), recording.clone());
        wrapper.initialize().await.unwrap();

        let mut meta = Map::new();
        meta.insert("traceId".to_string(), Value::String("abc-123".to_string()));

        wrapper
            .call_tool_with_meta("my_tool", serde_json::json!({ "a": 1 }), Some(meta.clone()))
            .await
            .unwrap();

        let calls = recording.calls();
        assert_eq!(calls.len(), 1);
        // Routed to the configured server name.
        assert_eq!(calls[0].server, "higress");
        assert_eq!(calls[0].tool_name, "my_tool");
        // Untouched: same keys and values reach the transport.
        assert_eq!(calls[0].meta, Some(meta));
    }

    #[tokio::test]
    async fn call_tool_without_meta_passes_none() {
        let recording = Arc::new(RecordingConnection::default());
        let wrapper = HigressMcpClientWrapper::with_transport(test_config(), recording.clone());
        wrapper.initialize().await.unwrap();

        // Two-argument overload (Java `callTool(name, args)` -> `callTool(name,
        // args, null)`) and the explicit `None` overload must both send no
        // `_meta`; Java's `testCallTool_PassesMetaStraightThrough` pins null.
        wrapper
            .call_tool("my_tool", serde_json::json!({ "a": 1 }))
            .await
            .unwrap();
        wrapper
            .call_tool_with_meta("my_tool", serde_json::json!({ "a": 1 }), None)
            .await
            .unwrap();

        let calls = recording.calls();
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|call| call.meta.is_none()));
    }

    #[tokio::test]
    async fn search_tools_single_argument_omits_top_k() {
        let recording = Arc::new(RecordingConnection::default());
        *recording.call_response.lock().unwrap() = Some(tool_search_response());
        let wrapper = HigressMcpClientWrapper::with_transport(
            tool_search_config("weather", 7),
            recording.clone(),
        );

        let result = wrapper.search_tools("weather").await.unwrap();

        assert!(result.is_success());
        assert_eq!(result.tool_names(), vec!["map___maps_weather"]);
        let arguments = recording.calls()[0].arguments.as_object().unwrap().clone();
        assert_eq!(
            arguments.get("query"),
            Some(&Value::String("weather".into()))
        );
        // Java `searchTools(String query)` sends only `{"query": q}`
        // (`HigressMcpClientWrapper.java:317-319`).
        assert!(!arguments.contains_key("topK"));
    }

    #[test]
    fn convert_to_mcp_tool_forwards_output_schema() {
        let info = HigressToolInfo {
            name: "n".into(),
            description: None,
            title: None,
            input_schema: None,
            output_schema: Some(serde_json::json!({
                "type": "object",
                "properties": { "result": { "type": "string" } },
                "required": ["result"]
            })),
        };

        let tool = convert_to_mcp_tool(&info);

        let output = tool.output_schema.unwrap();
        assert_eq!(output.required, vec!["result".to_string()]);
        assert!(output.properties.unwrap().contains_key("result"));
    }

    #[test]
    fn convert_to_mcp_tool_drops_non_object_output_schema() {
        // Rust's typed `ToolOutputSchema` can only describe object results; an
        // array schema must not be reported as an object shape.
        let info = HigressToolInfo {
            name: "n".into(),
            description: None,
            title: None,
            input_schema: None,
            output_schema: Some(serde_json::json!({ "type": "array" })),
        };

        let tool = convert_to_mcp_tool(&info);

        assert!(tool.output_schema.is_none());
    }

    #[test]
    fn convert_to_mcp_tool_handles_typeless_object_output_schema() {
        let info = HigressToolInfo {
            name: "n".into(),
            description: None,
            title: None,
            input_schema: None,
            output_schema: Some(serde_json::json!({
                "properties": { "ok": { "type": "boolean" } }
            })),
        };

        let tool = convert_to_mcp_tool(&info);

        let output = tool.output_schema.unwrap();
        assert!(output.properties.unwrap().contains_key("ok"));
        assert!(output.required.is_empty());
    }
}
