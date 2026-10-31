//! Error type for the MEM extension crate.
//!
//! Mirrors the failures surfaced by the Java clients (`IOException` wrapping HTTP
//! status / transport / JSON problems). We deliberately do not reuse
//! `agentscope_core::Error` for transport details because the vendor clients need a
//! status-carrying error; callers can downgrade to `String` at the boundary.

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, MemError>;

/// Vendor label carried by an [`MemError::HttpStatus`] raised at the transport seam,
/// before any vendor client attached its own context.
///
/// Combined with [`UNSPECIFIED_OPERATION`] it keeps the transport-seam message
/// readable (`"memory API request failed with status …"`) instead of rendering empty
/// labels; every vendor client overwrites both with the Java names through
/// [`MemError::with_context`].
pub const UNSPECIFIED_VENDOR: &str = "memory";

/// Operation label for an [`MemError::HttpStatus`] raised at the transport seam.
pub const UNSPECIFIED_OPERATION: &str = "request";

/// Errors raised by the MEM vendor adapters.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MemError {
    /// Invalid client configuration (blank base URL, missing required field, ...).
    #[error("invalid memory configuration: {0}")]
    Config(String),

    /// Low-level transport failure (connect / timeout / decode).
    #[error("memory transport error: {0}")]
    Transport(String),

    /// Non-2xx HTTP response, mirroring the Java
    /// `"<Vendor> API <op> failed with status <code>: <body>"`.
    ///
    /// The vendor and operation labels are part of the message because that is the only
    /// place a Java caller sees them: `Mem0Client#executePostRaw`,
    /// `BailianMemoryClient#executePost` and `ReMeClient#executePost` all format
    /// `"<Vendor> API " + operationName + " failed with status " + code + ": " + body`.
    /// Without them a `401` cannot tell an `add` from a `search`.
    #[error("{vendor} API {operation} failed with status {status}: {body}")]
    HttpStatus {
        /// Vendor name (`"Mem0"` / `"Bailian"` / `"ReMe"`), or
        /// [`UNSPECIFIED_VENDOR`] for a failure raised by the transport seam.
        vendor: &'static str,
        /// Java operation name (`"add request"` / `"search memory"` /
        /// `"summary_personal_memory"` / ...), or [`UNSPECIFIED_OPERATION`].
        operation: &'static str,
        /// HTTP status code.
        status: u16,
        /// Raw response body (may be empty).
        body: String,
    },

    /// JSON serialization / deserialization error.
    #[error("memory JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

impl MemError {
    /// Builds an [`MemError::HttpStatus`] without vendor / operation context.
    ///
    /// The [`HttpTransport`](crate::http::HttpTransport) seam is vendor- and
    /// operation-agnostic, so it raises the failure through this constructor and each
    /// vendor client attaches the missing labels with [`MemError::with_context`].
    pub fn http_status(status: u16, body: impl Into<String>) -> Self {
        Self::HttpStatus {
            vendor: UNSPECIFIED_VENDOR,
            operation: UNSPECIFIED_OPERATION,
            status,
            body: body.into(),
        }
    }

    /// Attaches the vendor / operation labels the Java clients embed in their message.
    ///
    /// Only [`MemError::HttpStatus`] carries those labels (Java formats them in the
    /// status branch only); every other kind passes through unchanged.
    pub fn with_context(self, vendor: &'static str, operation: &'static str) -> Self {
        match self {
            Self::HttpStatus {
                status, body, ..
            } => Self::HttpStatus {
                vendor,
                operation,
                status,
                body,
            },
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_status_error_display_includes_status_and_body() {
        // The transport seam has no vendor/operation knowledge: the placeholders keep
        // the pre-MEM-02 wording so a bare transport failure still reads naturally.
        let err = MemError::http_status(401, "nope");
        assert_eq!(
            err.to_string(),
            "memory API request failed with status 401: nope"
        );
    }

    #[test]
    fn with_context_adds_the_vendor_and_operation_like_java() {
        let err = MemError::http_status(401, "bad key").with_context("Mem0", "add request");
        assert_eq!(
            err.to_string(),
            "Mem0 API add request failed with status 401: bad key"
        );
    }

    #[test]
    fn with_context_leaves_non_status_errors_untouched() {
        let err = MemError::Transport("down".into()).with_context("Mem0", "add request");
        assert_eq!(err.to_string(), "memory transport error: down");
    }

    #[test]
    fn config_and_transport_errors_display() {
        assert!(MemError::Config("bad".into()).to_string().contains("bad"));
        assert!(MemError::Transport("down".into())
            .to_string()
            .contains("down"));
    }

    #[test]
    fn serde_json_errors_convert_into_mem_error() {
        let err: MemError = serde_json::from_str::<serde_json::Value>("{")
            .unwrap_err()
            .into();
        assert!(matches!(err, MemError::Json(_)));
    }
}
