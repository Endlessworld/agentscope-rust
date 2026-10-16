//! What the connected ACP client advertised during `initialize`.
//!
//! Several `session/update` variants are capability-gated: the agent MUST only
//! send them when the client advertised support, otherwise it has to fall back
//! to a plain agent message. `ClientSupport` captures the client's
//! `ClientCapabilities` from the `initialize` request and is shared with every
//! session so those gates can be evaluated at the point an update is emitted.

use std::sync::{Arc, RwLock};

use agent_client_protocol::schema::v1::ClientCapabilities;
/// Capability view of the connected ACP client.
///
/// Defaults to "nothing advertised", i.e. every optional update is suppressed
/// until `initialize` says otherwise (the conservative protocol behaviour).
#[derive(Clone, Default)]
pub struct ClientSupport {
    capabilities: Arc<RwLock<ClientCapabilities>>,
}

impl ClientSupport {
    /// Creates an empty (nothing advertised) capability view.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the capabilities advertised in the `initialize` request.
    pub fn set_capabilities(&self, capabilities: ClientCapabilities) {
        *self.capabilities.write().unwrap() = capabilities;
    }

    /// A snapshot of the advertised capabilities.
    pub fn capabilities(&self) -> ClientCapabilities {
        self.capabilities.read().unwrap().clone()
    }

    /// Whether the client advertised `session.notices`.
    pub fn supports_notices(&self) -> bool {
        self.capabilities
            .read()
            .unwrap()
            .session
            .as_ref()
            .is_some_and(|session| session.notices.is_some())
    }

    /// Whether the client advertised `session.compaction`.
    pub fn supports_compaction(&self) -> bool {
        self.capabilities
            .read()
            .unwrap()
            .session
            .as_ref()
            .is_some_and(|session| session.compaction.is_some())
    }

    /// Whether the client advertised plan operations (`plan_update` /
    /// `plan_removed`).
    pub fn supports_plan_operations(&self) -> bool {
        self.capabilities.read().unwrap().plan.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::ClientSupport;
    use agent_client_protocol::schema::v1::{
        ClientCapabilities, ClientSessionCapabilities, CompactionCapabilities, NoticeCapabilities,
        PlanCapabilities,
    };

    #[test]
    fn nothing_advertised_suppresses_every_optional_update() {
        let support = ClientSupport::new();
        assert!(!support.supports_notices());
        assert!(!support.supports_compaction());
        assert!(!support.supports_plan_operations());
    }

    #[test]
    fn session_sub_capabilities_are_detected_independently() {
        let support = ClientSupport::new();
        support.set_capabilities(
            ClientCapabilities::new().session(
                ClientSessionCapabilities::new()
                    .notices(NoticeCapabilities::new())
                    .compaction(CompactionCapabilities::new()),
            ),
        );
        assert!(support.supports_notices());
        assert!(support.supports_compaction());
        assert!(!support.supports_plan_operations());
    }

    #[test]
    fn plan_capability_is_detected() {
        let support = ClientSupport::new();
        support.set_capabilities(ClientCapabilities::new().plan(PlanCapabilities::new()));
        assert!(support.supports_plan_operations());
        assert!(!support.supports_notices());
    }
}
