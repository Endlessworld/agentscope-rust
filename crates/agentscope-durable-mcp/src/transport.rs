//! The one port a real MCP connection has to implement.

use async_trait::async_trait;

use crate::error::TransportError;
use crate::jsonrpc::{Request, Response};

/// Carries one JSON-RPC request to an MCP peer and returns its reply.
///
/// The trait is deliberately request/response only: it is the exact shape the durable
/// executor needs, and it keeps stdio/SSE/HTTP implementations from leaking their framing
/// into the durable layer. Implementations are responsible for their own I/O and must not
/// retry: a retry is a durable decision (it re-reads the execution's effect knowledge), not
/// a transport one.
#[async_trait]
pub trait McpTransport: Send + Sync {
    /// Sends `request` and waits for the reply addressed to it.
    ///
    /// Returns [`TransportError::Rpc`] when the peer answered with an error object (that is a
    /// *reply*, not a transport failure), and the other variants when no usable reply exists.
    async fn call(&self, request: Request) -> Result<Response, TransportError>;
}
