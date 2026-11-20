//! Committed event streaming over Server-Sent Events.
//!
//! Only committed public events carry an `id`; the client reconnects with `Last-Event-ID` and the
//! server replays from there. A cursor the backend already pruned answers `resync` instead of
//! pretending the gap never existed, so the observer fetches a fresh snapshot before reconnecting.
//! Heartbeats keep idle connections alive without acknowledging anything.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use agentscope_durable::{
    DurableError, ErrorCode, EventStreamOptions, PublicEvent, SessionBackend, SessionKey,
};
use axum::body::Bytes;
use futures::stream::{self, Stream};

#[derive(Clone)]
struct Cursor {
    after: Option<String>,
    first: bool,
    done: bool,
}

/// Streams one session's committed events after `after`, polling every `interval`.
///
/// Dropping the stream stops the polling; nothing here cancels a turn or acknowledges an event on
/// behalf of the client.
pub fn stream(
    backend: Arc<dyn SessionBackend>,
    session: SessionKey,
    after: Option<String>,
    limit: usize,
    options: EventStreamOptions,
    interval: Duration,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let interval = interval.max(Duration::from_millis(1));
    let state = Cursor {
        after,
        first: true,
        done: false,
    };
    stream::unfold(state, move |mut state| {
        let backend = backend.clone();
        let session = session.clone();
        async move {
            if state.done {
                return None;
            }
            if !state.first {
                tokio::time::sleep(interval).await;
            }
            state.first = false;
            match backend
                .events_with(&session, state.after.as_deref(), limit, options)
                .await
            {
                Ok(page) if page.events.is_empty() => {
                    Some((Ok(Bytes::from_static(b": heartbeat\n\n")), state))
                }
                Ok(page) => {
                    let mut frames = String::new();
                    for event in &page.events {
                        frames.push_str(&durable_frame(event));
                    }
                    let last = page.events.last().map(|event| event.cursor.clone());
                    state.after = page.cursor.or(last).or_else(|| state.after.clone());
                    Some((Ok(Bytes::from(frames)), state))
                }
                Err(error) if error.code() == ErrorCode::CursorExpired => {
                    state.done = true;
                    Some((Ok(Bytes::from_static(RESYNC.as_bytes())), state))
                }
                Err(error) => {
                    state.done = true;
                    Some((Ok(Bytes::from(error_frame(&error))), state))
                }
            }
        }
    })
}

const RESYNC: &str = "event: resync\ndata: {\"code\":\"CURSOR_EXPIRED\"}\n\n";

fn durable_frame(event: &PublicEvent) -> String {
    let data = serde_json::to_string(event).unwrap_or_else(|_| "{}".to_string());
    format!("id: {}\nevent: durable\ndata: {data}\n\n", event.cursor)
}

fn error_frame(error: &DurableError) -> String {
    let body = serde_json::json!({ "code": error.code().as_str(), "message": error.message() });
    format!("event: error\ndata: {body}\n\n")
}
