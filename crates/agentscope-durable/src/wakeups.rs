//! Who tells a host that a thread has work, without the host polling.
//!
//! A wakeup carries a thread key and a reason, never turn state: the host that wakes up reads the
//! authoritative state itself. Publication is fail-open — a backend whose wakeup channel is down
//! still commits, and the host falls back to its interval scan.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::types::ThreadKey;
use crate::Result;

/// Why a wakeup was published.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WakeupReason {
    /// A turn was admitted.
    Submitted,
    /// A commit landed (a tool result, a status change, an input).
    Committed,
}

/// A thread that may have work, and why the publisher thought so.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Wakeup {
    pub thread: ThreadKey,
    pub at: i64,
    pub reason: WakeupReason,
}

/// Where a wakeup is delivered. A sink must not block: a host that needs to do work
/// asynchronously hands the wakeup to its own queue (for example a channel) and returns.
pub type WakeupSink = Arc<dyn Fn(&Wakeup) + Send + Sync>;

/// Identifies one subscription, for [`Wakeups::unsubscribe`].
pub type SubscriptionId = u64;

/// Publishes wakeups to the hosts that asked for them.
pub trait Wakeups: Send + Sync {
    fn publish(&self, wakeup: &Wakeup) -> Result<()>;
    fn subscribe(&self, sink: WakeupSink) -> SubscriptionId;
    fn unsubscribe(&self, id: SubscriptionId);
}

/// The in-process implementation: subscribers are called on the publishing thread.
#[derive(Default)]
pub struct LocalWakeups {
    next: AtomicU64,
    sinks: Mutex<Vec<(SubscriptionId, WakeupSink)>>,
}

impl LocalWakeups {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Wakeups for LocalWakeups {
    fn publish(&self, wakeup: &Wakeup) -> Result<()> {
        // Snapshot under the lock, deliver outside it: a sink is allowed to unsubscribe itself
        // (or subscribe another one) without deadlocking the publisher.
        let sinks: Vec<WakeupSink> = match self.sinks.lock() {
            Ok(sinks) => sinks.iter().map(|(_, sink)| Arc::clone(sink)).collect(),
            Err(_) => return Ok(()),
        };
        for sink in sinks {
            sink(wakeup);
        }
        Ok(())
    }

    fn subscribe(&self, sink: WakeupSink) -> SubscriptionId {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        if let Ok(mut sinks) = self.sinks.lock() {
            sinks.push((id, sink));
        }
        id
    }

    fn unsubscribe(&self, id: SubscriptionId) {
        if let Ok(mut sinks) = self.sinks.lock() {
            sinks.retain(|(held, _)| *held != id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::types::SessionKey;

    fn wakeup() -> Wakeup {
        Wakeup {
            thread: ThreadKey::new(SessionKey::new("ns", "s1").expect("key"), "main")
                .expect("thread"),
            at: 1,
            reason: WakeupReason::Committed,
        }
    }

    #[test]
    fn a_subscriber_receives_every_publication() {
        let wakeups = LocalWakeups::new();
        let seen = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&seen);
        let id = wakeups.subscribe(Arc::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
        }));
        wakeups.publish(&wakeup()).expect("publish");
        wakeups.publish(&wakeup()).expect("publish");
        assert_eq!(seen.load(Ordering::SeqCst), 2);
        wakeups.unsubscribe(id);
        wakeups.publish(&wakeup()).expect("publish");
        assert_eq!(seen.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn one_sink_can_leave_while_another_stays() {
        let wakeups = LocalWakeups::new();
        let first = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&first);
        let id = wakeups.subscribe(Arc::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
        }));
        let second = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&second);
        wakeups.subscribe(Arc::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
        }));
        wakeups.unsubscribe(id);
        wakeups.publish(&wakeup()).expect("publish");
        assert_eq!(first.load(Ordering::SeqCst), 0);
        assert_eq!(second.load(Ordering::SeqCst), 1);
    }
}
