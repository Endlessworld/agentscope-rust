//! Meta tooling: tool invocation parameters, extended model schema merging, and
//! the `reset_equipped_tools` meta tool factory.
//! Mirrors `io.agentscope.core.tool.{ToolCallParam, ExtendedModel, SimpleExtendedModel,
//! MetaToolFactory}`.

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::message::{JsonMap, ToolResultBlock, ToolUseBlock};

use super::emitter::ToolEmitter;
use super::tool::FunctionTool;
use super::toolkit::Toolkit;

/// Parameters for a tool invocation, mirroring `io.agentscope.core.tool.ToolCallParam`.
///
/// The calling agent is adapted from the Java `Agent` reference to an opaque agent
/// identifier string; the runtime context is carried as a flat JSON map.
#[derive(Clone, Default)]
pub struct ToolCallParam {
    tool_use_block: Option<ToolUseBlock>,
    input: JsonMap,
    agent_id: Option<String>,
    runtime_context: JsonMap,
    emitter: Option<Arc<dyn ToolEmitter>>,
}

impl std::fmt::Debug for ToolCallParam {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCallParam")
            .field("tool_use_block", &self.tool_use_block)
            .field("input", &self.input)
            .field("agent_id", &self.agent_id)
            .field("has_emitter", &self.emitter.is_some())
            .finish()
    }
}

impl ToolCallParam {
    /// The tool-use block carrying the call id and tool name.
    pub fn tool_use_block(&self) -> Option<&ToolUseBlock> {
        self.tool_use_block.as_ref()
    }

    /// The (defensively copied) input parameters for the tool call.
    pub fn input(&self) -> &JsonMap {
        &self.input
    }

    /// The opaque identifier of the calling agent, if known.
    pub fn agent_id(&self) -> Option<&str> {
        self.agent_id.as_deref()
    }

    /// Runtime context carried as a flat JSON map.
    pub fn runtime_context(&self) -> &JsonMap {
        &self.runtime_context
    }

    /// The streaming emitter (falls back to a no-op by the caller if absent).
    pub fn emitter(&self) -> Option<Arc<dyn ToolEmitter>> {
        self.emitter.clone()
    }

    /// Starts a builder.
    pub fn builder() -> ToolCallParamBuilder {
        ToolCallParamBuilder::default()
    }

    /// Convenience constructor from a tool-use block.
    pub fn from(tool_use_block: ToolUseBlock) -> Self {
        Self {
            tool_use_block: Some(tool_use_block),
            ..Default::default()
        }
    }
}

/// Builder for [`ToolCallParam`].
#[derive(Default)]
pub struct ToolCallParamBuilder {
    tool_use_block: Option<ToolUseBlock>,
    input: Option<JsonMap>,
    agent_id: Option<String>,
    runtime_context: Option<JsonMap>,
    emitter: Option<Arc<dyn ToolEmitter>>,
}

impl ToolCallParamBuilder {
    pub fn tool_use_block(mut self, block: ToolUseBlock) -> Self {
        self.tool_use_block = Some(block);
        self
    }

    pub fn input(mut self, input: JsonMap) -> Self {
        self.input = Some(input);
        self
    }

    pub fn agent_id(mut self, id: impl Into<String>) -> Self {
        self.agent_id = Some(id.into());
        self
    }

    pub fn runtime_context(mut self, ctx: JsonMap) -> Self {
        self.runtime_context = Some(ctx);
        self
    }

    pub fn emitter(mut self, emitter: Arc<dyn ToolEmitter>) -> Self {
        self.emitter = Some(emitter);
        self
    }

    pub fn build(self) -> ToolCallParam {
        ToolCallParam {
            tool_use_block: self.tool_use_block,
            input: self.input.unwrap_or_default(),
            agent_id: self.agent_id,
            runtime_context: self.runtime_context.unwrap_or_default(),
            emitter: self.emitter,
        }
    }
}

// ---------------------------------------------------------------------------
// ExtendedModel
// ---------------------------------------------------------------------------

/// Extends a tool's base parameter schema with additional properties and required
/// fields, mirroring `io.agentscope.core.tool.ExtendedModel`.
pub trait ExtendedModel: Send + Sync {
    /// Additional properties merged into the base schema's `properties`.
    fn additional_properties(&self) -> &JsonMap;

    /// Additional fields appended to the base schema's `required` array.
    fn additional_required(&self) -> &[String];

