//! Bounded retry helper mirroring `AgentRunRetry` / `DaytonaRetry` / `E2bRetry`.
//!
//! The Java implementations retry transient failures (HTTP 429/502/503, plus 408 for E2B)
//! using a linear backoff of `200ms * (attempt + 1)`. This module models the *decision*
//! logic as pure, testable helpers plus an async `with_retries` driver.

use std::time::Duration;

use crate::error::{SandboxExtensionError, SandboxResult};

/// Retry policy for transient remote failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Maximum number of attempts (>= 1).
    pub max_attempts: u32,
    /// HTTP status codes considered transient.
    pub retryable_statuses: Vec<u16>,
    /// Base backoff in milliseconds; attempt `i` sleeps `base_ms * (i + 1)`.
    pub base_backoff_ms: u64,
}

impl RetryPolicy {
    /// AgentRun policy: 3 attempts, retries HTTP 429/502/503 (no 408).
    pub fn agentrun(max_attempts: u32) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            retryable_statuses: vec![429, 502, 503],
            base_backoff_ms: 200,
        }
    }

    /// Daytona policy: same transient set as AgentRun.
    pub fn daytona(max_attempts: u32) -> Self {
        Self::agentrun(max_attempts)
    }

    /// E2B policy: additionally treats HTTP 408 (request timeout) as transient.
    pub fn e2b(max_attempts: u32) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            retryable_statuses: vec![408, 429, 502, 503],
            base_backoff_ms: 200,
        }
    }

    /// Kubernetes policy: retries server-side throttling and gateway errors.
    pub fn kubernetes(max_attempts: u32) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            retryable_statuses: vec![429, 500, 502, 503, 504],
            base_backoff_ms: 200,
        }
    }

    /// Returns whether the given HTTP status is considered transient.
    pub fn is_retryable_status(&self, status: u16) -> bool {
        self.retryable_statuses.contains(&status)
    }

    /// Returns whether the given error is retryable under this policy.
    pub fn is_retryable(&self, err: &SandboxExtensionError) -> bool {
        match err {
            SandboxExtensionError::Http { status, .. } => self.is_retryable_status(*status),
            _ => false,
        }
    }

    /// Backoff for a zero-based attempt index.
    pub fn backoff(&self, attempt: u32) -> Duration {
        Duration::from_millis(self.base_backoff_ms * (attempt as u64 + 1))
    }
}

/// Executes `op` with bounded retries governed by `policy`.
///
/// The operation is retried only when it returns a retryable
/// [`SandboxExtensionError`] and attempts remain. The final error is returned unchanged.
pub async fn with_retries<F, Fut, T>(policy: &RetryPolicy, mut op: F) -> SandboxResult<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = SandboxResult<T>>,
{
    let attempts = policy.max_attempts.max(1);
    let mut last_err: Option<SandboxExtensionError> = None;
    for attempt in 0..attempts {
        match op().await {
            Ok(value) => return Ok(value),
            Err(err) => {
                let is_last = attempt + 1 >= attempts;
                if is_last || !policy.is_retryable(&err) {
                    return Err(err);
                }
                tokio::time::sleep(policy.backoff(attempt)).await;
                last_err = Some(err);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| {
        SandboxExtensionError::Transport("retry exhausted".to_string())
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[test]
    fn agentrun_policy_status_set() {
        let p = RetryPolicy::agentrun(3);
        assert!(p.is_retryable_status(429));
        assert!(p.is_retryable_status(503));
        assert!(p.is_retryable_status(502));
        assert!(!p.is_retryable_status(408));
        assert!(!p.is_retryable_status(500));
    }

    #[test]
    fn e2b_policy_includes_408() {
        assert!(RetryPolicy::e2b(3).is_retryable_status(408));
    }

    #[test]
    fn backoff_is_linear() {
        let p = RetryPolicy::agentrun(3);
        assert_eq!(p.backoff(0), Duration::from_millis(200));
        assert_eq!(p.backoff(1), Duration::from_millis(400));
        assert_eq!(p.backoff(2), Duration::from_millis(600));
    }

    #[test]
    fn max_attempts_is_at_least_one() {
        assert_eq!(RetryPolicy::agentrun(0).max_attempts, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn retries_until_success() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter2 = counter.clone();
        let policy = RetryPolicy::agentrun(3);
        let result: SandboxResult<u32> = with_retries(&policy, move || {
            let counter = counter2.clone();
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    Err(SandboxExtensionError::http(503, "unavailable"))
                } else {
                    Ok(n)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 2);
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn non_retryable_stops_immediately() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter2 = counter.clone();
        let policy = RetryPolicy::agentrun(5);
        let result: SandboxResult<()> = with_retries(&policy, move || {
            let counter = counter2.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err(SandboxExtensionError::http(400, "bad request"))
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn exhausts_attempts() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter2 = counter.clone();
        let policy = RetryPolicy::e2b(2);
        let result: SandboxResult<()> = with_retries(&policy, move || {
            let counter = counter2.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err(SandboxExtensionError::http(429, "throttled"))
            }
        })
        .await;
        assert_eq!(result.unwrap_err().status(), Some(429));
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }
}
