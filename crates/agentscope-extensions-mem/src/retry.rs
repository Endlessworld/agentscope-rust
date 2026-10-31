//! Retry / backoff policy for the MEM vendor clients.
//!
//! The Java clients (`Mem0Client` / `BailianMemoryClient` / `ReMeClient`) do **not**
//! retry — a failed request surfaces immediately. `RetryPolicy::default()` is
//! therefore a single attempt, so the Rust port matches Java out of the box. The
//! policy exists so callers can opt into bounded exponential backoff without
//! forking the clients: it lives at the [`HttpTransport`] seam, so every vendor
//! client inherits it through its `*_and_policy` constructor.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::error::{MemError, Result};
use crate::http::HttpTransport;

/// Exponential-backoff retry policy.
#[derive(Debug, Clone, PartialEq)]
pub struct RetryPolicy {
    /// Total number of attempts, including the first one (clamped to `>= 1`).
    pub max_attempts: u32,
    /// Backoff applied before the second attempt.
    pub initial_backoff: Duration,
    /// Upper bound for any single backoff.
    pub max_backoff: Duration,
    /// Growth factor applied per additional retry.
    pub multiplier: f64,
}

impl Default for RetryPolicy {
    /// Java parity: exactly one attempt, no sleeping.
    fn default() -> Self {
        Self::none()
    }
}

impl RetryPolicy {
    /// Java-parity policy: a single attempt, no backoff.
    pub fn none() -> Self {
        Self {
            max_attempts: 1,
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
            multiplier: 1.0,
        }
    }

    /// Builds a policy with exponential backoff.
    ///
    /// `max_attempts` is clamped to at least 1 and `multiplier` to at least 1.0,
    /// so a malformed policy can never produce an unbounded or shrinking backoff.
    pub fn new(
        max_attempts: u32,
        initial_backoff: Duration,
        max_backoff: Duration,
        multiplier: f64,
    ) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            initial_backoff,
            max_backoff,
            multiplier: if multiplier.is_finite() {
                multiplier.max(1.0)
            } else {
                1.0
            },
        }
    }

    /// Backoff to wait before retry number `retry` (1-based), capped by
    /// [`Self::max_backoff`]. Returns zero when no backoff is configured.
    pub fn backoff_before_retry(&self, retry: u32) -> Duration {
        if retry == 0 || self.initial_backoff.is_zero() {
            return Duration::ZERO;
        }
        let exponent = retry.saturating_sub(1).min(32) as i32;
        let factor = self.multiplier.powi(exponent);
        let millis = self.initial_backoff.as_secs_f64() * 1000.0 * factor;
        let capped = millis.min(self.max_backoff.as_secs_f64() * 1000.0);
        Duration::from_secs_f64((capped / 1000.0).max(0.0))
    }

    /// Whether a failure is worth retrying.
    ///
    /// Transport failures (connect / timeout / decode) and the transient HTTP
    /// statuses `408`, `429` and `5xx` are retried; configuration and JSON shape
    /// errors are not — retrying them could never succeed.
    pub fn is_retryable(err: &MemError) -> bool {
        match err {
            MemError::Transport(_) => true,
            MemError::HttpStatus { status, .. } => {
                *status == 408 || *status == 429 || *status >= 500
            }
            MemError::Config(_) | MemError::Json(_) => false,
        }
    }
}

/// [`HttpTransport`] decorator that applies a [`RetryPolicy`] to the wrapped
/// transport and counts the retries it performs.
pub struct RetryTransport {
    inner: Arc<dyn HttpTransport>,
    policy: RetryPolicy,
    retries: AtomicU64,
}

impl RetryTransport {
    /// Wraps `inner` with `policy`.
    pub fn new(inner: Arc<dyn HttpTransport>, policy: RetryPolicy) -> Self {
        Self {
            inner,
            policy,
            retries: AtomicU64::new(0),
        }
    }

    /// The policy in force.
    pub fn policy(&self) -> &RetryPolicy {
        &self.policy
    }

