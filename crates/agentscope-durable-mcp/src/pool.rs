//! Reusing MCP transports across executors.
//!
//! The pool never opens a connection itself: [`register`](McpConnectionPool::register) takes one
//! the caller already established, and [`get`](McpConnectionPool::get) hands out the same `Arc`
//! while it is in the pool. That keeps this crate free of a transport implementation while
//! letting two executors on one server share a session.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::error::TransportError;
use crate::transport::McpTransport;

struct Entry {
    transport: Arc<dyn McpTransport>,
    last_used: Instant,
}

/// A bounded registry of live MCP transports.
pub struct McpConnectionPool {
    max_connections: usize,
    idle_timeout: Duration,
    entries: Mutex<HashMap<String, Entry>>,
}

impl McpConnectionPool {
    pub fn new(max_connections: usize, idle_timeout: Duration) -> Self {
        Self {
            max_connections,
            idle_timeout,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Eight connections, sixty seconds idle: enough for the usual set of MCP servers.
    pub fn with_defaults() -> Self {
        Self::new(8, Duration::from_secs(60))
    }

    pub fn max_connections(&self) -> usize {
        self.max_connections
    }

    pub fn idle_timeout(&self) -> Duration {
        self.idle_timeout
    }

    /// Adds a connection under `server_key`, replacing the one already there.
    ///
    /// A new key when the pool is full is refused: the caller has to sweep one out first.
    pub fn register(
        &self,
        server_key: impl Into<String>,
        transport: Arc<dyn McpTransport>,
    ) -> Result<(), TransportError> {
        let mut entries = self.entries.lock().map_err(|_| poisoned())?;
        let key = server_key.into();
        if !entries.contains_key(&key) && entries.len() >= self.max_connections {
            return Err(TransportError::Transport(format!(
                "MCP connection pool is full ({})",
                self.max_connections
            )));
        }
        entries.insert(
            key,
            Entry {
                transport,
                last_used: Instant::now(),
            },
        );
        Ok(())
    }

    /// The connection registered under `server_key`, if it is still live.
    pub fn get(&self, server_key: &str) -> Result<Arc<dyn McpTransport>, TransportError> {
        let mut entries = self.entries.lock().map_err(|_| poisoned())?;
        match entries.get_mut(server_key) {
            Some(entry) => {
                entry.last_used = Instant::now();
                Ok(entry.transport.clone())
            }
            None => Err(TransportError::Transport(format!(
                "No MCP connection registered for `{server_key}`"
            ))),
        }
    }

    /// Drops the entries idle for at least `idle_timeout`; answers how many were dropped.
    pub fn sweep(&self) -> usize {
        let Ok(mut entries) = self.entries.lock() else {
            return 0;
        };
        let before = entries.len();
        let now = Instant::now();
        entries.retain(|_, entry| now.duration_since(entry.last_used) < self.idle_timeout);
        before - entries.len()
    }

    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .map(|entries| entries.len())
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn poisoned() -> TransportError {
    TransportError::Transport("MCP connection pool lock is poisoned".to_string())
}
