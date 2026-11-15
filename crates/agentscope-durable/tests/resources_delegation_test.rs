//! Session resources, delegation and restoration: the three branch-level facts the backend now
//! carries, and the invariants they must not relax.

use std::time::Duration;

use agentscope_durable::backend::TxnBackend;
use agentscope_durable::types::*;
use agentscope_durable::{ErrorCode, SessionBackend};
use serde_json::{json, Value};

const TTL: Duration = Duration::from_secs(30);

fn binding() -> ThreadBinding {
    ThreadBinding::new("harness-a", "def-1", "state-1").expect("binding")
}

async fn created() -> (TxnBackend, SessionKey, ThreadKey) {
    let backend = TxnBackend::in_memory();
    let session = SessionKey::new("ns", "s1").expect("session key");
    backend
        .create(SessionSpec {
            key: session.clone(),
            primary_binding: binding(),
            metadata: Value::Null,
        })
        .await
        .expect("create session");
    let thread = ThreadKey::new(session.clone(), "main").expect("thread key");
    (backend, session, thread)
}

async fn running() -> (TxnBackend, SessionKey, ThreadKey, TurnReceipt, Lease) {
    let (backend, session, thread) = created().await;
    let receipt = backend
        .submit(&thread, "req-parent", json!({ "text": "go" }))
        .await
        .expect("submit");
    let lease = backend
        .claim(&thread, "writer-a", TTL)
        .await
        .expect("claim")
        .expect("a claimable turn");
    (backend, session, thread, receipt, lease)
}

fn request(
    request_id: &str,
    operation: ResourceOperation,
    namespace: &[&str],
    key: Option<&str>,
) -> ResourceRequest {
    ResourceRequest {
        request_id: request_id.to_string(),
        operation,
        namespace: namespace.iter().map(|part| part.to_string()).collect(),
        key: key.map(str::to_string),
        value: json!({}),
        expected_version: 0,
        limit: 0,
        offset: 0,
    }
}

fn value(namespace: &[&str], key: &str, value: Value, version: u64) -> ResourceValue {
    ResourceValue {
        namespace: namespace.iter().map(|part| part.to_string()).collect(),
        key: key.to_string(),
        value,
        version,
    }
}

#[tokio::test]
async fn a_resource_written_under_the_lease_reads_back_with_its_version() {
    let (backend, _session, thread, _receipt, lease) = running().await;
    assert_eq!(
        backend
            .thread_state(&thread)
            .await
            .expect("state")
            .resource_position,
        0
    );

    let mut put = request("res-1", ResourceOperation::Put, &["notes"], Some("a"));
    put.value = json!({ "text": "hi" });
    let written = backend.resources(&lease, put.clone()).await.expect("put");
    assert_eq!(
        written,
        vec![value(&["notes"], "a", json!({ "text": "hi" }), 1)]
    );

    // A replayed mutation answers the first application and writes nothing.
    let mut replay = put.clone();
    replay.value = json!({ "text": "hi" });
    assert_eq!(
        backend.resources(&lease, replay).await.expect("replay"),
        written
    );
    assert_eq!(
        backend
            .thread_state(&thread)
            .await
            .expect("state")
            .resource_position,
        1
    );

    // The same identity with a different body is a conflict, not a second write.
    let mut changed = put.clone();
    changed.value = json!({ "text": "other" });
    assert_eq!(
        backend
            .resources(&lease, changed)
            .await
            .expect_err("conflict")
            .code(),
        ErrorCode::Conflict
    );

    let read = backend
        .resources(
            &lease,
            request("res-2", ResourceOperation::Get, &["notes"], Some("a")),
        )
        .await
        .expect("get");
    assert_eq!(read, written);
    assert_eq!(
        backend
            .thread_state(&thread)
            .await
            .expect("state")
            .resource_position,
        1,
        "a read must not advance the generation"
    );

    // Compare-and-set against the observed version, then a stale one.
    let mut cas = request("res-3", ResourceOperation::Cas, &["notes"], Some("a"));
    cas.value = json!({ "text": "v2" });
    cas.expected_version = 1;
    let updated = backend.resources(&lease, cas.clone()).await.expect("cas");
    assert_eq!(updated[0].version, 2);
    cas.request_id = "res-4".to_string();
    assert_eq!(
        backend
            .resources(&lease, cas)
            .await
            .expect_err("stale cas")
            .code(),
        ErrorCode::Conflict
    );

    let deleted = backend
        .resources(
            &lease,
            request("res-5", ResourceOperation::Delete, &["notes"], Some("a")),
        )
        .await
        .expect("delete");
    assert!(deleted.is_empty());
    assert!(backend
        .resources(
            &lease,
            request("res-6", ResourceOperation::Get, &["notes"], Some("a")),
        )
        .await
        .expect("get after delete")
        .is_empty());
    assert_eq!(
        backend
            .thread_state(&thread)
            .await
            .expect("state")
            .resource_position,
        3,
        "two writes and one delete make three generations"
    );
}