    /// Number of retries performed so far.
    pub fn retries(&self) -> u64 {
        self.retries.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl HttpTransport for RetryTransport {
    async fn post_json(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<String> {
        let max_attempts = self.policy.max_attempts.max(1);
        let mut attempt: u32 = 1;
        loop {
            match self.inner.post_json(url, headers, body).await {
                Ok(response) => return Ok(response),
                Err(err) => {
                    if attempt >= max_attempts || !RetryPolicy::is_retryable(&err) {
                        return Err(err);
                    }
                    let backoff = self.policy.backoff_before_retry(attempt);
                    self.retries.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(
                        attempt,
                        backoff_ms = backoff.as_millis() as u64,
                        error = %err,
                        "retrying memory request"
                    );
                    if !backoff.is_zero() {
                        tokio::time::sleep(backoff).await;
                    }
                    attempt += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::mock::MockTransport;

    fn transport(mock: &MockTransport, policy: RetryPolicy) -> RetryTransport {
        let inner: Arc<dyn HttpTransport> = Arc::new(mock.clone());
        RetryTransport::new(inner, policy)
    }

    fn fast_policy(max_attempts: u32) -> RetryPolicy {
        RetryPolicy::new(max_attempts, Duration::ZERO, Duration::ZERO, 1.0)
    }

    #[test]
    fn default_policy_is_a_single_attempt_like_java() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.max_attempts, 1);
        assert_eq!(policy, RetryPolicy::none());
        assert_eq!(policy.backoff_before_retry(1), Duration::ZERO);
    }

    #[test]
    fn backoff_grows_exponentially_and_is_capped() {
        let policy = RetryPolicy::new(
            5,
            Duration::from_millis(100),
            Duration::from_millis(250),
            2.0,
        );
        // Allow a 1ms rounding slack: the schedule is computed in f64 seconds.
        let approx = |actual: Duration, expected_ms: u64| {
            (actual.as_millis() as i64 - expected_ms as i64).abs() <= 1
        };
        assert_eq!(policy.backoff_before_retry(0), Duration::ZERO);
        assert!(approx(policy.backoff_before_retry(1), 100));
        assert!(approx(policy.backoff_before_retry(2), 200));
        // 400ms would exceed the cap.
        assert!(approx(policy.backoff_before_retry(3), 250));
        assert!(approx(policy.backoff_before_retry(9), 250));
    }

    #[test]
    fn policy_clamps_degenerate_values() {
        let policy = RetryPolicy::new(0, Duration::from_millis(1), Duration::ZERO, 0.5);
        assert_eq!(policy.max_attempts, 1);
        assert_eq!(policy.multiplier, 1.0);
        let nan = RetryPolicy::new(2, Duration::from_millis(1), Duration::ZERO, f64::NAN);
        assert_eq!(nan.multiplier, 1.0);
    }

    #[test]
    fn retryability_matches_transient_failures_only() {
        assert!(RetryPolicy::is_retryable(&MemError::Transport("e".into())));
        for status in [408_u16, 429, 500, 502, 503] {
            assert!(RetryPolicy::is_retryable(&MemError::http_status(
                status,
                String::new()
            )));
        }
        for status in [400_u16, 401, 403, 404, 422] {
            assert!(!RetryPolicy::is_retryable(&MemError::http_status(
                status,
                String::new()
            )));
        }
        assert!(!RetryPolicy::is_retryable(&MemError::Config("c".into())));
        let json = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        assert!(!RetryPolicy::is_retryable(&MemError::Json(json)));
    }

    #[tokio::test]
    async fn retries_transport_errors_until_success() {
        let mock = MockTransport::new();
        mock.push_response(Err(MemError::Transport("down".into())));
        mock.push_response(Err(MemError::Transport("still down".into())));
        mock.push_body("{\"ok\":true}");
        let retry = transport(&mock, fast_policy(3));
        let body = retry.post_json("http://x/y", &[], "{}").await.unwrap();
        assert_eq!(body, "{\"ok\":true}");
        assert_eq!(mock.requests().len(), 3);
        assert_eq!(retry.retries(), 2);
    }

    #[tokio::test]
    async fn retries_server_errors_but_not_client_errors() {
        let server = MockTransport::new();
        server.push_response(Err(MemError::http_status(503, "busy")));
        server.push_body("ok");
        let retry = transport(&server, fast_policy(3));
        assert_eq!(retry.post_json("http://x/y", &[], "{}").await.unwrap(), "ok");
        assert_eq!(server.requests().len(), 2);

        let client_err = MockTransport::new();
        client_err.push_response(Err(MemError::http_status(400, "bad request")));
        let retry = transport(&client_err, fast_policy(3));
        assert!(retry.post_json("http://x/y", &[], "{}").await.is_err());
        assert_eq!(client_err.requests().len(), 1);
        assert_eq!(retry.retries(), 0);
    }

    #[tokio::test]
    async fn stops_after_max_attempts() {
        let mock = MockTransport::new();
        for _ in 0..3 {
            mock.push_response(Err(MemError::Transport("down".into())));
        }
        let retry = transport(&mock, fast_policy(3));
        assert!(matches!(
            retry.post_json("http://x/y", &[], "{}").await,
            Err(MemError::Transport(_))
        ));
        assert_eq!(mock.requests().len(), 3);
        assert_eq!(retry.retries(), 2);
    }
}
