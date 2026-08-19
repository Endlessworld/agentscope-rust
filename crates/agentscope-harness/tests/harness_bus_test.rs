//! Integration tests for the harness bus module: the filesystem-backed message
//! bus (drain queue / replay log / inbox / wakeup / session events) and the
//! async-tool registry.
use std::sync::Arc;

use agentscope_harness::{
    AsyncToolRecord, AsyncToolRegistry, Filesystem, LocalFilesystem, MessageBus,
    WorkspaceAsyncToolRegistry, WorkspaceMessageBus,
};
use chrono::{Duration as ChronoDuration, Utc};
use serde_json::json;

async fn temp_dir(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "agentscope_harness_bus_{tag}_{}",
        std::process::id()
    ));
    let _ = tokio::fs::remove_dir_all(&base).await;
    tokio::fs::create_dir_all(&base).await.unwrap();
    base
}

// ---- Mode A: drain queue ----

#[tokio::test]
async fn queue_drain_consumes_atomically() {
    let dir = temp_dir("queue").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let bus = WorkspaceMessageBus::new(fs, "/bus");

    let id1 = bus.queue_push("q", json!({"n": 1})).await.unwrap();
    let id2 = bus.queue_push("q", json!({"n": 2})).await.unwrap();
    assert_ne!(id1, id2);

    let drained = bus.queue_drain("q", 10).await.unwrap();
    assert_eq!(drained.len(), 2);
    assert_eq!(drained[0].id, id1);
    assert_eq!(drained[1].id, id2);
    assert_eq!(drained[1].payload["n"], 2);

    // Second drain returns nothing: entries were consumed atomically.
    let again = bus.queue_drain("q", 10).await.unwrap();
    assert!(again.is_empty());

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn queue_peek_and_delete() {
    let dir = temp_dir("queue_peek").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let bus = WorkspaceMessageBus::new(fs, "/bus");

    assert!(!bus.queue_peek("peek-q").await.unwrap());
    bus.queue_push("peek-q", json!({"a": 1})).await.unwrap();
    assert!(bus.queue_peek("peek-q").await.unwrap());

    bus.queue_delete("peek-q").await.unwrap();
    assert!(!bus.queue_peek("peek-q").await.unwrap());
    // Delete is idempotent.
    bus.queue_delete("peek-q").await.unwrap();

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn queue_drain_honors_max_count() {
    let dir = temp_dir("queue_max").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let bus = WorkspaceMessageBus::new(fs, "/bus");

    for i in 0..5 {
        bus.queue_push("partial", json!({"i": i})).await.unwrap();
    }
    let drained = bus.queue_drain("partial", 2).await.unwrap();
    assert_eq!(drained.len(), 2);
    assert_eq!(drained[0].payload["i"], 0);
    assert_eq!(drained[1].payload["i"], 1);

    // The remaining three are still there.
    let rest = bus.queue_drain("partial", 10).await.unwrap();
    assert_eq!(rest.len(), 3);
    assert_eq!(rest[0].payload["i"], 2);

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

// ---- Mode C: replay log ----

#[tokio::test]
async fn log_append_read_with_since_cursor() {
    let dir = temp_dir("log").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let bus = WorkspaceMessageBus::new(fs, "/bus");

    let e1 = bus.log_append("log", json!({"i": 1}), 100).await.unwrap();
    let e2 = bus.log_append("log", json!({"i": 2}), 100).await.unwrap();
    let e3 = bus.log_append("log", json!({"i": 3}), 100).await.unwrap();

    // Read from the beginning.
    let all = bus.log_read("log", None, 100).await.unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].id, e1);
    assert_eq!(all[2].id, e3);

    // Read strictly after cursor e1.
    let after = bus.log_read("log", Some(&e1), 100).await.unwrap();
    assert_eq!(after.len(), 2);
    assert_eq!(after[0].id, e2);
    assert_eq!(after[1].id, e3);

    // maxCount caps results.
    let capped = bus.log_read("log", None, 2).await.unwrap();
    assert_eq!(capped.len(), 2);

    // Non-destructive: reading again returns the same entries.
    let all_again = bus.log_read("log", None, 100).await.unwrap();
    assert_eq!(all_again.len(), 3);

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn log_append_trims_to_max_len() {
    let dir = temp_dir("log_cap").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let bus = WorkspaceMessageBus::new(fs, "/bus");

    for i in 0..5 {
        bus.log_append("cap", json!({"i": i}), 3).await.unwrap();
    }
    let all = bus.log_read("cap", None, 100).await.unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].payload["i"], 2); // oldest two trimmed
    assert_eq!(all[2].payload["i"], 4);

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn log_trim_removes_entries() {
    let dir = temp_dir("log_trim").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let bus = WorkspaceMessageBus::new(fs, "/bus");

    bus.log_append("lt", json!({"x": 1}), 100).await.unwrap();
    bus.log_append("lt", json!({"x": 2}), 100).await.unwrap();
    assert_eq!(bus.log_read("lt", None, 100).await.unwrap().len(), 2);

    bus.log_trim("lt").await.unwrap();
    assert!(bus.log_read("lt", None, 100).await.unwrap().is_empty());
    bus.log_trim("lt").await.unwrap(); // idempotent

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

// ---- Domain helpers: inbox ----

#[tokio::test]
async fn inbox_push_drain_has_messages() {
    let dir = temp_dir("inbox").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let bus = WorkspaceMessageBus::new(fs, "/bus");

    assert!(!bus.inbox_has_messages("sess-1").await.unwrap());

    bus.inbox_push("sess-1", json!({"kind": "hint", "text": "hello"}))
        .await
        .unwrap();
    bus.inbox_push("sess-1", json!({"kind": "hint", "text": "world"}))
        .await
        .unwrap();
    assert!(bus.inbox_has_messages("sess-1").await.unwrap());

    let drained = bus.inbox_drain("sess-1", 10).await.unwrap();
    assert_eq!(drained.len(), 2);
    assert_eq!(drained[0].payload["text"], "hello");
    assert_eq!(drained[1].payload["text"], "world");

    // Inbox is isolated per session.
    assert!(!bus.inbox_has_messages("sess-2").await.unwrap());
    assert!(!bus.inbox_has_messages("sess-1").await.unwrap());

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

// ---- Domain helpers: session events ----

#[tokio::test]
async fn session_publish_and_read_events() {
    let dir = temp_dir("events").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let bus = WorkspaceMessageBus::new(fs, "/bus");

    let e1 = bus
        .session_publish_event("sess-9", json!({"type": "start"}))
        .await
        .unwrap();
    let e2 = bus
        .session_publish_event("sess-9", json!({"type": "finish"}))
        .await
        .unwrap();
    assert_ne!(e1, e2);

    let events = bus.session_read_events("sess-9", None, 100).await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].id, e1);
    assert_eq!(events[1].payload["type"], "finish");

    // Cursor-based replay.
    let tail = bus.session_read_events("sess-9", Some(&e1), 100).await.unwrap();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].id, e2);

    // Trimming clears the log for late subscribers.
    bus.session_trim_events("sess-9").await.unwrap();
    assert!(bus.session_read_events("sess-9", None, 100).await.unwrap().is_empty());

    // Events are isolated per session.
    bus.session_publish_event("other", json!({"type": "x"})).await.unwrap();
    let other = bus.session_read_events("other", None, 100).await.unwrap();
    assert_eq!(other.len(), 1);
    assert!(bus.session_read_events("sess-9", None, 100).await.unwrap().is_empty());

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

