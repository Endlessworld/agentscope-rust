//! Normalised model-API error handling.
//!
//! Upstream providers disagree on how they shape an error body. Some nest it as
//! `{"error": {"message": ..., "type": ..., "code": ...}}` (OpenAI), some use a
//! bare `{"message": ...}` / `{"msg": ...}` / `{"errorMsg": ...}` / `{"detail":
//! ...}` (various Chinese vendors and gateways), and some wrap it in
//! `{"header": {"message": ...}}`. Streaming responses may additionally carry an
//! `error` frame after HTTP 200.
//!
//! [`ModelError::parse`] collapses all of those into one [`ModelError`], so
//! retry decisions and user-facing messages stop depending on the raw body being
//! dumped verbatim into an opaque error string (mirrors new-api's
//! `GeneralErrorResponse` / `NewAPIError`).

use agentscope_core::error::Error;
use serde_json::Value;

/// Coarse classification of an upstream model-API failure.
///
/// The classification drives retry behaviour and lets callers branch without
/// string-matching provider messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Missing or invalid credentials (HTTP 401).
    Authentication,
    /// Authenticated but not allowed (HTTP 403).
    Permission,
    /// Temporarily rate limited (HTTP 429).
    RateLimit,
    /// Out of quota / billing issue — retrying will not help.
    Quota,
    /// Request exceeded the model context window.
    ContextLength,
    /// Malformed request (bad parameters, invalid tool schema, ...).
    InvalidRequest,
    /// Model / endpoint not found.
    NotFound,
    /// Blocked by a content filter or safety system.
    ContentFilter,
    /// Upstream overloaded (e.g. Anthropic 529).
    Overloaded,
    /// Unspecified upstream server failure (5xx).
    Server,
    /// Network timeout / gateway timeout.
    Timeout,
    /// Transport-level failure (connection reset, DNS, TLS, ...).
    Network,
    /// Caller cancelled the request.
    Cancelled,
    /// Anything not covered above.
    Unknown,
}

impl ErrorKind {
    /// Stable lower-case identifier used in messages and logs.
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorKind::Authentication => "authentication_error",
            ErrorKind::Permission => "permission_error",
            ErrorKind::RateLimit => "rate_limit_error",
            ErrorKind::Quota => "quota_error",
            ErrorKind::ContextLength => "context_length_exceeded",
            ErrorKind::InvalidRequest => "invalid_request_error",
            ErrorKind::NotFound => "not_found_error",
            ErrorKind::ContentFilter => "content_filter_error",
            ErrorKind::Overloaded => "overloaded_error",
            ErrorKind::Server => "server_error",
            ErrorKind::Timeout => "timeout_error",
            ErrorKind::Network => "network_error",
            ErrorKind::Cancelled => "cancelled",
            ErrorKind::Unknown => "unknown_error",
        }
    }

    /// Whether an error of this kind is worth retrying as-is.
    ///
    /// Mirrors new-api's retry gating: transient capacity / connectivity
    /// failures are retryable; auth, quota, context-length, invalid-request and
    /// content-filter failures are not.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            ErrorKind::RateLimit
                | ErrorKind::Overloaded
                | ErrorKind::Server
                | ErrorKind::Timeout
                | ErrorKind::Network
        )
    }
}

/// A structured, provider-agnostic model-API error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelError {
    /// Provider id that produced the error (e.g. `openai`, `gemini`).
    pub provider: String,
    /// HTTP status, or `0` when the failure happened mid-stream.
    pub status: u16,
    /// Coarse classification.
    pub kind: ErrorKind,
    /// Provider-specific error code, when present.
    pub code: Option<String>,
    /// Provider-specific error type, when present.
    pub error_type: Option<String>,
    /// Human-readable message, extracted from whichever field carried it.
    pub message: String,
}

impl ModelError {
    /// Builds an error from already-known parts.
    pub fn new(
        provider: impl Into<String>,
        status: u16,
        message: impl Into<String>,
    ) -> Self {
        let provider = provider.into();
        let message = message.into();
        let kind = classify(status, &message, None, None);
        Self { provider, status, kind, code: None, error_type: None, message }
    }

    /// Builds an error from explicitly supplied provider fields (no JSON body).
    pub fn from_parts(
        provider: impl Into<String>,
        status: u16,
        message: impl Into<String>,
        error_type: Option<String>,
        code: Option<String>,
    ) -> Self {
        let provider = provider.into();
        let message = message.into();
        let kind = classify(status, &message, code.as_deref(), error_type.as_deref());
        let err = Self { provider, status, kind, code, error_type, message };
        err.log();
        err
    }

    /// Whether this error is worth retrying.
    pub fn retryable(&self) -> bool {
        self.kind.retryable()
    }

