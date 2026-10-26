//! Offline transport double shared by the embedding provider tests.
//!
//! Every provider test injects this instead of a real `reqwest` client, so the suite never
//! opens a socket. Responses are replayed LIFO — the last pushed is served first — so a
//! "fail then succeed" retry script pushes the success **first**.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::error::{RagError, Result};
use crate::transport::{HttpRequest, HttpResponse, HttpTransport};

/// Records outgoing requests and replays scripted responses.
#[derive(Default)]
pub(crate) struct ScriptedTransport {
    responses: Mutex<Vec<HttpResponse>>,
    requests: Mutex<Vec<HttpRequest>>,
}

impl ScriptedTransport {
    /// Builds a transport replaying `responses` (last element first).
    pub(crate) fn with_responses(responses: Vec<HttpResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses),
            requests: Mutex::new(Vec::new()),
        })
    }

    /// The requests seen so far, oldest first.
    pub(crate) fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl HttpTransport for ScriptedTransport {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
        self.requests.lock().unwrap().push(request);
        self.responses
            .lock()
            .unwrap()
            .pop()
            .ok_or_else(|| RagError::Http("no scripted response left".to_string()))
    }
}
