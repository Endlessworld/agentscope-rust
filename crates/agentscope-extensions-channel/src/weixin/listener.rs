//! Host-facing runtime observations without imposing a host product lifecycle.
//!
//! Rust port of `io.agentscope.extensions.channel.weixin.WeixinRuntimeListener`. Every method has a
//! default no-op body, so a host implements only the events it cares about; a listener that panics
//! is caught and logged so one broken host callback cannot take the consumer down.

use super::state_store::WeixinLease;

/// Receives technical runtime observations from a running Weixin consumer.
pub trait WeixinRuntimeListener: Send + Sync {
    /// The account lease was acquired; this instance is now the active consumer.
    fn on_lease_acquired(&self, _account_id: &str, _lease: &WeixinLease) {}

    /// The provider session started and the poll loop is live.
    fn on_running(&self, _account_id: &str) {}

    /// The consumer released its lease and stopped.
    fn on_stopped(&self, _account_id: &str) {}

    /// iLink rejected the current credential (`-14`); the host decides whether that means
    /// reauthorization, a notification, or something else.
    fn on_credential_rejected(&self, _account_id: &str, _reason: &str) {}

    /// A recoverable failure: the consumer backs off and retries.
    fn on_transient_failure(&self, _account_id: &str, _reason: &str) {}

    /// A reply the provider refused. The agent has already run, so a delivery failure never
    /// replays the inbound message; the host reports it and its own delivery queue decides what
    /// happens next.
    fn on_delivery_failed(&self, _account_id: &str, _reason: &str) {}

    /// An accepted inbound message whose dispatch kept failing, so it was abandoned instead of
    /// retried forever. Distinct from [`WeixinRuntimeListener::on_delivery_failed`]: the agent may
    /// never have finished.
    fn on_dispatch_failed(&self, _account_id: &str, _reason: &str) {}

    /// A poll after a transient failure succeeded again.
    fn on_recovered(&self, _account_id: &str) {}
}

/// A listener that ignores every observation.
#[derive(Debug, Default)]
pub struct NoopWeixinRuntimeListener;

impl WeixinRuntimeListener for NoopWeixinRuntimeListener {}

/// A listener that records every observation, for tests and diagnostics.
#[derive(Debug, Default)]
pub struct RecordingWeixinRuntimeListener {
    events: std::sync::Mutex<Vec<String>>,
}

impl RecordingWeixinRuntimeListener {
    pub fn new() -> Self {
        Self::default()
    }

    /// The recorded `"<event>:<account>"` strings, in order.
    pub fn events(&self) -> Vec<String> {
        self.events
            .lock()
            .expect("listener mutex poisoned")
            .clone()
    }

    fn record(&self, event: &str, account_id: &str) {
        self.events
            .lock()
            .expect("listener mutex poisoned")
            .push(format!("{event}:{account_id}"));
    }
}

impl WeixinRuntimeListener for RecordingWeixinRuntimeListener {
    fn on_lease_acquired(&self, account_id: &str, _lease: &WeixinLease) {
        self.record("lease-acquired", account_id);
    }

    fn on_running(&self, account_id: &str) {
        self.record("running", account_id);
    }

    fn on_stopped(&self, account_id: &str) {
        self.record("stopped", account_id);
    }

    fn on_credential_rejected(&self, account_id: &str, _reason: &str) {
        self.record("credential-rejected", account_id);
    }

    fn on_transient_failure(&self, account_id: &str, _reason: &str) {
        self.record("transient-failure", account_id);
    }

    fn on_delivery_failed(&self, account_id: &str, _reason: &str) {
        self.record("delivery-failed", account_id);
    }

    fn on_dispatch_failed(&self, account_id: &str, _reason: &str) {
        self.record("dispatch-failed", account_id);
    }

    fn on_recovered(&self, account_id: &str) {
        self.record("recovered", account_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_listener_ignores_every_event() {
        let listener = NoopWeixinRuntimeListener;
        let lease = WeixinLease::new("holder", 1).unwrap();
        listener.on_lease_acquired("account", &lease);
        listener.on_running("account");
        listener.on_stopped("account");
        listener.on_credential_rejected("account", "reason");
        listener.on_transient_failure("account", "reason");
        listener.on_delivery_failed("account", "reason");
        listener.on_dispatch_failed("account", "reason");
        listener.on_recovered("account");
    }

    #[test]
    fn the_recording_listener_captures_events_in_order() {
        let listener = RecordingWeixinRuntimeListener::new();
        let lease = WeixinLease::new("holder", 1).unwrap();
        listener.on_lease_acquired("account", &lease);
        listener.on_running("account");
        listener.on_credential_rejected("account", "iLink credential rejected");
        listener.on_stopped("account");
        assert_eq!(
            listener.events(),
            vec![
                "lease-acquired:account",
                "running:account",
                "credential-rejected:account",
                "stopped:account",
            ]
        );
    }
}
