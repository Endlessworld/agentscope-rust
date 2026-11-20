//! The smallest durable host: one backend, one adapter, two turns.
//!
//! Run it twice against the same root. The first run admits `run-1` and settles it; the second
//! run finds that turn in the store and settles `run-2` on top of it. `--sqlite` keeps the same
//! turns in a SQLite file instead of the file store, which is the whole backend swap.

use std::sync::Arc;

use agentscope_durable::harness_adapter::{AdapterCapabilities, HarnessAdapter, HarnessContext};
use agentscope_durable::types::*;
use agentscope_durable::{
    DurableRuntime, DurableSession, LocalBackend, Result, SessionBackend, TickOutcome,
};
use async_trait::async_trait;
use serde_json::json;

/// A harness that commits one answer and ends the turn. A real adapter drives a model here.
struct EchoAdapter;

#[async_trait]
impl HarnessAdapter for EchoAdapter {
    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            state_version: "quickstart-1".to_string(),
            recovery_boundary: true,
            event_delivery: false,
        }
    }

    async fn execute(&self, ctx: &HarnessContext) -> Result<()> {
        let text = ctx
            .input()
            .get("text")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        let frame = ctx.backend.frame(&ctx.lease).await?;
        let mut commit = Commit::new(
            format!("echo-{}", frame.turn.revision),
            frame.turn.revision,
            TurnStatus::Succeeded,
        );
        commit.output = json!({ "reply": format!("echo: {text}") });
        ctx.backend.commit(&ctx.lease, commit).await?;
        Ok(())
    }
}

fn binding() -> ThreadBinding {
    ThreadBinding::new("echo", "quickstart-1", "quickstart-1").expect("binding")
}

/// Creates the session and its thread on the first run, and finds them on every later one.
async fn ensure_session(
    backend: &Arc<dyn SessionBackend>,
    session: &SessionKey,
) -> Result<ThreadKey> {
    if backend.session(session).await.is_err() {
        backend
            .create(SessionSpec {
                key: session.clone(),
                primary_binding: binding(),
                metadata: json!({}),
            })
            .await?;
    }
    let thread = ThreadKey::new(session.clone(), "main")?;
    if backend.thread_state(&thread).await.is_err() {
        backend
            .create_thread(ThreadSpec {
                key: thread.clone(),
                binding: binding(),
            })
            .await?;
    }
    Ok(thread)
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut root = std::path::PathBuf::from("./durable-quickstart");
    let mut sqlite = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--sqlite" => sqlite = true,
            other => root = std::path::PathBuf::from(other),
        }
    }

    let backend: Arc<dyn SessionBackend> = if sqlite {
        std::fs::create_dir_all(&root).ok();
        let path = root.join("quickstart.db");
        println!("backend: sqlite {}", path.display());
        agentscope_durable_sqlite::open_backend(path)?
    } else {
        println!("backend: file {}", root.display());
        LocalBackend::open(&root)?
    };

    let session_key = SessionKey::new("quickstart", "demo")?;
    let thread = ensure_session(&backend, &session_key).await?;
    let runtime =
        DurableRuntime::new(backend, "quickstart-host").with_adapter("echo", Arc::new(EchoAdapter));
    let session = DurableSession::new(runtime, thread);

    // The second run finds the first turn and continues with a new one: this is what a host
    // does after its process died and came back.
    if let Some(receipt) = session.admission("run-1").await? {
        println!("run-1 was already admitted as turn {}", receipt.turn_id);
    } else {
        let receipt = session
            .submit("run-1", json!({ "text": "hello durable" }))
            .await?;
        println!("admitted run-1 as turn {}", receipt.turn_id);
    }
    let second = session
        .submit("run-2", json!({ "text": "second run" }))
        .await?;
    println!("admitted run-2 as turn {}", second.turn_id);

    loop {
        match session.tick().await? {
            TickOutcome::Settled => {}
            TickOutcome::Idle => break,
            _ => break,
        }
    }

    for turn in session.turns(None, 100).await?.items {
        println!("turn {} -> {:?} {}", turn.turn_id, turn.status, turn.output);
    }
    let events = session.events(None, 100).await?;
    println!("{} committed events", events.events.len());
    for event in events.events.iter().take(10) {
        println!("  {} {}", event.event_type, event.event_id);
    }
    Ok(())
}
