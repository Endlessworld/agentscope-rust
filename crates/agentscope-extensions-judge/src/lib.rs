//! `agentscope-extensions-judge` — ports the Java `agentscope-extensions-judge` module.
//!
//! Two decision APIs live here, both of which return calibrated, closed-world answers instead
//! of free-form chat:
//!
//! * [`jev`] — TypeSafe **System One** (codename JEV): yes/no (`noul`), closed choice and
//!   ordered-rubric (`score`) questions answered with probability distributions.
//! * [`openai_decisions`] — the provider's **Decisions** endpoint: named predicate/choice/score
//!   questions over one shared text-or-image input, with per-question refusals.
//!
//! ## Layered layout
//!
//! | Layer | JEV | Decisions |
//! |---|---|---|
//! | wire DTOs | [`jev::dto`] | [`openai_decisions::dto`] |
//! | encoding / validation | [`jev::validation`], [`jev::probability`] | [`openai_decisions::protocol`] |
//! | HTTP client | [`jev::client::JevClient`] | [`openai_decisions::OpenAIDecisionsClient`] |
//! | errors | [`JevError`] | [`openai_decisions::DecisionsError`] |
//!
//! Both clients reach the network only through [`http::JudgeHttpTransport`], so production uses
//! [`http::ReqwestJudgeTransport`] while unit tests inject an in-memory transport and never open
//! a socket. Timeouts, retry budgets (408/409/429/529/5xx for JEV, 429/5xx for Decisions) and
//! sanitized error mapping are owned by each client.
//!
//! JEV also carries the support layers: [`jev::guardrail`] (content-safety verdict fold),
//! [`jev::review`] (deterministic review regions), [`jev::judge`] (the all-required-criteria judge),
//! [`jev::execution`] (per-subscription budget/observation DTOs), and the orchestration packages
//! [`jev::evaluation`], [`jev::context`], [`jev::application`], [`jev::evidence`],
//! [`jev::routing`], [`jev::supervision`], [`jev::integration`] and [`jev::code_review`].
//!
//! ## Host bindings (`jev::runtime`)
//!
//! [`jev::runtime`] is the only module that binds the framework-free state machines to
//! `agentscope-core` / `agentscope-harness`:
//!
//! * the four middleware hooks (`TraceEvaluationMiddleware`, `ResponseMiddleware`,
//!   `SupervisionMiddleware`, `PhaseRoutingMiddleware`), including the guardrail screening caller
//!   ([`jev::screening::GuardrailScreener`] — `JevGuardrail.screen` over the real `JevClient`), the
//!   quality/reviser branch and the supervision capture that classifies the agent result into
//!   `worker_active` / `worker_failed` (Java `6ec57d245`);
//! * the `@Tool` layer (`register_jev_tools` for `search_evidence`, `search_knowledge`,
//!   `review_code_snapshot`) and the staged `JevCodeReviewer` driver;
//! * `StoreJevContextArchive` over the harness `BaseStore` (content-addressed, create-if-absent).
//!
//! [`jev::confirmed_calls`] binds tool approvals to a `RuntimeContext` keyed slot
//! (`RuntimeContext::set_extension`), so a resumed call only matches the arguments the user saw.
//! `tests/runtime_integration.rs` drives these through the real `MiddlewareChain` / `Toolkit` /
//! `BaseStore` entry points.
//!
//! ## Not landed (no gap claimed as done)
//!
//! * `browser/` (Playwright) — recorded as `已完成（设计决定，不迁移）`: a Node-driven real browser
//!   runtime contradicts this crate's offline testable scope. Replacement: the trait/source seams
//!   (`jev::runtime::tools::EvidenceSource`, `jev::runtime::tools::CodeReviewSource`) let a host
//!   supply them.
//! * the JEV `example/` package — recorded as `已完成（设计决定，不迁移）`: it is packaging, not runtime
//!   code, and its model-selection role is covered by [`jev::application::stage_router`] plus
//!   [`jev::runtime::PhaseRoutingMiddleware`]. The *openai-decisions* `example/` package is **not**
//!   in that category: it holds real middleware and is ported as
//!   [`openai_decisions::middleware`].
//! * `JevStageRouter`'s legacy model-selection mode (needs `example/JevModelRouterMiddleware`); the
//!   explicit stage entry + `JevPhaseRouting` remain.
//!
//! All are tracked in `docs/extensions/JUDGE__GAP_TRACKER.md`.

pub mod error;
pub mod http;
pub mod jev;
pub mod openai_decisions;

pub use error::JevError;
pub use http::{
    backoff_ms, JudgeHttpRequest, JudgeHttpResponse, JudgeHttpTransport, JudgeTransportError,
    ReqwestJudgeTransport, JSON_CONTENT_TYPE,
};
pub use jev::{JevClient, JevClientBuilder};
pub use openai_decisions::{DecisionsError, OpenAIDecisionsClient, OpenAIDecisionsClientBuilder};