    /// Parses an upstream HTTP error body into a [`ModelError`].
    ///
    /// Never fails: unknown shapes degrade to the raw (truncated) body text.
    pub fn parse(provider: &str, status: u16, body: &str) -> Self {
        let value: Option<Value> = serde_json::from_str(body).ok();
        if let Some(value) = value {
            let message = extract_message(&value).unwrap_or_else(|| truncate(body));
            let code = extract_code(&value);
            let error_type = extract_type(&value);
            let kind = classify(status, &message, code.as_deref(), error_type.as_deref());
            let err = Self {
                provider: provider.to_string(),
                status,
                kind,
                code,
                error_type,
                message,
            };
            err.log();
            return err;
        }
        Self::new(provider, status, truncate(body))
    }

    /// Parses a mid-stream `error` frame (HTTP 200 already sent).
    pub fn parse_stream_value(provider: &str, value: &Value) -> Self {
        let message = extract_message(value).unwrap_or_else(|| value.to_string());
        let code = extract_code(value);
        let error_type = extract_type(value);
        let kind = classify(0, &message, code.as_deref(), error_type.as_deref());
        let err = Self {
            provider: provider.to_string(),
            status: 0,
            kind,
            code,
            error_type,
            message,
        };
        err.log();
        err
    }

    /// Renders the normalised human-readable message.
    pub fn display_message(&self) -> String {
        let mut suffix: Vec<String> = Vec::new();
        suffix.push(format!("kind={}", self.kind.as_str()));
        if let Some(t) = &self.error_type {
            suffix.push(format!("type={t}"));
        }
        if let Some(c) = &self.code {
            suffix.push(format!("code={c}"));
        }
        format!("[{}] {} ({})", self.provider, self.message, suffix.join(", "))
    }

    /// Converts into the core error type, preserving both the status code and
    /// the retryability classification.
    ///
    /// `agentscope_core::Error::is_retryable` only retries 429/5xx
    /// [`Error::HttpStatus`], so a retryable failure that carries no such status
    /// (a transport failure, whose status is 0, or a 408 the gateway reported)
    /// is surfaced as [`Error::Http`] — always retryable. Without this the
    /// agent's model retry loop never fires for those failures.
    pub fn to_core(&self) -> Error {
        if self.retryable() && !matches!(self.status, 429 | 500..=599) {
            return Error::Http(self.display_message());
        }
        Error::http_status(self.status, self.display_message())
    }

    /// Best-effort reconstruction of a [`ModelError`] from a core error.
    pub fn from_core(err: &Error) -> Option<Self> {
        match err {
            Error::HttpStatus { status, message } => {
                Some(Self::new("model", *status, message.clone()))
            }
            Error::Http(message) => Some(Self {
                provider: "model".to_string(),
                status: 0,
                kind: ErrorKind::Network,
                code: None,
                error_type: None,
                message: message.clone(),
            }),
            _ => None,
        }
    }

    pub(crate) fn log(&self) {
        if self.retryable() {
            tracing::warn!(
                provider = %self.provider,
                status = self.status,
                kind = self.kind.as_str(),
                code = ?self.code,
                error_type = ?self.error_type,
                "retryable model API error: {}",
                self.message
            );
        } else {
            tracing::error!(
                provider = %self.provider,
                status = self.status,
                kind = self.kind.as_str(),
                code = ?self.code,
                error_type = ?self.error_type,
                "model API error: {}",
                self.message
            );
        }
    }
}

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display_message())
    }
}

impl std::error::Error for ModelError {}

impl From<ModelError> for Error {
    fn from(err: ModelError) -> Self {
        err.to_core()
    }
}

