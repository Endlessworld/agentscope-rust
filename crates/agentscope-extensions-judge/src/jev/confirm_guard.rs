//! `JevConfirmGuard`: binds a HITL approval to the exact call that the user saw (`JUDGE-04`).
//!
//! The 2026-10-31 裁定 is **not** to change `agentscope-core`: the core HITL path knows nothing
//! about the JEV approval ledger. Instead this crate ships the guard the *assembly layer* wraps
//! around its own [`ConfirmHandler`], which is where the prompt and the resume both happen:
//!
//!```text
//! prompt:   guard.wrap(host_handler)  --snapshot(call)-->  user decides  --> remember(approved)
//! resume:   guard.verify(call)?       --compare SHA-256(name, arguments)-->  execute
//!```
//!
//! [`JevConfirmGuard::wrap`] snapshots the call **before** the handler runs and remembers it only
//! when the user confirmed — including the case where the host let the user *edit* the arguments
//! ([`ConfirmResult::tool_call`]): the edited call is what gets bound. [`JevConfirmGuard::verify`]
//! then rejects a resumed call whose id was approved but whose name or arguments changed, which is
//! exactly the 「用户确认的是 A 参数、恢复时执行 B 参数」 failure `JUDGE-04` describes.
//!
//! The guard is optional for the host: a host that never calls [`JevConfirmGuard::verify`] keeps
//! the unguarded semantics, which is the documented residual risk of not touching the core.

use std::sync::Arc;

use agentscope_core::agent::{ConfirmHandler, RuntimeContext};
use agentscope_core::event::ConfirmResult;
use agentscope_core::message::ToolUseBlock;

use crate::jev::confirmed_calls::JevConfirmedCalls;

/// Why a resumed call was rejected by [`JevConfirmGuard`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmGuardError {
    /// The call id was approved, but its name or arguments changed while the user was deciding.
    ArgumentsChanged {
        /// The tool call id whose approved arguments no longer match.
        call_id: String,
    },
}

impl ConfirmGuardError {
    /// The offending tool call id.
    pub fn call_id(&self) -> &str {
        match self {
            ConfirmGuardError::ArgumentsChanged { call_id } => call_id,
        }
    }
}

impl std::fmt::Display for ConfirmGuardError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfirmGuardError::ArgumentsChanged { call_id } => write!(
                formatter,
                "tool call {call_id} was approved with different arguments; re-ask the user"
            ),
        }
    }
}

impl std::error::Error for ConfirmGuardError {}

/// Wraps a host [`ConfirmHandler`] so an approval is bound to the exact arguments.
#[derive(Debug, Clone)]
pub struct JevConfirmGuard {
    ledger: Arc<JevConfirmedCalls>,
}

impl JevConfirmGuard {
    /// Binds the guard to an explicit ledger.
    pub fn new(ledger: Arc<JevConfirmedCalls>) -> Self {
        Self { ledger }
    }

    /// Installs a ledger on `ctx` (reusing an existing one) and binds the guard to it.
    ///
    /// Use this at the prompt site so the resume site can rebuild the same guard with
    /// [`JevConfirmGuard::for_context`].
    pub fn install(ctx: &mut RuntimeContext) -> Self {
        Self::new(JevConfirmedCalls::install(ctx))
    }

    /// Binds the guard to the ledger already installed on `ctx`, installing one when absent.
    ///
    /// Same contract as [`Self::install`]; the alias exists so the resume site can read as
    /// "bind to whatever the prompt site installed" while staying identical in behaviour.
    pub fn for_context(ctx: &mut RuntimeContext) -> Self {
        Self::install(ctx)
    }

    /// The ledger this guard reads and writes.
    pub fn ledger(&self) -> &Arc<JevConfirmedCalls> {
        &self.ledger
    }

    /// Wraps `inner` (the host's prompt/handler) with the approval binding.
    ///
    /// The snapshot is taken **before** the prompt, so a model or hook that rewrites the call
    /// while the user is deciding cannot smuggle its new arguments past the approval. A denied
    /// call is never remembered.
    pub fn wrap(&self, inner: ConfirmHandler) -> ConfirmHandler {
        let ledger = Arc::clone(&self.ledger);
        let handler: ConfirmHandler = Arc::new(move |call: ToolUseBlock| {
            let requested = JevConfirmedCalls::snapshot(&call);
            let ledger = Arc::clone(&ledger);
            let inner = Arc::clone(&inner);
            Box::pin(async move {
                let result: ConfirmResult = inner(call).await;
                if result.is_confirmed() {
                    // A host may return the user-edited call; bind what the user actually saw last.
                    let approved = match result.tool_call.as_ref() {
                        Some(edited) => JevConfirmedCalls::snapshot(edited),
                        None => requested,
                    };
                    ledger.remember(approved);
                }
                result
            })
        });
        handler
    }

