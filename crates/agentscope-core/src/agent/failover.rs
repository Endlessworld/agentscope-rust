//! Failover observation, mirroring `io.agentscope.core.agent.config.FailoverListener`.

use crate::error::Error;

/// Observer notified when the primary model failed and the configured fallback model takes
/// over (`ReActAgentBuilder::fallback_model`).
///
/// The switch and its cause are otherwise unobservable in-process: the primary's error is
/// consumed by the model call itself, below both the `on_model_call` middleware seam and the
/// event conversion.
///
/// Contract: invoked synchronously at the switch site and shared by the agent's concurrent
/// runs, so implementations must be fast, non-blocking and thread-safe. A listener cannot
/// throw in Rust, so no error containment is needed.
///
/// Java passes the `Model` instance; Rust passes the model *name*, which is the only part a
/// listener can portably use without holding a live provider handle.
pub trait FailoverListener: Send + Sync {
    /// Called once when the fallback takes over from the failed primary.
    ///
    /// `error` is the failure that exhausted the primary's retry budget.
    fn on_failover(&self, primary_model: &str, error: &Error);
}
