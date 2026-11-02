//! JEV context compaction and archiving (`io.agentscope.extensions.judge.jev.context`).
//!
//! Sub-modules, in dependency order:
//!
//! * [`message`] — the neutral message projection the compactor rewrites.
//! * [`archive`] — the host-authorized snapshot archive contract and its envelope.
//! * [`transcript`] — call/result pairing, state fitting and the rewrite.
//! * [`compactor`] — the decision orchestration (mode, budget, batching, reduction check).
//! * [`file_archive`] — a local, content-addressed, atomically published archive.
//!
//! Deviations from Java, all tracked in `docs/extensions/JUDGE__GAP_TRACKER.md`:
//!
//! * Messages are the crate-local [`message::ContextMessage`] projection instead of
//!   `agentscope-core`'s `Msg` (this crate does not depend on core).
//! * `JevContextCompactor` does not implement the harness' `ConversationCompactionStrategy`;
//!   [`compactor::CompactionRequest`] is the equivalent seam.
//! * `StoreJevContextArchive` (harness `BaseStore`) is represented by
//!   [`archive::UnavailableContextArchive`] until the host supplies a store adapter.

pub mod archive;
pub mod compactor;
pub mod file_archive;
pub mod message;
pub mod transcript;

pub use archive::{ArchiveEnvelope, ArchiveScope, JevContextArchive, UnavailableContextArchive};
pub use compactor::{Action, CompactionRequest, Config, ContextPlan, JevContextCompactor, Scores};
pub use file_archive::FileJevContextArchive;
pub use message::{
    ContextBlock, ContextMessage, ContextResultPart, ToolResultState, ROLE_ASSISTANT, ROLE_SYSTEM,
    ROLE_TOOL, ROLE_USER,
};
pub use transcript::{estimate_tokens, truncate, CompactionTranscript, Fitted, Pair};