    /// Whether the call may run: approved ids must still carry their approved arguments.
    pub fn verify(&self, call: &ToolUseBlock) -> Result<(), ConfirmGuardError> {
        if self.ledger.contains(call) && !self.ledger.matches(call) {
            return Err(ConfirmGuardError::ArgumentsChanged {
                call_id: call.id.clone(),
            });
        }
        Ok(())
    }

    /// [`Self::verify`] for a whole acting batch; the first mismatch wins.
    pub fn verify_all(&self, calls: &[ToolUseBlock]) -> Result<(), ConfirmGuardError> {
        for call in calls {
            self.verify(call)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    fn call(id: &str, name: &str, amount: i64) -> ToolUseBlock {
        let serde_json::Value::Object(input) = json!({ "amount": amount }) else {
            panic!("arguments must be an object");
        };
        ToolUseBlock::new(id, name, input)
    }

    fn handler(result: ConfirmResult) -> ConfirmHandler {
        Arc::new(move |_call: ToolUseBlock| {
            let result = result.clone();
            Box::pin(async move { result })
        })
    }

    fn guard() -> (JevConfirmGuard, RuntimeContext) {
        let mut ctx = RuntimeContext::new();
        let guard = JevConfirmGuard::install(&mut ctx);
        (guard, ctx)
    }

    #[tokio::test]
    async fn an_approved_call_verifies_after_the_prompt() {
        let (guard, _ctx) = guard();
        let wrapped = guard.wrap(handler(ConfirmResult::allowed()));
        let approved = call("call-1", "refund", 1);
        assert!(wrapped(approved.clone()).await.is_confirmed());

        assert_eq!(guard.ledger().len(), 1);
        assert!(guard.verify(&approved).is_ok());
        assert!(guard.verify_all(&[approved]).is_ok());
    }

    #[tokio::test]
    async fn changed_arguments_are_rejected_at_resume() {
        let (guard, _ctx) = guard();
        let wrapped = guard.wrap(handler(ConfirmResult::allowed()));
        assert!(wrapped(call("call-1", "refund", 1)).await.is_confirmed());

        // The resumed call carries the same id but a different amount.
        let error = guard.verify(&call("call-1", "refund", 100)).unwrap_err();
        assert_eq!(error.call_id(), "call-1");
        assert!(error
            .to_string()
            .contains("was approved with different arguments"));
        // The same id with a different tool name is rejected too.
        assert!(guard.verify(&call("call-1", "transfer", 1)).is_err());
    }

    #[tokio::test]
    async fn a_denied_call_is_never_remembered() {
        let (guard, _ctx) = guard();
        let wrapped = guard.wrap(handler(ConfirmResult::denied("no")));
        let denied = call("call-1", "refund", 1);
        let result = wrapped(denied.clone()).await;
        assert!(!result.is_confirmed());
        assert_eq!(result.message, "no");
        assert!(guard.ledger().is_empty());
        assert!(guard.verify(&denied).is_ok(), "nothing was approved");
    }

    #[tokio::test]
    async fn a_user_edited_call_binds_the_edited_arguments() {
        let (guard, _ctx) = guard();
        let edited = call("call-1", "refund", 5);
        let wrapped = guard.wrap(handler(ConfirmResult::allowed_with(edited.clone())));
        assert!(wrapped(call("call-1", "refund", 1)).await.is_confirmed());

        assert!(guard.verify(&edited).is_ok(), "the edited call is approved");
        assert!(
            guard.verify(&call("call-1", "refund", 1)).is_err(),
            "the original arguments are no longer approved"
        );
    }

    #[tokio::test]
    async fn the_wrapper_records_every_prompt_once() {
        let (guard, _ctx) = guard();
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&prompts);
        let inner: ConfirmHandler = Arc::new(move |tool_call: ToolUseBlock| {
            seen.lock().unwrap().push(tool_call.id.clone());
            Box::pin(async move { ConfirmResult::allowed() })
        });
        let wrapped = guard.wrap(inner);
        wrapped(call("call-1", "refund", 1)).await;
        wrapped(call("call-2", "refund", 2)).await;

        assert_eq!(*prompts.lock().unwrap(), vec!["call-1", "call-2"]);
        assert_eq!(guard.ledger().len(), 2);
    }

    #[tokio::test]
    async fn installing_twice_reuses_the_same_ledger() {
        let mut ctx = RuntimeContext::new();
        let first = JevConfirmGuard::install(&mut ctx);
        let second = JevConfirmGuard::for_context(&mut ctx);
        assert!(Arc::ptr_eq(first.ledger(), second.ledger()));

        // An approval remembered through one guard is visible to the other.
        let wrapped = first.wrap(handler(ConfirmResult::allowed()));
        let approved = call("call-1", "refund", 1);
        wrapped(approved.clone()).await;
        assert!(second.verify(&approved).is_ok());

        // A call without an id cannot be bound, so verification never blocks it.
        let unnamed = ToolUseBlock::new("", "refund", Default::default());
        assert!(second.verify(&unnamed).is_ok());
        assert!(!second.ledger().contains(&unnamed));
    }
}
