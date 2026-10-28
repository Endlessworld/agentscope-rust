//! agentscope-extensions-higress — Higress AI Gateway tool-selection extension.
//!
//! Ports `io.agentscope.extensions.higress` from AgentScope Java. Higress's
//! unified MCP endpoint (`union-tools-search`) exposes a special
//! `x_higress_tool_search` tool that returns the subset of MCP tools most
//! relevant to a user query. This crate wraps a harness MCP client to surface
//! that capability and integrates it with the core toolkit.
//!
//! See `docs/extensions/HIGRESS__GAP_TRACKER.md`.

pub mod client;
pub mod search_result;
pub mod toolkit;

pub use client::{
    convert_to_mcp_tool, HigressClientConfig, HigressMcpClientBuilder, HigressMcpClientWrapper,
    HigressMcpTransport, McpConnectRequest, TransportType, DEFAULT_INITIALIZATION_TIMEOUT,
    DEFAULT_TIMEOUT, DEFAULT_TOOL_SEARCH_TOP_K, TOOL_SEARCH_NAME,
};
pub use search_result::{HigressToolInfo, HigressToolSearchResult};
pub use toolkit::{HigressToolkit, McpClientRegistration, McpToolSource};
