//! Harness memory management, mirroring `io.agentscope.harness.agent.memory`.
//!
//! - Compaction (conversation summarization), session-tree persistence and freshness
//!   evaluation.
//! - The unified long-term memory `MemoryConfig`, plus the LLM-driven flush and
//!   consolidation managers and the per-tool-result eviction config.

pub mod compaction;
pub mod consolidator;
pub mod eviction;
pub mod flush;
pub mod session;

use std::sync::Arc;

use chrono::Duration as ChronoDuration;

use agentscope_core::model::Model;

pub use consolidator::MemoryConsolidator;
pub use eviction::ToolResultEvictionConfig;
pub use flush::MemoryFlushManager;

/// Default consolidation token budget.
pub const DEFAULT_CONSOLIDATION_MAX_TOKENS: usize = 4_000;
/// Default minimum gap between consolidation/maintenance runs (30 minutes).
pub const DEFAULT_CONSOLIDATION_MIN_GAP_MINUTES: i64 = 30;
/// Default retention before a daily ledger is archived (90 days).
pub const DEFAULT_DAILY_FILE_RETENTION_DAYS: i64 = 90;
/// Default retention before a session JSONL log is pruned (180 days).
pub const DEFAULT_SESSION_RETENTION_DAYS: i64 = 180;

/// Strategy for the per-call flush hook. See [`FlushTrigger`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushMode {
    /// Flush after every agent call.
    Always,
    /// Disable per-call flush entirely (offload still runs).
    Never,
    /// Flush at most once per `FlushTrigger::min_gap`.
    Throttled,
}


/// Trigger policy for the per-call flush hook.
///
/// `throttled(Duration::zero())` normalizes to `always()` so callers do not need a
/// special branch for the degenerate case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlushTrigger {
    mode: FlushMode,
    min_gap: ChronoDuration,
}

impl FlushTrigger {
    /// Flush after every agent call.
    pub fn always() -> Self {
        Self { mode: FlushMode::Always, min_gap: ChronoDuration::zero() }
    }

    /// Disable per-call flush entirely (offload still runs).
    pub fn never() -> Self {
        Self { mode: FlushMode::Never, min_gap: ChronoDuration::zero() }
    }

    /// Flush at most once per `min_gap`. A zero gap normalizes to [`Self::always`].
    pub fn throttled(min_gap: ChronoDuration) -> Self {
        if min_gap.is_zero() {
            return Self::always();
        }
        Self { mode: FlushMode::Throttled, min_gap }
    }

    pub fn mode(&self) -> FlushMode {
        self.mode
    }

    pub fn min_gap(&self) -> &ChronoDuration {
        &self.min_gap
    }
}

/// Unified configuration for the long-term memory pipeline (flush + consolidation).
///
/// The harness has three LLM-driven memory operations: flush (extract long-term memories
/// into today's daily ledger), consolidation (periodically merge daily ledgers into
/// `MEMORY.md`) and compaction summary (see [`CompactionConfig`]). All fields have
/// sensible defaults; [`MemoryConfig::defaults`] reproduces the harness's historical
/// behaviour.
#[derive(Clone)]
pub struct MemoryConfig {
    /// Optional model override for memory operations; `None` uses the agent's primary model.
    pub model: Option<Arc<dyn Model>>,
    /// Flush prompt; `None` uses [`flush::DEFAULT_FLUSH_PROMPT`].
    pub flush_prompt: Option<String>,
    /// Consolidation prompt; `None` uses [`consolidator::DEFAULT_CONSOLIDATION_PROMPT`].
    pub consolidation_prompt: Option<String>,
    /// Token budget passed to the consolidation prompt.
    pub consolidation_max_tokens: usize,
    /// Minimum gap between two consolidation/maintenance runs.
    pub consolidation_min_gap: ChronoDuration,
    /// Days before a daily ledger is moved to `memory/archive/`.
    pub daily_file_retention_days: i64,
    /// Days before a session JSONL log is pruned.
    pub session_retention_days: i64,
    /// Trigger policy for the per-call flush hook.
    pub flush_trigger: FlushTrigger,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

impl MemoryConfig {
    /// Returns a config equivalent to the harness's historical defaults.
    pub fn defaults() -> Self {
        Self::builder().build()
    }

