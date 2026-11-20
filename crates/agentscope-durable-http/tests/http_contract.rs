//! The same invariants `agentscope-durable/tests/backend_contract.rs` proves in process, proved
//! again through the real wire: an axum router served by `tower::oneshot`, addressed by
//! [`HttpSessionBackend`]. Anything the mapper drops (a header, a code, a receipt) fails here.
//!
//! Focus is the contract that a remote execution host depends on: fences, commit receipts,
//! waiting/ready, tool settlement, checkpoints, the management surface and SSE cursors.
#![cfg(feature = "server")]

use std::sync::Arc;
use std::time::Duration;

use agentscope_durable::backend::TxnBackend;
use agentscope_durable::types::*;
use agentscope_durable::{DeltaFilter, ErrorCode, EventStreamOptions, SessionBackend};
use agentscope_durable_http::server::DurableService;
use agentscope_durable_http::{
    HttpRequest, HttpResponse, HttpSessionBackend, Method, Transport, TransportError,
};
use async_trait::async_trait;
use axum::body::Body;
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt;

/// Sends each wire request into the router without a socket.
struct RouterTransport {
    app: Router,
}

#[async_trait]
impl Transport for RouterTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let method = match request.method {
            Method::Get => axum::http::Method::GET,
            Method::Post => axum::http::Method::POST,
        };
        let mut builder = axum::http::Request::builder()
            .method(method)
            .uri(request.path_and_query());
        for (name, value) in request.headers {
            builder = builder.header(name, value);
        }
        let body = if request.body.is_empty() {
            Body::empty()
        } else {
            Body::from(request.body)
        };
        let request = builder
            .body(body)
            .map_err(|error| TransportError::new(error.to_string()))?;
        let response = match self.app.clone().oneshot(request).await {
            Ok(response) => response,
            // The router's service error is `Infallible`; matching proves it cannot happen.
            Err(error) => match error {},
        };
        let status = response.status().as_u16();
        let body = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
            .await
            .map_err(|error| TransportError::new(error.to_string()))?;
        Ok(HttpResponse {
            status,
            body: body.to_vec(),
        })
    }
}

fn binding() -> ThreadBinding {
    ThreadBinding::new("harness-a", "def-1", "state-1").expect("binding")
}

fn contract_backend() -> (Arc<dyn SessionBackend>, HttpSessionBackend) {
    let backend: Arc<dyn SessionBackend> = Arc::new(TxnBackend::in_memory());
    let app = DurableService::new(backend.clone()).router();
    let transport: Arc<dyn Transport> = Arc::new(RouterTransport { app });
    (backend, HttpSessionBackend::new(transport))
}

async fn created() -> (HttpSessionBackend, ThreadKey) {
    let (_host, backend) = contract_backend();
    let key = SessionKey::new("ns", "s1").expect("session key");
    backend
        .create(SessionSpec {
            key: key.clone(),
            primary_binding: binding(),
            metadata: Value::Null,
        })
        .await
        .expect("create session");
    let thread = ThreadKey::new(key, "main").expect("thread key");
    (backend, thread)
}

async fn running() -> (HttpSessionBackend, ThreadKey, TurnReceipt, Lease) {
    let (backend, thread) = created().await;
    let receipt = backend
        .submit(&thread, "req-1", json!({ "text": "hi" }))
        .await
        .expect("submit");
    let lease = backend
        .claim(&thread, "owner-a", Duration::from_secs(30))
        .await
        .expect("claim")
        .expect("a claimable turn");
    (backend, thread, receipt, lease)
}

#[tokio::test]
async fn a_turn_reaches_a_terminal_state_through_a_fenced_commit() {
    let (backend, thread, _receipt, lease) = running().await;
    let frame = backend.frame(&lease).await.expect("frame");
    assert_eq!(frame.turn.status, TurnStatus::Running);

    let mut commit = Commit::new("c1", frame.turn.revision, TurnStatus::Succeeded);
    commit.output = json!({ "reply": "ok" });
    let done = backend
        .commit(&lease, commit.clone())
        .await
        .expect("commit");
    assert_eq!(done.revision, 1);

    // A replayed commit answers with the original receipt even though the lease is gone.
    let replay = backend.commit(&lease, commit).await.expect("replay");
    assert_eq!(replay, done);

    let view = backend.turn(&thread, &lease.turn_id).await.expect("turn");
    assert_eq!(view.status, TurnStatus::Succeeded);

    let receipt = backend
        .receipt(&thread, &lease.turn_id, "c1")
        .await
        .expect("receipt");
    assert_eq!(receipt, Some(done));
}