#[tokio::test]
async fn a_resource_search_pages_by_namespace_prefix() {
    let (backend, _session, _thread, _receipt, lease) = running().await;
    for (index, (namespace, key)) in [("notes", "a"), ("notes", "b"), ("other", "c")]
        .into_iter()
        .enumerate()
    {
        let mut put = request(
            &format!("search-{index}"),
            ResourceOperation::Put,
            &[namespace],
            Some(key),
        );
        put.value = json!({ "n": index });
        backend.resources(&lease, put).await.expect("put");
    }

    let page = backend
        .resources(
            &lease,
            request("search-notes", ResourceOperation::Search, &["notes"], None),
        )
        .await
        .expect("search");
    assert_eq!(
        page.iter()
            .map(|value| value.key.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "b"]
    );

    let mut window = request("search-window", ResourceOperation::Search, &["notes"], None);
    window.offset = 1;
    window.limit = 1;
    let page = backend
        .resources(&lease, window)
        .await
        .expect("paged search");
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].key, "b");

    let all = backend
        .resources(
            &lease,
            request("search-all", ResourceOperation::Search, &[], None),
        )
        .await
        .expect("search all");
    assert_eq!(all.len(), 3);
}

#[tokio::test]
async fn a_resource_request_from_a_stale_lease_is_refused() {
    let (backend, _session, _thread, _receipt, lease) = running().await;
    let stale = Lease {
        owner: "writer-b".to_string(),
        ..lease
    };
    let error = backend
        .resources(
            &stale,
            request("stale-get", ResourceOperation::Get, &["notes"], Some("a")),
        )
        .await
        .expect_err("fenced");
    assert_eq!(error.code(), ErrorCode::Fenced);
}

#[tokio::test]
async fn a_resource_generation_makes_the_same_checkpoint_a_new_baseline() {
    let (backend, _session, thread, receipt, lease) = running().await;
    let turn = backend.turn(&thread, &receipt.turn_id).await.expect("turn");
    let mut first = Commit::new("c1", turn.revision, TurnStatus::Running);
    first.checkpoint = json!({ "messages": [] });
    let committed = backend.commit(&lease, first).await.expect("commit");

    let mut put = request("gen-1", ResourceOperation::Put, &["notes"], Some("a"));
    put.value = json!({ "text": "hi" });
    backend.resources(&lease, put).await.expect("put");

    // The same checkpoint, now paired with a resource generation it was not taken at, is a
    // different baseline: it is recorded as a new history position rather than reused.
    let mut second = Commit::new("c2", committed.revision, TurnStatus::Running);
    second.checkpoint = json!({ "messages": [] });
    backend.commit(&lease, second).await.expect("commit");
    assert_eq!(
        backend
            .checkpoints(&thread, 0, 10)
            .await
            .expect("checkpoints"),
        vec![1, 2]
    );
}