    /// Merges the extended model into `base`, detecting property conflicts.
    fn merge_with_base_schema(&self, base: JsonMap) -> Result<JsonMap> {
        let mut merged = base;
        let base_props = merged
            .get("properties")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        let base_required = merged
            .get("required")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        // Conflict detection: extended properties must not collide with base ones.
        let conflicts = self
            .additional_properties()
            .keys()
            .filter(|k| base_props.contains_key(*k))
            .cloned()
            .collect::<Vec<_>>();
        if !conflicts.is_empty() {
            return Err(Error::Tool(format!(
                "Extended model has conflicting properties with base schema: {:?}",
                conflicts
            )));
        }

        let mut props = base_props;
        for (k, v) in self.additional_properties() {
            props.insert(k.clone(), v.clone());
        }
        merged.insert("properties".to_string(), serde_json::Value::Object(props));

        if !self.additional_required().is_empty() {
            let mut required = base_required;
            for r in self.additional_required() {
                if !required.contains(r) {
                    required.push(r.clone());
                }
            }
            merged.insert(
                "required".to_string(),
                serde_json::Value::Array(
                    required
                        .into_iter()
                        .map(serde_json::Value::String)
                        .collect(),
                ),
            );
        }
        Ok(merged)
    }
}

/// A simple [`ExtendedModel`] backed by plain maps.
#[derive(Debug, Clone, Default)]
pub struct SimpleExtendedModel {
    additional_properties: JsonMap,
    additional_required: Vec<String>,
}

impl SimpleExtendedModel {
    pub fn new(additional_properties: JsonMap, additional_required: Vec<String>) -> Self {
        Self {
            additional_properties,
            additional_required,
        }
    }
}

impl ExtendedModel for SimpleExtendedModel {
    fn additional_properties(&self) -> &JsonMap {
        &self.additional_properties
    }

    fn additional_required(&self) -> &[String] {
        &self.additional_required
    }
}

// ---------------------------------------------------------------------------
// MetaToolFactory
// ---------------------------------------------------------------------------

/// Factory for meta tools that let an agent dynamically manage its tool groups,
/// mirroring `io.agentscope.core.tool.MetaToolFactory`.
#[derive(Clone)]
pub struct MetaToolFactory {
    toolkit: Toolkit,
}

impl MetaToolFactory {
    /// Creates a factory bound to a snapshot of the given toolkit.
    pub fn new(toolkit: &Toolkit) -> Self {
        Self {
            toolkit: toolkit.copy(),
        }
    }

    /// Creates the `reset_equipped_tools` meta tool.
    ///
    /// The returned tool is toolkit-aware: [`Toolkit::copy`] rebinds it to the
    /// new copy so group mutations always hit the owning toolkit snapshot
    /// (Java `ToolkitAware.rebindToolkit` semantics).
    pub fn create_reset_equipped_tools(&self) -> FunctionTool {
        Self::build_reset_equipped_tools(&self.toolkit)
    }

    /// Builds a rebound reset tool bound to `toolkit`.
    fn build_reset_equipped_tools(toolkit: &Toolkit) -> FunctionTool {
        let toolkit = toolkit.clone();
        let description = format!(
            "Reset your equipped tools based on your current task requirements. These tools \
tools are organized into different groups, and you can activate/deactivate them by \
specifying which groups to keep active.\n\n\
**Important: The input list is the FINAL set of active tool groups, not incremental \
changes.** Any group not included in the list will be deactivated, regardless of its \
previous state.\n\n\
**Best practice**: Activate only what you need for the current task, and promptly \
deactivate groups as soon as they are no longer needed to conserve context space.\n\n{}",
            toolkit.groups_notes()
        );
        let parameters = reset_equipped_tools_schema(&toolkit);
        FunctionTool::new(
            "reset_equipped_tools",
            description,
            parameters,
            Box::new(move |args, _ctx| {
                let to_activate = args
                    .get("to_activate")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                match toolkit.reset_equipped_tools(&to_activate) {
                    Ok(text) => Ok(ToolResultBlock::text(text)),
                    Err(e) => Ok(ToolResultBlock::error(e.to_string())),
                }
            }),
        )
        .toolkit_aware(Box::new(|new_toolkit: &Toolkit| {
            Arc::new(Self::build_reset_equipped_tools(new_toolkit))
        }))
    }
}

/// Builds the JSON Schema for the `reset_equipped_tools` tool.
fn reset_equipped_tools_schema(toolkit: &Toolkit) -> serde_json::Value {
    let mut properties = JsonMap::new();
    let mut items = serde_json::Map::new();
    items.insert(
        "type".to_string(),
        serde_json::Value::String("string".into()),
    );
    let meta_groups = toolkit.meta_group_names();
    if !meta_groups.is_empty() {
        items.insert(
            "enum".to_string(),
            serde_json::Value::Array(
                meta_groups
                    .into_iter()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
    }
    properties.insert(
        "to_activate".to_string(),
        serde_json::json!({
            "type": "array",
            "items": serde_json::Value::Object(items),
            "description": "The FINAL list of tool group names to keep active. Groups NOT in \
        this list will be deactivated. Pass an empty list to deactivate all groups.",
        }),
    );
    serde_json::json!({
        "type": "object",
        "properties": serde_json::Value::Object(properties),
        "required": ["to_activate"],
    })
}
