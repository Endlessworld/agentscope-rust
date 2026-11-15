//! Transport failures and the JSON-RPC -> durable error classification.
//!
//! # Why a classification instead of a bare code
//!
//! The durable runtime does not ask "what did the peer say", it asks "may the effect have
//! happened, and can I try again". Those two answers are not derivable from an error code
//! alone unless somebody writes the table down, so this module does:
//!
//! | code   | name               | durable class | effect may have run | retryable |
//! |--------|--------------------|---------------|---------------------|-----------|
//! | -32700 | `PARSE_ERROR`      | `INVALID`     | no (local decode)   | no        |
//! | -32600 | `INVALID_REQUEST`  | `INVALID`     | no (local decode)   | no        |
//! | -32601 | `METHOD_NOT_FOUND` | `INCOMPATIBLE`| no                  | no        |
//! | -32602 | `INVALID_PARAMS`   | `INVALID`     | no                  | no        |
//! | -32603 | `INTERNAL_ERROR`   | `UNAVAILABLE` | **yes**             | yes       |
//! | -32001 | `TOOL_NOT_FOUND`   | `NOT_FOUND`   | no                  | no        |
//! | -32002 | `TOOL_UNAVAILABLE` | `UNAVAILABLE` | no                  | yes       |
//! | other  | `UNKNOWN`          | `UNAVAILABLE` | **yes**             | no        |
//!
//! Two notes on the table:
//!
//! * `-32700`/`-32600` are *reply-side* observations too: if the peer's answer cannot be
//!   decoded we cannot claim the tool did not run. [`TransportError::effect_may_have_run`]
//!   therefore reports `true` for them. The table above describes the *protocol* meaning used
//!   for [`classify`]; message-level effect knowledge is tracked by `TransportError`.
//! * An unknown code is classified conservatively: the peer failed, we do not understand
//!   why, so the outcome is `UNKNOWN` and the call must not be retried blindly.

use std::fmt;

use agentscope_durable::{DurableError, ErrorCode};

use crate::jsonrpc::{codes, ErrorObject};

/// Everything that can go wrong between building a request and trusting a reply.
#[derive(Debug, Clone, PartialEq)]
pub enum TransportError {
    /// The peer's bytes were not JSON (`-32700`).
    Parse(String),
    /// The peer's bytes were JSON but not a JSON-RPC 2.0 message (`-32600`).
    InvalidRequest(String),
    /// We could not encode our own message; nothing was sent, so nothing can have run.
    Encode(String),
    /// The peer answered with an error object.
    Rpc(ErrorObject),
    /// The reply was addressed elsewhere, or carried no usable payload.
    Protocol(String),
    /// No reply at all (closed pipe, timeout, refused connection).
    Transport(String),
    /// The tool is not in the peer's `tools/list`, so we refuse to call it (`-32001`).
    ToolNotAdvertised(String),
}

impl TransportError {
    /// The JSON-RPC code this failure corresponds to, when it has one.
    pub fn code(&self) -> Option<i64> {
        match self {
            Self::Parse(_) => Some(codes::PARSE_ERROR),
            Self::InvalidRequest(_) => Some(codes::INVALID_REQUEST),
            Self::Encode(_) => None,
            Self::Rpc(error) => Some(error.code),
            Self::Protocol(_) => None,
            Self::Transport(_) => None,
            Self::ToolNotAdvertised(_) => Some(codes::TOOL_NOT_FOUND),
        }
    }

    /// Whether the effect may already have happened despite this failure.
    ///
    /// Anything we cannot attribute to a deterministic rejection is reported as `true`:
    /// guessing "it did not run" is what makes a durable runtime double-apply a tool.
    pub fn effect_may_have_run(&self) -> bool {
        match self {
            Self::Parse(_) | Self::InvalidRequest(_) | Self::Protocol(_) | Self::Transport(_) => {
                true
            }
            Self::Encode(_) | Self::ToolNotAdvertised(_) => false,
            Self::Rpc(error) => classify(error.code).effect_may_have_run,
        }
    }

    /// The durable error a caller should surface for this failure.
    pub fn to_durable(&self) -> DurableError {
        match self {
            Self::Encode(_) | Self::ToolNotAdvertised(_) => DurableError::invalid(self.to_string()),
            Self::Rpc(error) => DurableError::new(classify(error.code).durable, self.to_string()),
            Self::Parse(_) | Self::InvalidRequest(_) | Self::Protocol(_) | Self::Transport(_) => {
                DurableError::unavailable(self.to_string())
            }
        }
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(message) => write!(formatter, "invalid JSON: {message}"),
            Self::InvalidRequest(message) => write!(formatter, "invalid JSON-RPC: {message}"),
            Self::Encode(message) => write!(formatter, "cannot encode: {message}"),
            Self::Rpc(error) => write!(
                formatter,
                "MCP error {} (`{}`): {}",
                error.code,
                classify(error.code).name,
                error.message
            ),
            Self::Protocol(message) => write!(formatter, "protocol violation: {message}"),
            Self::Transport(message) => write!(formatter, "transport failure: {message}"),
            Self::ToolNotAdvertised(name) => {
                write!(formatter, "tool `{name}` is not advertised by the peer")
            }
        }
    }
}

