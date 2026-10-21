//! Adapter configuration, mirroring `AguiAdapterConfig`.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::agui::adapter::strategy::{
    AgentEventConverter, AguiEventEnricher, BaseEventPropertiesEnricher,
};
use agentscope_core::tool::ToolMergeMode;

/// Default run timeout, mirroring the Java `Duration.ofMinutes(10)`.
pub const DEFAULT_RUN_TIMEOUT: Duration = Duration::from_secs(600);

/// Configuration for the AG-UI agent adapter.
#[derive(Clone)]
pub struct AguiAdapterConfig {
    tool_merge_mode: ToolMergeMode,
    emit_state_events: bool,
    emit_tool_call_args: bool,
    emit_token_usage: bool,
    enable_reasoning: bool,
    emit_run_finished_after_error: bool,
    run_timeout: Duration,
    default_agent_id: Option<String>,
    event_converters: Vec<Arc<dyn AgentEventConverter>>,
    event_enrichers: Vec<Arc<dyn AguiEventEnricher>>,
    base_event_properties_enricher_enabled: bool,
    emit_subagent_events_as_native: bool,
}

impl fmt::Debug for AguiAdapterConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AguiAdapterConfig")
            .field("tool_merge_mode", &self.tool_merge_mode)
            .field("emit_state_events", &self.emit_state_events)
            .field("emit_tool_call_args", &self.emit_tool_call_args)
            .field("emit_token_usage", &self.emit_token_usage)
            .field("enable_reasoning", &self.enable_reasoning)
            .field(
                "emit_run_finished_after_error",
                &self.emit_run_finished_after_error,
            )
            .field("run_timeout", &self.run_timeout)
            .field("default_agent_id", &self.default_agent_id)
            .field("event_converters", &self.event_converters.len())
            .field("event_enrichers", &self.event_enrichers.len())
            .field(
                "base_event_properties_enricher_enabled",
                &self.base_event_properties_enricher_enabled,
            )
            .field(
                "emit_subagent_events_as_native",
                &self.emit_subagent_events_as_native,
            )
            .finish()
    }
}

impl Default for AguiAdapterConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl AguiAdapterConfig {
    /// Creates a builder.
    pub fn builder() -> AguiAdapterConfigBuilder {
        AguiAdapterConfigBuilder::default()
    }

    /// Creates the default configuration.
    pub fn default_config() -> Self {
        Self::default()
    }

    /// The tool merge mode.
    pub fn tool_merge_mode(&self) -> ToolMergeMode {
        self.tool_merge_mode
    }

    /// Whether state events should be emitted.
    pub fn emit_state_events(&self) -> bool {
        self.emit_state_events
    }

    /// Whether tool-call argument events should be emitted.
    pub fn emit_tool_call_args(&self) -> bool {
        self.emit_tool_call_args
    }

    /// Whether accumulated token usage should be emitted as `token_usage` custom events.
    pub fn emit_token_usage(&self) -> bool {
        self.emit_token_usage
    }

    /// Whether reasoning/thinking content should be emitted as `REASONING_*` events.
    pub fn enable_reasoning(&self) -> bool {
        self.enable_reasoning
    }

    /// Whether `RUN_FINISHED` should follow `RUN_ERROR` (legacy clients).
    pub fn emit_run_finished_after_error(&self) -> bool {
        self.emit_run_finished_after_error
    }

    /// The run timeout.
    pub fn run_timeout(&self) -> Duration {
        self.run_timeout
    }

    /// The default agent id, when configured.
    pub fn default_agent_id(&self) -> Option<&str> {
        self.default_agent_id.as_deref()
    }

    /// Custom event converters (registered after the built-ins).
    pub fn event_converters(&self) -> &[Arc<dyn AgentEventConverter>] {
        &self.event_converters
    }

    /// Configured AG-UI event enrichers, including the default base-properties enricher.
    pub fn event_enrichers(&self) -> &[Arc<dyn AguiEventEnricher>] {
        &self.event_enrichers
    }

    /// Whether the default base event properties enricher is enabled.
    pub fn base_event_properties_enricher_enabled(&self) -> bool {
        self.base_event_properties_enricher_enabled
    }

