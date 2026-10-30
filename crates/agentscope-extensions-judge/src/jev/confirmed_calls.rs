//! `JevConfirmedCalls`: binds a tool approval to the exact call that was approved.
//!
//! A permission prompt asks the user about one tool call. Between the prompt and the resumed
//! execution the arguments must not change, and an approval must not silently transfer to another
//! call: the ledger therefore stores `call_id -> SHA-256(name, arguments)` captured **at prompt
//! time**, and a resumed call only matches when both the id and the re-computed fingerprint agree.
//!
//! Java keeps the ledger in the `RuntimeContext` attribute map (`ctx.get(JevConfirmedCalls.class)` /
//! `ctx.put(...)`). Rust's [`RuntimeContext`] already exposes the same keyed slot through
//! [`RuntimeContext::set_extension`] / [`RuntimeContext::extension`] (see
//! `agentscope-core/src/agent/runtime_context.rs`), so this module needs **no** core change: it only
//! defines the key and uses the slot. Because middleware sees `&RuntimeContext`, the ledger itself is
//! interior-mutable (`Mutex<HashMap<..>>`), which also keeps concurrent runs isolated per context.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::ToolUseBlock;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// The [`RuntimeContext`] extension key this ledger lives under.
pub const CONFIRMED_CALLS_KEY: &str = "agentscope.jev.confirmed_calls";

/// One approval captured before the user was asked (`JevConfirmedCalls.Approval`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    /// The tool call id.
    pub call_id: String,
    /// SHA-256 over `{name, arguments}` with object keys sorted.
    pub fingerprint: String,
}

/// The per-context approval ledger (`JevConfirmedCalls`).
#[derive(Debug, Default)]
pub struct JevConfirmedCalls {
    approved: Mutex<HashMap<String, String>>,
}

impl JevConfirmedCalls {
    /// Creates an empty ledger.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the ledger installed on `ctx`, if any (`ctx.get(JevConfirmedCalls.class)`).
    pub fn ledger(ctx: &RuntimeContext) -> Option<Arc<Self>> {
        ctx.extension::<Self>(CONFIRMED_CALLS_KEY)
    }

    /// Installs a ledger on `ctx` and returns it, keeping an existing one
    /// (`ctx.put(new JevConfirmedCalls())` only when absent).
    pub fn install(ctx: &mut RuntimeContext) -> Arc<Self> {
        if let Some(existing) = Self::ledger(ctx) {
            return existing;
        }
        let ledger = Arc::new(Self::new());
        ctx.set_extension_arc(CONFIRMED_CALLS_KEY, Arc::clone(&ledger));
        ledger
    }

    /// Captures the approval **before** asking the user (`JevConfirmedCalls.snapshot`).
    ///
    /// Returns `None` for a call without a usable id, or when the arguments cannot be fingerprinted
    /// (Java catches `IllegalArgumentException` and returns `null`).
    pub fn snapshot(call: &ToolUseBlock) -> Option<Approval> {
        if call.id.trim().is_empty() {
            return None;
        }
        Self::fingerprint(call).map(|fingerprint| Approval {
            call_id: call.id.clone(),
            fingerprint,
        })
    }

    /// Remembers an approval; a `None` approval is ignored (`remember(ctx, (Approval) null)`).
    pub fn remember(&self, approval: Option<Approval>) {
        let Some(approval) = approval else {
            return;
        };
        self.approved
            .lock()
            .expect("confirmed-calls lock")
            .insert(approval.call_id, approval.fingerprint);
    }

    /// Convenience: snapshot and remember one call (`remember(ctx, call)`).
    pub fn remember_call(&self, call: &ToolUseBlock) {
        self.remember(Self::snapshot(call));
    }

    /// Whether the id was approved at all, ignoring the arguments (`contains`).
    pub fn contains(&self, call: &ToolUseBlock) -> bool {
        if call.id.trim().is_empty() {
            return false;
        }
        self.approved
            .lock()
            .expect("confirmed-calls lock")
            .contains_key(&call.id)
    }

    /// Whether the call still matches the approved name and arguments (`matches`).
    pub fn matches(&self, call: &ToolUseBlock) -> bool {
        if call.id.trim().is_empty() {
            return false;
        }
        let expected = self
            .approved
            .lock()
            .expect("confirmed-calls lock")
            .get(&call.id)
            .cloned();
        let Some(expected) = expected else {
            return false;
        };
        Self::fingerprint(call)
            .map(|actual| actual == expected)
            .unwrap_or(false)
    }

    /// Number of remembered approvals.
    pub fn len(&self) -> usize {
        self.approved.lock().expect("confirmed-calls lock").len()
    }

    /// Whether nothing was approved.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drops every approval (a new turn re-prompts).
    pub fn clear(&self) {
        self.approved.lock().expect("confirmed-calls lock").clear();
    }

    /// The fingerprint of one call: SHA-256 over the name and the arguments with sorted keys.
    fn fingerprint(call: &ToolUseBlock) -> Option<String> {
        let payload = canonical_json(json!({
            "name": call.name,
            "arguments": Value::Object(call.input.clone()),
        }));
        let bytes = serde_json::to_vec(&payload).ok()?;
        let digest = Sha256::digest(&bytes);
        Some(digest.iter().map(|byte| format!("{byte:02x}")).collect())
    }
}

