//! Higress-aware toolkit.
//!
//! Mirrors `io.agentscope.extensions.higress.HigressToolkit`, which extends the
//! core `Toolkit` to cache the registered Higress MCP client for later access.
//!
//! Tool registration, grouping and preset parameters are already provided by the
//! core [`Toolkit`](agentscope_core::tool::Toolkit) (including its fluent
//! `ToolRegistration`); this crate adds only what core has no equivalent for:
//!
//! * a [`McpToolSource`] port (the harness one, shared with the other extension
//!   crates) so *any* MCP client can be registered, while only Higress clients
//!   are cached (Java `instanceof HigressMcpClientWrapper`),
//! * `enableTools` / `disableTools` filtering and `toolNamePrefix` renaming
//!   (core `ToolRegistration` has neither),
//! * per-registration metadata propagation, reusing the harness
//!   [`McpMetaPropagationOptions`] option set and core
//!   [`McpTool::with_propagate_meta`].
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agentscope_core::error::{Error, Result};
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::tool::{McpAsyncHandler, McpTool, Tool, Toolkit, ToolkitConfig};
use agentscope_harness::agent::{convert_call_tool_result, McpMetaPropagationOptions};
use async_trait::async_trait;
use rust_mcp_sdk::schema::{CallToolResult, Tool as McpSchemaTool};
use serde_json::Value;

use crate::client::HigressMcpClientWrapper;

/// The MCP tool-source port a [`HigressToolkit`] registers, **re-exported from**
/// the harness (HIGRESS-07).
///
/// Mirrors Java's `McpClientWrapper` base class: `Toolkit.registerMcpClient` is
/// declared against the base type, so any MCP client can be registered. The
/// port now lives in `agentscope_harness::agent` and is shared by every
/// extension crate; the harness also ships [`McpClientSource`], an adapter for
/// one server on an [`McpClientManager`].
///
/// Only Higress clients are cached for later access (Java
/// `mcpClientWrapper instanceof HigressMcpClientWrapper`). The shared port has
/// no `Any` supertrait to downcast through, so the equivalent is an explicit
/// typed entry point: [`HigressToolkit::register_higress_client`] /
/// [`McpClientRegistration::higress_client`].
///
/// [`McpClientSource`]: agentscope_harness::agent::McpClientSource
/// [`McpClientManager`]: agentscope_harness::agent::McpClientManager
pub use agentscope_harness::agent::McpToolSource;

/// [`McpToolSource`] implementation for the Higress wrapper.
///
/// Delegates to the wrapper's own methods, which is what
/// [`McpClientRegistration::apply`] drives; Higress-specific behaviour (tool
/// search, caching, connection-level metadata switch) therefore stays in the
/// wrapper instead of leaking into the shared port.
#[async_trait]
impl McpToolSource for HigressMcpClientWrapper {
    fn name(&self) -> &str {
        HigressMcpClientWrapper::name(self)
    }

    async fn initialize(&self) -> Result<()> {
        HigressMcpClientWrapper::initialize(self).await
    }

    async fn list_tools(&self) -> Result<Vec<McpSchemaTool>> {
        HigressMcpClientWrapper::list_tools(self).await
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
        meta: Option<JsonMap>,
    ) -> Result<CallToolResult> {
        HigressMcpClientWrapper::call_tool_with_meta(self, tool_name, arguments, meta).await
    }
}

/// A [`Toolkit`] that additionally caches the most recently registered
/// [`HigressMcpClientWrapper`].
///
/// Mirrors `io.agentscope.extensions.higress.HigressToolkit`.
#[derive(Clone, Default)]
pub struct HigressToolkit {
    toolkit: Toolkit,
    higress_client: Arc<Mutex<Option<Arc<HigressMcpClientWrapper>>>>,
}

impl std::fmt::Debug for HigressToolkit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HigressToolkit")
            .field("tools", &self.toolkit.tool_names())
            .field(
                "has_higress_client",
                &self.get_higress_mcp_client().is_some(),
            )
            .finish()
    }
}

impl HigressToolkit {
    /// Creates a new Higress toolkit with default configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a new Higress toolkit with custom configuration.
    pub fn with_config(config: ToolkitConfig) -> Self {
        Self {
            toolkit: Toolkit::with_config(config),
            higress_client: Arc::new(Mutex::new(None)),
        }
    }

    /// Returns the underlying core toolkit.
    pub fn toolkit(&self) -> &Toolkit {
        &self.toolkit
    }

    /// Consumes the wrapper and returns the underlying core toolkit.
    pub fn into_toolkit(self) -> Toolkit {
        self.toolkit
    }

