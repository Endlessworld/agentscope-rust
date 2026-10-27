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
use agentscope_harness::agent::{McpClientManager, McpServerConfig, McpToolInfo};
use async_trait::async_trait;
use rust_mcp_sdk::schema::{CallToolResult, TextContent, Tool, ToolInputSchema};
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
}

impl HigressClientConfig {
    /// Builds the endpoint with any configured query parameters appended.
    ///
    /// Parameters are merged with whatever the endpoint already carries,
    /// matching the Java builder's documented behaviour.
    pub fn endpoint_with_query(&self) -> String {
        if self.query_params.is_empty() {
            return self.endpoint.clone();
        }
        let separator = if self.endpoint.contains('?') {
            '&'
        } else {
            '?'
        };
        let query = self
            .query_params
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("&");
        format!("{}{}{}", self.endpoint, separator, query)
    }

    /// Projects this configuration into a harness [`McpServerConfig`] so the
    /// caller can establish a real connection through [`McpClientManager`].
    ///
    /// The harness selects the concrete transport from the URL (SSE when the
    /// URL contains `/sse`, Streamable HTTP otherwise). `transport_type` is
    /// validated against that routing rule in
    /// [`HigressMcpClientWrapper::initialize`], so a misconfigured transport is
    /// reported instead of being silently ignored.
    pub fn to_server_config(&self) -> McpServerConfig {
        McpServerConfig::new(self.client_name.clone())
            .url(self.endpoint_with_query())
            .headers(self.headers.clone())
    }
}

/// Everything the wrapper needs to hand to the connection layer when it
/// establishes the real MCP connection.
///
/// [`McpServerConfig`] cannot carry the transport choice or the two timeouts,
/// so they travel alongside it. Keeping them in one value makes it possible to
/// assert (in tests and in diagnostics) that no configured knob is dropped.
#[derive(Debug, Clone)]
pub(crate) struct McpConnectRequest {
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
/// exists only as a seam for the Higress wrapper: it is implemented for the real
/// manager below and by an in-test recorder. It deliberately covers nothing but
/// the operations the wrapper uses — it is not a parallel MCP abstraction.
#[async_trait]
pub(crate) trait McpConnection: Send + Sync {
    /// Creates and connects a client for `request`, propagating any failure.
    async fn connect_server(&self, request: McpConnectRequest) -> Result<()>;
    /// Returns the connected client id for `server`, if any.
    async fn find_client_id_by_server(&self, server: &str) -> Option<String>;
    /// Lists the tools discovered for `client_id`.
    async fn list_tools(&self, client_id: &str) -> Result<Vec<McpToolInfo>>;
    /// Calls a tool on the connected client for `server`.
    async fn call_tool_by_server(
        &self,
        server: &str,
        tool_name: &str,
        arguments: Value,
    ) -> Result<Value>;
    /// Disconnects the client for `server`, if one is connected.
    async fn disconnect_by_server(&self, server: &str) -> Result<()>;
}

#[async_trait]
impl McpConnection for McpClientManager {
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
    ) -> Result<Value> {
        McpClientManager::call_tool_by_server(self, server, tool_name, arguments).await
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
    connection: Arc<dyn McpConnection>,
    /// Server projection handed to the connection layer on `initialize`.
    server_config: McpServerConfig,
    transport_type: TransportType,
    timeout: Duration,
    initialization_timeout: Duration,
    enable_tool_search: bool,
    tool_search_query: Option<String>,
    tool_search_top_k: usize,
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
        Self::with_connection(config, manager)
    }

