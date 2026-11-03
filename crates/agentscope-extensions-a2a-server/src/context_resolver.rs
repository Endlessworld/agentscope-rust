//! Deployment-side adjudication of the user identity of an A2A call.
//!
//! Mirrors `io.agentscope.core.a2a.server.executor.A2aContextResolver`. A2A is
//! server-to-server: the deployment can read an already verified identity from the transport
//! (an mTLS certificate, an OAuth token, ...) through [`A2aCallContext::headers`]. The user id a
//! client asserts in the message envelope (`_agentscope_user_id`) is a delegation claim - it
//! names whom the caller acts for - and must not override a transport-verified identity.
//!
//! Priority: resolver (transport-verified) > envelope-asserted user id > none.

use std::fmt;

use crate::transport::TransportHeaders;

/// The transport-level context of one A2A call, handed to an [`A2aContextResolver`].
#[derive(Debug, Clone)]
pub struct A2aCallContext {
    headers: TransportHeaders,
    method: String,
}

impl A2aCallContext {
    /// Builds the context of one call from its transport headers and JSON-RPC method.
    pub fn new(headers: TransportHeaders, method: impl Into<String>) -> Self {
        Self {
            headers,
            method: method.into(),
        }
    }

    /// The context of an in-process call, which carries no transport headers.
    pub fn in_process(method: impl Into<String>) -> Self {
        Self::new(TransportHeaders::new(), method)
    }

    /// The request headers, e.g. the `authorization` header or a client certificate hint.
    pub fn headers(&self) -> &TransportHeaders {
        &self.headers
    }

    /// One request header, matched case-insensitively (HTTP header names are not case
    /// sensitive, and the casing depends on the transport that filled the map).
    pub fn header(&self, name: &str) -> Option<&str> {
        if let Some(value) = self.headers.get(name) {
            return Some(value.as_str());
        }
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The JSON-RPC method of the call.
    pub fn method(&self) -> &str {
        &self.method
    }
}

/// The identity of one call, as resolved by the deployment.
///
/// Only the user id is adjudicated: the session id still comes from the protocol `contextId`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct A2aIdentity {
    user_id: String,
}

impl A2aIdentity {
    /// Creates an identity for `user_id`.
    pub fn new(user_id: impl Into<String>) -> Self {
        Self {
            user_id: user_id.into(),
        }
    }

    /// The transport-verified user id.
    pub fn user_id(&self) -> &str {
        &self.user_id
    }
}

/// The error a resolver reports when it cannot verify the caller.
///
/// A resolver that fails must report the failure instead of returning `Ok(None)`, so the call
/// is rejected (fail-closed) rather than downgraded to the unverified envelope assertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct A2aResolveError {
    message: String,
}

impl A2aResolveError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for A2aResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for A2aResolveError {}

/// Resolves the identity of an A2A call from its transport context.
///
/// A resolver returning `Ok(None)`, or an identity with a blank user id, deliberately keeps the
/// envelope-asserted identity: verification passed without producing an identity of its own.
/// A resolver whose verification *fails* returns an error, which rejects the call.
///
/// ```text
/// let resolver = |context: &A2aCallContext| {
///     match verified_user_id_of(context.header("authorization")) {
///         Some(user_id) => Ok(Some(A2aIdentity::new(user_id))),
///         None => Err(A2aResolveError::new("unknown caller")),
///     }
/// };
/// ```
pub trait A2aContextResolver: Send + Sync {
    /// Adjudicates the identity of one call.
    fn resolve(&self, context: &A2aCallContext) -> Result<Option<A2aIdentity>, A2aResolveError>;
}

impl<F> A2aContextResolver for F
where
    F: Fn(&A2aCallContext) -> Result<Option<A2aIdentity>, A2aResolveError> + Send + Sync,
{
    fn resolve(&self, context: &A2aCallContext) -> Result<Option<A2aIdentity>, A2aResolveError> {
        self(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(entries: &[(&str, &str)]) -> TransportHeaders {
        entries
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn a_header_is_matched_case_insensitively() {
        let context = A2aCallContext::new(headers(&[("Authorization", "Bearer t")]), "SendMessage");

        assert_eq!(context.header("authorization"), Some("Bearer t"));
        assert_eq!(context.header("x-missing"), None);
        assert_eq!(context.method(), "SendMessage");
        assert_eq!(context.headers().len(), 1);

        // An in-process call carries the method but no transport headers.
        let in_process = A2aCallContext::in_process("GetTask");
        assert_eq!(in_process.method(), "GetTask");
        assert!(in_process.headers().is_empty());
    }

    #[test]
    fn a_resolve_error_renders_its_message() {
        let error = A2aResolveError::new("unknown caller");
        assert_eq!(error.message(), "unknown caller");
        assert_eq!(error.to_string(), "unknown caller");
    }

    #[test]
    fn a_closure_is_a_resolver() {
        let resolver = |context: &A2aCallContext| -> Result<Option<A2aIdentity>, A2aResolveError> {
            Ok(context
                .header("x-user-id")
                .map(|user_id| A2aIdentity::new(user_id)))
        };

        let context = A2aCallContext::new(headers(&[("x-user-id", "alice")]), "SendMessage");
        assert_eq!(
            resolver.resolve(&context).unwrap().unwrap().user_id(),
            "alice"
        );
        assert_eq!(
            resolver
                .resolve(&A2aCallContext::new(TransportHeaders::new(), "SendMessage"))
                .unwrap(),
            None
        );
    }
}