#[tokio::test]
async fn a_changed_input_for_the_same_request_is_a_conflict() {
    let (backend, thread) = created().await;
    let first = backend
        .submit(&thread, "req-1", json!({ "text": "hi" }))
        .await
        .expect("submit");
    let again = backend
        .submit(&thread, "req-1", json!({ "text": "hi" }))
        .await
        .expect("idempotent submit");
    assert_eq!(first.turn_id, again.turn_id);

    let conflict = backend
        .submit(&thread, "req-1", json!({ "text": "other" }))
        .await;
    assert_eq!(conflict.expect_err("conflict").code(), ErrorCode::Conflict);
}

#[tokio::test]
async fn a_stale_revision_or_a_stale_epoch_cannot_commit() {
    let (backend, thread, _receipt, lease) = running().await;
    let mut stale = Commit::new("c-stale", 7, TurnStatus::Running);
    stale.output = json!({ "partial": true });
    let conflict = backend.commit(&lease, stale).await;
    assert_eq!(conflict.expect_err("revision").code(), ErrorCode::Conflict);

    backend
        .release(&lease, Duration::ZERO)
        .await
        .expect("release");
    let takeover = backend
        .claim(&thread, "owner-b", Duration::from_secs(30))
        .await
        .expect("claim")
        .expect("a claimable turn");
    assert!(takeover.epoch > lease.epoch);

    let mut commit = Commit::new("c-epoch", 0, TurnStatus::Running);
    commit.output = json!({});
    let fenced = backend.commit(&lease, commit).await;
    assert_eq!(fenced.expect_err("fenced").code(), ErrorCode::Fenced);
}

#[tokio::test]
async fn a_waiting_turn_is_only_ready_after_its_signal_arrives() {
    let (backend, thread, receipt, lease) = running().await;
    let frame = backend.frame(&lease).await.expect("frame");
    let mut commit = Commit::new("c-wait", frame.turn.revision, TurnStatus::Waiting);
    commit.waiting = Some(WaitCondition::new("approval", 0).expect("wait condition"));
    backend.commit(&lease, commit).await.expect("wait");

    let selector = HarnessSelector::all("harness-a").expect("selector");
    assert!(backend
        .ready(&[selector.clone()], 10)
        .await
        .expect("ready")
        .is_empty());

    backend
        .signal(
            &thread,
            &receipt.turn_id,
            Signal::new("sig-1", "approval", json!({ "ok": true })).expect("signal"),
        )
        .await
        .expect("signal");

    let ready = backend.ready(&[selector], 10).await.expect("ready");
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].key.thread_id, "main");
}

#[tokio::test]
async fn an_identical_checkpoint_does_not_create_a_new_position() {
    let (backend, thread, _receipt, lease) = running().await;
    let mut first = Commit::new("c1", 0, TurnStatus::Running);
    first.checkpoint = json!({ "messages": [], "iteration": 1 });
    backend
        .commit(&lease, first)
        .await
        .expect("first checkpoint");

    let mut second = Commit::new("c2", 1, TurnStatus::Running);
    second.checkpoint = json!({ "messages": [], "iteration": 1 });
    backend
        .commit(&lease, second)
        .await
        .expect("second checkpoint");

    // The management route answers the same baseline a local host would read. The checkpoint was
    // written by the first commit, which carried no journal entry, so its journal position is 0.
    let view = backend.checkpoint(&thread, 1).await.expect("checkpoint");
    assert_eq!(view.metadata.position, "1");
    assert!(!view.settled);
}

