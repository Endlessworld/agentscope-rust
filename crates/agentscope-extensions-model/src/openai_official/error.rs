//! Error translation for the `openai-official` provider.
//!
//! Mirrors `OpenAIErrorTranslator` + `OpenAIOfficialModelException`:
//!
//! * HTTP-status-bearing failures keep their status, message, `type` and `code`.
//! * `408`, `409`, `429` and `5xx` are retryable (the official SDK retries the
//!   same set); the core [`ModelError`] classifier only knows 429/5xx, so
//!   [`translate_core`] routes a retryable 408/409 through [`Error::Http`].
//! * Transport / parsing failures carry no status and are non-retryable unless
//!   the caller is explicit about a timeout.
//!
//! Unlike the Java translator (which receives typed SDK exceptions), the Rust
//! adapter speaks the REST protocol through [`crate::common::HttpTransport`], so
//! the input here is the raw `{status, type, code, message}` tuple the transport
//! and stream assembler observe.

use agentscope_core::error::Error;

use crate::common::error::{ErrorKind, ModelError};

use super::constants::{is_retryable_http_status, PROVIDER_ID};

/// Normalised provider error for `openai-official`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAIOfficialModelError {
    /// HTTP status, or `None` for transport/parsing failures.
    pub status: Option<u16>,
    /// Provider error type (e.g. `rate_limit_error`).
    pub error_type: Option<String>,
    /// Provider error code (e.g. `rate_limit_exceeded`).
    pub code: Option<String>,
    /// Human-readable message (never blank).
    pub message: String,
}

impl OpenAIOfficialModelError {
    /// Mirrors `OpenAIOfficialModelException.isRetryableHttpStatus`.
    pub fn is_retryable_http_status(&self) -> bool {
        self.status.map(is_retryable_http_status).unwrap_or(false)
    }

    /// Whether the error is retryable under the official provider policy.
    pub fn is_retryable(&self) -> bool {
        self.is_retryable_http_status()
            || self.status.is_none()
                && matches!(
                    classify_without_status(&self.message),
                    ErrorKind::Timeout | ErrorKind::Network | ErrorKind::RateLimit
                )
    }

    /// Converts into the shared structured error.
    pub fn to_model_error(&self) -> ModelError {
        let mut err = match self.status {
            Some(status) => ModelError::from_parts(
                PROVIDER_ID,
                status,
                self.message.clone(),
                self.error_type.clone(),
                self.code.clone(),
            ),
            None => {
                let mut err = ModelError::new(PROVIDER_ID, 0, self.message.clone());
                err.error_type = self.error_type.clone();
                err.code = self.code.clone();
                err
            }
        };
        // Common's status classifier treats 409 as a permission failure; the
        // official SDK retries it. Keep the structured fields but normalise the
        // coarse kind so callers that inspect `kind` see a retryable category.
        if self.is_retryable_http_status() && !err.retryable() {
            err.kind = ErrorKind::Server;
        }
        if self.status.is_none() {
            err.kind = classify_without_status(&self.message);
        }
        err
    }

    /// Converts into a core [`Error`], preserving retryability.
    pub fn to_core(&self) -> Error {
        let err = self.to_model_error();
        if self.is_retryable_http_status() && !matches!(err.status, 429 | 500..=599) {
            // 408/409 are not in core's retryable-status set; surface them as a
            // transport-class failure so the agent retry loop still fires.
            return Error::Http(err.display_message());
        }
        err.to_core()
    }
}

/// Translates a status-bearing API error.
pub fn translate(
    status: u16,
    message: impl Into<String>,
    error_type: Option<String>,
    code: Option<String>,
) -> OpenAIOfficialModelError {
    OpenAIOfficialModelError {
        status: Some(status),
        error_type,
        code,
        message: safe_message(Some(&message.into()), Some(status)),
    }
}

/// Translates an error that may or may not carry an HTTP status.
pub fn translate_optional_status(
    status: Option<u16>,
    message: Option<&str>,
    error_type: Option<String>,
    code: Option<String>,
) -> OpenAIOfficialModelError {
    OpenAIOfficialModelError {
        status,
        error_type,
        code,
        message: safe_message(message, status),
    }
}

/// Translates a raw error body (`{"error": {...}}` / flat variants).
pub fn translate_body(status: u16, body: &str) -> OpenAIOfficialModelError {
    let parsed = ModelError::parse(PROVIDER_ID, status, body);
    OpenAIOfficialModelError {
        status: Some(status),
        error_type: parsed.error_type,
        code: parsed.code,
        message: safe_message(Some(&parsed.message), Some(status)),
    }
}

