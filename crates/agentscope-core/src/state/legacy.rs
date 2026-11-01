//! One-time v1 -> 2.0 state migration entry point, mirroring
//! `io.agentscope.core.state.LegacyStateLoader`.
//!
//! Reads the v1 session keys ([`MEMORY_MESSAGES_KEY`], [`TOOLKIT_ACTIVE_GROUPS_KEY`]) and
//! rebuilds an equivalent [`AgentState`]. The original session data is neither modified nor
//! migrated in place: this is an **explicit** migration entry point, and normal agent
//! activation never reads these keys. Persist the result explicitly or import it through
//! [`import_baseline`](crate::session::migration::import_baseline).
//!
//! The v1 keys carry no 2.0-era [`PermissionContextState`], so callers that need a specific
//! permission context (for example `BYPASS` for a pre-seeded privileged session) must pass it
//! in explicitly; otherwise the reconstructed state silently falls back to the default mode.

use serde::{Deserialize, Serialize};

use crate::message::Msg;
use crate::permission::PermissionContextState;

use super::context::ToolContextState;
use super::store::AgentStateStore;
use super::AgentState;

/// v1 conversation-history key.
pub const MEMORY_MESSAGES_KEY: &str = crate::memory::MEMORY_MESSAGES_KEY;

/// v1 tool-activation key. Keeps the Java spelling (`toolkit_activeGroups`).
pub const TOOLKIT_ACTIVE_GROUPS_KEY: &str = "toolkit_activeGroups";

/// Deserialization shape for the v1 [`TOOLKIT_ACTIVE_GROUPS_KEY`] entry.
///
/// Mirrors Java `state/legacy/ToolkitState`. Retained only for the migration above; new writes
/// never use this type.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ToolkitState {
    /// Tool groups active in the v1 session.
    pub active_groups: Vec<String>,
}

impl ToolkitState {
    pub fn new(active_groups: Vec<String>) -> Self {
        Self { active_groups }
    }
}

/// The reconstructed state and whether any v1 key was present.
///
/// Mirrors Java `LegacyStateLoader.LegacyLoadResult`. The presence bit matters because an
/// explicitly persisted empty group list is different from a missing key even though both
/// produce an empty [`ToolContextState`].
#[derive(Debug, Clone, PartialEq)]
pub struct LegacyLoadResult {
    pub state: AgentState,
    /// `true` for non-empty v1 messages or a present (possibly empty) tool-group entry.
    pub found: bool,
}

/// Rebuilds an [`AgentState`] from a v1 session.
///
/// Unreadable entries (a value that does not deserialize into a [`Msg`] / [`ToolkitState`]) are
/// skipped rather than turning the whole migration into an error: the state store surface is
/// best-effort by contract.
pub async fn load(
    store: &dyn AgentStateStore,
    user_id: Option<&str>,
    session_id: &str,
    permission_context: Option<PermissionContextState>,
) -> LegacyLoadResult {
    let messages: Vec<Msg> = store
        .get_list(user_id, session_id, MEMORY_MESSAGES_KEY)
        .await
        .iter()
        .filter_map(|value| serde_json::from_value(value.clone()).ok())
        .collect();
    let toolkit: Option<ToolkitState> = store
        .get(user_id, session_id, TOOLKIT_ACTIVE_GROUPS_KEY)
        .await
        .and_then(|value| serde_json::from_value(value).ok());

    let found = !messages.is_empty() || toolkit.is_some();
    let mut builder = AgentState::builder().context(messages);
    if let Some(permission_context) = permission_context {
        builder = builder.permission_context(permission_context);
    }
    if let Some(toolkit) = toolkit {
        let mut tool_context = ToolContextState::builder();
        for group in toolkit.active_groups {
            tool_context = tool_context.add_activated_group(group);
        }
        builder = builder.tool_context(tool_context.build());
    }
    LegacyLoadResult {
        state: builder.build(),
        found,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::PermissionMode;
    use crate::session::{
        import_baseline, AtomicSessionStorage, JournalSessionLog, MemorySessionStorage,
        SessionLog, SessionProjection,
    };
    use crate::state::InMemoryAgentStateStore;
    use std::sync::Arc;

    const USER: Option<&str> = Some("user-1");
    const SESSION: &str = "session-1";

    async fn seed(store: &InMemoryAgentStateStore) {
        store
            .save_list(
                USER,
                SESSION,
                MEMORY_MESSAGES_KEY,
                &[serde_json::to_value(Msg::user("hello from v1")).unwrap()],
            )
            .await;
        store
            .save(
                USER,
                SESSION,
                TOOLKIT_ACTIVE_GROUPS_KEY,
                serde_json::to_value(ToolkitState::new(vec!["team-alpha".to_string()])).unwrap(),
            )
            .await;
    }

    #[tokio::test]
    async fn legacy_keys_rebuild_the_conversation_and_active_groups() {
        let store = InMemoryAgentStateStore::new();
        seed(&store).await;

        let result = load(&store, USER, SESSION, None).await;

        assert!(result.found);
        assert_eq!(result.state.context().len(), 1);
        assert_eq!(result.state.context()[0].text_content(), "hello from v1");
        assert_eq!(
            result.state.tool_context().activated_groups(),
            ["team-alpha".to_string()],
            "v1 tool activation groups must survive reconstruction"
        );
    }

    #[tokio::test]
    async fn a_session_without_legacy_keys_yields_an_empty_state() {
        let store = InMemoryAgentStateStore::new();

        let result = load(&store, USER, SESSION, None).await;

        assert!(!result.found);
        assert!(result.state.context().is_empty());
        assert!(result.state.tool_context().activated_groups().is_empty());
        assert_eq!(
            result.state.permission_context().mode,
            PermissionMode::Default
        );
    }

    #[tokio::test]
    async fn an_explicitly_empty_group_list_still_counts_as_present() {
        let store = InMemoryAgentStateStore::new();
        store
            .save(
                USER,
                SESSION,
                TOOLKIT_ACTIVE_GROUPS_KEY,
                serde_json::to_value(ToolkitState::new(Vec::new())).unwrap(),
            )
            .await;

        let result = load(&store, USER, SESSION, None).await;

        assert!(result.found, "a persisted empty group list is not a missing key");
        assert!(result.state.tool_context().activated_groups().is_empty());
    }

    #[tokio::test]
    async fn a_supplied_permission_context_is_preserved() {
        let store = InMemoryAgentStateStore::new();
        seed(&store).await;

        let result = load(
            &store,
            USER,
            SESSION,
            Some(PermissionContextState::default().with_mode(PermissionMode::Bypass)),
        )
        .await;

        assert_eq!(
            result.state.permission_context().mode,
            PermissionMode::Bypass
        );
        assert_eq!(result.state.context().len(), 1);
    }

    #[tokio::test]
    async fn the_reconstructed_state_imports_as_a_session_baseline() {
        let store = InMemoryAgentStateStore::new();
        seed(&store).await;
        let legacy = load(&store, USER, SESSION, None).await;

        let storage: Arc<dyn AtomicSessionStorage> = Arc::new(MemorySessionStorage::new());
        let target: Arc<dyn SessionLog> = Arc::new(JournalSessionLog::new(storage, SESSION));
        import_baseline(Arc::clone(&target), &legacy.state, "legacy_store", 1)
            .await
            .unwrap();

        let projection = SessionProjection::read(&*target).await.unwrap();
        let restored = projection.restore().unwrap().unwrap();
        assert_eq!(restored.context().len(), 1);
        assert_eq!(restored.context()[0].text_content(), "hello from v1");
    }
}
