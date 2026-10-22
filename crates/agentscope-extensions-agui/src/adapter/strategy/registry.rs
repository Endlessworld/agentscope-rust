//! Event conversion strategy registry, mirroring `AgentEventConverterRegistry`.
//!
//! Built-in converters are registered first and custom converters afterwards, so a custom
//! converter overrides the mapping for the same [`AgentEventType`]. Enrichers run after
//! conversion and can add cross-cutting AG-UI base properties.

use std::sync::Arc;

use agentscope_core::{AgentEvent, AgentEventType};

use crate::adapter::strategy::context::AguiStreamContext;
use crate::adapter::strategy::converters::{
    AgentEventConverter, AgentLifecycleEventConverter, CustomAgentEventConverter,
    ExternalExecutionEventConverter, ModelCallUsageEventConverter, PermissionConfirmEventConverter,
    RawAgentEventConverter, SubagentEventConverter, TextBlockEventConverter,
    ThinkingBlockEventConverter, ToolCallEventConverter, ToolResultEventConverter,
};
use crate::adapter::strategy::enricher::AguiEventEnricher;
use crate::event::AguiEvent;

/// Converter lookup table.
///
/// `AgentEventType` (core) implements `Eq` but not `Hash`, so the table keeps registration
/// order in a small vector and looks up linearly; because registration is bounded by the
/// converter count this is cheaper than an allocation-per-event hash map.
/// Later registrations override earlier ones for the same event type.
type ConverterTable = Vec<(AgentEventType, Arc<dyn AgentEventConverter>)>;

/// Maps AgentScope [`AgentEvent`]s to AG-UI protocol events.
pub struct AgentEventConverterRegistry {
    converters: ConverterTable,
    raw_converter: RawAgentEventConverter,
    subagent_converter: SubagentEventConverter,
    enrichers: Vec<Arc<dyn AguiEventEnricher>>,
    emit_subagent_events_as_native: bool,
}

impl std::fmt::Debug for AgentEventConverterRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut types: Vec<&str> = self
            .converters
            .iter()
            .map(|(event_type, _)| event_type.wire_name())
            .collect();
        types.sort_unstable();
        f.debug_struct("AgentEventConverterRegistry")
            .field("converters", &types)
            .field("enrichers", &self.enrichers.len())
            .field(
                "emit_subagent_events_as_native",
                &self.emit_subagent_events_as_native,
            )
            .finish()
    }
}

impl Default for AgentEventConverterRegistry {
    fn default() -> Self {
        Self::new(Vec::new(), Vec::new(), false)
    }
}

impl AgentEventConverterRegistry {
    /// Creates a registry from custom converters, enrichers and the subagent presentation mode.
    ///
    /// When `emit_subagent_events_as_native` is `true`, child events use the same converters as
    /// the parent; when `false` (the default), events with a non-null `source` become
    /// `subagent.*` custom or raw events.
    pub fn new(
        custom_converters: Vec<Arc<dyn AgentEventConverter>>,
        enrichers: Vec<Arc<dyn AguiEventEnricher>>,
        emit_subagent_events_as_native: bool,
    ) -> Self {
        let mut converters: ConverterTable = Vec::new();
        let built_ins: Vec<Arc<dyn AgentEventConverter>> = vec![
            Arc::new(AgentLifecycleEventConverter),
            Arc::new(PermissionConfirmEventConverter),
            Arc::new(ExternalExecutionEventConverter),
            Arc::new(TextBlockEventConverter),
            Arc::new(ThinkingBlockEventConverter),
            Arc::new(ToolCallEventConverter),
            Arc::new(ToolResultEventConverter),
            Arc::new(ModelCallUsageEventConverter),
            Arc::new(CustomAgentEventConverter),
        ];
        for converter in built_ins {
            register(&mut converters, converter);
        }
        for converter in custom_converters {
            register(&mut converters, converter);
        }
        Self {
            converters,
            raw_converter: RawAgentEventConverter,
            subagent_converter: SubagentEventConverter,
            enrichers,
            emit_subagent_events_as_native,
        }
    }

