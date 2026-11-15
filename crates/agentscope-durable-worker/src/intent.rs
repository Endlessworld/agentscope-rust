//! What the brain wants next. An intent names work; it never performs it.

use agentscope_durable::types::ExecutionRequest;
use serde_json::Value;

/// One decision of a [`Brain`](crate::Brain).
///
/// A step turns at most one intent into at most one effect, which is what keeps a step inside a
/// single platform call (R2). A model response that names several tool calls is still one model
/// call: the brain persists the whole response as a fact and hands back one `Act` at a time.
#[derive(Debug, Clone, PartialEq)]
pub enum Intent {
    /// Have the [hands](crate::Hands) apply one external effect.
    ///
    /// The durable identity is [`ExecutionRequest::execution_id`], derived from the turn and the
    /// call id, so two writers that decide the same thing name the same effect.
    Act(ExecutionRequest),
    /// Nothing can advance until the signal key `key` is satisfied.
    Wait {
        /// The wait key a host will satisfy with a durable signal.
        key: String,
    },
    /// The turn is finished as far as the brain is concerned.
    Done {
        /// What the brain wants recorded as the turn's output.
        output: Value,
    },
}

impl Intent {
    pub fn wait(key: impl Into<String>) -> Self {
        Self::Wait { key: key.into() }
    }

    pub fn done(output: Value) -> Self {
        Self::Done { output }
    }

    pub fn kind(&self) -> IntentKind {
        match self {
            Self::Act(_) => IntentKind::Act,
            Self::Wait { .. } => IntentKind::Wait,
            Self::Done { .. } => IntentKind::Done,
        }
    }
}

/// The shape of an [`Intent`], for reports and assertions that do not need its payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentKind {
    Act,
    Wait,
    Done,
}