impl std::error::Error for TransportError {}

/// One row of the error-classification table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RpcErrorClass {
    /// The JSON-RPC code.
    pub code: i64,
    /// The stable name used in logs and error text.
    pub name: &'static str,
    /// How the durable runtime should classify the failure.
    pub durable: ErrorCode,
    /// Whether the effect may have run before the peer reported the failure.
    pub effect_may_have_run: bool,
    /// Whether re-sending the identical call can plausibly succeed.
    pub retryable: bool,
}

/// Classifies a JSON-RPC error code.
///
/// Unknown codes are classified as `UNKNOWN`/`UNAVAILABLE`/effect-unknown/not-retryable: the
/// peer failed for a reason we do not model, so the effect has an unknown outcome.
pub fn classify(code: i64) -> RpcErrorClass {
    match code {
        codes::PARSE_ERROR => RpcErrorClass {
            code,
            name: "PARSE_ERROR",
            durable: ErrorCode::Invalid,
            effect_may_have_run: false,
            retryable: false,
        },
        codes::INVALID_REQUEST => RpcErrorClass {
            code,
            name: "INVALID_REQUEST",
            durable: ErrorCode::Invalid,
            effect_may_have_run: false,
            retryable: false,
        },
        codes::METHOD_NOT_FOUND => RpcErrorClass {
            code,
            name: "METHOD_NOT_FOUND",
            durable: ErrorCode::Incompatible,
            effect_may_have_run: false,
            retryable: false,
        },
        codes::INVALID_PARAMS => RpcErrorClass {
            code,
            name: "INVALID_PARAMS",
            durable: ErrorCode::Invalid,
            effect_may_have_run: false,
            retryable: false,
        },
        codes::INTERNAL_ERROR => RpcErrorClass {
            code,
            name: "INTERNAL_ERROR",
            durable: ErrorCode::Unavailable,
            effect_may_have_run: true,
            retryable: true,
        },
        codes::TOOL_NOT_FOUND => RpcErrorClass {
            code,
            name: "TOOL_NOT_FOUND",
            durable: ErrorCode::NotFound,
            effect_may_have_run: false,
            retryable: false,
        },
        codes::TOOL_UNAVAILABLE => RpcErrorClass {
            code,
            name: "TOOL_UNAVAILABLE",
            durable: ErrorCode::Unavailable,
            effect_may_have_run: false,
            retryable: true,
        },
        _ => RpcErrorClass {
            code,
            name: "UNKNOWN",
            durable: ErrorCode::Unavailable,
            effect_may_have_run: true,
            retryable: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_five_standard_codes_map_to_durable_classes() {
        let expected = [
            (
                codes::PARSE_ERROR,
                "PARSE_ERROR",
                ErrorCode::Invalid,
                false,
                false,
            ),
            (
                codes::INVALID_REQUEST,
                "INVALID_REQUEST",
                ErrorCode::Invalid,
                false,
                false,
            ),
            (
                codes::METHOD_NOT_FOUND,
                "METHOD_NOT_FOUND",
                ErrorCode::Incompatible,
                false,
                false,
            ),
            (
                codes::INVALID_PARAMS,
                "INVALID_PARAMS",
                ErrorCode::Invalid,
                false,
                false,
            ),
            (
                codes::INTERNAL_ERROR,
                "INTERNAL_ERROR",
                ErrorCode::Unavailable,
                true,
                true,
            ),
        ];
        for (code, name, durable, effect, retryable) in expected {
            let class = classify(code);
            assert_eq!(class.code, code);
            assert_eq!(class.name, name);
            assert_eq!(class.durable, durable);
            assert_eq!(class.effect_may_have_run, effect);
            assert_eq!(class.retryable, retryable);
        }
    }

    #[test]
    fn local_and_unknown_codes_are_classified_conservatively() {
        let missing = classify(codes::TOOL_NOT_FOUND);
        assert_eq!(missing.durable, ErrorCode::NotFound);
        assert!(!missing.effect_may_have_run);

        let busy = classify(codes::TOOL_UNAVAILABLE);
        assert_eq!(busy.durable, ErrorCode::Unavailable);
        assert!(busy.retryable);
        assert!(!busy.effect_may_have_run);

        let unknown = classify(-31999);
        assert_eq!(unknown.name, "UNKNOWN");
        assert_eq!(unknown.durable, ErrorCode::Unavailable);
        assert!(unknown.effect_may_have_run);
        assert!(!unknown.retryable);
    }

    #[test]
    fn transport_errors_never_claim_a_clean_failure_by_default() {
        assert!(TransportError::Transport("pipe closed".into()).effect_may_have_run());
        assert!(TransportError::Protocol("odd reply".into()).effect_may_have_run());
        assert!(!TransportError::Encode("nope".into()).effect_may_have_run());
        assert!(!TransportError::ToolNotAdvertised("x".into()).effect_may_have_run());
        assert_eq!(
            TransportError::ToolNotAdvertised("x".into())
                .to_durable()
                .code(),
            ErrorCode::Invalid
        );
        assert_eq!(
            TransportError::Rpc(ErrorObject::method_not_found("tools/poke"))
                .to_durable()
                .code(),
            ErrorCode::Incompatible
        );
    }
}
