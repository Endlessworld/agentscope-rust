//! Tests for the graceful-shutdown refinements (tracker §2.12):
//! per-request tracking, the `GracefulShutdownMiddleware`, the shutdown state
//! saver and the timeout force-interrupt monitor.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::{Error, Result};
use agentscope_core::event::AgentEvent;
use agentscope_core::middleware::{AgentInput, EventStream, Middleware, Next};
use agentscope_core::shutdown::{GracefulShutdownManager, ShutdownState, ShutdownStateSaver};
use agentscope_core::state::AgentState;
use agentscope_harness::agent::middleware::GracefulShutdownMiddleware;
use agentscope_harness::agent::shutdown::{
    GracefulShutdownController, JsonFileShutdownStateSaver, StateStoreShutdownStateSaver,
};
use agentscope_harness::HarnessAgent;
use futures::StreamExt;

/// A saver that counts how many times it was invoked.
struct CountingSaver {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ShutdownStateSaver for CountingSaver {
    async fn save(&self, _state: &AgentState) -> Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// A saver that always fails.
struct FailingSaver;

#[async_trait::async_trait]
impl ShutdownStateSaver for FailingSaver {
    async fn save(&self, _state: &AgentState) -> Result<()> {
        Err(Error::Other("state store unavailable".into()))
    }
}

fn empty_stream() -> EventStream {
    Box::pin(futures::stream::empty())
}

fn pending_stream() -> EventStream {
    Box::pin(futures::stream::pending::<Result<AgentEvent>>())
}

// ---- per-request tracking ----

#[tokio::test]
async fn per_request_registration_keeps_count_and_ids_consistent() {
    let mgr = GracefulShutdownManager::new();
    assert_eq!(mgr.get_active_request_count(), 0);
    assert!(mgr.active_request_ids().is_empty());

    mgr.register_request_with_id("req-1");
    mgr.register_request_with_id("req-2");
    assert_eq!(mgr.get_active_request_count(), 2);
    let ids = mgr.active_request_ids();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&"req-1".to_string()));
    assert!(ids.contains(&"req-2".to_string()));
    assert!(mgr.is_request_active("req-1"));

    // The snapshot carries the registration timestamp.
    let snapshot = mgr.active_requests_snapshot();
    assert_eq!(snapshot.len(), 2);
    assert!(snapshot
        .iter()
        .all(|info| info.started_at <= std::time::Instant::now()));

    // Unregistering a known id drops both the id and the count.
    assert!(mgr.unregister_request_with_id("req-1"));
    assert_eq!(mgr.get_active_request_count(), 1);
    assert_eq!(mgr.active_request_ids(), vec!["req-2".to_string()]);

    // Unregistering an unknown id must not corrupt the count.
    assert!(!mgr.unregister_request_with_id("does-not-exist"));
    assert_eq!(mgr.get_active_request_count(), 1);
    assert_eq!(mgr.active_request_ids().len(), 1);

    // The anonymous form keeps the same invariant.
    mgr.register_request();
    assert_eq!(mgr.get_active_request_count(), 2);
    mgr.unregister_request();
    assert_eq!(mgr.get_active_request_count(), 1);
    assert_eq!(mgr.active_request_ids(), vec!["req-2".to_string()]);
}

#[tokio::test]
async fn reset_for_testing_clears_state_and_active_requests() {
    let mgr = GracefulShutdownManager::new();
    mgr.register_request_with_id("req-1");
    assert!(mgr.perform_graceful_shutdown().await);
    assert_eq!(mgr.get_state(), ShutdownState::ShuttingDown);
    assert_eq!(mgr.get_active_request_count(), 1);

    mgr.reset_for_testing();

    assert_eq!(mgr.get_state(), ShutdownState::Running);
    assert_eq!(mgr.get_active_request_count(), 0);
    assert!(mgr.active_request_ids().is_empty());
    assert!(mgr.is_accepting_requests());
}

// ---- GracefulShutdownMiddleware ----

