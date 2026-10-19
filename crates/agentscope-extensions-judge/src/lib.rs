//! `agentscope-extensions-judge` — ports the Java
//! `agentscope-extensions-judge/agentscope-extensions-jev` module (TypeSafe *System One*
//! client, codename **JEV**).
//!
//! Jev is not a chat model: the caller declares a closed set of questions (yes/no, closed
//! choice, ordered rubric) and the model answers with a calibrated probability distribution.
//! This crate mirrors that contract:
//!
//! * [`jev::dto`] — typed [`jev::dto::Question`] / [`jev::dto::Answer`] hierarchies and the
//!   System One request/response envelopes (serde, `type`-tagged like Jackson's
//!   `@JsonTypeInfo`).
//! * [`jev::probability`] — the probability-sum acceptance rule, including the 4-decimal
//!   rounding allowance relaxed by Java commit `a5be76105`
//!   (*accept probability sums within the API's 4-decimal rounding bound*).
//! * [`jev::validation`] — request/response contract validation (`JevClient.validateRequest`
//!   / `validateResponse`), typed through [`JevError`].
//! * [`jev::review`] — [`jev::review::ReviewRegions`], deterministic evidence locations derived
//!   from supplied text (never from model-generated line numbers).
//! * [`jev::guardrail`] — the pure content-safety verdict fold (hazard outcomes + severity).
//! * [`jev::execution`] — the shared per-subscription execution budget/observation DTOs.
//!
//! ## Not landed yet (no gaps claimed as done)
//!
//! `JevClient` itself (HTTP transport, retry/backoff, timeout), `JevGuardrail::screen`'s
//! reactive caller, `JevConfirmedCalls`, `JevBrowserSession`/Playwright, `JevContextArchive`
//! and the `application/`, `evaluation/`, `evidence/`, `example/`, `integration/`, `routing/`,
//! `supervision/` sub-packages all need seams (`reqwest`, a scheduler, the harness workspace)
//! that are outside this module's landed scope.

pub mod error;
pub mod jev;

pub use error::JevError;
