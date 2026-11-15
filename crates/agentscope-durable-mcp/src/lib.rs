//! `agentscope-durable-mcp`
//!
//! The durable runtime keeps side effects behind an `Executor` port so that a crashed turn can
//! be resumed without re-running a tool blindly. This crate is that port for MCP servers: it
//! speaks JSON-RPC 2.0 / MCP by hand (no SDK, no HTTP client, no `agentscope-extensions-*`
//! dependency) and turns `tools/call` into a durable execution result. It mirrors the role of
//! the Java `agentscope-extensions-durable-mcp` module: protocol and transport mapping only,
//! no business rules.
//!
//! # What is here
//!
//! * [`jsonrpc`] -- request/response/notification/error types with symmetric encode/decode.
//!   Params hold a [`RawValue`](serde_json::value::RawValue), so the field order of the typed
//!   params structs is the wire order (that is what makes the golden-byte tests meaningful).
//! * [`transport`] -- the only port a real connection implements: [`McpTransport`].
//! * [`session`] -- the four handshake/discovery builders, plus [`Method`] parsing.
//! * [`catalog`] -- `tools/list` -> [`ToolCatalog`]; a call for an unadvertised tool is a hard
//!   error, never an empty success.
//! * [`mapping`] -- durable `ExecutionRequest` -> `tools/call`, and `tools/call` ->
//!   [`ExecutionResult`](agentscope_durable::ExecutionResult).
//! * [`executor`] -- [`McpExecutor`], the `Executor` implementation.
//! * [`testing`] -- [`FakeTransport`] for downstream tests.
//!
//! # The mapping, and its tradeoffs
//!
//! | MCP observation | durable state |
//! |---|---|
//! | `content`/`structuredContent`, with or without `isError` | `Succeeded`, payload kept as-is |
//! | `task.status` `working`/`input_required` | `Pending`, `handle` = `taskId` |
//! | `task.status` `completed` / `failed` / `cancelled` | `Succeeded` / `Failed` / `Cancelled` |
//! | error code that proves the call never started | `Failed` |
//! | error code `-32603`, unknown code, unreadable or mis-addressed reply, no reply | `Unknown` |
//!
//! Three decisions are worth stating explicitly, because getting them wrong is what makes a
//! durable runtime double-apply effects:
//!
//! 1. **`isError` is not a durable failure.** It marks a failure *inside a completed round
//!    trip*: the tool ran and reported something. Mapping it to `Failed` would claim the effect
//!    did not happen and leave the call retryable. Instead the whole result (including
//!    `isError`) is stored as `Succeeded` and applied to the model, which sees the tool's error
//!    text just as it would locally. See [`mapping`] for the long version.
//! 2. **Pending means pending.** A long-running tool answers with the MCP `tasks` pointer
//!    instead of content; that is `Pending` with the task id as the handle, so the turn stays
//!    open instead of blocking a transport call forever. Everything else the peer says is
//!    treated as final.
//! 3. **Unknown is not "did not happen".** `-32603`, unknown codes, malformed results and
//!    missing replies all end in `Unknown`, never in `Failed`, because the only honest record
//!    is "we do not know" -- and only `Unknown` stops an automatic retry from running the
//!    tool twice.
//!
//! # Error codes
//!
//! [`classify`] maps `-32700`/`-32600`/`-32601`/`-32602`/`-32603` to durable
//! `INVALID`/`INVALID`/`INCOMPATIBLE`/`INVALID`/`UNAVAILABLE`, plus two local codes `-32001`
//! (tool not advertised -> `NOT_FOUND`) and `-32002` (tool unavailable -> `UNAVAILABLE`). See
//! [`error`] for the table and its rationale; unknown codes classify as
//! `UNKNOWN`/`UNAVAILABLE`/effect-unknown.
//!
//! # Not implemented (on purpose)
//!
//! * No real transport: stdio/SSE/HTTP framing belongs to a caller-side crate; this one owns
//!   the protocol model only.
//! * No lookup or cancellation: MCP has no standard verb for either, so `query`/`cancel`
//!   report `Unknown` and `ExecutorCapabilities` is all-`false`.
//! * No automatic `tools/list` pagination: [`ToolCatalog::from_pages`] merges pages the caller
//!   fetched.
//! * No content-block model: content is kept as opaque JSON, because this layer never
//!   interprets tool output.
//! * No retries: a retry is a durable decision, not a transport one.

#![warn(missing_docs)]

pub mod catalog;
pub mod error;
pub mod executor;
pub mod jsonrpc;
pub mod mapping;
pub mod session;
pub mod testing;
pub mod transport;

pub use catalog::{ToolCatalog, ToolDefinition, ToolsListResult};
pub use error::{classify, RpcErrorClass, TransportError};
pub use executor::McpExecutor;
pub use jsonrpc::{
    codes, ErrorObject, Notification, Request, RequestId, Response, JSONRPC_VERSION,
};
pub use mapping::{
    error_to_execution_result, tool_not_advertised_result, tools_call_for_execution,
    tools_call_params, tools_call_request, tools_call_result_to_execution, CallToolResult,
    TaskPointer, TaskStatus, ToolsCallParams, METHOD_TOOLS_CALL,
};
pub use session::{
    initialize_request, initialize_request_with, initialized_notification, server_info,
    tools_list_page_request, tools_list_request, ClientCapabilities, EmptyCapability,
    Implementation, InitializeParams, Method, RootsCapability, ToolsListParams, CLIENT_NAME,
    CLIENT_VERSION, METHOD_INITIALIZE, METHOD_INITIALIZED, METHOD_TOOLS_LIST, PROTOCOL_VERSION,
};
pub use testing::FakeTransport;
pub use transport::McpTransport;
