//! Process-wide `session/update` fan-out.
//!
//! The WebSocket/HTTP server builds **one agent per connection** (deliberately isomorphic to the
//! stdio `one process, one conversation` model), so a client reload detaches from the agent
//! that is running the turn and resumes the session through a fresh instance. A turn's
//! notifications used to be written to the connection that sent the prompt; after a reload that
//! socket is gone, so the agent kept generating into the void and the new connection saw
//! nothing until the turn had finished and been persisted.
//!
//! This registry gives every session a small broadcast channel inside the process: a turn
//! publishes into it, and whichever connection is attached to that session (`session/new` or
//! `session/resume`) forwards the frames. Reloading mid-turn now continues the stream instead
//! of truncating it.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock};

use agent_client_protocol::v2::schema as v2;
use agent_client_protocol::{Client, v2::ConnectionTo};
use tokio::sync::broadcast;

/// Backlog per session. Same spirit as the desktop bridge: a client that falls behind loses
/// old frames instead of stalling the turn.
const CAPACITY: usize = 4096;

/// One frame: the session it belongs to, its `_meta`, and the update itself.
pub(crate) type Frame = (v2::SessionId, Option<v2::Meta>, v2::SessionUpdate);

fn registry() -> &'static Mutex<HashMap<String, broadcast::Sender<Frame>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, broadcast::Sender<Frame>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock() -> MutexGuard<'static, HashMap<String, broadcast::Sender<Frame>>> {
    registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Publishes one update to every connection attached to the session.
///
/// No attached connection means the frame is dropped: output from a turn nobody is watching
/// must not pile up in memory while the client reloads.
pub(crate) fn publish(frame: Frame) {
    let key = frame.0.0.clone();
    publish_routed(&key, frame);
}

/// Publishes one update under an explicit routing key.
///
/// A frame's id and its routing key differ for subagent cards: they are routed by the parent
/// session (the one a client attaches to) while still naming the child session they belong to.
pub(crate) fn publish_routed(key: &str, frame: Frame) {
    let sender = {
        let registry = lock();
        registry.get(key).cloned()
    };
    if let Some(sender) = sender {
        if sender.send(frame).is_err() {
            tracing::trace!("session/update dropped: no connection is attached to the session");
        }
    }
}

/// Attaches a connection to a session's frame stream.
///
/// The forwarding task ends when the connection terminates, so a reload drops its predecessor
/// instead of leaving a subscriber behind.
pub(crate) fn attach(session_id: v2::SessionId, cx: ConnectionTo<Client>) {
    let mut rx = {
        let mut registry = lock();
        registry
            .entry(session_id.0.to_string())
            .or_insert_with(|| broadcast::channel(CAPACITY).0)
            .subscribe()
    };
    tracing::debug!(session = %session_id.0, "v2 session bus: connection attached");
    let task_cx = cx.clone();
    let _ = cx.spawn(async move {
        loop {
            let frame = tokio::select! {
                frame = rx.recv() => frame,
                _ = task_cx.terminated() => break,
            };
            match frame {
                Ok((sid, meta, update)) => {
                    let mut out = v2::UpdateSessionNotification::new(sid, update);
                    if let Some(meta) = meta {
                        out = out.meta(meta);
                    }
                    if task_cx.send_notification(out).is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(dropped)) => {
                    tracing::warn!(dropped, "session/update frames outran the client; dropped");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
        Ok(())
    });
}

/// Drops a session's channel once the session itself is gone.
pub(crate) fn forget(session_id: &str) {
    lock().remove(session_id);
}