async fn delegating() -> (TxnBackend, SessionKey, ThreadKey, Lease, String) {
    let (backend, session, thread, receipt, lease) = running().await;
    backend
        .prepare_run(&lease, "invoke-1")
        .await
        .expect("run record");
    let call =
        ToolCall::new("call-1", "subagents", "delegate", json!({ "task": "x" })).expect("call");
    let turn = backend.turn(&thread, &receipt.turn_id).await.expect("turn");
    let mut intent = Commit::new("intent", turn.revision, TurnStatus::Running);
    intent.checkpoint = json!({ "messages": [] });
    intent.tools = vec![call];
    backend.commit(&lease, intent).await.expect("intent");
    let execution_id = backend.frame(&lease).await.expect("frame").tools[0]
        .request
        .execution_id
        .clone();
    (backend, session, thread, lease, execution_id)
}

#[tokio::test]
async fn a_delegation_records_the_child_and_replays_without_a_second_one() {
    let (backend, session, thread, lease, execution_id) = delegating().await;
    let child = ThreadKey::new(session, "child").expect("child key");
    let target = DelegationTarget {
        child: ThreadSpec {
            key: child.clone(),
            binding: binding(),
        },
        input: json!({ "task": "x" }),
    };
    let view = backend
        .delegate(&lease, &execution_id, "delegate-1", target.clone())
        .await
        .expect("delegate");
    let link = view.child.clone().expect("a child link");
    assert_eq!(link.thread, child);
    assert_eq!(view.parent.execution_id, execution_id);
    assert!(!view.cancel_requested);

    // The child turn names its parent, so a recovered child is attributable.
    let child_turn = backend
        .turn(&child, &link.turn_id)
        .await
        .expect("child turn");
    let parent = child_turn.parent.expect("parent identity");
    assert_eq!(parent.thread, thread);
    assert_eq!(parent.execution_id, execution_id);

    // The parent tool call holds the link back.
    let tool = backend
        .execution(&thread, &execution_id)
        .await
        .expect("execution");
    assert_eq!(tool.delegation, Some(link.clone()));

    // A replay answers the same child and creates nothing.
    assert_eq!(
        backend
            .delegate(&lease, &execution_id, "delegate-1", target)
            .await
            .expect("replay"),
        view
    );
    let threads = backend
        .threads(&thread.session, None, 10)
        .await
        .expect("threads");
    assert_eq!(threads.items.len(), 2);
}

#[tokio::test]
async fn a_delegation_to_a_second_child_is_a_conflict() {
    let (backend, session, _thread, lease, execution_id) = delegating().await;
    let target = |thread_id: &str| DelegationTarget {
        child: ThreadSpec {
            key: ThreadKey::new(session.clone(), thread_id).expect("child key"),
            binding: binding(),
        },
        input: json!({ "task": "x" }),
    };
    backend
        .delegate(&lease, &execution_id, "delegate-1", target("child-a"))
        .await
        .expect("delegate");
    let error = backend
        .delegate(&lease, &execution_id, "delegate-2", target("child-b"))
        .await
        .expect_err("one call delegates once");
    assert_eq!(error.code(), ErrorCode::Conflict);
}

fn baseline(thread: &ThreadKey, request_id: &str) -> Restoration {
    Restoration {
        request_id: request_id.to_string(),
        expected_position: 0,
        target: ThreadSpec {
            key: thread.clone(),
            binding: binding(),
        },
        checkpoint: json!({ "messages": [{ "role": "user", "content": "hi" }] }),
        journal: vec![json!({ "type": "message" }), json!({ "type": "message" })],
        resources: vec![value(&["notes"], "a", json!({ "text": "restored" }), 3)],
    }
}

