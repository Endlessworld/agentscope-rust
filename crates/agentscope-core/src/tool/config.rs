//! Toolkit configuration, mirroring `io.agentscope.core.tool.ToolkitConfig`.
use crate::model::ExecutionConfig;

/// Configuration for a [`Toolkit`](super::Toolkit).
#[derive(Debug, Clone)]
pub struct ToolkitConfig {
    /// Whether tool calls within a batch may run in parallel.
    pub parallel: bool,
    /// Default execution config (timeout / retry) applied to tool calls.
    pub execution_config: Option<ExecutionConfig>,
    /// Whether tools/groups may be removed at runtime. When false, removal
    /// calls are ignored (with a warning) instead of deleting.
    pub allow_tool_deletion: bool,
}

impl Default for ToolkitConfig {
    fn default() -> Self {
        Self {
            // Java `new Toolkit()` defaults to parallel execution; individual
            // concurrency-unsafe tools still run in their own serial slot.
            parallel: true,
            execution_config: Some(ExecutionConfig::tool_defaults()),
            allow_tool_deletion: true,
        }
    }
}

impl ToolkitConfig {
    pub fn builder() -> ToolkitConfigBuilder {
        ToolkitConfigBuilder::default()
    }
}

/// Builder for [`ToolkitConfig`].
pub struct ToolkitConfigBuilder {
    parallel: bool,
    execution_config: Option<ExecutionConfig>,
    allow_tool_deletion: bool,
}

impl Default for ToolkitConfigBuilder {
    fn default() -> Self {
        Self {
            parallel: true,
            execution_config: Some(ExecutionConfig::tool_defaults()),
            allow_tool_deletion: true,
        }
    }
}

impl ToolkitConfigBuilder {
    pub fn parallel(mut self, v: bool) -> Self {
        self.parallel = v;
        self
    }
    pub fn execution_config(mut self, v: ExecutionConfig) -> Self {
        self.execution_config = Some(v);
        self
    }
    pub fn allow_tool_deletion(mut self, v: bool) -> Self {
        self.allow_tool_deletion = v;
        self
    }
    pub fn build(self) -> ToolkitConfig {
        ToolkitConfig {
            parallel: self.parallel,
            execution_config: self.execution_config,
            allow_tool_deletion: self.allow_tool_deletion,
        }
    }
}