    /// Converts one AgentScope event into zero or more enriched AG-UI events.
    pub fn convert(&self, event: &AgentEvent, context: &mut AguiStreamContext) -> Vec<AguiEvent> {
        context.begin_event();
        let source = event.source();
        if !self.emit_subagent_events_as_native && source.is_some_and(|s| !s.trim().is_empty()) {
            self.subagent_converter.convert(event, context);
        } else {
            let event_type = event.event_type();
            match self
                .converters
                .iter()
                .find(|(registered, _)| *registered == event_type)
                .map(|(_, converter)| converter)
            {
                Some(converter) => converter.convert(event, context),
                None => self.raw_converter.convert(event, context),
            }
        }
        let events = context.drain_events();
        self.enrich(Some(event), events, context)
    }

    /// Applies the configured enrichers in registration order.
    pub fn enrich(
        &self,
        source: Option<&AgentEvent>,
        events: Vec<AguiEvent>,
        context: &AguiStreamContext,
    ) -> Vec<AguiEvent> {
        let mut enriched = events;
        for enricher in &self.enrichers {
            enriched = enricher.enrich(source, enriched, context);
        }
        enriched
    }

    /// The event types with a registered converter (used by tests and diagnostics).
    pub fn registered_event_types(&self) -> Vec<AgentEventType> {
        let mut types: Vec<AgentEventType> = self
            .converters
            .iter()
            .map(|(event_type, _)| *event_type)
            .collect();
        types.sort_by_key(|t| t.wire_name());
        types
    }
}

