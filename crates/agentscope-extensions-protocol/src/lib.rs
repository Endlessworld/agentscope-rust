//! agentscope-extensions-protocol - umbrella crate over the per-protocol extensions.
//!
//! Mirrors the Java `agentscope-extensions-protocol` aggregator: that module is a
//! Maven parent with one child module per protocol, not a single artifact. The
//! Rust layout follows the same split so dependency sets stay isolated:
//!
//! * [`a2a`] - A2A wire model / card resolution / `Msg` conversion / JSON-RPC
//!   client (`agentscope-extensions-a2a-client`) plus the axum server
//!   (`agentscope-extensions-a2a-server`).
//! * [`agent_protocol`] - the "agent protocol" task lifecycle HTTP+SSE API
//!   (`agentscope-extensions-agent-protocol`).
//! * [`agui`] - the AG-UI event taxonomy and adapter stack
//!   (`agentscope-extensions-agui`).
//! * [`chat_completions`] - the OpenAI-compatible Chat Completions web adapter
//!   (`agentscope-extensions-chat-completions-web`).
//!
//! Each module is behind a feature of the same name; all are enabled by default.
//! Prefer depending on the individual `agentscope-extensions-*` crate when you
//! only need one protocol.

#[cfg(feature = "a2a")]
pub mod a2a {
    pub use agentscope_extensions_a2a_client::*;

    /// A2A server side: runner contract, message conversion and JSON-RPC handler.
    pub mod server {
        pub use agentscope_extensions_a2a_server::*;
    }
}

#[cfg(feature = "agent-protocol")]
pub use agentscope_extensions_agent_protocol as agent_protocol;

#[cfg(feature = "agui")]
pub use agentscope_extensions_agui as agui;

#[cfg(feature = "chat-completions-web")]
pub use agentscope_extensions_chat_completions_web as chat_completions;