    /// Whether subagent-sourced events keep their native AG-UI event types.
    pub fn emit_subagent_events_as_native(&self) -> bool {
        self.emit_subagent_events_as_native
    }

    /// Copies this configuration with a different run timeout.
    pub fn with_run_timeout(mut self, run_timeout: Duration) -> Self {
        self.run_timeout = run_timeout;
        self
    }
}

/// Builder for [`AguiAdapterConfig`].
#[derive(Default)]
pub struct AguiAdapterConfigBuilder {
    tool_merge_mode: Option<ToolMergeMode>,
    emit_state_events: Option<bool>,
    emit_tool_call_args: Option<bool>,
    emit_token_usage: Option<bool>,
    enable_reasoning: Option<bool>,
    emit_run_finished_after_error: Option<bool>,
    run_timeout: Option<Duration>,
    default_agent_id: Option<String>,
    event_converters: Vec<Arc<dyn AgentEventConverter>>,
    event_enrichers: Vec<Arc<dyn AguiEventEnricher>>,
    base_event_properties_enricher_enabled: Option<bool>,
    emit_subagent_events_as_native: Option<bool>,
}

impl AguiAdapterConfigBuilder {
    /// Sets the tool merge mode.
    pub fn tool_merge_mode(mut self, mode: ToolMergeMode) -> Self {
        self.tool_merge_mode = Some(mode);
        self
    }

    /// Sets whether state events are emitted.
    pub fn emit_state_events(mut self, value: bool) -> Self {
        self.emit_state_events = Some(value);
        self
    }

    /// Sets whether tool-call argument events are emitted.
    pub fn emit_tool_call_args(mut self, value: bool) -> Self {
        self.emit_tool_call_args = Some(value);
        self
    }

    /// Sets whether token usage custom events are emitted.
    pub fn emit_token_usage(mut self, value: bool) -> Self {
        self.emit_token_usage = Some(value);
        self
    }

    /// Sets whether reasoning events are emitted.
    pub fn enable_reasoning(mut self, value: bool) -> Self {
        self.enable_reasoning = Some(value);
        self
    }

    /// Sets whether `RUN_FINISHED` follows `RUN_ERROR`.
    pub fn emit_run_finished_after_error(mut self, value: bool) -> Self {
        self.emit_run_finished_after_error = Some(value);
        self
    }

    /// Sets the run timeout.
    pub fn run_timeout(mut self, value: Duration) -> Self {
        self.run_timeout = Some(value);
        self
    }

    /// Sets the default agent id.
    pub fn default_agent_id(mut self, value: impl Into<String>) -> Self {
        self.default_agent_id = Some(value.into());
        self
    }

    /// Adds a custom event converter.
    pub fn add_event_converter(mut self, converter: Arc<dyn AgentEventConverter>) -> Self {
        self.event_converters.push(converter);
        self
    }

    /// Replaces the custom event converters.
    pub fn event_converters(mut self, converters: Vec<Arc<dyn AgentEventConverter>>) -> Self {
        self.event_converters = converters;
        self
    }

    /// Adds an event enricher.
    pub fn add_event_enricher(mut self, enricher: Arc<dyn AguiEventEnricher>) -> Self {
        self.event_enrichers.push(enricher);
        self
    }

    /// Replaces the event enrichers.
    pub fn event_enrichers(mut self, enrichers: Vec<Arc<dyn AguiEventEnricher>>) -> Self {
        self.event_enrichers = enrichers;
        self
    }

    /// Sets whether the default base event properties enricher is enabled.
    pub fn base_event_properties_enricher_enabled(mut self, value: bool) -> Self {
        self.base_event_properties_enricher_enabled = Some(value);
        self
    }

    /// Sets whether subagent events keep their native AG-UI event types.
    pub fn emit_subagent_events_as_native(mut self, value: bool) -> Self {
        self.emit_subagent_events_as_native = Some(value);
        self
    }

