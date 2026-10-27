//! Per-channel routing configuration, mirroring `ChannelConfig`.

use super::binding::ChannelBinding;
use super::dm_scope::DmScope;

/// Per-channel routing configuration: default agent fallback, DM session scope, and
/// ordered binding rules evaluated by `ChannelRouter`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelConfig {
    pub channel_id: String,
    pub default_agent_id: Option<String>,
    pub dm_scope: DmScope,
    pub bindings: Vec<ChannelBinding>,
}

impl ChannelConfig {
    /// Minimal config: channel id only, using the global agent default and `Main` scope.
    pub fn of(channel_id: impl Into<String>) -> Self {
        Self {
            channel_id: channel_id.into(),
            default_agent_id: None,
            dm_scope: DmScope::default_scope(),
            bindings: Vec::new(),
        }
    }

    /// Config with an explicit default agent (no binding rules).
    pub fn of_with_default(
        channel_id: impl Into<String>,
        default_agent_id: impl Into<String>,
    ) -> Self {
        Self {
            channel_id: channel_id.into(),
            default_agent_id: Some(default_agent_id.into()),
            dm_scope: DmScope::default_scope(),
            bindings: Vec::new(),
        }
    }

    /// Returns a builder for constructing channel configs fluently.
    pub fn builder(channel_id: impl Into<String>) -> ChannelConfigBuilder {
        ChannelConfigBuilder::new(channel_id.into())
    }
}

/// Builder for `ChannelConfig`.
#[derive(Debug, Clone)]
pub struct ChannelConfigBuilder {
    channel_id: String,
    default_agent_id: Option<String>,
    dm_scope: DmScope,
    bindings: Vec<ChannelBinding>,
}

impl ChannelConfigBuilder {
    pub fn new(channel_id: String) -> Self {
        Self {
            channel_id,
            default_agent_id: None,
            dm_scope: DmScope::default_scope(),
            bindings: Vec::new(),
        }
    }

    pub fn default_agent_id(mut self, default_agent_id: impl Into<String>) -> Self {
        self.default_agent_id = Some(default_agent_id.into());
        self
    }

    pub fn dm_scope(mut self, dm_scope: DmScope) -> Self {
        self.dm_scope = dm_scope;
        self
    }

    pub fn binding(mut self, binding: ChannelBinding) -> Self {
        self.bindings.push(binding);
        self
    }

    pub fn bindings(mut self, bindings: impl IntoIterator<Item = ChannelBinding>) -> Self {
        self.bindings.extend(bindings);
        self
    }

    pub fn build(self) -> ChannelConfig {
        ChannelConfig {
            channel_id: self.channel_id,
            default_agent_id: self.default_agent_id,
            dm_scope: self.dm_scope,
            bindings: self.bindings,
        }
    }
}
