//! A canned [`McpTransport`] for tests.
//!
//! It lives here (not behind `#[cfg(test)]`) so downstream crates can wire an
//! [`McpExecutor`](crate::McpExecutor) without standing up a socket. It records every request
//! it was given, in order, so tests can assert what actually reached the wire.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;

use crate::error::TransportError;
use crate::jsonrpc::{Request, Response};
use crate::transport::McpTransport;

/// A transport that answers from a queue and records what it was asked.
#[derive(Debug, Default)]
pub struct FakeTransport {
    replies: Mutex<VecDeque<Result<Response, TransportError>>>,
    sent: Mutex<Vec<Request>>,
}

impl FakeTransport {
    /// A transport with a queue of replies, consumed in order.
    pub fn new(replies: Vec<Result<Response, TransportError>>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            sent: Mutex::new(Vec::new()),
        }
    }

    /// A transport with no replies queued; every call fails with a transport failure.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Queues one more reply.
    pub fn push(&self, reply: Result<Response, TransportError>) {
        lock(&self.replies).push_back(reply);
    }

    /// The requests received so far, oldest first.
    pub fn sent(&self) -> Vec<Request> {
        lock(&self.sent).clone()
    }

    /// The last request received, if any.
    pub fn last_sent(&self) -> Option<Request> {
        lock(&self.sent).last().cloned()
    }
}

#[async_trait]
impl McpTransport for FakeTransport {
    async fn call(&self, request: Request) -> Result<Response, TransportError> {
        lock(&self.sent).push(request);
        match lock(&self.replies).pop_front() {
            Some(reply) => reply,
            None => Err(TransportError::Transport(
                "fake transport has no reply queued".to_owned(),
            )),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