    /// Creates a wrapper backed by an arbitrary [`McpConnection`].
    fn with_connection(config: HigressClientConfig, connection: Arc<dyn McpConnection>) -> Self {
        let server_config = config.to_server_config();
        Self {
            name: config.client_name,
            connection,
            server_config,
            transport_type: config.transport_type,
            timeout: config.timeout,
            initialization_timeout: config.initialization_timeout,
            enable_tool_search: config.enable_tool_search,
            tool_search_query: config.tool_search_query,
            tool_search_top_k: config.tool_search_top_k,
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

        self.validate_transport()?;

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
        let request = McpConnectRequest {
            server: self.server_config.clone(),
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

    /// Ensures the configured transport agrees with how the harness routes the
    /// URL (`/sse` in the URL selects SSE, everything else Streamable HTTP).
    ///
    /// Without this check `transport_type` would be silently ignored, because
    /// [`McpServerConfig`] has no transport field and the manager infers it.
    fn validate_transport(&self) -> Result<()> {
        let Some(url) = self.server_config.url.as_deref() else {
            return Ok(());
        };
        let routes_to_sse = url.contains("/sse");
        match (self.transport_type, routes_to_sse) {
            (TransportType::Sse, false) => Err(Error::Other(format!(
                "Higress MCP client '{}' is configured for SSE but endpoint '{}' does not \
                 contain '/sse'; the harness McpClientManager would use Streamable HTTP. \
                 Use an SSE endpoint or streamable_http_endpoint().",
                self.name, url
            ))),
            (TransportType::StreamableHttp, true) => Err(Error::Other(format!(
                "Higress MCP client '{}' is configured for Streamable HTTP but endpoint '{}' \
                 contains '/sse'; the harness McpClientManager would use SSE. Use \
                 sse_endpoint() or an endpoint without '/sse'.",
                self.name, url
            ))),
            _ => Ok(()),
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
    pub async fn call_tool(&self, tool_name: &str, arguments: Value) -> Result<CallToolResult> {
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
                    .call_tool_by_server(&self.name, tool_name, arguments),
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
    /// which omits `topK` and defaults to the configured `tool_search_top_k`.
    pub async fn search_tools(&self, query: &str) -> Result<HigressToolSearchResult> {
        self.search_tools_with_top_k(query, self.tool_search_top_k)
            .await
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

    Tool {
        name: tool_info.name.clone(),
        title: tool_info.title.clone(),
        description: tool_info.description.clone(),
        input_schema,
        output_schema: None,
        annotations: None,
        meta: None,
        execution: None,
        icons: Vec::new(),
    }
}

/// Builds a [`ToolInputSchema`] from a raw JSON schema value.
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

/// Converts a harness-discovered [`McpToolInfo`] into an MCP [`Tool`].
///
/// [`McpToolInfo`]: agentscope_harness::agent::McpToolInfo
fn mcp_tool_info_to_tool(info: &agentscope_harness::agent::McpToolInfo) -> Tool {
    let input_schema = build_input_schema(&info.parameters)
        .unwrap_or_else(|| ToolInputSchema::new(Vec::new(), None, None));
    Tool {
        name: info.name.clone(),
        title: None,
        description: info.description.clone(),
        input_schema,
        output_schema: None,
        annotations: None,
        meta: None,
        execution: None,
        icons: Vec::new(),
    }
}

/// Convenience constructor for a text content block (used by tests).
pub(crate) fn text_content(text: impl Into<String>) -> TextContent {
    TextContent::new(text.into(), None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal recording double for [`McpConnection`].
    ///
    /// The harness [`McpClientManager`] is concrete and opens real sockets, so
    /// it cannot be observed from a unit test. This double records every
    /// connect request and simulates a live server, which is exactly the seam
    /// `initialize`/`list_tools` talk to.
    #[derive(Default)]
    struct RecordingConnection {
        requests: Mutex<Vec<McpConnectRequest>>,
        connected: Mutex<HashMap<String, Vec<McpToolInfo>>>,
        tools: Vec<McpToolInfo>,
        connect_error: Mutex<Option<String>>,
        connect_delay: Mutex<Option<Duration>>,
        tool_calls: Mutex<Vec<(String, String)>>,
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
    }

    #[async_trait]
    impl McpConnection for RecordingConnection {
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
            _arguments: Value,
        ) -> Result<Value> {
            self.tool_calls
                .lock()
                .unwrap()
                .push((server.to_string(), tool_name.to_string()));
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
        let wrapper = HigressMcpClientWrapper::with_connection(test_config(), recording.clone());

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
        let wrapper = HigressMcpClientWrapper::with_connection(test_config(), recording.clone());

        wrapper.initialize().await.unwrap();
        wrapper.initialize().await.unwrap();

        assert_eq!(recording.requests().len(), 1);
    }

    #[tokio::test]
    async fn initialize_propagates_connection_failure() {
        let recording = Arc::new(RecordingConnection::default());
        *recording.connect_error.lock().unwrap() = Some("connection refused".to_string());
        let wrapper = HigressMcpClientWrapper::with_connection(test_config(), recording.clone());

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
        let wrapper = HigressMcpClientWrapper::with_connection(config, recording);

        let error = wrapper.initialize().await.unwrap_err();
        assert!(error.to_string().contains("Timed out"));
        assert!(!wrapper.is_initialized());
    }

    #[tokio::test]
    async fn initialize_rejects_transport_mismatch() {
        let recording = Arc::new(RecordingConnection::default());
        let config = HigressMcpClientBuilder::create("higress")
            .unwrap()
            // SSE selected but the URL has no `/sse`, so the manager would
            // silently route it to Streamable HTTP.
            .sse_endpoint("https://gw.example.com/mcp")
            .build_config()
            .unwrap();
        let wrapper = HigressMcpClientWrapper::with_connection(config, recording.clone());

        let error = wrapper.initialize().await.unwrap_err();
        assert!(error.to_string().contains("configured for SSE"));
        assert!(recording.requests().is_empty());
    }

    #[tokio::test]
    async fn list_tools_after_initialize_does_not_report_missing_client() {
        let recording = Arc::new(RecordingConnection::with_tools(vec![
            test_tool("echo"),
            test_tool("add"),
        ]));
        let wrapper = HigressMcpClientWrapper::with_connection(test_config(), recording);

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
        let wrapper = HigressMcpClientWrapper::with_connection(test_config(), recording.clone());
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
}
