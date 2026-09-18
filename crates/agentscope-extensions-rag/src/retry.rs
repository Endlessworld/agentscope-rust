//! HTTP retry helper shared by remote backends.
//!
//! Ports the `RetryInterceptor` embedded in Java `DifyRAGClient`: retry on transport
//! failures, HTTP 429 and 5xx; propagate 4xx immediately. Backoff is exponential with a
//! fixed factor (the Java interceptor only logs; no delay is applied, so the Rust port
//! keeps the same shape but sleeps between attempts so it is actually useful).

use std::time::Duration;

use crate::error::{RagError, Result};

/// Configuration for [`retry_async`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Maximum number of retries after the initial attempt.
    pub max_retries: u32,
    /// Base delay before the first retry; doubled per subsequent attempt.
    pub base_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        // Matches Java `DifyRAGConfig.DEFAULT_MAX_RETRIES = 3`.
        Self {
            max_retries: 3,
            base_delay: Duration::from_millis(200),
        }
    }
}

impl RetryPolicy {
    /// Creates a policy with the given retry budget and no delay (useful in tests).
    pub fn new(max_retries: u32) -> Self {
        Self {
            max_retries,
            base_delay: Duration::ZERO,
        }
    }
}

/// Runs `op`, retrying while the error is retryable and the budget allows it.
///
/// Returns the last error when the budget is exhausted. A non-retryable error is
/// returned immediately.
pub async fn retry_async<F, Fut, T>(policy: &RetryPolicy, mut op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut attempt = 0u32;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(err) => {
                if attempt >= policy.max_retries || !is_retryable_error(&err) {
                    return Err(err);
                }
                let delay = policy.base_delay * 2u32.pow(attempt);
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                attempt += 1;
            }
        }
    }
}

/// Whether an error should be retried (transport, 429 or 5xx).
fn is_retryable_error(err: &RagError) -> bool {
    match err {
        RagError::Http(_) | RagError::Io(_) => true,
        RagError::Api { status_code, .. } => *status_code == 429 || *status_code >= 500,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[tokio::test]
    async fn retries_retryable_errors_then_succeeds() {
        let policy = RetryPolicy::new(3);
        let calls = Cell::new(0);
        let result: Result<u32> = retry_async(&policy, || {
            let n = calls.get() + 1;
            calls.set(n);
            async move {
                if n < 3 {
                    Err(RagError::Http("boom".to_string()))
                } else {
                    Ok(n)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 3);
        assert_eq!(calls.get(), 3);
    }

    #[tokio::test]
    async fn does_not_retry_client_errors() {
        let policy = RetryPolicy::new(3);
        let calls = Cell::new(0);
        let result: Result<u32> = retry_async(&policy, || {
            calls.set(calls.get() + 1);
            async move {
                Err(RagError::Api {
                    status_code: 400,
                    error_code: None,
                    message: "bad request".to_string(),
                })
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.get(), 1);
    }

    #[tokio::test]
    async fn retries_429_and_5xx() {
        for status in [429u16, 500, 503] {
            let policy = RetryPolicy::new(1);
            let calls = Cell::new(0);
            let result: Result<u32> = retry_async(&policy, || {
                calls.set(calls.get() + 1);
                async move {
                    Err(RagError::Api {
                        status_code: status,
                        error_code: None,
                        message: "retry me".to_string(),
                    })
                }
            })
            .await;
            assert!(result.is_err());
            assert_eq!(calls.get(), 2, "status {status} should be retried once");
        }
    }

    #[tokio::test]
    async fn exhausts_budget_and_returns_last_error() {
        let policy = RetryPolicy::new(2);
        let calls = Cell::new(0);
        let result: Result<u32> = retry_async(&policy, || {
            calls.set(calls.get() + 1);
            async move { Err(RagError::Http("always offline".to_string())) }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn default_policy_matches_java_max_retries() {
        assert_eq!(RetryPolicy::default().max_retries, 3);
    }
}
