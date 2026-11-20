//! The resident and edge loops: the cadence around the durable tick.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agentscope_durable::backend::TxnBackend;
use agentscope_durable::harness_adapter::{AdapterCapabilities, HarnessAdapter, HarnessContext};
use agentscope_durable::types::*;
use agentscope_durable::{DurableRuntime, Result as DurableResult, SessionBackend};
use agentscope_durable_worker::{EdgeRunner, ResidentOptions, ResidentRunner};
use async_trait::async_trait;
use serde_json::json;
use tokio::sync::oneshot;
use tokio::time::timeout;

struct Counting(Arc<AtomicUsize>);

#[async_trait]
impl HarnessAdapter for Counting {
    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            state_version: "state-1".to_string(),
            recovery_boundary: true,
            event_delivery: false,
        }
    }

    async fn execute(&self, _ctx: &HarnessContext) -> DurableResult<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn options() -> ResidentOptions {
    ResidentOptions {
        interval: Duration::from_millis(10),
        parallelism: 1,
        limit: 32,
    }
}

async fn world(calls: Arc<AtomicUsize>) -> (DurableRuntime, ThreadKey) {
    let backend: Arc<dyn SessionBackend> = Arc::new(TxnBackend::in_memory());
    let session = SessionKey::new("ns", "s1").expect("session key");
    let binding = ThreadBinding::new("stub", "def-1", "state-1").expect("binding");
    backend
        .create(SessionSpec {
            key: session.clone(),
            primary_binding: binding.clone(),
            metadata: json!({}),
        })
        .await
        .expect("create session");
    let thread = ThreadKey::new(session, "main").expect("thread");
    backend
        .create_thread(ThreadSpec {
            key: thread.clone(),
            binding,
        })
        .await
        .expect("create thread");
    backend
        .submit(&thread, "request-1", json!({"text": "hi"}))
        .await
        .expect("submit");
    let runtime =
        DurableRuntime::new(backend, "host-1").with_adapter("stub", Arc::new(Counting(calls)));
    (runtime, thread)
}

#[tokio::test]
async fn an_edge_runner_scans_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let (runtime, _thread) = world(Arc::clone(&calls)).await;
    let selector = HarnessSelector::all("stub").expect("selector");
    let runner = EdgeRunner::new(runtime, vec![selector], options());
    let outcomes = runner.run_once().await.expect("scan");
    assert_eq!(outcomes.len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_resident_runner_scans_until_shutdown() {
    let calls = Arc::new(AtomicUsize::new(0));
    let (runtime, _thread) = world(Arc::clone(&calls)).await;
    let selector = HarnessSelector::all("stub").expect("selector");
    let runner = ResidentRunner::new(runtime, vec![selector], options());
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let handle = tokio::spawn(runner.run_until(shutdown_rx));
    tokio::time::sleep(Duration::from_millis(50)).await;
    shutdown_tx.send(()).expect("shutdown");
    timeout(Duration::from_secs(2), handle)
        .await
        .expect("participant stops")
        .expect("join");
    assert!(calls.load(Ordering::SeqCst) >= 1);
}
