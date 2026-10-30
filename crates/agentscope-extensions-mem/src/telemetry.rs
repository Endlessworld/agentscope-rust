//! Metrics and tracing spans for the MEM HTTP seam.
//!
//! Java only logs through SLF4J (`log.warn("Failed to record/retrieve ...")` in the
//! swallowing paths). The Rust port keeps that log parity through `tracing::warn!`
//! in the `*_lossy` adapters and adds per-request observability here: an
//! [`InstrumentedTransport`] decorator emits a `mem.http.post` span (url, status,
//! latency) and maintains process-local counters ([`MemMetrics`]).
//!
//! No external metrics crate is pulled in: the counters are plain atomics that a
//! host can scrape through [`MemMetrics::snapshot`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use tracing::Instrument;

use crate::error::Result;
use crate::http::HttpTransport;

/// Point-in-time snapshot of [`MemMetrics`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemMetricsSnapshot {
    /// Number of HTTP attempts made (a retried request counts per attempt).
    pub requests: u64,
    /// Number of failed attempts.
    pub failures: u64,
    /// Sum of per-attempt latencies, in milliseconds.
    pub latency_ms_total: u64,
}

/// In-process counters for memory HTTP traffic.
#[derive(Debug, Default)]
pub struct MemMetrics {
    requests: AtomicU64,
    failures: AtomicU64,
    latency_ms_total: AtomicU64,
}

impl MemMetrics {
    /// Creates a shared handle.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Reads the current counter values.
    pub fn snapshot(&self) -> MemMetricsSnapshot {
        MemMetricsSnapshot {
            requests: self.requests.load(Ordering::Relaxed),
            failures: self.failures.load(Ordering::Relaxed),
            latency_ms_total: self.latency_ms_total.load(Ordering::Relaxed),
        }
    }

    fn record(&self, latency_ms: u64, ok: bool) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        if !ok {
            self.failures.fetch_add(1, Ordering::Relaxed);
        }
        self.latency_ms_total
            .fetch_add(latency_ms, Ordering::Relaxed);
    }
}

/// [`HttpTransport`] decorator that records counters and emits one span per attempt.
pub struct InstrumentedTransport {
    inner: Arc<dyn HttpTransport>,
    metrics: Arc<MemMetrics>,
}

impl InstrumentedTransport {
    /// Wraps `inner`, recording into `metrics`.
    pub fn new(inner: Arc<dyn HttpTransport>, metrics: Arc<MemMetrics>) -> Self {
        Self { inner, metrics }
    }

    /// The metrics handle this decorator feeds.
    pub fn metrics(&self) -> &Arc<MemMetrics> {
        &self.metrics
    }
}

#[async_trait]
impl HttpTransport for InstrumentedTransport {
    async fn post_json(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<String> {
        let span = tracing::debug_span!(
            "mem.http.post",
            url = url,
            status = tracing::field::Empty,
            latency_ms = tracing::field::Empty,
        );
        let started = Instant::now();
        let result = self
            .inner
            .post_json(url, headers, body)
            .instrument(span.clone())
            .await;
        let latency_ms = started.elapsed().as_millis() as u64;
        span.record("latency_ms", latency_ms);
        span.record("status", if result.is_ok() { "ok" } else { "error" });
        self.metrics.record(latency_ms, result.is_ok());
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::MemError;
    use crate::http::mock::MockTransport;

    fn instrumented(mock: &MockTransport) -> (InstrumentedTransport, Arc<MemMetrics>) {
        let inner: Arc<dyn HttpTransport> = Arc::new(mock.clone());
        let metrics = MemMetrics::shared();
        (
            InstrumentedTransport::new(inner, Arc::clone(&metrics)),
            metrics,
        )
    }

    #[tokio::test]
    async fn records_successful_requests() {
        let mock = MockTransport::new();
        mock.push_body("{}");
        mock.push_body("{}");
        let (transport, metrics) = instrumented(&mock);
        transport.post_json("http://x/a", &[], "{}").await.unwrap();
        transport.post_json("http://x/b", &[], "{}").await.unwrap();
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.requests, 2);
        assert_eq!(snapshot.failures, 0);
        assert!(snapshot.latency_ms_total < 60_000);
    }

    #[tokio::test]
    async fn records_failed_attempts_and_returns_the_error_unchanged() {
        let mock = MockTransport::new();
        mock.push_response(Err(MemError::HttpStatus {
            status: 500,
            body: "boom".into(),
        }));
        let (transport, metrics) = instrumented(&mock);
        let err = transport.post_json("http://x/a", &[], "{}").await.unwrap_err();
        assert!(matches!(err, MemError::HttpStatus { status: 500, .. }));
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.requests, 1);
        assert_eq!(snapshot.failures, 1);
    }

    #[tokio::test]
    async fn metrics_handle_is_shared_with_the_decorator() {
        let mock = MockTransport::new();
        mock.push_body("ok");
        let (transport, metrics) = instrumented(&mock);
        assert!(Arc::ptr_eq(transport.metrics(), &metrics));
        assert_eq!(metrics.snapshot(), MemMetricsSnapshot::default());
    }
}
