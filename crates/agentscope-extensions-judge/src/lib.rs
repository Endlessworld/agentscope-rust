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
//! ## Not landed (no gap claimed as done)
//!
//! Everything below needs a `agentscope-core` / `agentscope-harness` seam this crate deliberately
//! does not depend on, plus `JevGuardrail::screen`'s reactive caller and `JevConfirmedCalls`:
//!
//! * the reactive middleware hooks (`JevTraceEvaluationMiddleware::onAgent/onModelCall`,
//!   `JevSupervisionMiddleware`'s observer loop, `JevResponseMiddleware::onModelCall`,
//!   `JevPhaseRouting::onModelCall`) — their pure state machines/tables *are* ported;
//! * tool registration (`JevEvidenceTool`, `JevKnowledgeTool`, `JevCodeReviewTool`) — the
//!   agent-visible projections are ported;
//! * `JevStageRouter` (needs `JevModelRouterMiddleware` from `example/`) and `JevCodeReviewer`'s
//!   staged driver;
//! * `StoreJevContextArchive` (needs harness `BaseStore`) and the `browser/` + `example/`
//!   sub-packages.
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
