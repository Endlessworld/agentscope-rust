use std::time::Duration;

/// Resource limits for one script execution.
#[derive(Debug, Clone)]
pub struct ScriptLimits {
    /// Wall-clock budget. Enforced by an interrupt handler (stops dead loops) plus an outer
    /// `tokio::time::timeout` so the caller always returns.
    pub timeout: Duration,
    /// QuickJS heap cap, in bytes. `0` means unlimited (not recommended).
    pub memory_bytes: usize,
    /// QuickJS stack cap, in bytes.
    pub stack_bytes: usize,
}

impl Default for ScriptLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            memory_bytes: 16 << 20,
            stack_bytes: 512 << 10,
        }
    }
}