#[tokio::test]
async fn an_unsettled_tool_blocks_a_terminal_commit_until_it_is_applied() {
    let (backend, thread, receipt, lease) = running().await;
    let frame = backend.frame(&lease).await.expect("frame");
    let call = ToolCall::new("call-1", "shell", "run", json!({ "cmd": "ls" })).expect("call");
    let mut intent = Commit::new("c-intent", frame.turn.revision, TurnStatus::Running);
    intent.tools = vec![call];
    backend.commit(&lease, intent).await.expect("tool intent");

    let frame = backend.frame(&lease).await.expect("frame");
    let execution_id = frame.tools[0].request.execution_id.clone();

    let rejected = backend
        .commit(&lease, Commit::new("c-settled", 1, TurnStatus::Succeeded))
        .await;
    assert_eq!(rejected.expect_err("unsettled").code(), ErrorCode::Invalid);

    let before = backend
        .begin_dispatch(&lease, &execution_id)
        .await
        .expect("dispatch");
    assert!(!before.dispatch_attempted);
    let after = backend
        .begin_dispatch(&lease, &execution_id)
        .await
        .expect("dispatch again");
    assert!(after.dispatch_attempted);

    backend
        .accept_result(
            &thread,
            &execution_id,
            ExecutionResult::succeeded(json!({ "out": 1 })),
        )
        .await
        .expect("result");

    let mut terminal = Commit::new("c-applied", 1, TurnStatus::Succeeded);
    terminal.checkpoint = json!({ "messages": [], "iteration": 2 });
    terminal.applied_results = vec![AppliedResult {
        execution_id: execution_id.clone(),
        applied_at: RunStepRef::new("run-1", None).expect("origin"),
    }];
    let done = backend.commit(&lease, terminal).await.expect("terminal");
    assert_eq!(done.revision, 2);

    let execution = backend
        .execution(&thread, &execution_id)
        .await
        .expect("execution");
    assert!(execution.applied);
    let view = backend.turn(&thread, &receipt.turn_id).await.expect("turn");
    assert_eq!(view.status, TurnStatus::Succeeded);
}

#[tokio::test]
async fn the_management_surface_reads_and_writes_over_http() {
    let (backend, thread) = created().await;
    let key = thread.session.clone();

    let view = backend.session(&key).await.expect("session");
    assert_eq!(view.primary_thread_id, "main");
    let thread_view = backend.thread(&thread).await.expect("thread");
    assert_eq!(thread_view.definition.key, thread);
    assert_eq!(
        backend
            .threads(&key, None, 10)
            .await
            .expect("threads")
            .items
            .len(),
        1
    );

    let receipt = backend
        .submit(&thread, "req-1", json!({ "text": "hi" }))
        .await
        .expect("submit");
    assert!(backend
        .admission(&key, "req-1")
        .await
        .expect("admission")
        .is_some());
    assert_eq!(
        backend
            .turns(&thread, None, 10)
            .await
            .expect("turns")
            .items
            .len(),
        1
    );
    backend.history(&thread, None, 10).await.expect("history");

    let command = ControlCommand {
        turn_id: receipt.turn_id.clone(),
        request_id: "cmd-1".to_string(),
        kind: ControlKind::Pause,
        target: None,
        value: Value::Null,
        expected_revision: 0,
    };
    let applied = backend.control(&thread, command).await.expect("control");
    assert_eq!(applied.request_id, "cmd-1");

    let snapshot = backend.snapshot(&key).await.expect("snapshot");
    assert_eq!(snapshot.session, key);
    assert!(!snapshot.cursor.is_empty());

    let state = backend.thread_state(&thread).await.expect("thread state");
    assert_eq!(state.definition.key, thread);
}

#[tokio::test]
async fn resources_round_trip_over_http() {
    let (backend, _thread, _receipt, lease) = running().await;
    let request = ResourceRequest {
        request_id: "r-1".to_string(),
        operation: ResourceOperation::Put,
        namespace: vec!["workspace".to_string()],
        key: Some("a.txt".to_string()),
        value: json!({ "body": "x" }),
        expected_version: 0,
        limit: 0,
        offset: 0,
    };
    let values = backend
        .resources(&lease, request)
        .await
        .expect("resource put");
    assert_eq!(values.len(), 1);
    assert_eq!(values[0].key, "a.txt");

    let read = ResourceRequest {
        request_id: "r-2".to_string(),
        operation: ResourceOperation::Get,
        namespace: vec!["workspace".to_string()],
        key: Some("a.txt".to_string()),
        value: Value::Null,
        expected_version: 0,
        limit: 0,
        offset: 0,
    };
    let values = backend.resources(&lease, read).await.expect("resource get");
    assert_eq!(values[0].value, json!({ "body": "x" }));
}