    /// Starts a fluent registration for an MCP client.
    ///
    /// Mirrors Java `Toolkit.registration()` combined with `.mcpClient(client)`:
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use agentscope_extensions_higress::{HigressToolkit, HigressMcpClientWrapper};
    /// # async fn demo(toolkit: &HigressToolkit, client: Arc<HigressMcpClientWrapper>) -> agentscope_core::error::Result<()> {
    /// toolkit
    ///     .registration()
    ///     .mcp_client(client)
    ///     .enable_tools(["tool1", "tool2"])
    ///     .group("myGroup")
    ///     .apply()
    ///     .await?;
    /// # Ok(()) }
    /// ```
    pub fn registration(&self) -> McpClientRegistration<'_> {
        McpClientRegistration::new(self)
    }

    /// Registers any MCP client with the toolkit using default options.
    ///
    /// Nothing is cached: the shared [`McpToolSource`] port carries no `Any`
    /// supertrait, so Java's `instanceof HigressMcpClientWrapper` cannot be
    /// expressed here. Use
    /// [`register_higress_client`](Self::register_higress_client) to register a
    /// [`HigressMcpClientWrapper`] and make it retrievable through
    /// [`get_higress_mcp_client`](Self::get_higress_mcp_client).
    pub async fn register_mcp_client(&self, mcp_client: Arc<dyn McpToolSource>) -> Result<usize> {
        self.registration().mcp_client(mcp_client).apply().await
    }

    /// Registers a Higress client and caches it once registration succeeded.
    ///
    /// Mirrors the Java `registerMcpClient` override (which caches only after
    /// `super.registerMcpClient(...)` succeeds).
    pub async fn register_higress_client(
        &self,
        client: Arc<HigressMcpClientWrapper>,
    ) -> Result<usize> {
        self.registration().higress_client(client).apply().await
    }

    /// Registers a tool directly (delegates to the core toolkit).
    pub fn register_tool(&self, tool: Arc<dyn Tool>) {
        self.toolkit.register_tool(tool);
    }

    /// The names of all registered tools.
    pub fn tool_names(&self) -> Vec<String> {
        self.toolkit.tool_names()
    }

    /// Retrieves a registered tool by name.
    pub fn get_tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.toolkit.get_tool(name)
    }

    /// Returns the most recently registered Higress MCP client, if any.
    pub fn get_higress_mcp_client(&self) -> Option<Arc<HigressMcpClientWrapper>> {
        self.higress_client.lock().unwrap().clone()
    }
}

/// Fluent registration of one MCP client, mirroring Java
/// `Toolkit.ToolRegistration` for the `.mcpClient(...)` branch.
///
/// Grouping, preset parameters and the MCP-client tag are delegated to core
/// `ToolRegistration`; filtering and prefixing have no core equivalent and are
/// applied here before the tools reach the toolkit (see the module docs).
pub struct McpClientRegistration<'a> {
    toolkit: &'a HigressToolkit,
    client: Option<Arc<dyn McpToolSource>>,
    /// Set by [`higress_client`](Self::higress_client): the typed handle cached
    /// after a successful registration.
    higress_client: Option<Arc<HigressMcpClientWrapper>>,
    enable_tools: Option<Vec<String>>,
    disable_tools: Option<Vec<String>>,
    group: Option<String>,
    tool_name_prefix: String,
    preset_parameters: HashMap<String, JsonMap>,
    meta: McpMetaPropagationOptions,
}

impl<'a> McpClientRegistration<'a> {
    fn new(toolkit: &'a HigressToolkit) -> Self {
        Self {
            toolkit,
            client: None,
            higress_client: None,
            enable_tools: None,
            disable_tools: None,
            group: None,
            tool_name_prefix: String::new(),
            preset_parameters: HashMap::new(),
            meta: McpMetaPropagationOptions::new(),
        }
    }

    /// Sets the MCP client to register.
    ///
    /// Overrides any Higress handle set by [`higress_client`](Self::higress_client),
    /// so the raw input schemas used during registration always come from the
    /// client that is actually registered.
    pub fn mcp_client(mut self, client: Arc<dyn McpToolSource>) -> Self {
        self.client = Some(client);
        self.higress_client = None;
        self
    }

    /// Sets a Higress client to register and to cache after a successful
    /// registration.
    ///
    /// The Rust equivalent of Java's `instanceof HigressMcpClientWrapper`
    /// guard: the typed argument is what makes the caching decision, because the
    /// shared [`McpToolSource`] port cannot be downcast.
    pub fn higress_client(mut self, client: Arc<HigressMcpClientWrapper>) -> Self {
        self.higress_client = Some(Arc::clone(&client));
        self.client = Some(client);
        self
    }

