//! Brain/Hands scheduling over the durable tick: who decides, who acts, who wakes them up.
//!
//! The durable runtime already owns the only state machine there is
//! ([`DurableRuntime::tick`](agentscope_durable::DurableRuntime::tick) advances a thread by one
//! step and never waits). What a host still needs is a place to answer three questions without
//! duplicating that state machine:
//!
//! * **who decides what to do next** — [`Brain`]: reads a durable snapshot, returns one
//!   [`Intent`], and cannot execute anything;
//! * **who carries the effect out** — [`Hands`]: the durable [`Executor`](agentscope_durable::executor::Executor)
//!   port under an intent-level name, so there is exactly one definition of "applied an effect";
//! * **who says when to run** — [`Driver`]: [`LocalDriver`] loops while the worker says more work
//!   exists right now, [`EdgeDriver`] runs exactly one step per wakeup.
//!
//! [`Worker::step`] is the composition: one tick, at most one decision, at most one effect, and
//! the effect is written back durably (a `hands.effect` signal) before the step returns.
//!
//! # The four rules this crate is built around
//!
//! * **R1 — no in-memory truth**: a step reads the turn through the backend
//!   (`thread_state` + `turn`) and writes through the backend (the signal); nothing is cached
//!   between steps, so a host that is recycled loses no work.
//! * **R2 — no unbounded blocking**: a step awaits a bounded number of calls (one tick, one
//!   decision, one apply, one signal) and never waits for an effect to finish; a long effect is
//!   reported as `PENDING`/`RUNNING` and looked up by a later step.
//! * **R3 — no implicit ordering**: correctness does not depend on which driver or thread ran; a
//!   driver only decides *how often* to call [`Worker::step`].
//! * **R4 — no clock dependence**: nothing sleeps or schedules a wakeup here; the host owns the
//!   clock, which is why the same code runs in a resident process and on an edge host that only
//!   runs when woken.
//!
//! # Two drivers, one worker
//!
//! [`LocalDriver`] and [`EdgeDriver`] hold the same [`Worker`] and differ **only** in how many
//! steps they take per call: the local host drains the work that is already due, the edge host
//! takes one step and leaves the rest to its next wakeup. Both stop on the same verdict
//! ([`StepReport::more_work`]), which the worker computes — so no business branch lives in a
//! driver (see `docs/cloud/CF_DEPLOYMENT_ARCHITECTURE.md` §18.2).

pub mod brain;
pub mod driver;
pub mod hands;
pub mod intent;
pub mod worker;

pub use brain::{Brain, BrainView, StubBrain, StubReply};
pub use driver::{DriveReport, Driver, EdgeDriver, LocalDriver};
pub use hands::{Hands, StubHands};
pub use intent::{Intent, IntentKind};
pub use worker::{StepReport, Worker, HANDS_SIGNAL_KEY};

/// The durable result type, re-exported so a brain or a hands implementation needs one import.
pub use agentscope_durable::{DurableError, Result};