    /// Starts a builder for [`MemoryConfig`].
    pub fn builder() -> MemoryConfigBuilder {
        MemoryConfigBuilder::default()
    }
}

/// Builder for [`MemoryConfig`].
#[derive(Clone)]
pub struct MemoryConfigBuilder {
    pub model: Option<Arc<dyn Model>>,
    pub flush_prompt: Option<String>,
    pub consolidation_prompt: Option<String>,
    pub consolidation_max_tokens: usize,
    pub consolidation_min_gap: ChronoDuration,
    pub daily_file_retention_days: i64,
    pub session_retention_days: i64,
    pub flush_trigger: FlushTrigger,
}

impl Default for MemoryConfigBuilder {
    fn default() -> Self {
        Self {
            model: None,
            flush_prompt: None,
            consolidation_prompt: None,
            consolidation_max_tokens: DEFAULT_CONSOLIDATION_MAX_TOKENS,
            consolidation_min_gap: ChronoDuration::minutes(DEFAULT_CONSOLIDATION_MIN_GAP_MINUTES),
            daily_file_retention_days: DEFAULT_DAILY_FILE_RETENTION_DAYS,
            session_retention_days: DEFAULT_SESSION_RETENTION_DAYS,
            flush_trigger: FlushTrigger::always(),
        }
    }
}

impl MemoryConfigBuilder {
    pub fn model(mut self, v: Arc<dyn Model>) -> Self {
        self.model = Some(v);
        self
    }
    pub fn flush_prompt(mut self, v: impl Into<String>) -> Self {
        self.flush_prompt = Some(v.into());
        self
    }
    /// Overrides the consolidation prompt. Must contain exactly two `%d` placeholders.
    pub fn consolidation_prompt(mut self, v: impl Into<String>) -> Self {
        let s = v.into();
        if count_occurrences(&s, "%d") != 2 {
            panic!(
                "consolidationPrompt must contain exactly two %d placeholders \
                 (max-tokens, max-chars), found {}",
                count_occurrences(&s, "%d")
            );
        }
        self.consolidation_prompt = Some(s);
        self
    }
    pub fn consolidation_max_tokens(mut self, v: usize) -> Self {
        assert!(v > 0, "consolidation_max_tokens must be positive");
        self.consolidation_max_tokens = v;
        self
    }
    pub fn consolidation_min_gap(mut self, v: ChronoDuration) -> Self {
        assert!(v >= ChronoDuration::zero(), "consolidation_min_gap must not be negative");
        self.consolidation_min_gap = v;
        self
    }
    pub fn daily_file_retention_days(mut self, v: i64) -> Self {
        assert!(v > 0, "daily_file_retention_days must be positive");
        self.daily_file_retention_days = v;
        self
    }
    pub fn session_retention_days(mut self, v: i64) -> Self {
        assert!(v > 0, "session_retention_days must be positive");
        self.session_retention_days = v;
        self
    }
    pub fn flush_trigger(mut self, v: FlushTrigger) -> Self {
        self.flush_trigger = v;
        self
    }
    pub fn build(self) -> MemoryConfig {
        MemoryConfig {
            model: self.model,
            flush_prompt: self.flush_prompt,
            consolidation_prompt: self.consolidation_prompt,
            consolidation_max_tokens: self.consolidation_max_tokens,
            consolidation_min_gap: self.consolidation_min_gap,
            daily_file_retention_days: self.daily_file_retention_days,
            session_retention_days: self.session_retention_days,
            flush_trigger: self.flush_trigger,
        }
    }
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_historical_behaviour() {
        let cfg = MemoryConfig::defaults();
        assert_eq!(cfg.consolidation_max_tokens, 4_000);
        assert_eq!(cfg.consolidation_min_gap, ChronoDuration::minutes(30));
        assert_eq!(cfg.daily_file_retention_days, 90);
        assert_eq!(cfg.session_retention_days, 180);
        assert_eq!(cfg.flush_trigger.mode(), FlushMode::Always);
        assert!(cfg.model.is_none());
    }

    /// Java documents this invariant on `MemoryConfig.DEFAULT_CONSOLIDATION_MIN_GAP`
    /// ("matches `MemoryMaintenanceMiddleware`"); keep both sides in lockstep.
    #[test]
    fn consolidation_min_gap_default_matches_maintenance_middleware() {
        let cfg = MemoryConfig::defaults();
        let mw = crate::agent::middleware::MemoryMaintenanceMiddleware::new(
            std::sync::Arc::new(crate::filesystem::LocalFilesystem::sandboxed(
                std::env::temp_dir(),
            )),
            None,
        );
        assert_eq!(cfg.consolidation_min_gap, *mw.min_gap());
    }

    #[test]
    fn throttled_zero_gap_normalizes_to_always() {
        let t = FlushTrigger::throttled(ChronoDuration::zero());
        assert_eq!(t.mode(), FlushMode::Always);
        let t2 = FlushTrigger::throttled(ChronoDuration::minutes(5));
        assert_eq!(t2.mode(), FlushMode::Throttled);
        assert_eq!(*t2.min_gap(), ChronoDuration::minutes(5));
        assert_eq!(FlushTrigger::never().mode(), FlushMode::Never);
    }

    #[test]
    fn builder_enforces_two_percent_d_placeholders() {
        let ok = MemoryConfig::builder()
            .consolidation_prompt("budget %d tokens (~%d chars)")
            .build();
        assert_eq!(ok.consolidation_prompt.as_deref(), Some("budget %d tokens (~%d chars)"));
    }

    #[test]
    #[should_panic]
    fn builder_rejects_wrong_placeholder_count() {
        MemoryConfig::builder().consolidation_prompt("only one %d").build();
    }
}