#[tokio::test]
async fn shutdown_middleware_registers_and_unregisters_the_request() {
    let mgr = Arc::new(GracefulShutdownManager::new());
    let mw = GracefulShutdownMiddleware::new(Arc::clone(&mgr));
    let ctx = RuntimeContext::new();

    let observed = Arc::new(AtomicUsize::new(0));
    let observed_during = Arc::clone(&observed);
    let mgr_for_next = Arc::clone(&mgr);
    let next: Next<AgentInput> = Box::new(move |_input: AgentInput| {
        // The request is registered while the inner chain runs.
        observed_during.store(mgr_for_next.get_active_request_count(), Ordering::SeqCst);
        empty_stream()
    });

    let events = mw
        .on_agent(&ctx, AgentInput::new(vec![]), next)
        .collect::<Vec<_>>()
        .await;

    assert!(events.is_empty(), "the inner stream is forwarded unchanged");
    assert_eq!(
        observed.load(Ordering::SeqCst),
        1,
        "one active request in flight"
    );
    assert_eq!(
        mgr.get_active_request_count(),
        0,
        "the request is unregistered"
    );
    assert!(mgr.active_request_ids().is_empty());
}

#[tokio::test]
async fn shutdown_middleware_unregisters_when_the_stream_is_dropped() {
    let mgr = Arc::new(GracefulShutdownManager::new());
    let mw = GracefulShutdownMiddleware::new(Arc::clone(&mgr));
    let ctx = RuntimeContext::new();

    let next: Next<AgentInput> = Box::new(|_input: AgentInput| pending_stream());
    let stream = mw.on_agent(&ctx, AgentInput::new(vec![]), next);
    assert_eq!(mgr.get_active_request_count(), 1);

    // Cancellation: the caller drops the stream before it completes.
    drop(stream);
    assert_eq!(mgr.get_active_request_count(), 0);
    assert!(mgr.active_request_ids().is_empty());
}

#[tokio::test]
async fn shutdown_middleware_rejects_new_requests_during_shutdown() {
    let mgr = Arc::new(GracefulShutdownManager::new());
    assert!(mgr.perform_graceful_shutdown().await);

    let mw = GracefulShutdownMiddleware::new(Arc::clone(&mgr));
    let ctx = RuntimeContext::new();
    let next: Next<AgentInput> =
        Box::new(|_input: AgentInput| panic!("the inner chain must not run while shutting down"));

    let events = mw
        .on_agent(&ctx, AgentInput::new(vec![]), next)
        .collect::<Vec<_>>()
        .await;

    assert_eq!(events.len(), 1, "exactly one error is surfaced");
    let err = events[0]
        .as_ref()
        .err()
        .expect("expected a rejection error");
    assert!(matches!(err, Error::ShuttingDown(_)), "got: {err:?}");
    assert!(
        err.to_string().contains("shutting down"),
        "the error must be recognisable, got: {err}"
    );
    assert_eq!(mgr.get_active_request_count(), 0);
}

// ---- shutdown state saver ----

#[tokio::test]
async fn shutdown_flow_calls_the_state_saver_once() {
    let mgr = Arc::new(GracefulShutdownManager::new());
    let calls = Arc::new(AtomicUsize::new(0));
    mgr.bind_state_saver(
        Arc::new(CountingSaver {
            calls: Arc::clone(&calls),
        }),
        Arc::new(Mutex::new(AgentState::default())),
    );

    let controller = GracefulShutdownController::new(Arc::clone(&mgr));
    assert!(controller.shutdown().await, "the shutdown must terminate");

    assert_eq!(mgr.get_state(), ShutdownState::Terminated);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the saver runs exactly once, before the state flips to Terminated"
    );
}

#[tokio::test]
async fn shutdown_flow_survives_a_failing_state_saver() {
    let mgr = Arc::new(GracefulShutdownManager::new());
    mgr.bind_state_saver(
        Arc::new(FailingSaver),
        Arc::new(Mutex::new(AgentState::default())),
    );

    let controller = GracefulShutdownController::new(Arc::clone(&mgr));
    assert!(
        controller.shutdown().await,
        "a failing saver must not break the shutdown flow"
    );
    assert_eq!(mgr.get_state(), ShutdownState::Terminated);

    // The failure is recorded (surfaced), not swallowed.
    let err = mgr
        .save_state_now()
        .await
        .expect("the saver error must be reported");
    assert!(matches!(&err, Error::Other(_)), "got: {err:?}");
}