    /// Only registers the listed tools (an empty list means "all tools").
    ///
    /// Mirrors Java `enableTools`: it takes precedence over
    /// [`disable_tools`](Self::disable_tools).
    pub fn enable_tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.enable_tools = Some(tools.into_iter().map(Into::into).collect());
        self
    }

    /// Skips the listed tools (an empty list means "disable none").
    pub fn disable_tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.disable_tools = Some(tools.into_iter().map(Into::into).collect());
        self
    }

    /// Assigns every registered tool to `group`.
    ///
    /// Follows core `ToolRegistration.group` semantics: the group is created when
    /// it does not exist yet.
    pub fn group(mut self, group: impl Into<String>) -> Self {
        self.group = Some(group.into());
        self
    }

    /// Prefixes the model-facing tool names (Java `mcpToolNamePrefix`).
    ///
    /// The gateway still receives the original remote tool name.
    pub fn tool_name_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.tool_name_prefix = prefix.into();
        self
    }

    /// Sets preset parameters for several tools at once, keyed by the **remote**
    /// tool name (before any prefix) — the same key Java uses.
    pub fn preset_parameters(mut self, presets: HashMap<String, JsonMap>) -> Self {
        self.preset_parameters.extend(presets);
        self
    }

    /// Sets preset parameters for one tool, keyed by the **remote** tool name.
    pub fn preset_parameters_for(mut self, tool_name: impl Into<String>, presets: JsonMap) -> Self {
        self.preset_parameters.insert(tool_name.into(), presets);
        self
    }

    /// Registration-level default for metadata propagation (`true` when unset).
    pub fn propagate_meta(mut self, propagate_meta: bool) -> Self {
        self.meta = self.meta.propagate_meta(propagate_meta);
        self
    }

    /// Per-tool metadata-propagation override, keyed by the **remote** tool name.
    ///
    /// An override that names no registered tool fails the whole registration
    /// (Java `McpClientManager`), so a silencing override is never lost silently.
    pub fn propagate_meta_for(
        mut self,
        tool_name: impl Into<String>,
        propagate_meta: bool,
    ) -> Result<Self> {
        self.meta = self.meta.propagate_meta_for(tool_name, propagate_meta)?;
        Ok(self)
    }

    /// Registers the client's tools and returns how many were registered.
    ///
    /// Mirrors Java `ToolRegistration.apply()` for the `.mcpClient(...)` branch:
    /// initialize, list tools, filter, then register each tool with the group,
    /// preset parameters and MCP-client name.
    pub async fn apply(self) -> Result<usize> {
        let Self {
            toolkit,
            client,
            higress_client,
            enable_tools,
            disable_tools,
            group,
            tool_name_prefix,
            preset_parameters,
            meta,
        } = self;
        let client = client.ok_or_else(|| {
            Error::Other(
                "Must call mcp_client(...) before apply() for a Higress registration".to_string(),
            )
        })?;

        client.initialize().await?;
        // HIGRESS-01: the shared port returns rust-mcp-sdk's closed
        // `ToolInputSchema`, which cannot carry root JSON-Schema keywords or
        // boolean sub-schemas. A Higress client also hands back the gateway's raw
        // schema, so those reach the model (Java copies the raw map verbatim).
        let specs: Vec<(McpSchemaTool, Value)> = match &higress_client {
            Some(higress) => higress.list_tool_specs().await?,
            None => client
                .list_tools()
                .await?
                .into_iter()
                .map(|tool| (tool, Value::Null))
                .collect(),
        };
        let selected: Vec<(McpSchemaTool, Value)> = specs
            .into_iter()
            .filter(|(tool, _)| {
                should_register_tool(
                    &tool.name,
                    enable_tools.as_deref(),
                    disable_tools.as_deref(),
                )
            })
            .collect();

        // Fail loud on a metadata override that matches no registered tool.
        if !meta.tool_overrides.is_empty() {
            let unknown: Vec<&str> = {
                let mut names: Vec<&str> = meta
                    .tool_overrides
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .filter(|name| !selected.iter().any(|(tool, _)| tool.name == *name))
                    .collect();
                names.sort_unstable();
                names.dedup();
                names
            };
            if !unknown.is_empty() {
                return Err(Error::Other(format!(
                    "Unknown MCP tool(s) in propagateMeta override for client '{}': [{}]",
                    client.name(),
                    unknown.join(", ")
                )));
            }
        }

        let mut registered = 0usize;
        for (tool, raw_schema) in selected {
            let remote_name = tool.name.clone();
            let registered_name = format!("{}{}", tool_name_prefix, remote_name);
            let presets = preset_parameters
                .get(&remote_name)
                .cloned()
                .unwrap_or_default();
            let propagate_meta = resolve_propagate_meta(&meta, &remote_name);

            let parameters = tool_parameters(&tool, &raw_schema, &presets);
            let handler_client = Arc::clone(&client);
            let handler_tool = remote_name.clone();
            let async_handler: McpAsyncHandler = Box::new(move |_tool, args, meta| {
                let client = Arc::clone(&handler_client);
                let tool_name = handler_tool.clone();
                Box::pin(async move {
                    let result = client
                        .call_tool(&tool_name, Value::Object(args), meta)
                        .await?;
                    Ok(render_call_result(&tool_name, &result))
                })
            });

            let mut mcp_tool = McpTool::new(
                client.name(),
                registered_name,
                tool.description.clone().unwrap_or_default(),
                parameters,
                Box::new(|tool: &McpTool, _args: JsonMap| {
                    Ok(ToolResultBlock::error(format!(
                        "MCP tool '{}' has no async handler",
                        tool.name()
                    )))
                }),
            )
            .with_async_handler(async_handler)
            .with_propagate_meta(propagate_meta);
            if let Some(title) = &tool.title {
                mcp_tool = mcp_tool.with_title(title.clone());
            }

            let mut registration = toolkit
                .toolkit
                .register()
                .tool(Arc::new(mcp_tool))
                .mcp_client(client.name().to_string());
            if let Some(group) = &group {
                registration = registration.group(group.clone());
            }
            if !presets.is_empty() {
                registration = registration.preset_parameters(presets);
            }
            registration.apply()?;
            registered += 1;
        }

        // Java caches only Higress clients (`instanceof`), and only after a
        // successful registration. The shared port cannot be downcast, so the
        // caller opts in through `higress_client(...)`.
        if let Some(higress) = higress_client {
            *toolkit.higress_client.lock().unwrap() = Some(higress);
            tracing::info!(
                "Higress MCP client '{}' registered successfully",
                client.name()
            );
        }

        Ok(registered)
    }
}

