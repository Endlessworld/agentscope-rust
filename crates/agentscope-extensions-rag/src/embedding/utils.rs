//! Shared helpers for the HTTP embedding providers.
//!
//! Ports `io.agentscope.core.embedding.EmbeddingUtils`: the Java class wrapped a `Mono` with a
//! timeout and a Reactor retry, and offered the `List<Double>` / `List<Float>` / `float[]` →
//! `double[]` converters the three SDKs needed. In Rust the converters collapse to a single
//! `f32 → f64` widening (JSON numbers already arrive as `f64`), and the timeout/retry pair is
//! expressed as [`EmbeddingExecutionConfig`], which composes the crate's existing
//! [`crate::retry::RetryPolicy`] with `tokio::time::timeout`.
//!
//! Retry lives here (and not in the transport) on purpose: the `reqwest` transport built by
//! [`build_transport`] is configured with `max_retries = 0`, so an embedding call is attempted
//! exactly `retry.max_retries + 1` times — the same split as Java, where `EmbeddingUtils` owned
//! the retry loop and the OkHttp client did not retry.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use agentscope_core::message::ContentBlock;

use crate::error::{RagError, Result};
use crate::retry::{retry_async, RetryPolicy};
use crate::transport::{HttpTransport, ReqwestTransport, TransportOptions};

/// Java `EmbeddingUtils.DEFAULT_INITIAL_BACKOFF`.
pub const DEFAULT_INITIAL_BACKOFF: Duration = Duration::from_secs(1);
/// Java `EmbeddingUtils.DEFAULT_MAX_BACKOFF`.
pub const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(10);
/// Default per-attempt timeout.
///
/// Java's `ExecutionConfig.MODEL_DEFAULTS` carries a 300s timeout for chat models; embeddings are
/// single small requests, so this port keeps the crate-wide 60s default instead of inheriting
/// the chat value. Callers can override it through [`EmbeddingExecutionConfig::with_timeout`].
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// Default retry budget, matching the sibling backends' `RetryInterceptor` default.
pub const DEFAULT_MAX_RETRIES: u32 = 3;

/// Timeout + retry configuration for one embedding model.
///
/// Java modelled this as the shared `ExecutionConfig`; the Rust core has no embedding-facing
/// equivalent, so the two knobs that actually mattered are carried here. The backoff is Java
/// `EmbeddingUtils`' 1s initial delay capped at its `DEFAULT_MAX_BACKOFF` of 10s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingExecutionConfig {
    /// Per-attempt timeout; `None` disables it (Java: a `null` timeout).
    pub timeout: Option<Duration>,
    /// Retry budget and base backoff.
    pub retry: RetryPolicy,
}

impl Default for EmbeddingExecutionConfig {
    fn default() -> Self {
        Self {
            timeout: Some(DEFAULT_TIMEOUT),
            retry: RetryPolicy {
                max_retries: DEFAULT_MAX_RETRIES,
                base_delay: DEFAULT_INITIAL_BACKOFF,
                max_delay: Some(DEFAULT_MAX_BACKOFF),
            },
        }
    }
}

impl EmbeddingExecutionConfig {
    /// Java `EmbeddingUtils.ensureDefaultExecutionConfig`: `null` becomes the defaults.
    pub fn ensure_default(config: Option<Self>) -> Self {
        config.unwrap_or_default()
    }

    /// Disables both timeout and retry (the shape tests use).
    pub fn disabled() -> Self {
        Self {
            timeout: None,
            retry: RetryPolicy::new(0),
        }
    }

    /// Overrides the per-attempt timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Removes the per-attempt timeout.
    pub fn without_timeout(mut self) -> Self {
        self.timeout = None;
        self
    }

