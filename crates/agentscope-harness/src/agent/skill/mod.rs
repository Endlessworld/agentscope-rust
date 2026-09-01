//! Harness skill runtime and curator, mirroring
//! `io.agentscope.harness.agent.skill`.
//!
//! The runtime renders the `<available_skills>` prompt block from a per-call
//! `SkillCatalog`; the marketplace stager materialises non-workspace skill
//! resources to `.skills-cache`; the curator maintains skill lifecycle
//! (active/stale/archived) and produces dry-run consolidation reports; the
//! repository / manage / promoter modules implement the self-learning loop
//! (mirroring `AgentSkillRepository`, `SkillManageTool`, `ProposeSkillTool`,
//! `SkillPromoter`, `SkillAuditLog` and the promotion gates).

pub mod curator;
pub mod load_tool;
pub mod manage;
pub mod model;
pub mod prompt;
pub mod promoter;
pub mod repository;
pub mod runtime;
pub mod shell_path;
pub mod stager;

pub use curator::{
    build_candidate, CuratorRunReport, FilesystemSkillUsageStore, InMemorySkillUsageStore,
    ScriptFilePreview, SkillCandidate, SkillCurator, SkillCuratorConfig, SkillCuratorState,
    SkillState, SkillUsageRecord, SkillUsageStore, TransitionCounts, UmbrellaPassMode,
};
pub use load_tool::{LazyLoadSkillTool, LoadSkillTool};
pub use manage::{build_promoter, default_gate, ProposeSkillTool, SkillManageConfig, SkillManageTool};
pub use model::{AgentSkill, AgentSkillBuilder, HarnessSkillEntry, SkillCatalog, SkillFilter};
pub use prompt::{SkillPromptBuilder, DEFAULT_CODE_EXECUTION_INSTRUCTION, DEFAULT_HEADER};
pub use promoter::{
    AllowListFilter, AllowListGate, AuditEntry, DenyListFilter, PromotionDecision, PromotionResult,
    RejectAllGate, SkillAuditLog, SkillPromoter, SkillPromotionGate, SkillVisibilityFilter,
};
pub use repository::{compose_skill_repositories, FileSystemSkillRepository, SkillRepository, WorkspaceSkillRepository};
pub use runtime::SkillRuntime;
pub use shell_path::{escape_spaces, Mode as ShellMode, ShellPathPolicy};
pub use stager::{
    resolve_source_namespaces, sha256_hex, should_be_executable, RepoBound, SkillRepo, StageResult,
};
