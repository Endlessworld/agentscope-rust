//! Offline test doubles shared by the platform-adapter test modules.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;

use super::error::Result;
use super::http::{HttpTransport, PreparedRequest, PreparedResponse};
use super::token::AccessTokenProvider;

/// An [`HttpTransport`] that records every request and replays canned responses.
///
/// When the response queue is exhausted it answers `200 {}` so a test that only inspects request
/// construction does not need to seed any response.
#[derive(Debug, Default)]
pub struct RecordingTransport {
    requests: Mutex<Vec<PreparedRequest>>,
    responses: Mutex<VecDeque<PreparedResponse>>,
}

impl RecordingTransport {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the canned responses, consumed in order.
    pub fn with_responses(responses: Vec<PreparedResponse>) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(responses.into_iter().collect()),
        }
    }

    /// Every request seen so far, in call order.
    pub fn requests(&self) -> Vec<PreparedRequest> {
        self.requests
            .lock()
            .expect("recording transport poisoned")
            .clone()
    }

    /// The most recent request, if any.
    pub fn last_request(&self) -> Option<PreparedRequest> {
        self.requests
            .lock()
            .expect("recording transport poisoned")
            .last()
            .cloned()
    }

    /// Number of requests observed.
    pub fn request_count(&self) -> usize {
        self.requests
            .lock()
            .expect("recording transport poisoned")
            .len()
    }
}

#[async_trait]
impl HttpTransport for RecordingTransport {
    async fn execute(&self, request: PreparedRequest) -> Result<PreparedResponse> {
        self.requests
            .lock()
            .expect("recording transport poisoned")
            .push(request);
        let canned = self
            .responses
            .lock()
            .expect("recording transport poisoned")
            .pop_front();
        Ok(canned.unwrap_or_else(|| PreparedResponse::new(200, "{}")))
    }
}

/// An [`AccessTokenProvider`] returning a fixed token; records `invalidate` calls.
#[derive(Debug)]
pub struct StaticTokenProvider {
    token: String,
    invalidated: AtomicBool,
}

impl StaticTokenProvider {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            invalidated: AtomicBool::new(false),
        }
    }

    pub fn was_invalidated(&self) -> bool {
        self.invalidated.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AccessTokenProvider for StaticTokenProvider {
    async fn token(&self) -> Result<String> {
        Ok(self.token.clone())
    }

    fn invalidate(&self) {
        self.invalidated.store(true, Ordering::SeqCst);
    }
}
