//! Tests for Phase 3 core modules: state, memory, permission, interruption, shutdown.
use agentscope_core::interruption::{InterruptControl, InterruptSource};
use agentscope_core::message::{ContentBlock, Msg, MsgRole, TextBlock};
use agentscope_core::permission::{
    PermissionBehavior, PermissionContextState, PermissionEngine, PermissionMode, PermissionRule,
};
use agentscope_core::shutdown::{GracefulShutdownManager, ShutdownState};
use agentscope_core::state::{
    AgentState, AgentStateStore, InMemoryAgentStateStore, JsonFileAgentStateStore, Task, TaskState,
};
use agentscope_core::Memory;
use agentscope_core::InMemoryMemory;

fn sample_msg(role: MsgRole, text: &str) -> Msg {
    Msg::new(
        format!("id-{}", text),
        None,
        role,
        vec![ContentBlock::Text(TextBlock::new(text))],
        None,
        None,
        None,
    )
}

#[test]
fn agent_state_json_round_trip() {
    let state = AgentState::builder()
        .session_id("sess-1")
        .user_id("alice")
        .summary("hello")
        .add_message(sample_msg(MsgRole::User, "hi"))
        .build();

    let json = state.to_json().unwrap();
    let back = AgentState::from_json_str(&json).unwrap();
    assert_eq!(back.session_id(), "sess-1");
    assert_eq!(back.user_id(), Some("alice"));
    assert_eq!(back.summary(), "hello");
    assert_eq!(back.context().len(), 1);
    assert!(json.contains("session_id"));
}

#[tokio::test]
async fn in_memory_store_round_trip() {
    let store = InMemoryAgentStateStore::new();
    store
        .save(None, "s1", "k", serde_json::json!({"v": 1}))
        .await;
    store
        .save_list(None, "s1", "lst", &[serde_json::json!(1), serde_json::json!(2)])
        .await;

    assert!(store.exists(None, "s1").await);
    let got = store.get(None, "s1", "k").await.unwrap();
    assert_eq!(got["v"], 1);
    let list = store.get_list(None, "s1", "lst").await;
    assert_eq!(list.len(), 2);
    assert_eq!(store.list_session_ids(None).await, vec!["s1".to_string()]);

    store.delete(None, "s1").await;
    assert!(!store.exists(None, "s1").await);
}

#[tokio::test]
async fn json_file_store_round_trip() {
    let dir = std::env::temp_dir().join(format!("agentscope-test-{}", uuid::Uuid::new_v4()));
    let store = JsonFileAgentStateStore::new(&dir);
    let state = AgentState::builder().session_id("s2").build();

    store
        .save(None, "s2", "agent_state", serde_json::to_value(&state).unwrap())
        .await;
    let loaded: AgentState =
        serde_json::from_value(store.get(None, "s2", "agent_state").await.unwrap()).unwrap();
    assert_eq!(loaded.session_id(), "s2");
    assert!(store.exists(None, "s2").await);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn json_file_store_survives_restart() {
    // Simulates a process restart: a brand-new store instance (same root) must
    // recover state persisted by an earlier store instance.
    let dir = std::env::temp_dir().join(format!("agentscope-restart-{}", uuid::Uuid::new_v4()));

    // "First run": save state + a list, then drop the store.
    {
        let store = JsonFileAgentStateStore::new(&dir);
        let state = AgentState::builder()
            .session_id("persist-sess")
            .user_id("bob")
            .summary("persisted summary")
            .add_message(sample_msg(MsgRole::User, "hello from run 1"))
            .build();
        store
            .save(Some("bob"), "persist-sess", "agent_state", serde_json::to_value(&state).unwrap())
            .await;
        store
            .save_list(Some("bob"), "persist-sess", "history", &[serde_json::json!("a"), serde_json::json!("b")])
            .await;
    }

    // "Second run": a fresh store instance reads the same files from disk.
    {
        let store = JsonFileAgentStateStore::new(&dir);
        assert!(store.exists(Some("bob"), "persist-sess").await);

        let loaded: AgentState = serde_json::from_value(
            store.get(Some("bob"), "persist-sess", "agent_state").await.unwrap(),
        )
        .unwrap();
        assert_eq!(loaded.session_id(), "persist-sess");
        assert_eq!(loaded.user_id(), Some("bob"));
        assert_eq!(loaded.summary(), "persisted summary");
        assert_eq!(loaded.context().len(), 1);
        assert_eq!(loaded.context()[0].text_content(), "hello from run 1");

        let history = store.get_list(Some("bob"), "persist-sess", "history").await;
        assert_eq!(history.len(), 2);
        assert_eq!(history[0], serde_json::json!("a"));

        // list_session_ids must survive restart too.
        let sessions = store.list_session_ids(Some("bob")).await;
        assert!(sessions.contains(&"persist-sess".to_string()));
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn permission_bypass_allows() {
    let ctx = PermissionContextState::builder()
        .mode(PermissionMode::Bypass)
        .build();
    let engine = PermissionEngine::new(ctx);
    let decision = engine.check_permission("rm", false, &serde_json::Map::new(), |_c, _i| true);
    assert_eq!(decision.behavior, PermissionBehavior::Allow);
}

#[test]
fn permission_explore_denies_write() {
    let ctx = PermissionContextState::builder()
        .mode(PermissionMode::Explore)
        .build();
    let engine = PermissionEngine::new(ctx);
    let decision = engine.check_permission("write", false, &serde_json::Map::new(), |_c, _i| true);
    assert_eq!(decision.behavior, PermissionBehavior::Deny);
}

#[test]
fn permission_deny_rule_wins() {
    let rule = PermissionRule::new("rm", None, PermissionBehavior::Deny, "test");
    let ctx = PermissionContextState::builder().add_deny_rule("rm", rule).build();
    let engine = PermissionEngine::new(ctx);
    let decision = engine.check_permission("rm", false, &serde_json::Map::new(), |_c, _i| true);
    assert_eq!(decision.behavior, PermissionBehavior::Deny);
}

#[test]
fn interrupt_control_lifecycle() {
    let control = InterruptControl::new();
    assert!(!control.is_interrupted());
    control.trigger(Some(InterruptSource::User), None);
    assert!(control.is_interrupted());
    assert_eq!(control.source(), InterruptSource::User);
    control.reset();
    assert!(!control.is_interrupted());
}

#[test]
fn task_json_round_trip() {
    let task = Task::builder().subject("sub").description("desc").build();
    assert_eq!(task.state, TaskState::Pending);
    let json = serde_json::to_string(&task).unwrap();
    let back: Task = serde_json::from_str(&json).unwrap();
    assert_eq!(back.subject, "sub");
    assert!(json.contains("created_at"));
}

#[tokio::test]
async fn memory_save_load_round_trip() {
    let store = InMemoryAgentStateStore::new();
    let mem = InMemoryMemory::new();
    mem.add_message(sample_msg(MsgRole::User, "hello"));
    mem.save_to(&store, None, "sess").await.unwrap();

    let mem2 = InMemoryMemory::new();
    mem2.load_from(&store, None, "sess").await.unwrap();
    assert_eq!(mem2.get_messages().len(), 1);
    assert_eq!(mem2.get_messages()[0].text_content(), "hello");
}

#[tokio::test]
async fn shutdown_manager_lifecycle() {
    let mgr = GracefulShutdownManager::new();
    assert_eq!(mgr.get_state(), ShutdownState::Running);
    assert!(mgr.perform_graceful_shutdown().await);
    assert!(!mgr.is_accepting_requests());
    assert!(mgr.ensure_accepting_requests().is_err());
}