    /// Overrides the retry policy.
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Runs `op` with the configured timeout and retry budget.
    ///
    /// Java `applyTimeoutAndRetry`: the timeout error is an `EmbeddingException` naming the
    /// operation, the model and the provider; retries only cover retryable errors (transport,
    /// 429 and 5xx), exactly like [`retry_async`].
    pub async fn apply<T, F, Fut>(
        &self,
        model_name: &str,
        provider: &str,
        operation_type: &str,
        mut op: F,
    ) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let timeout = self.timeout;
        retry_async(&self.retry, || {
            let future = op();
            async move {
                match timeout {
                    Some(limit) => tokio::time::timeout(limit, future).await.map_err(|_| {
                        RagError::Embedding(format!(
                            "{operation_type} request timeout after {}s (model={model_name}, provider={provider})",
                            limit.as_secs()
                        ))
                    })?,
                    None => future.await,
                }
            }
        })
        .await
    }
}

/// Java `EmbeddingUtils.convertFloatArrayToDoubleArray` / `convertFloatListToDoubleArray`.
///
/// The `List<Double>` / `List<Float>` variants collapse into this one widening conversion: JSON
/// numbers already decode into `f64`.
pub fn f32_slice_to_f64(values: &[f32]) -> Vec<f64> {
    values.iter().map(|value| f64::from(*value)).collect()
}

/// Joins a base URL with an endpoint path, tolerating a trailing slash on the base.
pub(crate) fn join_url(base_url: &str, endpoint: &str) -> String {
    format!(
        "{}{}",
        base_url.trim_end_matches('/'),
        if endpoint.starts_with('/') {
            endpoint.to_string()
        } else {
            format!("/{endpoint}")
        }
    )
}

/// The Java simple class name of a content block, used verbatim in error messages.
pub(crate) fn block_type_name(block: &ContentBlock) -> &'static str {
    match block {
        ContentBlock::Text(_) => "TextBlock",
        ContentBlock::Thinking(_) => "ThinkingBlock",
        ContentBlock::Image(_) => "ImageBlock",
        ContentBlock::Audio(_) => "AudioBlock",
        ContentBlock::Video(_) => "VideoBlock",
        ContentBlock::ToolUse(_) => "ToolUseBlock",
        ContentBlock::ToolResult(_) => "ToolResultBlock",
        ContentBlock::Hint(_) => "HintBlock",
        ContentBlock::Data(_) => "DataBlock",
    }
}