fn truncate(body: &str) -> String {
    const MAX: usize = 500;
    let trimmed = body.trim();
    if trimmed.chars().count() <= MAX {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(MAX).collect();
    format!("{head}…")
}

/// Looks up the first present message among the field shapes vendors use.
fn extract_message(v: &Value) -> Option<String> {
    const PATHS: &[&[&str]] = &[
        &["error", "message"],
        &["response", "error", "message"],
        &["header", "message"],
        &["error", "msg"],
        &["message"],
        &["msg"],
        &["errorMsg"],
        &["error_msg"],
        &["err"],
        &["detail"],
        &["error"],
    ];
    for path in PATHS {
        if let Some(s) = lookup(v, path).and_then(value_to_text) {
            let s = s.trim().to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

fn extract_code(v: &Value) -> Option<String> {
    for path in [&["error", "code"][..], &["code"][..], &["status_code"][..]] {
        if let Some(text) = lookup(v, path).and_then(value_to_text) {
            if !text.trim().is_empty() {
                return Some(text);
            }
        }
    }
    None
}

fn extract_type(v: &Value) -> Option<String> {
    for path in [
        &["error", "type"][..],
        &["type"][..],
        &["error", "status"][..],
        &["status"][..],
    ] {
        if let Some(text) = lookup(v, path).and_then(value_to_text) {
            if !text.trim().is_empty() {
                return Some(text);
            }
        }
    }
    None
}

fn lookup<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = v;
    for key in path {
        current = current.get(*key)?;
    }
    if current.is_null() {
        None
    } else {
        Some(current)
    }
}

fn value_to_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Object(_) => {
            // `{"error": {"message": ...}}` already handled by path order; as a
            // last resort pull a nested `message` out of the object.
            v.get("message")
                .or_else(|| v.get("msg"))
                .and_then(value_to_text)
        }
        _ => None,
    }
}

/// Classifies an error from its status plus any provider-provided hints.
fn classify(
    status: u16,
    message: &str,
    code: Option<&str>,
    error_type: Option<&str>,
) -> ErrorKind {
    // Provider-supplied type/status strings are authoritative when recognised,
    // because they disambiguate `status == 0` (mid-stream) frames and let a
    // "server_error"/"UNAVAILABLE" be retried even when the HTTP status is 200.
    if let Some(t) = error_type {
        match t.trim().to_ascii_lowercase().as_str() {
            "authentication_error" | "invalid_api_key" | "unauthenticated" => {
                return ErrorKind::Authentication
            }
            "permission_error" | "permission_denied" => return ErrorKind::Permission,
            "not_found_error" | "not_found" => return ErrorKind::NotFound,
            "rate_limit_error" | "rate_limit_exceeded" | "resource_exhausted" => {
                return ErrorKind::RateLimit
            }
            "insufficient_quota" => return ErrorKind::Quota,
            "overloaded_error" | "overloaded" => return ErrorKind::Overloaded,
            "server_error" | "internal_server_error" | "internal" | "unavailable" => {
                return ErrorKind::Server
            }
            "invalid_request_error" | "invalid_argument" => return ErrorKind::InvalidRequest,
            "deadline_exceeded" => return ErrorKind::Timeout,
            _ => {}
        }
    }

    let haystack = format!(
        "{} {} {}",
        message.to_ascii_lowercase(),
        code.unwrap_or("").to_ascii_lowercase(),
        error_type.unwrap_or("").to_ascii_lowercase()
    );
    let contains = |needles: &[&str]| needles.iter().any(|n| haystack.contains(n));

    if contains(&["insufficient_quota", "exceeded your current quota", "billing", "quota"])
        && (status == 429 || status == 402 || status == 403)
    {
        return ErrorKind::Quota;
    }
    if contains(&["content_filter", "content policy", "safety", "blocked by"]) {
        return ErrorKind::ContentFilter;
    }
    if status == 400
        && contains(&[
            "context length",
            "context_length",
            "maximum context",
            "max context",
            "too many tokens",
            "token limit",
            "reduce the length",
        ])
    {
        return ErrorKind::ContextLength;
    }
    match status {
        400 | 422 => ErrorKind::InvalidRequest,
        401 => ErrorKind::Authentication,
        402 | 403 => ErrorKind::Permission,
        404 => ErrorKind::NotFound,
        408 | 504 | 524 => ErrorKind::Timeout,
        429 => ErrorKind::RateLimit,
        529 => ErrorKind::Overloaded,
        500..=599 => {
            if contains(&["overloaded", "overload"]) {
                ErrorKind::Overloaded
            } else {
                ErrorKind::Server
            }
        }
        0 => {
            if contains(&["overloaded", "overload"]) {
                ErrorKind::Overloaded
            } else if contains(&["rate limit", "rate_limit"]) {
                ErrorKind::RateLimit
            } else if contains(&["timeout", "timed out"]) {
                ErrorKind::Timeout
            } else {
                ErrorKind::Unknown
            }
        }
        _ => ErrorKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_nested_error() {
        let err = ModelError::parse(
            "openai",
            429,
            r#"{"error":{"message":"Rate limit reached","type":"rate_limit_error","code":"rate_limit_exceeded"}}"#,
        );
        assert_eq!(err.kind, ErrorKind::RateLimit);
        assert!(err.retryable());
        assert_eq!(err.message, "Rate limit reached");
        assert_eq!(err.error_type.as_deref(), Some("rate_limit_error"));
        assert_eq!(err.code.as_deref(), Some("rate_limit_exceeded"));
    }

    #[test]
    fn parses_alternative_message_fields() {
        for body in [
            r#"{"message":"boom"}"#,
            r#"{"msg":"boom"}"#,
            r#"{"errorMsg":"boom"}"#,
            r#"{"detail":"boom"}"#,
            r#"{"error":"boom"}"#,
            r#"{"header":{"message":"boom"}}"#,
            r#"{"response":{"error":{"message":"boom"}}}"#,
        ] {
            let err = ModelError::parse("gateway", 500, body);
            assert_eq!(err.message, "boom", "body={body}");
            assert_eq!(err.kind, ErrorKind::Server);
            assert!(err.retryable());
        }
    }

    #[test]
    fn classifies_quota_vs_rate_limit() {
        let quota = ModelError::parse(
            "openai",
            429,
            r#"{"error":{"message":"You exceeded your current quota","type":"insufficient_quota"}}"#,
        );
        assert_eq!(quota.kind, ErrorKind::Quota);
        assert!(!quota.retryable());
    }

    #[test]
    fn classifies_context_length_and_auth() {
        let ctx = ModelError::parse(
            "openai",
            400,
            r#"{"error":{"message":"This model's maximum context length is 8192 tokens"}}"#,
        );
        assert_eq!(ctx.kind, ErrorKind::ContextLength);
        assert!(!ctx.retryable());

        let auth = ModelError::parse("openai", 401, r#"{"error":{"message":"Invalid API key"}}"#);
        assert_eq!(auth.kind, ErrorKind::Authentication);
        assert!(!auth.retryable());
    }

    #[test]
    fn non_json_body_degrades_to_raw_text() {
        let err = ModelError::parse("gemini", 503, "upstream unavailable");
        assert_eq!(err.message, "upstream unavailable");
        assert_eq!(err.kind, ErrorKind::Server);
    }

    /// A transport failure (no HTTP status at all) must stay retryable after the
    /// conversion to the core error, otherwise the agent's model retry loop does
    /// not fire and the timeout fails the whole turn.
    #[test]
    fn retryable_statusless_error_stays_retryable_in_core() {
        let mut timeout = ModelError::new(
            "openai",
            0,
            "error sending request for url (https://example.invalid/v1/chat/completions)",
        );
        timeout.kind = ErrorKind::Timeout;
        assert!(timeout.retryable());
        assert!(timeout.to_core().is_retryable(), "timeout must be retried");

        // Same shape, but a kind that is not worth retrying keeps failing fast.
        let mut invalid = ModelError::new("openai", 0, "invalid tool schema");
        invalid.kind = ErrorKind::InvalidRequest;
        assert!(!invalid.to_core().is_retryable());
    }

    /// The transport failure reported against the `opencode.ai/zen` gateway
    /// (`HTTP error 0: [openai] error sending request for url (...)
    /// (kind=network_error)`) carries no HTTP status, so it must be converted to
    /// a retryable core error — otherwise the whole turn dies on a network blip.
    #[test]
    fn gateway_network_error_is_retried() {
        let mut network = ModelError::new(
            "openai",
            0,
            "error sending request for url (https://opencode.ai/zen/go/v1/chat/completions)",
        );
        network.kind = ErrorKind::Network;
        assert_eq!(network.kind.as_str(), "network_error");
        assert!(network.retryable());
        assert!(
            network.to_core().is_retryable(),
            "a transport failure must be retried"
        );
    }

    /// A retryable status that `Error::is_retryable` does not recognise (408) is
    /// carried through as a transport error; 429/5xx keep their status code.
    #[test]
    fn retryable_status_is_preserved_for_core() {
        let gateway_timeout = ModelError::new("gateway", 408, "request timeout");
        assert_eq!(gateway_timeout.kind, ErrorKind::Timeout);
        assert!(gateway_timeout.to_core().is_retryable());

        let rate_limited = ModelError::parse(
            "openai",
            429,
            r#"{"error":{"message":"slow down","type":"rate_limit_error"}}"#,
        );
        let core = rate_limited.to_core();
        assert_eq!(core.http_status_code(), Some(429));
        assert!(core.is_retryable());

        let auth = ModelError::parse("openai", 401, r#"{"error":{"message":"nope"}}"#);
        let core = auth.to_core();
        assert_eq!(core.http_status_code(), Some(401));
        assert!(!core.is_retryable());
    }

    #[test]
    fn overloaded_error_is_retryable() {
        let err = ModelError::new("anthropic", 529, "Overloaded");
        assert_eq!(err.kind, ErrorKind::Overloaded);
        assert!(err.retryable());
    }

    #[test]
    fn display_message_includes_hints() {
        let err = ModelError::parse(
            "openai",
            429,
            r#"{"error":{"message":"slow down","type":"rate_limit_error","code":"x"}}"#,
        );
        let rendered = err.display_message();
        assert!(rendered.contains("[openai]"));
        assert!(rendered.contains("kind=rate_limit_error"));
        assert!(rendered.contains("type=rate_limit_error"));
        assert!(rendered.contains("code=x"));
    }
}
