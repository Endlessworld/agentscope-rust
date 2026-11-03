//! What the connected ACP client advertised during `initialize`.
//!
//! Several `session/update` variants are capability-gated: the agent MUST only
//! send them when the client advertised support, otherwise it has to fall back
//! to a plain agent message. `ClientSupport` captures the client's
//! `ClientCapabilities` from the `initialize` request and is shared with every
//! session so those gates can be evaluated at the point an update is emitted.

use std::sync::{Arc, RwLock};

use agent_client_protocol::v1::schema::{ClientCapabilities, Implementation};
/// Capability view of the connected ACP client.
///
/// Defaults to "nothing advertised", i.e. every optional update is suppressed
/// until `initialize` says otherwise (the conservative protocol behaviour).
#[derive(Clone, Default)]
pub struct ClientSupport {
    capabilities: Arc<RwLock<ClientCapabilities>>,
    client_info: Arc<RwLock<Option<Implementation>>>,
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

    /// Records the client identity (`clientInfo`) advertised at `initialize`.
    pub fn set_client_info(&self, client_info: Option<Implementation>) {
        *self.client_info.write().unwrap() = client_info;
    }

    /// A snapshot of the advertised client identity.
    pub fn client_info(&self) -> Option<Implementation> {
        self.client_info.read().unwrap().clone()
    }

    /// Whether the connected client is IntelliJ IDEA 2026.
    ///
    /// That client renders only flat select options: grouped model lists show an
    /// empty dropdown, so the agent must fall back to `Ungrouped`.
    pub fn is_intellij_2026(&self) -> bool {
        self.client_info()
            .is_some_and(|info| info.name.contains("IntelliJ") && info.version.contains("2026"))
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

    /// Whether the client advertised form-based elicitation (`elicitation.form`).
    ///
    /// Per the elicitation spec each mode must be advertised *explicitly*: an
    /// omitted or `null` `elicitation` field, or a missing/`null` `form` entry,
    /// all mean the agent MUST NOT send `elicitation/create` in form mode.
    pub fn supports_elicitation_form(&self) -> bool {
        self.capabilities
            .read()
            .unwrap()
            .elicitation
            .as_ref()
            .is_some_and(|elicitation| elicitation.form.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::ClientSupport;
    use agent_client_protocol::v1::schema::{
        ClientCapabilities, ClientSessionCapabilities, CompactionCapabilities,
        ElicitationCapabilities, ElicitationFormCapabilities, ElicitationUrlCapabilities,
        NoticeCapabilities, PlanCapabilities,
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

    #[test]
    fn elicitation_form_is_only_detected_when_explicitly_advertised() {
        let support = ClientSupport::new();
        assert!(!support.supports_elicitation_form());

        // A present-but-empty elicitation object advertises no mode.
        support.set_capabilities(
            ClientCapabilities::new().elicitation(ElicitationCapabilities::new()),
        );
        assert!(!support.supports_elicitation_form());

        // URL-only support must not enable form mode.
        support
            .set_capabilities(ClientCapabilities::new().elicitation(
                ElicitationCapabilities::new().url(ElicitationUrlCapabilities::new()),
            ));
        assert!(!support.supports_elicitation_form());

        support.set_capabilities(
            ClientCapabilities::new().elicitation(
                ElicitationCapabilities::new().form(ElicitationFormCapabilities::new()),
            ),
        );
        assert!(support.supports_elicitation_form());
    }

    #[test]
    fn intellij_2026_is_detected_from_client_info() {
        use agent_client_protocol::v1::schema::Implementation;

        let support = ClientSupport::new();
        assert!(!support.is_intellij_2026());

        support.set_client_info(Some(Implementation::new("JetBrains", "2024.3")));
        assert!(!support.is_intellij_2026());

        support.set_client_info(Some(Implementation::new("IntelliJ IDEA", "2026.1")));
        assert!(support.is_intellij_2026());
    }
}
