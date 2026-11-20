//! Wakeup publication and the parallel scan.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use agentscope_durable::backend::TxnBackend;
use agentscope_durable::harness_adapter::{
    settle_turn, AdapterCapabilities, HarnessAdapter, HarnessContext,
};
use agentscope_durable::runtime::{DurableRuntime, TickOutcome};
use agentscope_durable::types::{
    HarnessSelector, SessionKey, SessionSpec, ThreadBinding, ThreadKey, ThreadSpec, TurnStatus,
};
use agentscope_durable::wakeups::{Wakeup, WakeupReason};
use agentscope_durable::{LocalBackend, LocalWakeups, Result, Wakeups};

struct CountingAdapter(Arc<AtomicUsize>);

#[async_trait]
impl HarnessAdapter for CountingAdapter {
    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            state_version: "state-1".to_string(),
            recovery_boundary: true,
            event_delivery: false,
        }
    }

    async fn execute(&self, _ctx: &HarnessContext) -> Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Ends the turn `Succeeded`, which commits through the backend and therefore must publish a
/// `Committed` wakeup.
struct SettlingAdapter;

#[async_trait]
impl HarnessAdapter for SettlingAdapter {
    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            state_version: "state-1".to_string(),
            recovery_boundary: true,
            event_delivery: false,
        }
    }

    async fn execute(&self, ctx: &HarnessContext) -> Result<()> {
        settle_turn(
            ctx.backend.as_ref(),
            &ctx.lease,
            TurnStatus::Succeeded,
            json!({"ok": true}),
            None,
        )
        .await
    }
}

fn binding() -> ThreadBinding {
    ThreadBinding::new("stub", "definition-1", "state-1").expect("binding")
}

async fn create_session(backend: &Arc<dyn agentscope_durable::SessionBackend>) {
    let key = SessionKey::new("ns", "s1").expect("session key");
    backend
        .create(SessionSpec {
            key,
            primary_binding: binding(),
            metadata: json!({}),
        })
        .await
        .expect("create session");
}

async fn add_threads(
    backend: &Arc<dyn agentscope_durable::SessionBackend>,
    count: usize,
) -> Vec<ThreadKey> {
    let session = SessionKey::new("ns", "s1").expect("session key");
    let mut threads = Vec::with_capacity(count);
    for index in 0..count {
        let thread = ThreadKey::new(session.clone(), format!("t{index}")).expect("thread key");
        backend
            .create_thread(ThreadSpec {
                key: thread.clone(),
                binding: binding(),
            })
            .await
            .expect("create thread");
        threads.push(thread);
    }
    threads
}

#[tokio::test]
async fn an_admission_and_a_commit_publish_a_wakeup() {
    let wakeups = Arc::new(LocalWakeups::new());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    wakeups.subscribe(Arc::new(move |wakeup: &Wakeup| {
        recorded
            .lock()
            .map(|mut reasons| reasons.push(wakeup.reason))
            .ok();
    }));
    let backend: Arc<dyn agentscope_durable::SessionBackend> =
        Arc::new(TxnBackend::in_memory().with_wakeups(Arc::clone(&wakeups) as Arc<dyn Wakeups>));
    create_session(&backend).await;
    let thread = add_threads(&backend, 1).await.remove(0);
    backend
        .submit(&thread, "request-1", json!({"text": "hi"}))
        .await
        .expect("submit");
    assert!(seen
        .lock()
        .expect("reasons")
        .contains(&WakeupReason::Submitted));

    let runtime = DurableRuntime::new(Arc::clone(&backend), "host-1")
        .with_adapter("stub", Arc::new(SettlingAdapter));
    runtime.tick(&thread).await.expect("tick");
    assert!(seen
        .lock()
        .expect("reasons")
        .contains(&WakeupReason::Committed));
}

#[tokio::test]
async fn a_parallel_scan_ticks_every_ready_thread_once() {
    let backend = LocalBackend::in_memory();
    create_session(&backend).await;
    let threads = add_threads(&backend, 8).await;
    for (index, thread) in threads.iter().enumerate() {
        backend
            .submit(thread, &format!("request-{index}"), json!({}))
            .await
            .expect("submit");
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = DurableRuntime::new(Arc::clone(&backend), "host-1")
        .with_adapter("stub", Arc::new(CountingAdapter(Arc::clone(&calls))));
    let selector = HarnessSelector::all("stub").expect("selector");
    let outcomes = runtime.scan_with(&[selector], 100, 4).await.expect("scan");
    assert_eq!(outcomes.len(), 8);
    assert_eq!(calls.load(Ordering::SeqCst), 8);
    assert!(outcomes
        .iter()
        .all(|outcome| matches!(outcome, TickOutcome::Settled)));
}

#[tokio::test]
async fn a_serial_scan_matches_the_old_scan() {
    let selector = HarnessSelector::all("stub").expect("selector");

    let old_backend = LocalBackend::in_memory();
    create_session(&old_backend).await;
    let old_threads = add_threads(&old_backend, 4).await;
    for (index, thread) in old_threads.iter().enumerate() {
        old_backend
            .submit(thread, &format!("request-{index}"), json!({}))
            .await
            .expect("submit");
    }
    let old_calls = Arc::new(AtomicUsize::new(0));
    let old_runtime = DurableRuntime::new(Arc::clone(&old_backend), "host-1")
        .with_adapter("stub", Arc::new(CountingAdapter(Arc::clone(&old_calls))));
    let legacy = old_runtime
        .scan(&[selector.clone()], 100)
        .await
        .expect("scan");

    let new_backend = LocalBackend::in_memory();
    create_session(&new_backend).await;
    let new_threads = add_threads(&new_backend, 4).await;
    for (index, thread) in new_threads.iter().enumerate() {
        new_backend
            .submit(thread, &format!("request-{index}"), json!({}))
            .await
            .expect("submit");
    }
    let new_calls = Arc::new(AtomicUsize::new(0));
    let new_runtime = DurableRuntime::new(Arc::clone(&new_backend), "host-1")
        .with_adapter("stub", Arc::new(CountingAdapter(Arc::clone(&new_calls))));
    let planned = new_runtime
        .scan_with(&[selector], 100, 1)
        .await
        .expect("scan_with");

    assert_eq!(legacy, planned);
    assert_eq!(old_calls.load(Ordering::SeqCst), 4);
    assert_eq!(new_calls.load(Ordering::SeqCst), 4);
}