/// Registers a converter for every event type it declares.
fn register(converters: &mut ConverterTable, converter: Arc<dyn AgentEventConverter>) {
    for event_type in converter.event_types() {
        match converters
            .iter_mut()
            .find(|(registered, _)| *registered == event_type)
        {
            Some((_, slot)) => *slot = Arc::clone(&converter),
            None => converters.push((event_type, Arc::clone(&converter))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::config::AguiAdapterConfig;
    use crate::adapter::strategy::enricher::BaseEventPropertiesEnricher;
    use crate::event::AguiEventType as Wire;
    use agentscope_core::{AgentEventType, JsonMap};
    use serde_json::json;

    fn context() -> AguiStreamContext {
        AguiStreamContext::new("t", "r", AguiAdapterConfig::default_config(), None)
    }

    fn delta(type_: AgentEventType, fields: &[(&str, serde_json::Value)]) -> AgentEvent {
        let mut map = JsonMap::new();
        for (key, value) in fields {
            map.insert((*key).to_string(), value.clone());
        }
        AgentEvent::with_fields(type_, map)
    }

    /// A custom converter overriding the built-in text mapping.
    #[derive(Debug)]
    struct UpperTextConverter;

    impl AgentEventConverter for UpperTextConverter {
        fn event_types(&self) -> Vec<AgentEventType> {
            vec![AgentEventType::TextBlockDelta]
        }

        fn convert(&self, event: &AgentEvent, context: &mut AguiStreamContext) {
            let delta = event
                .data()
                .fields
                .get("delta")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_uppercase();
            context.emit(AguiEvent::TextMessageContent {
                thread_id: context.thread_id().to_string(),
                run_id: context.run_id().to_string(),
                message_id: "custom".to_string(),
                delta,
                timestamp: None,
                raw_event: None,
            });
        }
    }

    #[test]
    fn built_in_converters_cover_every_documented_event_type() {
        let registry = AgentEventConverterRegistry::default();
        let types = registry.registered_event_types();
        for expected in [
            AgentEventType::AgentStart,
            AgentEventType::AgentEnd,
            AgentEventType::AgentResult,
            AgentEventType::ModelCallStart,
            AgentEventType::ModelCallEnd,
            AgentEventType::TextBlockStart,
            AgentEventType::TextBlockDelta,
            AgentEventType::TextBlockEnd,
            AgentEventType::ThinkingBlockStart,
            AgentEventType::ThinkingBlockDelta,
            AgentEventType::ThinkingBlockEnd,
            AgentEventType::ToolCallStart,
            AgentEventType::ToolCallDelta,
            AgentEventType::ToolCallEnd,
            AgentEventType::ToolResultStart,
            AgentEventType::ToolProgress,
            AgentEventType::ToolResultTextDelta,
            AgentEventType::ToolResultDataDelta,
            AgentEventType::ToolResultEnd,
            AgentEventType::RequireUserConfirm,
            AgentEventType::UserConfirmResult,
            AgentEventType::RequireExternalExecution,
            AgentEventType::ExternalExecutionResult,
            AgentEventType::Custom,
        ] {
            assert!(
                types.contains(&expected),
                "missing converter for {}",
                expected.wire_name()
            );
        }
        let rendered = format!("{registry:?}");
        assert!(rendered.contains("AgentEventConverterRegistry"));
    }

    #[test]
    fn unmapped_events_fall_back_to_raw() {
        let registry = AgentEventConverterRegistry::default();
        let mut context = context();
        let events = registry.convert(
            &AgentEvent::new(AgentEventType::ExceedMaxIters),
            &mut context,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type(), Some(Wire::Raw));
    }

    #[test]
    fn custom_converters_override_the_built_ins() {
        let registry =
            AgentEventConverterRegistry::new(vec![Arc::new(UpperTextConverter)], Vec::new(), false);
        let mut context = context();
        let event = delta(
            AgentEventType::TextBlockDelta,
            &[("reply_id", json!("r")), ("delta", json!("hi"))],
        );
        let events = registry.convert(&event, &mut context);
        assert_eq!(events.len(), 1);
        let rendered = serde_json::to_string(&events[0]).unwrap();
        assert!(rendered.contains("\"delta\":\"HI\""), "got {rendered}");
        assert!(rendered.contains("\"messageId\":\"custom\""));
    }

    #[test]
    fn subagent_events_use_the_namespaced_converter_by_default() {
        let registry = AgentEventConverterRegistry::default();
        let mut context = context();
        let event = delta(
            AgentEventType::TextBlockDelta,
            &[("reply_id", json!("r")), ("delta", json!("child"))],
        )
        .with_source("main/child");
        let events = registry.convert(&event, &mut context);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type(), Some(Wire::Custom));
        assert!(serde_json::to_string(&events[0])
            .unwrap()
            .contains("subagent.text"));
    }

    #[test]
    fn subagent_events_keep_native_types_when_opted_in() {
        let registry = AgentEventConverterRegistry::new(Vec::new(), Vec::new(), true);
        let mut context = context();
        let event = delta(
            AgentEventType::TextBlockDelta,
            &[("reply_id", json!("r")), ("delta", json!("child"))],
        )
        .with_source("main/child");
        let events = registry.convert(&event, &mut context);
        // Native mode reuses the parent converters, which lazily start the text message on the
        // first non-empty delta: START then CONTENT, exactly as Java `appendTextDelta` does.
        assert_eq!(events[0].event_type(), Some(Wire::TextMessageStart));
        assert_eq!(events[1].event_type(), Some(Wire::TextMessageContent));
    }

    #[test]
    fn enrichers_run_after_conversion_in_registration_order() {
        let registry = AgentEventConverterRegistry::new(
            Vec::new(),
            vec![Arc::new(BaseEventPropertiesEnricher)],
            false,
        );
        let mut context = context();
        let event = delta(
            AgentEventType::TextBlockDelta,
            &[("reply_id", json!("r")), ("delta", json!("x"))],
        );
        let events = registry.convert(&event, &mut context);
        assert!(events.iter().all(|e| e.timestamp().is_some()));
    }

    #[test]
    fn enrich_is_publicly_usable_for_framework_created_events() {
        let registry = AgentEventConverterRegistry::new(
            Vec::new(),
            vec![Arc::new(BaseEventPropertiesEnricher)],
            false,
        );
        let context = context();
        let enriched = registry.enrich(
            None,
            vec![AguiEvent::RunFinished {
                thread_id: "t".to_string(),
                run_id: "r".to_string(),
                result: None,
                outcome: None,
                timestamp: None,
                raw_event: None,
            }],
            &context,
        );
        assert_eq!(enriched.len(), 1);
        assert!(enriched[0].timestamp().is_some());
    }

    #[test]
    fn blank_subagent_sources_use_the_parent_mapping() {
        let registry = AgentEventConverterRegistry::default();
        let mut context = context();
        let event = delta(
            AgentEventType::TextBlockDelta,
            &[("reply_id", json!("r")), ("delta", json!("own"))],
        )
        .with_source("  ");
        let events = registry.convert(&event, &mut context);
        // A blank source is treated as the parent, matching Java's `source.isBlank()` check.
        assert_eq!(events[0].event_type(), Some(Wire::TextMessageStart));
    }
}