/// Builds the production `reqwest` transport for an embedding model.
///
/// `max_retries` is forced to 0 because [`EmbeddingExecutionConfig::apply`] owns the retry loop;
/// letting the transport retry as well would multiply the attempt count.
pub(crate) fn build_transport(
    execution: &EmbeddingExecutionConfig,
    extra_headers: HashMap<String, String>,
) -> Result<Arc<dyn HttpTransport>> {
    let timeout = execution.timeout.unwrap_or(DEFAULT_TIMEOUT);
    let options = TransportOptions {
        connect_timeout: timeout,
        read_timeout: timeout,
        max_retries: 0,
        base_delay: Duration::ZERO,
        max_delay: None,
    };
    let transport = ReqwestTransport::with_extra_headers(&options, extra_headers)?;
    let transport: Arc<dyn HttpTransport> = Arc::new(transport);
    Ok(transport)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::{EmbeddingUsage, UsageListener};
    use agentscope_core::message::{ImageBlock, Source, TextBlock, URLSource};
    use std::sync::Mutex;

    #[test]
    fn default_config_matches_java_constants() {
        let config = EmbeddingExecutionConfig::default();
        assert_eq!(config.timeout, Some(Duration::from_secs(60)));
        assert_eq!(config.retry.max_retries, 3);
        assert_eq!(config.retry.base_delay, DEFAULT_INITIAL_BACKOFF);
        assert_eq!(config.retry.max_delay, Some(DEFAULT_MAX_BACKOFF));
    }

    #[test]
    fn ensure_default_only_fills_in_none() {
        assert_eq!(
            EmbeddingExecutionConfig::ensure_default(None),
            EmbeddingExecutionConfig::default()
        );
        let custom = EmbeddingExecutionConfig::disabled();
        assert_eq!(
            EmbeddingExecutionConfig::ensure_default(Some(custom.clone())),
            custom
        );
    }

    #[tokio::test]
    async fn timeout_is_reported_as_an_embedding_error() {
        let config = EmbeddingExecutionConfig::disabled().with_timeout(Duration::from_millis(1));
        let result: Result<u8> = config
            .apply("m", "p", "Embedding", || async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Ok(7)
            })
            .await;
        let err = result.unwrap_err();
        assert!(matches!(err, RagError::Embedding(_)), "err={err:?}");
        assert!(
            err.to_string().contains("Embedding request timeout after"),
            "err={err}"
        );
        assert!(err.to_string().contains("model=m") && err.to_string().contains("provider=p"));
    }

    #[tokio::test]
    async fn no_timeout_lets_the_operation_finish() {
        let config = EmbeddingExecutionConfig::disabled();
        let value = config
            .apply("m", "p", "Embedding", || async { Ok(9u8) })
            .await
            .unwrap();
        assert_eq!(value, 9);
    }

    #[tokio::test]
    async fn retries_non_retryable_errors_only_once() {
        let config = EmbeddingExecutionConfig::disabled();
        let calls = Mutex::new(0u32);
        let result: Result<u8> = config
            .apply("m", "p", "Embedding", || {
                let mut guard = calls.lock().unwrap();
                *guard += 1;
                let attempt = *guard;
                drop(guard);
                async move {
                    let _ = attempt;
                    Err(RagError::Api {
                        status_code: 400,
                        error_code: None,
                        message: "bad request".to_string(),
                    })
                }
            })
            .await;
        assert!(result.is_err());
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn retries_retryable_errors_until_success() {
        let config = EmbeddingExecutionConfig::disabled().with_retry(RetryPolicy::new(2));
        let calls = Mutex::new(0u32);
        let value = config
            .apply("m", "p", "Embedding", || {
                let mut guard = calls.lock().unwrap();
                *guard += 1;
                let attempt = *guard;
                drop(guard);
                async move {
                    if attempt < 3 {
                        Err(RagError::Http("boom".to_string()))
                    } else {
                        Ok(attempt)
                    }
                }
            })
            .await
            .unwrap();
        assert_eq!(value, 3);
        assert_eq!(*calls.lock().unwrap(), 3);
    }

    #[test]
    fn f32_slice_widens_to_f64() {
        assert_eq!(f32_slice_to_f64(&[0.5, -1.25]), vec![0.5, -1.25]);
        assert!(f32_slice_to_f64(&[]).is_empty());
    }

    #[test]
    fn join_url_normalizes_slashes() {
        assert_eq!(join_url("http://x/api", "/embed"), "http://x/api/embed");
        assert_eq!(join_url("http://x/api/", "/embed"), "http://x/api/embed");
        assert_eq!(join_url("http://x/api/", "embed"), "http://x/api/embed");
    }

    #[test]
    fn block_type_names_match_java_simple_class_names() {
        let text = ContentBlock::Text(TextBlock::new("hi"));
        let image = ContentBlock::Image(ImageBlock::new(Source::Url(URLSource::new("u"))));
        assert_eq!(block_type_name(&text), "TextBlock");
        assert_eq!(block_type_name(&image), "ImageBlock");
    }

    #[test]
    fn usage_listener_alias_is_object_safe() {
        let seen = Arc::new(Mutex::new(Vec::<u64>::new()));
        let captured = Arc::clone(&seen);
        let listener: UsageListener = Arc::new(move |usage: &EmbeddingUsage| {
            captured.lock().unwrap().push(usage.total_tokens);
        });
        listener(&EmbeddingUsage::new(2, 3));
        assert_eq!(*seen.lock().unwrap(), vec![5]);
    }
}
