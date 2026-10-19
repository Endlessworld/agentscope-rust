//! Module-owned Weixin failure surface plus host-facing safe diagnostics.
//!
//! Rust port of `WeixinOperationException` / `WeixinCredentialRejectedException` and the Java
//! channel's `safeMessage` helper. The rule the Java module states and this port keeps is that a
//! diagnostic handed to a host never carries provider text, a payload or a request URI: only
//! failures this module built itself (from constants and numeric protocol codes) may be reported
//! with their message; everything else is reported by its type alone.

use agentscope_core::error::Error as CoreError;
use thiserror::Error;

use crate::common::ChannelError;

/// Longest diagnostic handed to a host; keeps a pathological message bounded.
pub const MAX_SAFE_MESSAGE_LENGTH: usize = 160;

/// Result alias for the Weixin module's own fallible operations.
pub type WeixinResult<T> = std::result::Result<T, WeixinError>;

/// Failure raised by the Weixin adapter.
#[derive(Debug, Error)]
#[error(transparent)]
pub enum WeixinError {
    /// The provider answered with the `-14` credential-rejection code.
    #[error("iLink credential rejected during {operation} (code {code})")]
    CredentialRejected { operation: String, code: i32 },
    /// A module-owned operational failure. Messages must use constants and numeric protocol codes
    /// only, never provider text, payloads or request URIs.
    #[error("{0}")]
    Operation(String),
    /// A failure raised by the shared HTTP / property / crypto layer.
    #[error("{0}")]
    Transport(#[from] ChannelError),
}

impl WeixinError {
    /// Build an [`WeixinError::Operation`].
    pub fn operation(message: impl Into<String>) -> Self {
        Self::Operation(message.into())
    }

    /// Build an [`WeixinError::CredentialRejected`].
    pub fn credential_rejected(operation: impl Into<String>, code: i32) -> Self {
        Self::CredentialRejected {
            operation: operation.into(),
            code,
        }
    }

    /// Whether this failure means the current iLink credential can no longer be used.
    pub fn is_credential_rejected(&self) -> bool {
        matches!(self, Self::CredentialRejected { .. })
    }

    /// Project onto the shared channel error type (used where a `ChannelError` is the contract).
    pub fn into_channel(self) -> ChannelError {
        ChannelError::Parse(self.safe_message())
    }

    /// The diagnostic a host may see. Mirrors the Java `safeMessage`: module-owned failures keep
    /// their (re-sanitised) message, every other failure is named by type only.
    pub fn safe_message(&self) -> String {
        match self {
            Self::Transport(error) => format!("ChannelError({})", channel_error_kind(error)),
            Self::CredentialRejected { .. } => {
                let message = sanitize(&self.to_string());
                if message.is_empty() {
                    "WeixinCredentialRejectedException".to_string()
                } else {
                    format!("WeixinCredentialRejectedException: {message}")
                }
            }
            Self::Operation(message) => {
                let message = sanitize(message);
                if message.is_empty() {
                    "WeixinOperationException".to_string()
                } else {
                    format!("WeixinOperationException: {message}")
                }
            }
        }
    }
}

/// Drop ISO control characters and cap the length, mirroring the Java `safeMessage` scrubber.
pub fn sanitize(message: &str) -> String {
    let mut out = String::with_capacity(message.len().min(MAX_SAFE_MESSAGE_LENGTH));
    let mut used = 0usize;
    for c in message.chars() {
        if used >= MAX_SAFE_MESSAGE_LENGTH {
            break;
        }
        if !c.is_control() {
            out.push(c);
            used += 1;
        }
    }
    out
}

/// Static name of the shared channel error variant (no payload is copied).
pub fn channel_error_kind(error: &ChannelError) -> &'static str {
    match error {
        ChannelError::Http(_) => "Http",
        ChannelError::Json(_) => "Json",
        ChannelError::Config(_) => "Config",
        ChannelError::Parse(_) => "Parse",
        ChannelError::Unsupported(_) => "Unsupported",
        ChannelError::Crypto(_) => "Crypto",
        ChannelError::Remote { .. } => "Remote",
    }
}

/// Variant name of a core error, without copying its payload (the payload may quote provider text).
pub fn core_error_kind(error: &CoreError) -> String {
    let text = format!("{error:?}");
    text.split(['(', '{', ' '])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("Error")
        .to_string()
}

/// Map a Weixin failure onto the core error type used by `Channel::dispatch` / `deliver_reply`.
pub fn to_core_error(error: WeixinError) -> CoreError {
    CoreError::Other(error.safe_message())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_control_characters_and_caps_the_length() {
        assert_eq!(sanitize("a\u{1}b\ncd"), "abcd");
        let long = "x".repeat(MAX_SAFE_MESSAGE_LENGTH + 50);
        assert_eq!(sanitize(&long).chars().count(), MAX_SAFE_MESSAGE_LENGTH);
    }

    #[test]
    fn module_owned_failures_keep_their_message_and_foreign_ones_do_not() {
        assert_eq!(
            WeixinError::credential_rejected("getupdates", -14).safe_message(),
            "WeixinCredentialRejectedException: iLink credential rejected during getupdates (code -14)"
        );
        assert_eq!(
            WeixinError::operation("iLink sendmessage failed ret=1 errcode=2").safe_message(),
            "WeixinOperationException: iLink sendmessage failed ret=1 errcode=2"
        );
        // A transport failure may embed a URL, so only its type is reported.
        assert_eq!(
            WeixinError::from(ChannelError::Http(
                "error sending request for url (https://x/?token=secret)".to_string()
            ))
            .safe_message(),
            "ChannelError(Http)"
        );
    }

    #[test]
    fn credential_rejection_is_recognised_and_mapped_to_a_core_error() {
        let error = WeixinError::credential_rejected("notifystart", -14);
        assert!(error.is_credential_rejected());
        let mapped = to_core_error(error);
        assert!(mapped.to_string().contains("-14"));
    }

    #[test]
    fn core_error_kind_reports_only_the_variant_name() {
        let error = CoreError::Other("secret payload".to_string());
        assert_eq!(core_error_kind(&error), "Other");
    }
}
