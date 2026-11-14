//! Call-scoped iteration positions supplied by an execution host through [`RuntimeContext`],
//! mirroring `io.agentscope.core.agent.AgentExecutionPosition`.
//!
//! A host resolves these from committed reasoning/tool facts so a process restart is not treated
//! as a fresh iteration budget. Without a supplied position the native journal keeps an unfinished
//! invocation's committed position, and a fresh call starts at zero. Nothing here replays a
//! cancelled async stack or a middleware effect.

use serde::{Deserialize, Serialize};

use crate::agent::RuntimeContext;

/// Context key of [`AgentExecutionPosition`].
pub const EXECUTION_POSITION_CONTEXT_KEY: &str = "execution_position";

/// Optional call-scoped iteration positions.
///
/// * `reasoning_iteration` — iteration for the next reasoning entry (including the max-iteration
///   check).
/// * `acting_iteration` — iteration whose pending tool calls are being continued.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentExecutionPosition {
    pub reasoning_iteration: u32,
    pub acting_iteration: u32,
}

impl AgentExecutionPosition {
    /// Creates a position; both iterations are zero for a fresh invocation.
    pub fn new(reasoning_iteration: u32, acting_iteration: u32) -> Self {
        Self {
            reasoning_iteration,
            acting_iteration,
        }
    }
}

/// Reads the host-supplied position, when the context carries one.
pub fn execution_position(context: &RuntimeContext) -> Option<AgentExecutionPosition> {
    context
        .extension::<AgentExecutionPosition>(EXECUTION_POSITION_CONTEXT_KEY)
        .map(|position| *position)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_round_trips_camel_case() {
        let position = AgentExecutionPosition::new(3, 1);
        let json = serde_json::to_value(position).unwrap();
        assert_eq!(json["reasoningIteration"], 3);
        assert_eq!(json["actingIteration"], 1);
        assert_eq!(
            serde_json::from_value::<AgentExecutionPosition>(json).unwrap(),
            position
        );
    }

    #[test]
    fn absent_position_is_none_and_default_starts_at_zero() {
        let context = RuntimeContext::default();
        assert!(execution_position(&context).is_none());

        let mut context = RuntimeContext::default();
        context.set_extension(
            EXECUTION_POSITION_CONTEXT_KEY,
            AgentExecutionPosition::new(2, 2),
        );
        assert_eq!(
            execution_position(&context),
            Some(AgentExecutionPosition::new(2, 2))
        );
        assert_eq!(
            AgentExecutionPosition::default(),
            AgentExecutionPosition::new(0, 0)
        );
    }
}