/// Resolves the effective metadata-propagation switch for one remote tool.
///
/// Same precedence as the harness helper and Java: per-tool override >
/// registration-level default > `true`.
fn resolve_propagate_meta(meta: &McpMetaPropagationOptions, remote_name: &str) -> bool {
    meta.tool_overrides
        .iter()
        .rev()
        .find(|(name, _)| name == remote_name)
        .map(|(_, value)| *value)
        .or(meta.propagate_meta)
        .unwrap_or(true)
}

/// Decides whether `tool_name` should be registered.
///
/// Mirrors Java `McpClientManager.shouldRegisterTool`: `enableTools` takes
/// precedence over `disableTools`, and an empty/absent list means "no
/// restriction".
fn should_register_tool(
    tool_name: &str,
    enable_tools: Option<&[String]>,
    disable_tools: Option<&[String]>,
) -> bool {
    let mut result = true;
    if let Some(disable) = disable_tools {
        if !disable.is_empty() {
            result = !disable.iter().any(|name| name == tool_name);
        }
    }
    if let Some(enable) = enable_tools {
        if !enable.is_empty() {
            result = enable.iter().any(|name| name == tool_name);
        }
    }
    result
}

/// Projects a tool's input schema into the JSON parameter schema the model sees,
/// hiding the keys supplied through `presets`.
///
/// Mirrors Java `McpTool.convertMcpSchemaToParameters`: the raw schema is copied
/// wholesale, only the preset keys are removed, and the object keywords are then
/// normalized. Copying is what keeps root keywords (`$defs`,
/// `additionalProperties`, `$schema`) and boolean sub-schemas alive (HIGRESS-01);
/// core's `ToolRegistration` merges presets at call time only.
fn tool_parameters(tool: &McpSchemaTool, raw_schema: &Value, presets: &JsonMap) -> Value {
    let mut schema = raw_schema.as_object().cloned().unwrap_or_default();
    // A source that cannot hand back a raw schema (the shared port's closed
    // `ToolInputSchema`) still carries `properties`/`required` (HIGRESS-X08).
    let mut properties = match schema.get("properties").and_then(Value::as_object) {
        Some(props) => props.clone(),
        None => tool
            .input_schema
            .properties
            .iter()
            .flatten()
            .map(|(key, value)| (key.clone(), Value::Object(value.clone())))
            .collect(),
    };
    let mut required = match schema.get("required").and_then(Value::as_array) {
        Some(list) => list
            .iter()
            .filter_map(|value| value.as_str().map(String::from))
            .collect::<Vec<_>>(),
        None => tool.input_schema.required.clone(),
    };
    properties.retain(|key, _| !presets.contains_key(key));
    required.retain(|name| !presets.contains_key(name));

    let type_name = match schema.get("type").and_then(Value::as_str) {
        Some(name) if !name.trim().is_empty() => name.to_string(),
        _ => "object".to_string(),
    };
    schema.insert("type".to_string(), Value::String(type_name));
    schema.insert("properties".to_string(), Value::Object(properties));
    schema.insert(
        "required".to_string(),
        Value::Array(required.into_iter().map(Value::String).collect()),
    );
    Value::Object(schema)
}