#[tokio::test]
async fn json_file_state_saver_writes_the_snapshot() {
    let dir = std::env::temp_dir().join(format!(
        "agentscope_harness_shutdown_{}",
        std::process::id()
    ));
    let _ = tokio::fs::remove_dir_all(&dir).await;
    tokio::fs::create_dir_all(&dir).await.unwrap();

    let path = dir.join("nested").join("shutdown-state.json");
    let saver = JsonFileShutdownStateSaver::new(&path);

    let mut state = AgentState::default();
    state.session_id = "sess-shutdown".to_string();
    state.shutdown_interrupted = true;
    saver
        .save(&state)
        .await
        .expect("the snapshot must be written");

    let text = tokio::fs::read_to_string(&path).await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["session_id"], "sess-shutdown");
    assert_eq!(
        parsed["shutdown_interrupted"],
        serde_json::Value::Bool(true)
    );

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn state_store_state_saver_writes_into_the_store() {
    let store: Arc<dyn agentscope_core::state::AgentStateStore> =
        Arc::new(agentscope_core::state::InMemoryAgentStateStore::new());
    let saver = StateStoreShutdownStateSaver::new(Arc::clone(&store));

    let mut state = AgentState::default();
    state.session_id = "sess-store".to_string();
    state.user_id = Some("alice".to_string());
    saver
        .save(&state)
        .await
        .expect("the snapshot must be written");

    let stored = store
        .get(Some("alice"), "sess-store", "agent_state")
        .await
        .expect("the state must be persisted");
    assert_eq!(stored["session_id"], "sess-store");
}

// ---- timeout force-interrupt monitor ----

#[tokio::test]
async fn shutdown_monitor_force_terminates_after_the_timeout() {
    let mgr = Arc::new(GracefulShutdownManager::new());
    let mut config = mgr.config_snapshot();
    config.shutdown_timeout = Some(Duration::from_millis(60));
    mgr.set_config_now(config);

    // A request that never finishes keeps the shutdown open.
    mgr.register_request_with_id("req-stuck");
    assert!(mgr.perform_graceful_shutdown().await);
    assert_eq!(mgr.get_state(), ShutdownState::ShuttingDown);

    assert!(
        mgr.await_termination(Some(Duration::from_secs(5))).await,
        "the monitor must terminate the shutdown"
    );
    assert_eq!(mgr.get_state(), ShutdownState::Terminated);
}

#[tokio::test]
async fn shutdown_monitor_does_not_force_terminate_without_active_requests() {
    let mgr = Arc::new(GracefulShutdownManager::new());
    let mut config = mgr.config_snapshot();
    config.shutdown_timeout = Some(Duration::from_millis(50));
    mgr.set_config_now(config);

    let calls = Arc::new(AtomicUsize::new(0));
    mgr.bind_state_saver(
        Arc::new(CountingSaver {
            calls: Arc::clone(&calls),
        }),
        Arc::new(Mutex::new(AgentState::default())),
    );

    assert!(mgr.perform_graceful_shutdown().await);
    assert!(mgr.await_termination(Some(Duration::from_secs(5))).await);
    assert_eq!(mgr.get_state(), ShutdownState::Terminated);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Wait past the timeout window: with no active request the monitor must not
    // force-terminate (a force interrupt would save the state a second time).
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(mgr.get_state(), ShutdownState::Terminated);
}

// ---- HarnessAgent wiring ----

#[test]
fn harness_agent_builder_wires_the_graceful_shutdown_middleware() {
    let dir = std::env::temp_dir().join(format!(
        "agentscope_harness_shutdown_agent_{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let plain = HarnessAgent::builder().name("plain").cwd(&dir).build();
    let wired = HarnessAgent::builder()
        .name("wired")
        .cwd(&dir)
        .graceful_shutdown(true)
        .build();

    assert_eq!(
        wired.middleware().len(),
        plain.middleware().len() + 1,
        "the shutdown middleware must be part of the chain"
    );
    assert!(wired.shutdown_manager().is_accepting_requests());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn harness_agent_shutdown_terminates_the_agent() {
    let dir = std::env::temp_dir().join(format!(
        "agentscope_harness_shutdown_agent2_{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let agent = HarnessAgent::builder()
        .name("sd")
        .cwd(&dir)
        .graceful_shutdown(true)
        .shutdown_timeout(Duration::from_secs(5))
        .build();

    assert!(agent.shutdown().await);
    assert_eq!(
        agent.shutdown_manager().get_state(),
        ShutdownState::Terminated
    );
    assert!(!agent.shutdown_manager().is_accepting_requests());

    let _ = std::fs::remove_dir_all(&dir);
}