/// The SSE frames carry the committed cursor, and a reconnect after that cursor returns only what
/// came later: an observer cannot be handed the same committed event twice.
#[tokio::test]
async fn sse_streams_committed_frames_and_advances_the_cursor() {
    use agentscope_durable_http::sse;
    use futures::StreamExt;

    let txn: Arc<dyn SessionBackend> = Arc::new(TxnBackend::in_memory());
    let key = SessionKey::new("ns", "sse").expect("session key");
    txn.create(SessionSpec {
        key: key.clone(),
        primary_binding: binding(),
        metadata: Value::Null,
    })
    .await
    .expect("create");
    let thread = ThreadKey::new(key.clone(), "main").expect("thread key");
    let receipt = txn
        .submit(&thread, "req-1", json!({ "text": "hi" }))
        .await
        .expect("submit");
    let lease = txn
        .claim(&thread, "owner-a", Duration::from_secs(30))
        .await
        .expect("claim")
        .expect("turn");
    let frame = txn.frame(&lease).await.expect("frame");
    let mut commit = Commit::new("c1", frame.turn.revision, TurnStatus::Succeeded);
    commit.output = json!({ "reply": "ok" });
    txn.commit(&lease, commit).await.expect("commit");
    assert_eq!(receipt.turn_id, lease.turn_id);

    let mut first = Box::pin(sse::stream(
        txn.clone(),
        key.clone(),
        None,
        100,
        EventStreamOptions::defaults(),
        Duration::from_millis(5),
    ));
    let bytes = first.next().await.expect("a frame").expect("infallible");
    let text = String::from_utf8(bytes.to_vec()).expect("utf8");
    assert!(text.contains("event: durable"), "{text}");
    let cursor = text
        .lines()
        .filter_map(|line| line.strip_prefix("id: "))
        .last()
        .expect("a committed cursor")
        .to_string();
    drop(first);

    let page = txn
        .events(&key, Some(&cursor), 100)
        .await
        .expect("events after the cursor");
    assert!(
        page.events.is_empty(),
        "the cursor acknowledged events that were already delivered"
    );

    let mut resumed = Box::pin(sse::stream(
        txn,
        key,
        Some(cursor),
        100,
        EventStreamOptions::defaults(),
        Duration::from_millis(5),
    ));
    let bytes = resumed
        .next()
        .await
        .expect("a heartbeat")
        .expect("infallible");
    let text = String::from_utf8(bytes.to_vec()).expect("utf8");
    assert!(text.contains("heartbeat"), "{text}");
}

#[tokio::test]
async fn a_deltas_filter_narrows_the_event_page_over_the_wire() {
    let (backend, thread, _receipt, lease) = running().await;
    let frame = backend.frame(&lease).await.expect("frame");
    let mut commit = Commit::new("c1", frame.turn.revision, TurnStatus::Succeeded);
    commit.output = json!({ "reply": "ok" });
    backend.commit(&lease, commit).await.expect("commit");

    let all = backend
        .events(&thread.session, None, 100)
        .await
        .expect("all events");
    assert!(all
        .events
        .iter()
        .any(|event| event.event_type == "turn.queued"));

    let ending = backend
        .events_with(
            &thread.session,
            None,
            100,
            EventStreamOptions::defaults().with_deltas(DeltaFilter::None),
        )
        .await
        .expect("terminal events");
    assert_eq!(ending.events.len(), 1);
    assert_eq!(ending.events[0].event_type, "turn.succeeded");
    assert_eq!(ending.cursor, all.cursor);

    let messages = backend
        .events_with(
            &thread.session,
            None,
            100,
            EventStreamOptions::defaults().with_deltas(DeltaFilter::Message),
        )
        .await
        .expect("message events");
    assert!(messages.events.iter().all(|event| {
        event.event_type.starts_with("message/") || event.event_type == "turn.succeeded"
    }));
}

#[tokio::test]
async fn the_sse_stream_honours_a_deltas_filter() {
    use agentscope_durable_http::sse;
    use futures::StreamExt;

    let (host, _backend) = contract_backend();
    let key = SessionKey::new("ns", "s1").expect("session key");
    host.create(SessionSpec {
        key: key.clone(),
        primary_binding: binding(),
        metadata: Value::Null,
    })
    .await
    .expect("create session");
    let thread = ThreadKey::new(key.clone(), "main").expect("thread key");
    host.submit(&thread, "req-1", json!({ "text": "hi" }))
        .await
        .expect("submit");
    let lease = host
        .claim(&thread, "owner-a", Duration::from_secs(30))
        .await
        .expect("claim")
        .expect("a claimable turn");
    let frame = host.frame(&lease).await.expect("frame");
    let mut commit = Commit::new("c1", frame.turn.revision, TurnStatus::Succeeded);
    commit.output = json!({ "reply": "ok" });
    host.commit(&lease, commit).await.expect("commit");

    let mut stream = Box::pin(sse::stream(
        host,
        key,
        None,
        100,
        EventStreamOptions::defaults().with_deltas(DeltaFilter::None),
        Duration::from_millis(5),
    ));
    let bytes = stream.next().await.expect("a frame").expect("infallible");
    let text = String::from_utf8(bytes.to_vec()).expect("utf8");
    assert!(text.contains("turn.succeeded"), "{text}");
    assert!(!text.contains("turn.queued"), "{text}");
}