/// Recursively sorts object keys so the serialised form is canonical.
///
/// `serde_json::Map` is a `BTreeMap` only without the `preserve_order` feature; the workspace
/// enables it, so the map is insertion-ordered. Sorting here (rather than relying on the map
/// type) keeps the fingerprint stable regardless of how the caller built the arguments and
/// mirrors Java's `SerializationFeature.ORDER_MAP_ENTRIES_BY_KEYS`.
fn canonical_json(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map
                .into_iter()
                .map(|(key, value)| (key, canonical_json(value)))
                .collect();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            Value::Object(entries.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(canonical_json).collect()),
        other => other,
    }
}

/// `JevConfirmedCalls.remember(ctx, call)`.
pub fn remember(ctx: &RuntimeContext, call: &ToolUseBlock) {
    if let Some(ledger) = JevConfirmedCalls::ledger(ctx) {
        ledger.remember_call(call);
    }
}

/// `JevConfirmedCalls.contains(ctx, call)`.
pub fn contains(ctx: &RuntimeContext, call: &ToolUseBlock) -> bool {
    JevConfirmedCalls::ledger(ctx)
        .map(|ledger| ledger.contains(call))
        .unwrap_or(false)
}

/// `JevConfirmedCalls.matches(ctx, call)`.
pub fn matches(ctx: &RuntimeContext, call: &ToolUseBlock) -> bool {
    JevConfirmedCalls::ledger(ctx)
        .map(|ledger| ledger.matches(call))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, name: &str, arguments: Value) -> ToolUseBlock {
        let Value::Object(input) = arguments else {
            panic!("arguments must be an object");
        };
        ToolUseBlock::new(id, name, input)
    }

    fn refund(amount: i64) -> ToolUseBlock {
        call(
            "call-1",
            "refund",
            json!({ "nested": { "amount": amount } }),
        )
    }

    #[test]
    fn binds_parameters_at_prompt_time_not_at_confirmation_return() {
        let mut ctx = RuntimeContext::new();
        let ledger = JevConfirmedCalls::install(&mut ctx);
        let mut pending = refund(1);
        let snapshot = JevConfirmedCalls::snapshot(&pending);
        // The model (or a hook) mutates the arguments while the user is deciding.
        pending
            .input
            .insert("nested".to_string(), json!({ "amount": 100 }));
        ledger.remember(snapshot);
        assert!(!ledger.matches(&pending));
        assert!(ledger.contains(&pending));
    }

    #[test]
    fn binds_exact_arguments_and_context() {
        let mut ctx = RuntimeContext::new();
        JevConfirmedCalls::install(&mut ctx);
        let approved = refund(1);
        remember(&ctx, &approved);
        assert!(matches(&ctx, &approved));
        assert!(contains(&ctx, &approved));

        // Another context knows nothing about this approval.
        assert!(!matches(&RuntimeContext::new(), &approved));

        // Changed arguments and a changed tool name both stop matching.
        assert!(!matches(&ctx, &refund(100)));
        assert!(!matches(
            &ctx,
            &call("call-1", "send", json!({ "nested": { "amount": 1 } }))
        ));
    }

    #[test]
    fn a_call_without_an_id_is_never_remembered_or_matched() {
        let mut ctx = RuntimeContext::new();
        let ledger = JevConfirmedCalls::install(&mut ctx);
        let anonymous = call("", "refund", json!({}));
        assert!(JevConfirmedCalls::snapshot(&anonymous).is_none());
        ledger.remember_call(&anonymous);
        assert_eq!(ledger.len(), 0);
        assert!(!ledger.contains(&anonymous));
        assert!(!ledger.matches(&anonymous));
    }

    #[test]
    fn the_fingerprint_is_a_sorted_sha256_of_name_and_arguments() {
        let first =
            JevConfirmedCalls::snapshot(&call("id", "send", json!({ "b": 1, "a": 2 }))).unwrap();
        let second =
            JevConfirmedCalls::snapshot(&call("id", "send", json!({ "a": 2, "b": 1 }))).unwrap();
        assert_eq!(first.fingerprint, second.fingerprint);
        assert_eq!(first.fingerprint.len(), 64);
        assert!(first
            .fingerprint
            .chars()
            .all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase()));

        // Cross-checked against an independently computed digest of the canonical JSON.
        let expected = Sha256::digest(br#"{"arguments":{"a":2,"b":1},"name":"send"}"#);
        let expected: String = expected.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(first.fingerprint, expected);
    }

    #[test]
    fn installing_is_idempotent_and_keeps_the_ledger() {
        let mut ctx = RuntimeContext::new();
        let first = JevConfirmedCalls::install(&mut ctx);
        first.remember_call(&refund(1));
        let second = JevConfirmedCalls::install(&mut ctx);
        assert_eq!(second.len(), 1);
        assert!(Arc::ptr_eq(&first, &second));
        assert!(matches(&ctx, &refund(1)));
    }

    #[test]
    fn no_ledger_means_no_match() {
        let ctx = RuntimeContext::new();
        assert!(!matches(&ctx, &refund(1)));
        assert!(!contains(&ctx, &refund(1)));
    }
}