/// Renders an MCP [`CallToolResult`] as a [`ToolResultBlock`].
///
/// Delegates to the harness converter (HIGRESS-03), which keeps non-text content
/// in its richest core form instead of `Debug`-formatting it: images and audio
/// become base64 content blocks, embedded resources are unwrapped (text bodies
/// kept, binary bodies rendered as `image`/`audio` or a short description),
/// resource links render as `name` + `uri` text, and an empty `content` falls
/// back to `structuredContent`. `isError` semantics are unchanged — the text
/// still reaches the model, the block is just marked as an error.
fn render_call_result(tool_name: &str, result: &CallToolResult) -> ToolResultBlock {
    convert_call_tool_result(tool_name, result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::{JsonMap, ToolUseBlock};
    use agentscope_core::tool::ToolExecutionContext;
    use rust_mcp_sdk::schema::ToolInputSchema;
    use std::collections::BTreeMap;
    use std::sync::Mutex as StdMutex;

    /// An MCP tool as the gateway would advertise it.
    fn schema_tool(name: &str) -> McpSchemaTool {
        let mut properties = BTreeMap::new();
        let mut city = serde_json::Map::new();
        city.insert("type".to_string(), Value::String("string".to_string()));
        properties.insert("city".to_string(), city);
        let mut token = serde_json::Map::new();
        token.insert("type".to_string(), Value::String("string".to_string()));
        properties.insert("token".to_string(), token);
        McpSchemaTool {
            name: name.to_string(),
            title: Some(format!("{name} title")),
            description: Some(format!("{name} description")),
            input_schema: ToolInputSchema::new(
                vec!["city".to_string(), "token".to_string()],
                Some(properties),
                None,
            ),
            output_schema: None,
            annotations: None,
            meta: None,
            execution: None,
            icons: Vec::new(),
        }
    }

    /// One recorded [`McpToolSource::call_tool`] invocation.
    #[derive(Debug, Clone)]
    struct RecordedCall {
        tool_name: String,
        arguments: Value,
        meta: Option<JsonMap>,
    }

    /// A non-Higress [`McpToolSource`]: mirrors Java
    /// `HigressToolkitTest#testRegisterMcpClient_NonHigressClient`, a plain
    /// `McpClientWrapper` that must be registrable but never cached.
    #[derive(Default)]
    struct FakeSource {
        name: String,
        tools: Vec<McpSchemaTool>,
        initialized: StdMutex<bool>,
        calls: StdMutex<Vec<RecordedCall>>,
        call_error: StdMutex<Option<String>>,
    }

    impl FakeSource {
        fn new(name: &str, tools: Vec<McpSchemaTool>) -> Self {
            Self {
                name: name.to_string(),
                tools,
                ..Self::default()
            }
        }

        fn calls(&self) -> Vec<RecordedCall> {
            self.calls.lock().unwrap().clone()
        }

        fn was_initialized(&self) -> bool {
            *self.initialized.lock().unwrap()
        }
    }

    #[async_trait]
    impl McpToolSource for FakeSource {
        fn name(&self) -> &str {
            &self.name
        }

        async fn initialize(&self) -> Result<()> {
            *self.initialized.lock().unwrap() = true;
            Ok(())
        }

        async fn list_tools(&self) -> Result<Vec<McpSchemaTool>> {
            Ok(self.tools.clone())
        }

        async fn call_tool(
            &self,
            tool_name: &str,
            arguments: Value,
            meta: Option<JsonMap>,
        ) -> Result<CallToolResult> {
            self.calls.lock().unwrap().push(RecordedCall {
                tool_name: tool_name.to_string(),
                arguments,
                meta,
            });
            if let Some(error) = self.call_error.lock().unwrap().clone() {
                return Err(Error::Other(error));
            }
            Ok(serde_json::from_value(serde_json::json!({
                "content": [{ "type": "text", "text": "ok" }],
                "isError": false
            }))
            .unwrap())
        }
    }

    fn context_with_call(id: &str) -> ToolExecutionContext {
        ToolExecutionContext {
            tool_call: Some(ToolUseBlock {
                id: id.to_string(),
                name: "anything".to_string(),
                input: JsonMap::new(),
                content: None,
                metadata: JsonMap::new(),
                state: agentscope_core::message::ToolCallState::default(),
            }),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn register_mcp_client_registers_every_tool() {
        let source = Arc::new(FakeSource::new(
            "regular-client",
            vec![schema_tool("tool_a"), schema_tool("tool_b")],
        ));
        let toolkit = HigressToolkit::new();

        let registered = toolkit.register_mcp_client(source.clone()).await.unwrap();

        assert_eq!(registered, 2);
        // Java `registerMcpClient` initializes the client first.
        assert!(source.was_initialized());
        let mut names = toolkit.tool_names();
        names.sort();
        assert_eq!(names, vec!["tool_a".to_string(), "tool_b".to_string()]);
        // The MCP-client name is tagged on each tool (core
        // `Toolkit::set_mcp_client_name`).
        assert_eq!(
            toolkit.toolkit().mcp_client_name("tool_a").as_deref(),
            Some("regular-client")
        );
        assert!(toolkit.get_tool("tool_b").is_some());
    }

    /// HIGRESS-03: non-text MCP content must survive as structured blocks.
    ///
    /// The old renderer `Debug`-formatted everything that was not text, so an
    /// image reached the model as a Rust enum dump.
    #[test]
    fn render_call_result_keeps_image_blocks() {
        use agentscope_core::message::{ContentBlock, Source, ToolResultState};

        let result: CallToolResult = serde_json::from_value(serde_json::json!({
            "content": [
                { "type": "text", "text": "chart follows" },
                { "type": "image", "data": "aGk=", "mimeType": "image/png" }
            ],
            "isError": false
        }))
        .unwrap();

        let block = render_call_result("chart", &result);

        assert_eq!(block.name.as_deref(), Some("chart"));
        assert_eq!(block.output.len(), 2);
        assert_eq!(
            block.output[0],
            ContentBlock::Text(agentscope_core::message::TextBlock::new("chart follows"))
        );
        match &block.output[1] {
            ContentBlock::Image(image) => match &image.source {
                Source::Base64(base64) => {
                    assert_eq!(base64.media_type, "image/png");
                    assert_eq!(base64.data, "aGk=");
                }
                other => panic!("expected a base64 source, got {other:?}"),
            },
            other => panic!("expected an image block, got {other:?}"),
        }

        // `isError` keeps its old semantics: the text still reaches the model,
        // only the state changes.
        let error_result: CallToolResult = serde_json::from_value(serde_json::json!({
            "content": [{ "type": "text", "text": "boom" }],
            "isError": true
        }))
        .unwrap();
        let error = render_call_result("chart", &error_result);
        assert_eq!(error.state, ToolResultState::Error);
        match &error.output[..] {
            [ContentBlock::Text(text)] => assert!(text.text.contains("boom")),
            other => panic!("expected one error text block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_higress_client_is_not_cached() {
        let source = Arc::new(FakeSource::new(
            "regular-client",
            vec![schema_tool("tool_a")],
        ));
        let toolkit = HigressToolkit::new();

        toolkit.register_mcp_client(source).await.unwrap();

        // Java `testRegisterMcpClient_NonHigressClient`: caching is guarded by
        // `instanceof HigressMcpClientWrapper`.
        assert!(toolkit.get_higress_mcp_client().is_none());
    }

    #[tokio::test]
    async fn registration_filters_with_enable_and_disable_tools() {
        let source = Arc::new(FakeSource::new(
            "regular-client",
            vec![
                schema_tool("tool_a"),
                schema_tool("tool_b"),
                schema_tool("tool_c"),
            ],
        ));
        let toolkit = HigressToolkit::new();

        let registered = toolkit
            .registration()
            .mcp_client(source.clone())
            .enable_tools(["tool_a", "tool_b"])
            .apply()
            .await
            .unwrap();

        assert_eq!(registered, 2);
        let mut names = toolkit.tool_names();
        names.sort();
        assert_eq!(names, vec!["tool_a".to_string(), "tool_b".to_string()]);

        // `enableTools` wins over `disableTools` (Java
        // `McpClientManager.shouldRegisterTool`).
        let disabled = toolkit
            .registration()
            .mcp_client(source.clone())
            .enable_tools(["tool_a", "tool_b"])
            .disable_tools(["tool_a"])
            .apply()
            .await
            .unwrap();
        assert_eq!(disabled, 2);
        assert!(toolkit.tool_names().contains(&"tool_a".to_string()));

        // An empty enable list means "all".
        let toolkit = HigressToolkit::new();
        let all = toolkit
            .registration()
            .mcp_client(source)
            .disable_tools(["tool_c"])
            .apply()
            .await
            .unwrap();
        assert_eq!(all, 2);
        assert!(!toolkit.tool_names().contains(&"tool_c".to_string()));
    }

    #[tokio::test]
    async fn registration_applies_prefix_group_and_presets() {
        let source = Arc::new(FakeSource::new(
            "regular-client",
            vec![schema_tool("tool_a")],
        ));
        let toolkit = HigressToolkit::new();
        let mut presets = JsonMap::new();
        presets.insert("token".to_string(), Value::String("secret".to_string()));

        toolkit
            .registration()
            .mcp_client(source.clone())
            .tool_name_prefix("higress__")
            .group("higress-tools")
            .preset_parameters_for("tool_a", presets)
            .apply()
            .await
            .unwrap();

        // Model-facing name is prefixed, the gateway still gets the remote name.
        assert_eq!(toolkit.tool_names(), vec!["higress__tool_a".to_string()]);
        let tool = toolkit.get_tool("higress__tool_a").unwrap();
        // Preset parameters are hidden from the model-facing schema but still
        // present on the toolkit for call-time merging.
        let schema = tool.parameters_schema();
        assert!(schema["properties"].get("token").is_none());
        assert!(schema["properties"].get("city").is_some());
        assert_eq!(schema["required"], serde_json::json!(["city"]));
        assert_eq!(
            toolkit.toolkit().preset_parameters("higress__tool_a")["token"],
            Value::String("secret".to_string())
        );

        let group = toolkit.toolkit().get_tool_group("higress-tools").unwrap();
        assert!(group.tools().contains("higress__tool_a"));

        tool.call(
            serde_json::json!({ "city": "beijing" })
                .as_object()
                .unwrap()
                .clone(),
            ToolExecutionContext::default(),
        )
        .await
        .unwrap();
        let calls = source.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool_name, "tool_a");
        assert_eq!(calls[0].arguments["city"], "beijing");
    }

    #[tokio::test]
    async fn registered_tool_propagates_tool_call_metadata() {
        let source = Arc::new(FakeSource::new(
            "regular-client",
            vec![schema_tool("tool_a")],
        ));
        let toolkit = HigressToolkit::new();
        toolkit.register_mcp_client(source.clone()).await.unwrap();

        toolkit
            .get_tool("tool_a")
            .unwrap()
            .call(JsonMap::new(), context_with_call("call-1"))
            .await
            .unwrap();

        let calls = source.calls();
        assert_eq!(
            calls[0]
                .meta
                .as_ref()
                .and_then(|m| m.get("io.agentscope/toolCallId")),
            Some(&Value::String("call-1".to_string()))
        );
    }

    #[tokio::test]
    async fn propagate_meta_false_omits_metadata() {
        let source = Arc::new(FakeSource::new(
            "regular-client",
            vec![schema_tool("tool_a")],
        ));
        let toolkit = HigressToolkit::new();
        toolkit
            .registration()
            .mcp_client(source.clone())
            .propagate_meta(false)
            .apply()
            .await
            .unwrap();

        toolkit
            .get_tool("tool_a")
            .unwrap()
            .call(JsonMap::new(), context_with_call("call-1"))
            .await
            .unwrap();

        // `None` (not an empty map) so the transport omits `_meta` entirely.
        assert!(source.calls()[0].meta.is_none());
    }

    #[tokio::test]
    async fn propagate_meta_per_tool_override_rejects_unknown_tool() {
        let source = Arc::new(FakeSource::new(
            "regular-client",
            vec![schema_tool("tool_a")],
        ));
        let toolkit = HigressToolkit::new();

        let error = toolkit
            .registration()
            .mcp_client(source.clone())
            .propagate_meta_for("tool_a", false)
            .unwrap()
            .propagate_meta_for("tool_typo", false)
            .unwrap()
            .apply()
            .await
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("Unknown MCP tool(s) in propagateMeta override"));
        assert!(error.to_string().contains("tool_typo"));
        // Nothing is registered when the registration fails.
        assert!(toolkit.tool_names().is_empty());
    }

    #[tokio::test]
    async fn propagate_meta_per_tool_override_applies_to_that_tool_only() {
        let source = Arc::new(FakeSource::new(
            "regular-client",
            vec![schema_tool("tool_a"), schema_tool("tool_b")],
        ));
        let toolkit = HigressToolkit::new();
        toolkit
            .registration()
            .mcp_client(source.clone())
            .propagate_meta_for("tool_a", false)
            .unwrap()
            .apply()
            .await
            .unwrap();

        toolkit
            .get_tool("tool_a")
            .unwrap()
            .call(JsonMap::new(), context_with_call("call-1"))
            .await
            .unwrap();
        toolkit
            .get_tool("tool_b")
            .unwrap()
            .call(JsonMap::new(), context_with_call("call-2"))
            .await
            .unwrap();

        let calls = source.calls();
        assert!(calls[0].meta.is_none());
        assert!(calls[1].meta.is_some());
    }

    #[tokio::test]
    async fn apply_without_client_fails() {
        let toolkit = HigressToolkit::new();
        let error = toolkit.registration().apply().await.unwrap_err();
        assert!(error
            .to_string()
            .contains("Must call mcp_client(...) before apply()"));
    }

    #[tokio::test]
    async fn higress_client_is_cached_after_registration() {
        use crate::client::HigressMcpTransport;
        use crate::client::{HigressMcpClientBuilder, HigressMcpClientWrapper};

        /// Transport double that serves a fixed tool list.
        #[derive(Default)]
        struct FixedTransport {
            tools: Vec<agentscope_harness::agent::McpToolInfo>,
        }

        #[async_trait]
        impl HigressMcpTransport for FixedTransport {
            async fn connect_server(
                &self,
                _request: crate::client::McpConnectRequest,
            ) -> Result<()> {
                Ok(())
            }
            async fn find_client_id_by_server(&self, server: &str) -> Option<String> {
                Some(server.to_string())
            }
            async fn connected_server_url(&self, _server: &str) -> Option<String> {
                // The double models no endpoint, so the reuse guard (HIGRESS-04)
                // has nothing to compare against.
                None
            }
            async fn list_tools(
                &self,
                _client_id: &str,
            ) -> Result<Vec<agentscope_harness::agent::McpToolInfo>> {
                Ok(self.tools.clone())
            }
            async fn call_tool_by_server(
                &self,
                _server: &str,
                _tool_name: &str,
                _arguments: Value,
                _meta: Option<JsonMap>,
            ) -> Result<Value> {
                Ok(serde_json::json!({ "content": [], "isError": false }))
            }
            async fn disconnect_by_server(&self, _server: &str) -> Result<()> {
                Ok(())
            }
        }

        let transport = Arc::new(FixedTransport {
            tools: vec![agentscope_harness::agent::McpToolInfo {
                name: "weather".to_string(),
                description: Some("Weather".to_string()),
                parameters: serde_json::json!({ "type": "object", "properties": {} }),
                server: "higress".to_string(),
                title: Some("Weather Lookup".to_string()),
                output_schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": { "temp": { "type": "number" } }
                })),
            }],
        });
        let config = HigressMcpClientBuilder::create("higress")
            .unwrap()
            .streamable_http_endpoint("https://gw.example.com/mcp")
            .build_config()
            .unwrap();
        let client = Arc::new(HigressMcpClientWrapper::with_transport(config, transport));
        let toolkit = HigressToolkit::new();

        let registered = toolkit.register_higress_client(client).await.unwrap();

        assert_eq!(registered, 1);
        let cached = toolkit.get_higress_mcp_client().unwrap();
        assert_eq!(cached.name(), "higress");
        // HIGRESS-05: the gateway's display title reaches the registered tool
        // through the client's plain list path.
        assert_eq!(
            toolkit.get_tool("weather").unwrap().title(),
            Some("Weather Lookup")
        );
    }

    #[tokio::test]
    async fn harness_mcp_client_source_uses_the_shared_port() {
        use agentscope_harness::agent::{McpClientManager, McpClientSource, McpServerConfig};

        // HIGRESS-07: the adapter the harness ships for one managed server is a
        // first-class source here, so a host no longer needs a Higress-specific
        // client abstraction to register MCP tools.
        let manager = Arc::new(McpClientManager::new());
        manager
            .create_client(McpServerConfig::new("higress").url("https://gw.example.com/mcp"))
            .await
            .unwrap();
        let source: Arc<dyn McpToolSource> =
            Arc::new(McpClientSource::new(Arc::clone(&manager), "higress"));
        assert_eq!(source.name(), "higress");

        // Offline check of the adapter contract: without a connection its tool
        // listing fails loudly instead of returning an empty list.
        let error = source.list_tools().await.unwrap_err();
        assert!(error.to_string().contains("is not connected"));

        // …and it plugs into the fluent registration unchanged.
        let toolkit = HigressToolkit::new();
        let _registration = toolkit.registration().mcp_client(source);
    }

    /// A Higress transport double that advertises one tool carrying `parameters`
    /// as the gateway's raw input schema.
    struct RawSchemaTransport {
        parameters: Value,
    }

    #[async_trait]
    impl crate::client::HigressMcpTransport for RawSchemaTransport {
        async fn connect_server(&self, _request: crate::client::McpConnectRequest) -> Result<()> {
            Ok(())
        }
        async fn find_client_id_by_server(&self, server: &str) -> Option<String> {
            Some(server.to_string())
        }
        async fn connected_server_url(&self, _server: &str) -> Option<String> {
            None
        }
        async fn list_tools(
            &self,
            _client_id: &str,
        ) -> Result<Vec<agentscope_harness::agent::McpToolInfo>> {
            Ok(vec![agentscope_harness::agent::McpToolInfo {
                name: "weather".to_string(),
                description: Some("Weather".to_string()),
                parameters: self.parameters.clone(),
                server: "higress".to_string(),
                title: None,
                output_schema: None,
            }])
        }
        async fn call_tool_by_server(
            &self,
            _server: &str,
            _tool_name: &str,
            _arguments: Value,
            _meta: Option<JsonMap>,
        ) -> Result<Value> {
            Ok(serde_json::json!({ "content": [], "isError": false }))
        }
        async fn disconnect_by_server(&self, _server: &str) -> Result<()> {
            Ok(())
        }
    }

    fn higress_client_with(parameters: Value) -> Arc<HigressMcpClientWrapper> {
        let config = crate::client::HigressMcpClientBuilder::create("higress")
            .unwrap()
            .streamable_http_endpoint("https://gw.example.com/mcp")
            .build_config()
            .unwrap();
        Arc::new(HigressMcpClientWrapper::with_transport(
            config,
            Arc::new(RawSchemaTransport { parameters }),
        ))
    }

    /// HIGRESS-01: the model-facing parameters must keep the gateway's root
    /// JSON-Schema keywords, which the shared port's closed `ToolInputSchema`
    /// cannot carry.
    #[tokio::test]
    async fn registration_keeps_root_schema_keywords_from_the_gateway() {
        let client = higress_client_with(serde_json::json!({
            "type": "object",
            "properties": {
                "city": { "type": "string" },
                "token": { "type": "string" }
            },
            "required": ["city", "token"],
            "additionalProperties": false,
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$defs": { "location": { "type": "object", "properties": { "lat": { "type": "number" } } } }
        }));
        let toolkit = HigressToolkit::new();
        let mut presets = JsonMap::new();
        presets.insert("token".to_string(), Value::String("secret".to_string()));

        toolkit
            .registration()
            .higress_client(client)
            .preset_parameters_for("weather", presets)
            .apply()
            .await
            .unwrap();

        let schema = toolkit.get_tool("weather").unwrap().parameters_schema();
        // Java copies the raw schema (`parameters.putAll(inputSchema)`).
        assert_eq!(schema["additionalProperties"], Value::Bool(false));
        assert_eq!(
            schema["$schema"],
            Value::String("https://json-schema.org/draft/2020-12/schema".to_string())
        );
        assert_eq!(
            schema["$defs"]["location"]["properties"]["lat"]["type"],
            "number"
        );
        assert_eq!(schema["properties"]["city"]["type"], "string");
        // Preset keys stay hidden from the model.
        assert!(schema["properties"].get("token").is_none());
        assert_eq!(schema["required"], serde_json::json!(["city"]));
        assert_eq!(schema["type"], "object");
    }

    /// HIGRESS-01: boolean sub-schemas (`{"p": true}`) survive registration.
    #[tokio::test]
    async fn registration_keeps_boolean_sub_schemas_from_the_gateway() {
        let client = higress_client_with(serde_json::json!({
            "type": "object",
            "properties": { "anything": true, "city": { "type": "string" } }
        }));
        let toolkit = HigressToolkit::new();

        toolkit.register_higress_client(client).await.unwrap();

        let schema = toolkit.get_tool("weather").unwrap().parameters_schema();
        assert_eq!(schema["properties"]["anything"], Value::Bool(true));
        assert_eq!(schema["properties"]["city"]["type"], "string");
        // Java always writes `required`, even when the schema has none.
        assert_eq!(schema["required"], serde_json::json!([]));
    }
}
