//! Conversion diagnostics.
//!
//! A converter cannot always preserve every detail of a request: a protocol may
//! have no equivalent for `tool_choice: required`, a remote media URL may be
//! unreadable, or a field may be dropped because the target does not support it.
//! Silently degrading hides real information loss, so every such site emits a
//! [`ConversionDiagnostic`] (mirrors new-api's `ConversionDiagnostic`). Today the
//! diagnostics are surfaced through `tracing`; the type is public so a future
//! gateway layer can also attach them to a response.

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

/// Records a diagnostic with `tracing` (the default sink).
pub fn emit(diagnostic: &ConversionDiagnostic) {
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
}
