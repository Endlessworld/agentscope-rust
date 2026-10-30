//! # agentscope-extensions-studio
//!
//! Rust port of the Java `agentscope-extensions-studio` module, which bundles two
//! sub-domains under one aggregation module:
//!
//! - [`studio`] — AgentScope Studio integration (`io.agentscope.core.studio`):
//!   the REST client (`registerRun` / `pushMessage` / `requestUserInput`), the
//!   WebSocket transport used to receive `forwardUserInput` events, the request
//!   POJOs, the message-forwarding middleware and the global manager.
//! - [`telemetry`] — OpenTelemetry GenAI telemetry
//!   (`io.agentscope.core.tracing.telemetry`): semantic-convention attribute keys,
//!   the OTel message-part model, pure attribute extractors and the streaming
//!   response aggregator.
//!
//! ## Design notes (see `docs/extensions/STUDIO__GAP_TRACKER.md`)
//! - Java's `Hook` / `Tracer` interfaces do not exist in Rust core; the equivalents
//!   are implemented on top of [`agentscope_core::middleware::Middleware`].
//! - Java uses Socket.IO. Rust has no Socket.IO client in this workspace, so the
//!   wire format is abstracted behind [`studio::StudioTransport`] and the event
//!   parsing is implemented as pure, offline-testable functions.
//! - The REST wire call sits behind [`studio::StudioHttpTransport`] (`reqwest` in production), so
//!   `registerRun` / `pushMessage` / `requestUserInput` are covered offline.
//! - OTLP export is deferred (tracker §4.2): attributes are extracted as
//!   `(String, serde_json::Value)` pairs so no `opentelemetry` dependency is
//!   required for the core logic.

pub mod error;
pub mod studio;
pub mod telemetry;

pub use error::{StudioError, StudioResult};
pub use studio::{
    NoopStudioTransport, PendingInputRegistry, ReqwestStudioTransport, StudioClient, StudioConfig,
    StudioConfigBuilder, StudioHttpRequest, StudioHttpResponse, StudioHttpTransport, StudioManager,
    StudioMessageHook, StudioTransport, StudioUserAgent, StudioWebSocketClient, UserInputData,
};
