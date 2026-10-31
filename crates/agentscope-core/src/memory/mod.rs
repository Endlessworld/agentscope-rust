//! Memory components that store and manage conversation history, mirroring
//! `io.agentscope.core.memory`.
use async_trait::async_trait;

use crate::error::Result;
use crate::message::Msg;
use crate::state::{AgentState, AgentStateStore};

/// Key under which memory messages are persisted in an [`AgentStateStore`].
pub const MEMORY_MESSAGES_KEY: &str = "memory_messages";

/// Interface for memory components that store conversation history.
///
/// **Deprecated**: the conversation is owned by [`AgentState::context`], which the agent loop
/// reads and writes directly. Java marks `Memory` `@Deprecated since 2.0.0` for the same
/// reason. This trait and its two implementations stay as a compatibility layer for one
/// minor cycle so hosts that still hold a `Memory` keep compiling.
#[deprecated(
    since = "0.1.0",
    note = "the conversation is owned by AgentState::context (Java `Memory` is @Deprecated since 2.0.0); \
            kept as a compatibility layer for one minor cycle"
)]
#[async_trait]
pub trait Memory: Send + Sync {
    /// Adds a message to the memory.
    fn add_message(&self, message: Msg);

    /// Retrieves all messages stored in memory.
    fn get_messages(&self) -> Vec<Msg>;

    /// Deletes a message at the specified index (no-op when out of bounds).
    fn delete_message(&self, index: usize);

    /// Clears all messages from memory.
    fn clear(&self);

    /// Persist the message buffer under the `memory_messages` session key.
    async fn save_to(
        &self,
        store: &dyn AgentStateStore,
        user_id: Option<&str>,
        session_id: &str,
    ) -> Result<()>;

    /// Load a previously saved message buffer (no-op when none exists).
    async fn load_from(
        &self,
        store: &dyn AgentStateStore,
        user_id: Option<&str>,
        session_id: &str,
    ) -> Result<()>;
}

/// In-memory implementation of [`Memory`] with state persistence support.
#[derive(Debug, Clone, Default)]
pub struct InMemoryMemory {
    messages: std::sync::Arc<std::sync::Mutex<Vec<Msg>>>,
}

impl InMemoryMemory {
    pub fn new() -> Self {
        Self::default()
    }
}

#[allow(deprecated)]
#[async_trait]
impl Memory for InMemoryMemory {
    fn add_message(&self, message: Msg) {
        self.messages.lock().unwrap().push(message);
    }

    fn get_messages(&self) -> Vec<Msg> {
        self.messages.lock().unwrap().clone()
    }

    fn delete_message(&self, index: usize) {
        let mut g = self.messages.lock().unwrap();
        if index < g.len() {
            g.remove(index);
        }
    }

    fn clear(&self) {
        self.messages.lock().unwrap().clear();
    }

    async fn save_to(
        &self,
        store: &dyn AgentStateStore,
        user_id: Option<&str>,
        session_id: &str,
    ) -> Result<()> {
        let msgs = self.get_messages();
        let values: Vec<serde_json::Value> = msgs
            .iter()
            .map(serde_json::to_value)
            .collect::<serde_json::Result<_>>()?;
        store
            .save_list(user_id, session_id, MEMORY_MESSAGES_KEY, &values)
            .await;
        Ok(())
    }

    async fn load_from(
        &self,
        store: &dyn AgentStateStore,
        user_id: Option<&str>,
        session_id: &str,
    ) -> Result<()> {
        let values = store
            .get_list(user_id, session_id, MEMORY_MESSAGES_KEY)
            .await;
        let msgs: Vec<Msg> = values
            .iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect();
        *self.messages.lock().unwrap() = msgs;
        Ok(())
    }
}

/// A [`Memory`] implementation that delegates reads and writes to
/// [`AgentState::context_mut`].
#[derive(Debug, Clone)]
pub struct StateBackedMemory {
    state: std::sync::Arc<std::sync::RwLock<AgentState>>,
}

impl StateBackedMemory {
    pub fn new(state: AgentState) -> Self {
        Self {
            state: std::sync::Arc::new(std::sync::RwLock::new(state)),
        }
    }

    pub fn state(&self) -> AgentState {
        self.state.read().unwrap().clone()
    }
}

#[allow(deprecated)]
#[async_trait]
impl Memory for StateBackedMemory {
    fn add_message(&self, message: Msg) {
        self.state.write().unwrap().context_mut().push(message);
    }

    fn get_messages(&self) -> Vec<Msg> {
        self.state.read().unwrap().context().to_vec()
    }

    fn delete_message(&self, index: usize) {
        let mut guard = self.state.write().unwrap();
        let ctx = guard.context_mut();
        if index < ctx.len() {
            ctx.remove(index);
        }
    }

    fn clear(&self) {
        self.state.write().unwrap().context_mut().clear();
    }

    async fn save_to(
        &self,
        store: &dyn AgentStateStore,
        user_id: Option<&str>,
        session_id: &str,
    ) -> Result<()> {
        let values: Vec<serde_json::Value> = {
            let ctx = self.state.read().unwrap();
            ctx.context()
                .iter()
                .map(serde_json::to_value)
                .collect::<serde_json::Result<Vec<_>>>()?
        };
        store
            .save_list(user_id, session_id, MEMORY_MESSAGES_KEY, &values)
            .await;
        Ok(())
    }

    async fn load_from(
        &self,
        store: &dyn AgentStateStore,
        user_id: Option<&str>,
        session_id: &str,
    ) -> Result<()> {
        let values = store
            .get_list(user_id, session_id, MEMORY_MESSAGES_KEY)
            .await;
        let msgs: Vec<Msg> = values
            .iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect();
        *self.state.write().unwrap().context_mut() = msgs;
        Ok(())
    }
}
