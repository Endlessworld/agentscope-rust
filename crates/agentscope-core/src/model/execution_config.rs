//! Unified execution configuration for timeout and retry behavior.
use std::time::Duration;

use chrono::TimeDelta;

/// Controls timeout and retry behavior for model calls and tool executions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExecutionConfig {
    /// Timeout for a single execution.
    pub timeout: Option<Duration>,
    /// Maximum number of attempts (including the initial attempt).
    pub max_attempts: Option<u32>,
    /// Initial backoff for the first retry.
    pub initial_backoff: Option<Duration>,
    /// Maximum backoff between retries.
    pub max_backoff: Option<Duration>,
    /// Multiplier applied to backoff after each retry.
    pub backoff_multiplier: Option<f64>,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            timeout: None,
            max_attempts: None,
            initial_backoff: None,
            max_backoff: None,
            backoff_multiplier: None,
        }
    }
}

impl ExecutionConfig {
    /// Standard defaults for model API calls (5min per-attempt timeout, 7 attempts).
    ///
    /// The backoff doubles from 2s and is capped at 60s, so the waits after a
    /// failed attempt are 2s, 4s, 8s, 16s, 32s, 60s (~2min of waiting in total)
    /// — long enough for a rate-limited or briefly unreachable gateway to
    /// recover, which the previous 2s/4s ladder was not.
    pub fn model_defaults() -> Self {
        Self {
            timeout: Some(Duration::from_secs(300)),
            max_attempts: Some(7),
            initial_backoff: Some(Duration::from_secs(2)),
            max_backoff: Some(Duration::from_secs(60)),
            backoff_multiplier: Some(2.0),
        }
    }

    /// Standard defaults for tool executions (5min timeout, no retry).
    pub fn tool_defaults() -> Self {
        Self {
            timeout: Some(Duration::from_secs(300)),
            max_attempts: Some(1),
            initial_backoff: None,
            max_backoff: None,
            backoff_multiplier: None,
        }
    }

    /// Creates a new builder.
    pub fn builder() -> ExecutionConfigBuilder {
        ExecutionConfigBuilder::default()
    }

    /// Merges two configs, with `primary` taking precedence over `fallback`.
    pub fn merge_configs(primary: Option<&ExecutionConfig>, fallback: Option<&ExecutionConfig>) -> Option<ExecutionConfig> {
        match (primary, fallback) {
            (None, None) => None,
            (Some(p), None) => Some(*p),
            (None, Some(f)) => Some(*f),
            (Some(p), Some(f)) => Some(ExecutionConfig {
                timeout: p.timeout.or(f.timeout),
                max_attempts: p.max_attempts.or(f.max_attempts),
                initial_backoff: p.initial_backoff.or(f.initial_backoff),
                max_backoff: p.max_backoff.or(f.max_backoff),
                backoff_multiplier: p.backoff_multiplier.or(f.backoff_multiplier),
            }),
        }
    }

    /// Whether the given HTTP status code is retryable (429 or 5xx).
    pub fn is_retryable_status(status: u16) -> bool {
        status == 429 || status >= 500
    }

    /// Computes the backoff delay for the given attempt index (0-based retry).
    pub fn backoff_for(&self, attempt: u32) -> Duration {
        let mult = self.backoff_multiplier.unwrap_or(2.0);
        let init = self.initial_backoff.unwrap_or(Duration::from_secs(1));
        let max = self.max_backoff.unwrap_or(Duration::from_secs(60));
        let mut delay = init;
        for _ in 0..attempt {
            let next = delay.mul_f64(mult);
            if next >= max {
                delay = max;
                break;
            }
            delay = next;
        }
        delay.min(max)
    }

    /// Helper to convert a chrono `TimeDelta` to `Duration` (kept for API parity).
    pub fn from_timedelta(d: TimeDelta) -> Duration {
        Duration::from_secs(d.num_seconds().max(0) as u64)
    }
}

/// Builder for `ExecutionConfig`.
#[derive(Debug, Default, Clone, Copy)]
pub struct ExecutionConfigBuilder {
    timeout: Option<Duration>,
    max_attempts: Option<u32>,
    initial_backoff: Option<Duration>,
    max_backoff: Option<Duration>,
    backoff_multiplier: Option<f64>,
}

impl ExecutionConfigBuilder {
    pub fn timeout(mut self, v: Duration) -> Self { self.timeout = Some(v); self }
    pub fn max_attempts(mut self, v: u32) -> Self { self.max_attempts = Some(v); self }
    pub fn initial_backoff(mut self, v: Duration) -> Self { self.initial_backoff = Some(v); self }
    pub fn max_backoff(mut self, v: Duration) -> Self { self.max_backoff = Some(v); self }
    pub fn backoff_multiplier(mut self, v: f64) -> Self { self.backoff_multiplier = Some(v); self }
    pub fn build(self) -> ExecutionConfig {
        ExecutionConfig {
            timeout: self.timeout,
            max_attempts: self.max_attempts,
            initial_backoff: self.initial_backoff,
            max_backoff: self.max_backoff,
            backoff_multiplier: self.backoff_multiplier,
        }
    }
}
