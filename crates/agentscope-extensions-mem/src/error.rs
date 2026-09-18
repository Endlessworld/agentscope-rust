//! Error type for the MEM extension crate.
//!
//! Mirrors the failures surfaced by the Java clients (`IOException` wrapping HTTP
//! status / transport / JSON problems). We deliberately do not reuse
//! `agentscope_core::Error` for transport details because the vendor clients need a
//! status-carrying error; callers can downgrade to `String` at the boundary.

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, MemError>;

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

    /// Non-2xx HTTP response, mirroring the Java `"<Vendor> API <op> failed with status <code>: <body>"`.
    #[error("memory API request failed with status {status}: {body}")]
    HttpStatus {
        /// HTTP status code.
        status: u16,
        /// Raw response body (may be empty).
        body: String,
    },

    /// JSON serialization / deserialization error.
    #[error("memory JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_status_error_display_includes_status_and_body() {
        let err = MemError::HttpStatus { status: 401, body: "nope".into() };
        assert_eq!(err.to_string(), "memory API request failed with status 401: nope");
    }

    #[test]
    fn config_and_transport_errors_display() {
        assert!(MemError::Config("bad".into()).to_string().contains("bad"));
        assert!(MemError::Transport("down".into()).to_string().contains("down"));
    }

    #[test]
    fn serde_json_errors_convert_into_mem_error() {
        let err: MemError =
            serde_json::from_str::<serde_json::Value>("{").unwrap_err().into();
        assert!(matches!(err, MemError::Json(_)));
    }
}