/// Translates a status-bearing error directly into a core [`Error`].
pub fn translate_core(
    status: u16,
    message: impl Into<String>,
    error_type: Option<String>,
    code: Option<String>,
) -> Error {
    translate(status, message, error_type, code).to_core()
}

/// Whether an already-normalised error is retryable for this provider.
pub fn is_retryable(err: &ModelError) -> bool {
    is_retryable_http_status(err.status) || err.retryable()
}

/// Mirrors `OpenAIErrorTranslator.safeMessage`.
fn safe_message(message: Option<&str>, status: Option<u16>) -> String {
    if let Some(message) = message.map(str::trim).filter(|m| !m.is_empty()) {
        return message.to_string();
    }
    match status {
        Some(status) => format!("OpenAI API error (HTTP {status})"),
        None => "OpenAI API error".to_string(),
    }
}

fn classify_without_status(message: &str) -> ErrorKind {
    let lower = message.to_ascii_lowercase();
    if lower.contains("timeout") || lower.contains("timed out") {
        ErrorKind::Timeout
    } else if lower.contains("rate limit") || lower.contains("rate_limit") {
        ErrorKind::RateLimit
    } else if lower.contains("network")
        || lower.contains("connection")
        || lower.contains("error sending request")
    {
        ErrorKind::Network
    } else {
        ErrorKind::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_error_preserves_type_code_and_status() {
        let err = translate(
            429,
            "slow down",
            Some("rate_limit_error".to_string()),
            Some("rate_limit_exceeded".to_string()),
        );
        let model = err.to_model_error();
        assert_eq!(model.provider, "openai-official");
        assert_eq!(model.status, 429);
        assert_eq!(model.error_type.as_deref(), Some("rate_limit_error"));
        assert_eq!(model.code.as_deref(), Some("rate_limit_exceeded"));
        assert_eq!(model.kind, ErrorKind::RateLimit);
        assert!(err.is_retryable());
        let core = err.to_core();
        assert_eq!(core.http_status_code(), Some(429));
        assert!(core.is_retryable());
    }

    #[test]
    fn official_retryable_status_set_includes_408_and_409() {
        for status in [408u16, 409, 429, 500, 503, 599] {
            assert!(is_retryable_http_status(status), "status {status}");
        }
        for status in [400u16, 401, 403, 404, 422] {
            assert!(!is_retryable_http_status(status), "status {status}");
        }

        let conflict = translate(409, "conflict", None, None);
        assert!(conflict.is_retryable());
        // Core's coarse classifier says Permission; the override must still make
        // the surfaced error retryable (official SDK policy).
        assert!(conflict.to_core().is_retryable());

        let timeout = translate(408, "request timeout", None, None);
        assert!(timeout.to_core().is_retryable());
    }

    #[test]
    fn non_retryable_auth_error_keeps_status() {
        let auth = translate(401, "invalid api key", None, None);
        assert!(!auth.is_retryable());
        let core = auth.to_core();
        assert_eq!(core.http_status_code(), Some(401));
        assert!(!core.is_retryable());
    }

    #[test]
    fn raw_body_variants_are_parsed() {
        let nested = translate_body(
            400,
            r#"{"error":{"message":"bad tools","type":"invalid_request_error","code":"x"}}"#,
        );
        assert_eq!(nested.message, "bad tools");
        assert_eq!(nested.error_type.as_deref(), Some("invalid_request_error"));
        assert_eq!(nested.code.as_deref(), Some("x"));

        let flat = translate_body(500, r#"{"msg":"boom"}"#);
        assert_eq!(flat.message, "boom");
        assert!(flat.to_core().is_retryable());
    }

    #[test]
    fn blank_message_falls_back_like_java() {
        assert_eq!(
            safe_message(Some("   "), Some(503)),
            "OpenAI API error (HTTP 503)"
        );
        assert_eq!(safe_message(None, None), "OpenAI API error");
        assert_eq!(safe_message(Some("real"), Some(503)), "real");
    }

    #[test]
    fn statusless_timeout_is_retryable() {
        let err = translate_optional_status(None, Some("operation timed out"), None, None);
        assert!(err.is_retryable());
        assert!(err.to_core().is_retryable());
    }
}
