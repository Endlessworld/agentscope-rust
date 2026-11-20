//! A resident host loop: scan on an interval, and again when a wakeup says a thread may have
//! work.
//!
//! The runtime already owns the state machine; this module owns only the cadence. A scan that
//! fails is logged and retried on the next interval, because one unclaimable thread must not stop
//! the host from serving the rest.

use std::sync::Arc;
use std::time::Duration;

use agentscope_durable::types::HarnessSelector;
use agentscope_durable::{DurableRuntime, Result, TickOutcome, WakeupSink};
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

/// The cadence and width of a resident scan.
#[derive(Debug, Clone, Copy)]
pub struct ResidentOptions {
    /// How often a scan runs even when no wakeup arrives.
    pub interval: Duration,
    /// How many ready threads one scan ticks at a time.
    pub parallelism: usize,
    /// How many ready threads one scan asks the backend for.
    pub limit: usize,
}

impl Default for ResidentOptions {
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(500),
            parallelism: 1,
            limit: 32,
        }
    }
}

/// Scans on an interval and on every wakeup until the host shuts it down.
pub struct ResidentRunner {
    runtime: DurableRuntime,
    selectors: Vec<HarnessSelector>,
    options: ResidentOptions,
}

impl ResidentRunner {
    pub fn new(
        runtime: DurableRuntime,
        selectors: Vec<HarnessSelector>,
        options: ResidentOptions,
    ) -> Self {
        Self {
            runtime,
            selectors,
            options,
        }
    }

    /// One scan, with the configured width and limit.
    pub async fn step(&self) -> Result<Vec<TickOutcome>> {
        self.runtime
            .scan_with(
                &self.selectors,
                self.options.limit,
                self.options.parallelism,
            )
            .await
    }

    /// Runs until `shutdown` fires.
    ///
    /// A wakeup only shortens the wait: the interval scan still runs, so a host whose backend
    /// has no wakeup channel behaves exactly like a plain polling loop.
    pub async fn run_until(self, shutdown: oneshot::Receiver<()>) {
        let mut ticker = tokio::time::interval(self.options.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let (wake_tx, mut wake_rx) = mpsc::unbounded_channel();
        let subscription = self.runtime.backend().wakeups().map(|wakeups| {
            let sink: WakeupSink = Arc::new(move |wakeup| {
                // The sink is called on the publisher's thread and must not block; a channel
                // send never does. A closed receiver only means this host is shutting down.
                let _ = wake_tx.send(wakeup.thread.clone());
            });
            let id = wakeups.subscribe(sink);
            (wakeups, id)
        });
        let mut shutdown = shutdown;
        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                _ = ticker.tick() => self.scan_once().await,
                Some(_thread) = wake_rx.recv() => self.scan_once().await,
            }
        }
        if let Some((wakeups, id)) = subscription {
            wakeups.unsubscribe(id);
        }
    }

    async fn scan_once(&self) {
        if let Err(error) = self.step().await {
            warn!(error = %error, "durable resident scan failed");
        }
    }
}

/// Runs one scan: the edge host calls it once per wakeup and exits.
pub struct EdgeRunner {
    runtime: DurableRuntime,
    selectors: Vec<HarnessSelector>,
    options: ResidentOptions,
}

impl EdgeRunner {
    pub fn new(
        runtime: DurableRuntime,
        selectors: Vec<HarnessSelector>,
        options: ResidentOptions,
    ) -> Self {
        Self {
            runtime,
            selectors,
            options,
        }
    }

    pub async fn run_once(&self) -> Result<Vec<TickOutcome>> {
        self.runtime
            .scan_with(
                &self.selectors,
                self.options.limit,
                self.options.parallelism,
            )
            .await
    }
}
