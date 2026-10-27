//! Higress-aware toolkit.
//!
//! Mirrors `io.agentscope.extensions.higress.HigressToolkit`, which extends the
//! core `Toolkit` to cache the registered Higress MCP client for later access.
//!
//! Tool registration and MCP bridging are already provided by the core
//! [`Toolkit`](agentscope_core::tool::Toolkit) and the harness; this type only adds
//! the Higress client cache and registers MCP tools discovered from a
//! [`HigressMcpClientWrapper`].
use std::sync::{Arc, Mutex};

use agentscope_core::error::Result;
use agentscope_core::tool::{Tool, Toolkit, ToolkitConfig};

use crate::client::HigressMcpClientWrapper;

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

    /// Registers an MCP client with the toolkit.
    ///
    /// When `mcp_client` is a [`HigressMcpClientWrapper`], it is cached for
    /// later access via [`get_higress_mcp_client`](Self::get_higress_mcp_client),
    /// and its tools are registered into the toolkit.
    ///
    /// Mirrors the Java `registerMcpClient` override.
    pub async fn register_mcp_client(
        &self,
        mcp_client: Arc<HigressMcpClientWrapper>,
    ) -> Result<usize> {
        let tools = mcp_client.list_tools().await?;
        let mut registered = 0usize;
        for tool in tools {
            let entry = McpGatewayTool::new(mcp_client.clone(), tool);
            self.toolkit.register_tool(Arc::new(entry) as Arc<dyn Tool>);
            registered += 1;
        }
        *self.higress_client.lock().unwrap() = Some(mcp_client);
        Ok(registered)
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
    ///
    /// Mirrors the Java `getHigressMcpClient`.
    pub fn get_higress_mcp_client(&self) -> Option<Arc<HigressMcpClientWrapper>> {
        self.higress_client.lock().unwrap().clone()
    }

    /// Clears the cached Higress MCP client.
    pub fn clear_higress_mcp_client(&self) {
        *self.higress_client.lock().unwrap() = None;
    }
}

/// A core [`Tool`] that forwards invocations to the Higress gateway through a
/// [`HigressMcpClientWrapper`].
struct McpGatewayTool {
    client: Arc<HigressMcpClientWrapper>,
    name: String,
    description: String,
    parameters: serde_json::Value,
}

impl McpGatewayTool {
    fn new(client: Arc<HigressMcpClientWrapper>, tool: rust_mcp_sdk::schema::Tool) -> Self {
        let parameters = tool
            .input_schema
            .properties
            .as_ref()
            .map(|props| {
                let mut map = serde_json::Map::new();
                for (k, v) in props {
                    map.insert(k.clone(), serde_json::Value::Object(v.clone()));
                }
                map
            })
            .map(|props| {
                let mut schema = serde_json::Map::new();
                schema.insert("type".into(), serde_json::Value::String("object".into()));
                schema.insert("properties".into(), serde_json::Value::Object(props));
                schema.insert(
                    "required".into(),
                    serde_json::Value::Array(
                        tool.input_schema
                            .required
                            .iter()
                            .map(|r| serde_json::Value::String(r.clone()))
                            .collect(),
                    ),
                );
                serde_json::Value::Object(schema)
            })
            .unwrap_or_else(|| serde_json::json!({ "type": "object", "properties": {} }));
        Self {
            client,
            name: tool.name,
            description: tool.description.unwrap_or_default(),
            parameters,
        }
    }
}

#[async_trait::async_trait]
impl Tool for McpGatewayTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn is_external(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.parameters.clone()
    }

    async fn call(
        &self,
        args: agentscope_core::message::JsonMap,
        _ctx: agentscope_core::tool::ToolExecutionContext,
    ) -> Result<agentscope_core::message::ToolResultBlock> {
        let arguments = serde_json::Value::Object(args);
        let result = self.client.call_tool(&self.name, arguments).await?;
        let text = render_call_result(&result);
        if result.is_error == Some(true) {
            Ok(agentscope_core::message::ToolResultBlock::error(text))
        } else {
            Ok(agentscope_core::message::ToolResultBlock::new(
                "",
                &self.name,
                vec![agentscope_core::message::ContentBlock::Text(
                    agentscope_core::message::TextBlock::new(text),
                )],
            ))
        }
    }
}

/// Renders an MCP [`CallToolResult`] into a plain-text string.
///
/// [`CallToolResult`]: rust_mcp_sdk::schema::CallToolResult
fn render_call_result(result: &rust_mcp_sdk::schema::CallToolResult) -> String {
    use rust_mcp_sdk::schema::ContentBlock;
    let mut parts = Vec::new();
    for block in &result.content {
        match block {
            ContentBlock::TextContent(t) => parts.push(t.text.clone()),
            other => parts.push(format!("{:?}", other)),
        }
    }
    if parts.is_empty() {
        if let Some(structured) = &result.structured_content {
            return serde_json::to_string_pretty(&serde_json::Value::Object(structured.clone()))
                .unwrap_or_default();
        }
    }
    parts.join("\n")
}
