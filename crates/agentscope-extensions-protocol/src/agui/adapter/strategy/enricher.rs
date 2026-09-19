//! Post-conversion AG-UI event enrichers, mirroring `AguiEventEnricher` and
//! `BaseEventPropertiesEnricher`.

use std::sync::Arc;

use agentscope_core::AgentEvent;

use crate::agui::adapter::strategy::context::AguiStreamContext;
use crate::agui::event::{AguiEvent, AguiEvents};

/// Enriches AG-UI events after semantic conversion.
pub trait AguiEventEnricher: Send + Sync {
    /// Enriches the converted events.
    ///
    /// `source` is the originating [`AgentEvent`], or `None` for framework-created events.
    fn enrich(
        &self,
        source: Option<&AgentEvent>,
        events: Vec<AguiEvent>,
        context: &AguiStreamContext,
    ) -> Vec<AguiEvent>;
}

/// Fills missing AG-UI base event properties (timestamps).
///
/// Existing timestamps and raw event payloads are preserved. This enricher does not create a
/// base `rawEvent`; applications wanting to expose raw source data should supply a custom
/// [`AguiEventEnricher`].
#[derive(Debug, Clone, Copy, Default)]
pub struct BaseEventPropertiesEnricher;

impl AguiEventEnricher for BaseEventPropertiesEnricher {
    fn enrich(
        &self,
        _source: Option<&AgentEvent>,
        events: Vec<AguiEvent>,
        _context: &AguiStreamContext,
    ) -> Vec<AguiEvent> {
        let now = chrono::Utc::now().timestamp_millis();
        events
            .into_iter()
            .map(|event| {
                let timestamp = event.timestamp().or(Some(now));
                let raw_event = event.raw_event().cloned();
                AguiEvents::with_base_properties(&event, timestamp, raw_event)
            })
            .collect()
    }
}

/// An enricher that records the source event under `rawEvent`.
///
/// Provided as the Rust equivalent of the Java advice to "provide a custom `AguiEventEnricher`"
/// when raw source data should be exposed.
#[derive(Debug, Clone, Copy, Default)]
pub struct RawEventPropertiesEnricher;

impl AguiEventEnricher for RawEventPropertiesEnricher {
    fn enrich(
        &self,
        source: Option<&AgentEvent>,
        events: Vec<AguiEvent>,
        _context: &AguiStreamContext,
    ) -> Vec<AguiEvent> {
        let Some(source) = source else {
            return events;
        };
        let raw = serde_json::to_value(source).ok();
        events
            .into_iter()
            .map(|event| {
                let timestamp = event.timestamp();
                AguiEvents::with_base_properties(
                    &event,
                    timestamp,
                    raw.clone().or_else(|| event.raw_event().cloned()),
                )
            })
            .collect()
    }
}

/// Convenience alias for a shared enricher handle.
pub type SharedEventEnricher = Arc<dyn AguiEventEnricher>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agui::adapter::config::AguiAdapterConfig;

    fn context() -> AguiStreamContext {
        AguiStreamContext::new("t", "r", AguiAdapterConfig::default_config(), None)
    }

    fn content_event(timestamp: Option<i64>) -> AguiEvent {
        AguiEvent::TextMessageContent {
            thread_id: "t".to_string(),
            run_id: "r".to_string(),
            message_id: "m".to_string(),
            delta: "d".to_string(),
            timestamp,
            raw_event: None,
        }
    }

    #[test]
    fn base_enricher_fills_missing_timestamps_once() {
        let enricher = BaseEventPropertiesEnricher;
        let context = context();
        let enriched = enricher.enrich(None, vec![content_event(None)], &context);
        assert_eq!(enriched.len(), 1);
        let stamp = enriched[0].timestamp().expect("timestamp is filled");
        assert!(stamp > 0);

        // An existing timestamp is preserved verbatim.
        let enriched = enricher.enrich(None, vec![content_event(Some(7))], &context);
        assert_eq!(enriched[0].timestamp(), Some(7));
        // The event payload is untouched.
        assert_eq!(enriched[0].event_type(), Some(crate::agui::event::AguiEventType::TextMessageContent));
    }

    #[test]
    fn base_enricher_handles_empty_input() {
        let context = context();
        assert!(BaseEventPropertiesEnricher
            .enrich(None, Vec::new(), &context)
            .is_empty());
    }

    #[test]
    fn raw_event_enricher_records_the_source_event() {
        use agentscope_core::{AgentEvent, AgentEventType};
        let source = AgentEvent::new(AgentEventType::AgentStart);
        let context = context();
        let enriched = RawEventPropertiesEnricher.enrich(Some(&source), vec![content_event(None)], &context);
        let raw = enriched[0].raw_event().expect("rawEvent is attached");
        assert_eq!(raw["type"], serde_json::json!("AGENT_START"));
    }

    #[test]
    fn raw_event_enricher_is_a_noop_without_a_source() {
        let context = context();
        let enriched = RawEventPropertiesEnricher.enrich(None, vec![content_event(None)], &context);
        assert!(enriched[0].raw_event().is_none());
    }
}