// ---- Domain helpers: wakeup ----

#[tokio::test]
async fn enqueue_wakeup_writes_wakeups_queue() {
    let dir = temp_dir("wakeup").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let bus = WorkspaceMessageBus::new(fs, "/bus");

    bus.enqueue_wakeup("user-1", "sess-1", "agent-1").await.unwrap();

    let drained = bus.queue_drain("agentscope:wakeups", 10).await.unwrap();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].payload["userId"], "user-1");
    assert_eq!(drained[0].payload["sessionId"], "sess-1");
    assert_eq!(drained[0].payload["agentId"], "agent-1");

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

// ---- Mode D: degraded pub/sub ----

#[tokio::test]
async fn subscribe_returns_empty_poll_stream() {
    let dir = temp_dir("subscribe").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let bus = WorkspaceMessageBus::new(fs, "/bus");

    // Degraded pub/sub: no payload arrives immediately; ticks land every 3s.
    let mut rx = bus.subscribe("chan");
    assert!(rx.try_recv().is_err());
    let mut wakeup = bus.subscribe_wakeup();
    assert!(wakeup.try_recv().is_err());
    let mut events = bus.session_subscribe_events("sess-x");
    assert!(events.try_recv().is_err());

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

// ---- AsyncToolRegistry ----

#[tokio::test]
async fn async_tool_registry_register_and_complete() {
    let dir = temp_dir("tools").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let reg = WorkspaceAsyncToolRegistry::new(fs, "/bus/async-tools");

    let rec = AsyncToolRecord::new("rec-1", "sess-1", "shell_execute", "call-1", Utc::now());
    reg.register(rec).await.unwrap();

    // A fresh RUNNING record is not stale.
    let stale = reg.find_stale("sess-1", ChronoDuration::minutes(30)).await.unwrap();
    assert!(stale.is_empty());

    reg.complete("rec-1", "ok").await.unwrap();
    let stale = reg.find_stale("sess-1", ChronoDuration::seconds(0)).await.unwrap();
    assert!(stale.is_empty(), "completed records are not stale");

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn async_tool_registry_find_stale_and_mark_timeout() {
    let dir = temp_dir("tools_stale").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let reg = WorkspaceAsyncToolRegistry::new(fs, "/bus/async-tools");

    // A RUNNING record created long ago is stale.
    let old = Utc::now() - ChronoDuration::hours(2);
    let rec = AsyncToolRecord::new("rec-old", "sess-1", "shell_execute", "call-old", old);
    reg.register(rec).await.unwrap();

    // A fresh RUNNING record is not stale.
    let fresh = AsyncToolRecord::new("rec-new", "sess-1", "shell_execute", "call-new", Utc::now());
    reg.register(fresh).await.unwrap();

    let stale = reg.find_stale("sess-1", ChronoDuration::minutes(30)).await.unwrap();
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].id, "rec-old");

    // markTimeout flips the status so the record is no longer returned.
    reg.mark_timeout("rec-old").await.unwrap();
    let stale = reg.find_stale("sess-1", ChronoDuration::minutes(30)).await.unwrap();
    assert!(stale.is_empty());

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn async_tool_registry_fail_sets_status() {
    let dir = temp_dir("tools_fail").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let reg = WorkspaceAsyncToolRegistry::new(fs, "/bus/async-tools");

    let rec = AsyncToolRecord::new("rec-f", "sess-1", "shell_execute", "call-f", Utc::now());
    reg.register(rec).await.unwrap();
    reg.fail("rec-f", "boom").await.unwrap();

    // Even with a zero TTL, a FAILED record is never stale.
    let stale = reg.find_stale("sess-1", ChronoDuration::seconds(0)).await.unwrap();
    assert!(stale.is_empty());

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn async_tool_registry_updates_unknown_id_is_noop() {
    let dir = temp_dir("tools_noop").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let reg = WorkspaceAsyncToolRegistry::new(fs, "/bus/async-tools");

    reg.complete("does-not-exist", "x").await.unwrap();
    reg.mark_timeout("does-not-exist").await.unwrap();
    reg.fail("does-not-exist", "x").await.unwrap();

    let _ = tokio::fs::remove_dir_all(&dir).await;
}
