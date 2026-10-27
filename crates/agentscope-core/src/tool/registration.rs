//! Fluent tool registration builder, mirroring `Toolkit.ToolRegistration`.
//!
//! Builds a single registration against a [`Toolkit`]: accepts one tool source,
//! optionally assigns it to a group, applies preset parameters, merges an
//! [`ExtendedModel`], tags an MCP client origin and activates it, then commits
//! with `apply()`.
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::message::JsonMap;

use super::meta::ExtendedModel;
use super::tool::{Tool, ToolExecutionContext};
use super::toolkit::Toolkit;

/// A streaming, one-shot registration against a [`Toolkit`].
///
/// The primary tool source is supplied via [`Self::tool`]; group, preset
/// parameters, extended model and MCP client tagging are optional and applied
/// atomically on [`Self::apply`].
pub struct ToolRegistration<'a> {
    toolkit: &'a Toolkit,
    tool: Option<Arc<dyn Tool>>,
    group: Option<String>,
    presets: Option<JsonMap>,
    extended_model: Option<Arc<dyn ExtendedModel>>,
    mcp_client_name: Option<String>,
    sub_agent: Option<String>,
}

impl<'a> ToolRegistration<'a> {
    /// Starts a new registration against the given toolkit.
    pub fn new(toolkit: &'a Toolkit) -> Self {
        Self {
            toolkit,
            tool: None,
            group: None,
            presets: None,
            extended_model: None,
            mcp_client_name: None,
            sub_agent: None,
        }
    }

    /// Sets the tool to register.
    pub fn tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tool = Some(tool);
        self
    }

    /// Assigns the tool to a group (created if absent).
    pub fn group(mut self, group: impl Into<String>) -> Self {
        self.group = Some(group.into());
        self
    }

    /// Applies preset parameters the model input cannot override.
    pub fn preset_parameters(mut self, params: JsonMap) -> Self {
        self.presets = Some(params);
        self
    }

    /// Merges an [`ExtendedModel`] into the tool's parameter schema at commit
    /// time (Java `RegisteredToolFunction.extendedModel`).
    pub fn extended_model(mut self, model: Arc<dyn ExtendedModel>) -> Self {
        self.extended_model = Some(model);
        self
    }

    /// Tags the tool as originating from an MCP client
    /// (Java `RegisteredToolFunction.mcpClientName`).
    pub fn mcp_client(mut self, client_name: impl Into<String>) -> Self {
        self.mcp_client_name = Some(client_name.into());
        self
    }

    /// Tags the tool as exposing a sub-agent entry point
    /// (Java `RegisteredToolFunction.subAgent`).
    pub fn sub_agent(mut self, agent_name: impl Into<String>) -> Self {
        self.sub_agent = Some(agent_name.into());
        self
    }

    /// Commits the registration to the toolkit.
    pub fn apply(self) -> Result<()> {
        let tool = self
            .tool
            .ok_or_else(|| Error::Tool("ToolRegistration requires a tool".into()))?;
        let name = tool.name().to_string();

        let tool: Arc<dyn Tool> = match &self.extended_model {
            Some(model) => {
                let base = match tool.parameters_schema() {
                    serde_json::Value::Object(map) => map,
                    other => {
                        let mut m = serde_json::Map::new();
                        m.insert("type".into(), serde_json::Value::String("object".into()));
                        m.insert("properties".into(), other);
                        m
                    }
                };
                let merged = model.merge_with_base_schema(base)?;
                Arc::new(ExtendedSchemaTool::new(
                    tool,
                    serde_json::Value::Object(merged),
                ))
            }
            None => tool,
        };

        self.toolkit.register_tool(tool);
        if let Some(group) = &self.group {
            self.toolkit.create_tool_group(group, "", true);
            self.toolkit.add_tool_to_group(group, &name);
        }
        if let Some(presets) = &self.presets {
            self.toolkit.set_preset_parameters(&name, presets.clone());
        }
        if let Some(client) = &self.mcp_client_name {
            self.toolkit.set_mcp_client_name(&name, client.clone());
        }
        if let Some(agent) = &self.sub_agent {
            self.toolkit.set_sub_agent_name(&name, agent.clone());
        }
        Ok(())
    }
}

/// Adapter that overrides only the parameter schema of an inner tool with a
/// pre-merged (extended) schema; all other behaviour delegates.
pub(crate) struct ExtendedSchemaTool {
    inner: Arc<dyn Tool>,
    merged_schema: serde_json::Value,
}

impl ExtendedSchemaTool {
    fn new(inner: Arc<dyn Tool>, merged_schema: serde_json::Value) -> Self {
        Self {
            inner,
            merged_schema,
        }
    }
}

#[async_trait::async_trait]
impl Tool for ExtendedSchemaTool {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }
    fn is_external(&self) -> bool {
        self.inner.is_external()
    }
    fn is_concurrency_safe(&self) -> bool {
        self.inner.is_concurrency_safe()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        self.merged_schema.clone()
    }
    async fn call(
        &self,
        args: JsonMap,
        ctx: ToolExecutionContext,
    ) -> Result<crate::message::ToolResultBlock> {
        self.inner.call(args, ctx).await
    }
}
