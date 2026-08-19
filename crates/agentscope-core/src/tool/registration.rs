//! Fluent tool registration builder, mirroring `Toolkit.ToolRegistration`.
//!
//! Builds a single registration against a [`Toolkit`]: accepts one tool source,
//! optionally assigns it to a group, applies preset parameters and activates it,
//! then commits with `apply()`.
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::message::JsonMap;

use super::tool::Tool;
use super::toolkit::Toolkit;

/// A streaming, one-shot registration against a [`Toolkit`].
///
/// The primary tool source is supplied via [`Self::tool`]; group and preset
/// parameters are optional and applied atomically on [`Self::apply`].
pub struct ToolRegistration<'a> {
    toolkit: &'a Toolkit,
    tool: Option<Arc<dyn Tool>>,
    group: Option<String>,
    presets: Option<JsonMap>,
}

impl<'a> ToolRegistration<'a> {
    /// Starts a new registration against the given toolkit.
    pub fn new(toolkit: &'a Toolkit) -> Self {
        Self {
            toolkit,
            tool: None,
            group: None,
            presets: None,
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

    /// Commits the registration to the toolkit.
    pub fn apply(self) -> Result<()> {
        let tool = self
            .tool
            .ok_or_else(|| Error::Tool("ToolRegistration requires a tool".into()))?;
        let name = tool.name().to_string();
        self.toolkit.register_tool(tool);
        if let Some(group) = &self.group {
            self.toolkit.create_tool_group(group, "", true);
            self.toolkit.add_tool_to_group(group, &name);
        }
        if let Some(presets) = &self.presets {
            self.toolkit.set_preset_parameters(&name, presets.clone());
        }
        Ok(())
    }
}