#[tokio::test]
async fn a_restoration_installs_a_settled_baseline_once() {
    let (backend, _session, thread) = created().await;
    let restoration = baseline(&thread, "restore-1");
    let view = backend.restore(restoration.clone()).await.expect("restore");
    assert_eq!(view.definition.key, thread);

    let state = backend.thread_state(&thread).await.expect("state");
    assert_eq!(state.journal_position, 2);
    assert_eq!(state.resource_position, 1);
    let metadata = state.checkpoint_metadata.expect("checkpoint binding");
    assert_eq!(metadata.harness, "harness-a");

    let installed = backend.checkpoint(&thread, 1).await.expect("checkpoint");
    assert_eq!(installed.checkpoint, restoration.checkpoint);
    assert_eq!(installed.journal_position, 2);
    assert!(installed.settled, "a baseline is already settled");
    assert_eq!(
        backend
            .read_journal(&thread, 0, 10)
            .await
            .expect("journal")
            .len(),
        2
    );

    // A replay is a read: the baseline is installed once.
    assert_eq!(backend.restore(restoration).await.expect("replay"), view);
    let again = backend.thread_state(&thread).await.expect("state");
    assert_eq!(again.journal_position, 2);
    assert_eq!(
        backend
            .checkpoints(&thread, 0, 10)
            .await
            .expect("checkpoints"),
        vec![1]
    );

    // The restored resource carries the version the baseline froze.
    backend
        .submit(&thread, "req-after-restore", json!({ "text": "go" }))
        .await
        .expect("submit");
    let lease = backend
        .claim(&thread, "writer-a", TTL)
        .await
        .expect("claim")
        .expect("claimable");
    let read = backend
        .resources(
            &lease,
            request(
                "after-restore",
                ResourceOperation::Get,
                &["notes"],
                Some("a"),
            ),
        )
        .await
        .expect("get");
    assert_eq!(
        read,
        vec![value(&["notes"], "a", json!({ "text": "restored" }), 3)]
    );
}

#[tokio::test]
async fn a_restoration_needs_an_idle_target_at_the_expected_position() {
    let (backend, _session, thread) = created().await;
    let mut stale = baseline(&thread, "restore-stale");
    stale.expected_position = 5;
    assert_eq!(
        backend.restore(stale).await.expect_err("moved").code(),
        ErrorCode::Conflict
    );

    let mut empty = baseline(&thread, "restore-empty");
    empty.checkpoint = Value::Null;
    assert_eq!(
        backend
            .restore(empty)
            .await
            .expect_err("no baseline")
            .code(),
        ErrorCode::Invalid
    );

    let mut foreign = baseline(&thread, "restore-foreign");
    foreign.target.binding = ThreadBinding::new("harness-b", "def-1", "state-1").expect("binding");
    assert_eq!(
        backend
            .restore(foreign)
            .await
            .expect_err("binding is immutable")
            .code(),
        ErrorCode::Conflict
    );

    // A live turn means the state a restore would replace is still being written.
    backend
        .submit(&thread, "req-live", json!({ "text": "go" }))
        .await
        .expect("submit");
    assert_eq!(
        backend
            .restore(baseline(&thread, "restore-live"))
            .await
            .expect_err("live turn")
            .code(),
        ErrorCode::Conflict
    );
}

#[tokio::test]
async fn a_thread_written_before_resources_existed_reads_as_empty() {
    let (backend, _session, thread, _receipt, lease) = running().await;
    // No resource record was ever written: the generation is zero and the answer is empty, which
    // is what this thread truthfully has — not an error and not a fabricated value.
    assert_eq!(
        backend
            .thread_state(&thread)
            .await
            .expect("state")
            .resource_position,
        0
    );
    assert!(backend
        .resources(
            &lease,
            request("legacy-get", ResourceOperation::Get, &["notes"], Some("a")),
        )
        .await
        .expect("empty get")
        .is_empty());
    assert!(backend
        .resources(
            &lease,
            request("legacy-search", ResourceOperation::Search, &[], None),
        )
        .await
        .expect("empty search")
        .is_empty());
    assert_eq!(
        backend
            .resources(
                &lease,
                request(
                    "legacy-delete",
                    ResourceOperation::Delete,
                    &["notes"],
                    Some("a")
                ),
            )
            .await
            .expect_err("nothing to delete")
            .code(),
        ErrorCode::NotFound
    );
}
