//! Harness skill runtime and curator, mirroring
//! `io.agentscope.harness.agent.skill`.
//!
//! The runtime renders the `<available_skills>` prompt block from a per-call
//! `SkillCatalog`; the marketplace stager materialises non-workspace skill
//! resources to `.skills-cache`; the curator maintains skill lifecycle
//! (active/stale/archived) and produces dry-run consolidation reports.

pub mod curator;
pub mod model;
pub mod prompt;
pub mod runtime;
pub mod shell_path;
pub mod stager;

pub use curator::{
    build_candidate, CuratorRunReport, InMemorySkillUsageStore, ScriptFilePreview, SkillCandidate,
    SkillCurator, SkillCuratorConfig, SkillCuratorState, SkillState, SkillUsageRecord,
    SkillUsageStore, TransitionCounts, UmbrellaPassMode,
};
pub use model::{AgentSkill, AgentSkillBuilder, HarnessSkillEntry, SkillCatalog, SkillFilter};
pub use prompt::{SkillPromptBuilder, DEFAULT_CODE_EXECUTION_INSTRUCTION, DEFAULT_HEADER};
pub use runtime::SkillRuntime;
pub use shell_path::{escape_spaces, Mode as ShellMode, ShellPathPolicy};
pub use stager::{
    resolve_source_namespaces, sha256_hex, should_be_executable, RepoBound, SkillRepo, StageResult,
};
