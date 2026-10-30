//! Conversion diagnostics.
//!
//! A converter cannot always preserve every detail of a request: a protocol may
//! have no equivalent for `tool_choice: required`, a remote media URL may be
//! unreadable, or a field may be dropped because the target does not support it.
//! Silently degrading hides real information loss, so every such site emits a
//! [`ConversionDiagnostic`] (mirrors new-api's `ConversionDiagnostic`). Today the
//! diagnostics are surfaced through `tracing`; the type is public so a future
//! gateway layer can also attach them to a response.

use std::sync::{Arc, OnceLock, RwLock};

use serde::Serialize;

/// Severity of a conversion diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Informational — behaviour was adjusted but no semantic loss.
    Info,
    /// A field/feature was dropped or degraded.
    Warning,
    /// The conversion produced a knowingly incorrect result.
    Error,
}

/// A single conversion-side effect, with a machine-readable code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversionDiagnostic {
    /// Stable code, e.g. `unsupported_media_type`, `tool_choice_downgraded`.
    pub code: String,
    /// JSON-ish path of the affected field, e.g. `messages[0].content`.
    pub path: String,
    /// Human-readable explanation.
    pub message: String,
    /// Severity.
    pub severity: Severity,
}

impl ConversionDiagnostic {
    /// Builds a diagnostic.
    pub fn new(
        severity: Severity,
        code: impl Into<String>,
        path: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            path: path.into(),
            message: message.into(),
            severity,
        }
    }
}

/// A host-installed sink for structured collection.
///
/// [`Diagnostics`] is the per-conversion accumulator; this is the process-wide
/// fan-out for hosts (gateways, tests) that want every diagnostic without
/// plumbing a collector through every converter signature.
pub type DiagnosticSink = Arc<dyn Fn(&ConversionDiagnostic) + Send + Sync>;

static SINK: OnceLock<RwLock<Option<DiagnosticSink>>> = OnceLock::new();

fn sink_slot() -> &'static RwLock<Option<DiagnosticSink>> {
    SINK.get_or_init(|| RwLock::new(None))
}

/// Installs (or replaces) the process-wide diagnostic sink.
pub fn set_diagnostic_sink(sink: DiagnosticSink) {
    if let Ok(mut slot) = sink_slot().write() {
        *slot = Some(sink);
    }
}

/// Removes the process-wide diagnostic sink.
pub fn clear_diagnostic_sink() {
    if let Ok(mut slot) = sink_slot().write() {
        *slot = None;
    }
}

/// Records a diagnostic with `tracing` (the default sink).
pub fn emit(diagnostic: &ConversionDiagnostic) {
    if let Some(sink) = sink_slot().read().ok().and_then(|slot| slot.as_ref().cloned()) {
        sink(diagnostic);
    }
    match diagnostic.severity {
        Severity::Info => tracing::info!(
            code = %diagnostic.code,
            path = %diagnostic.path,
            "conversion diagnostic: {}",
            diagnostic.message
        ),
        Severity::Warning => tracing::warn!(
            code = %diagnostic.code,
            path = %diagnostic.path,
            "conversion diagnostic: {}",
            diagnostic.message
        ),
        Severity::Error => tracing::error!(
            code = %diagnostic.code,
            path = %diagnostic.path,
            "conversion diagnostic: {}",
            diagnostic.message
        ),
    }
}

/// Convenience: builds and emits a `warning` diagnostic.
pub fn warn(code: impl Into<String>, path: impl Into<String>, message: impl Into<String>) {
    emit(&ConversionDiagnostic::new(
        Severity::Warning,
        code,
        path,
        message,
    ));
}

/// Convenience: builds and emits an `info` diagnostic.
pub fn info(code: impl Into<String>, path: impl Into<String>, message: impl Into<String>) {
    emit(&ConversionDiagnostic::new(
        Severity::Info,
        code,
        path,
        message,
    ));
}

/// Accumulates diagnostics for callers that want to inspect them.
#[derive(Debug, Default, Clone)]
pub struct Diagnostics {
    items: Vec<ConversionDiagnostic>,
}

impl Diagnostics {
    /// Creates an empty collector.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a diagnostic (without emitting it).
    pub fn push(&mut self, diagnostic: ConversionDiagnostic) {
        self.items.push(diagnostic);
    }

    /// Records **and** emits a diagnostic.
    pub fn record(&mut self, diagnostic: ConversionDiagnostic) {
        emit(&diagnostic);
        self.items.push(diagnostic);
    }

    /// The collected diagnostics.
    pub fn as_slice(&self) -> &[ConversionDiagnostic] {
        &self.items
    }

    /// Whether anything was collected.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Consumes the collector, returning its diagnostics.
    pub fn into_vec(self) -> Vec<ConversionDiagnostic> {
        self.items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collector_records_without_emitting_twice() {
        let mut bag = Diagnostics::new();
        bag.push(ConversionDiagnostic::new(
            Severity::Warning,
            "unsupported_media_type",
            "messages[0].content",
            "audio part omitted",
        ));
        assert_eq!(bag.as_slice().len(), 1);
        assert_eq!(bag.as_slice()[0].code, "unsupported_media_type");
        assert!(!bag.is_empty());
    }

    #[test]
    fn severity_serialises_lowercase() {
        let value = serde_json::to_value(Severity::Warning).unwrap();
        assert_eq!(value, serde_json::json!("warning"));
    }

    #[test]
    fn installed_sink_receives_emitted_diagnostics() {
        let captured: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let sink_bag = captured.clone();
        set_diagnostic_sink(Arc::new(move |diagnostic| {
            if let Ok(mut guard) = sink_bag.lock() {
                guard.push(diagnostic.code.clone());
            }
        }));
        warn("tool_choice_downgraded", "tool_choice", "required -> auto");
        clear_diagnostic_sink();
        emit(&ConversionDiagnostic::new(
            Severity::Info,
            "after_clear",
            "",
            "no sink installed",
        ));

        let guard = captured.lock().unwrap();
        assert!(guard.contains(&"tool_choice_downgraded".to_string()));
        // Cleared sink no longer receives events.
        assert!(!guard.contains(&"after_clear".to_string()));
    }
}