    /// Builds the configuration.
    pub fn build(self) -> AguiAdapterConfig {
        let base_event_properties_enricher_enabled =
            self.base_event_properties_enricher_enabled.unwrap_or(false);
        let mut event_enrichers: Vec<Arc<dyn AguiEventEnricher>> = Vec::new();
        if base_event_properties_enricher_enabled {
            event_enrichers.push(Arc::new(BaseEventPropertiesEnricher));
        }
        event_enrichers.extend(self.event_enrichers);
        AguiAdapterConfig {
            tool_merge_mode: self
                .tool_merge_mode
                .unwrap_or(ToolMergeMode::MergeExternalPriority),
            emit_state_events: self.emit_state_events.unwrap_or(true),
            emit_tool_call_args: self.emit_tool_call_args.unwrap_or(true),
            emit_token_usage: self.emit_token_usage.unwrap_or(false),
            enable_reasoning: self.enable_reasoning.unwrap_or(false),
            emit_run_finished_after_error: self.emit_run_finished_after_error.unwrap_or(false),
            run_timeout: self.run_timeout.unwrap_or(DEFAULT_RUN_TIMEOUT),
            default_agent_id: self.default_agent_id,
            event_converters: self.event_converters,
            event_enrichers,
            base_event_properties_enricher_enabled,
            emit_subagent_events_as_native: self.emit_subagent_events_as_native.unwrap_or(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_java_builder() {
        let config = AguiAdapterConfig::default_config();
        assert_eq!(config.tool_merge_mode(), ToolMergeMode::MergeExternalPriority);
        assert!(config.emit_state_events());
        assert!(config.emit_tool_call_args());
        assert!(!config.emit_token_usage());
        assert!(!config.enable_reasoning());
        assert!(!config.emit_run_finished_after_error());
        assert_eq!(config.run_timeout(), Duration::from_secs(600));
        assert_eq!(config.default_agent_id(), None);
        assert!(config.event_converters().is_empty());
        assert!(!config.base_event_properties_enricher_enabled());
        assert!(!config.emit_subagent_events_as_native());
        // The base properties enricher is off by default, so no enrichers are installed.
        assert!(config.event_enrichers().is_empty());
    }

    #[test]
    fn builder_overrides_every_flag() {
        let config = AguiAdapterConfig::builder()
            .tool_merge_mode(ToolMergeMode::AgentOnly)
            .emit_state_events(false)
            .emit_tool_call_args(false)
            .emit_token_usage(true)
            .enable_reasoning(true)
            .emit_run_finished_after_error(true)
            .run_timeout(Duration::from_millis(5))
            .default_agent_id("assistant")
            .base_event_properties_enricher_enabled(true)
            .emit_subagent_events_as_native(true)
            .build();
        assert_eq!(config.tool_merge_mode(), ToolMergeMode::AgentOnly);
        assert!(!config.emit_state_events());
        assert!(!config.emit_tool_call_args());
        assert!(config.emit_token_usage());
        assert!(config.enable_reasoning());
        assert!(config.emit_run_finished_after_error());
        assert_eq!(config.run_timeout(), Duration::from_millis(5));
        assert_eq!(config.default_agent_id(), Some("assistant"));
        assert!(config.emit_subagent_events_as_native());
        // Enabling the default enricher prepends it to the configured enrichers.
        assert_eq!(config.event_enrichers().len(), 1);
    }

    #[test]
    fn clone_and_debug_preserve_configuration() {
        let config = AguiAdapterConfig::builder()
            .default_agent_id("a")
            .emit_token_usage(true)
            .build();
        let cloned = config.clone();
        assert_eq!(cloned.default_agent_id(), Some("a"));
        assert!(cloned.emit_token_usage());
        let rendered = format!("{config:?}");
        assert!(rendered.contains("AguiAdapterConfig"));
        assert!(rendered.contains("assistant") == false);
        assert!(rendered.contains("default_agent_id"));
    }

    #[test]
    fn with_run_timeout_replaces_only_the_timeout() {
        let config = AguiAdapterConfig::default_config().with_run_timeout(Duration::from_secs(1));
        assert_eq!(config.run_timeout(), Duration::from_secs(1));
        assert_eq!(config.tool_merge_mode(), ToolMergeMode::MergeExternalPriority);
    }
}
