//! Per-peer sliding-window throttle protecting the bot from runaway interaction loops.
//!
//! Rust port of `io.agentscope.extensions.channel.common.BotLoopGuard`. Defaults follow OpenClaw:
//! 20 events / 60 s window, then a 60 s cooldown during which [`BotLoopGuard::allow`] is `false`.
//! Tracked peers are reclaimed opportunistically: the first [`BotLoopGuard::allow`] call after each
//! `window + cooldown` period sweeps peers that recorded no event for longer than that, so the map
//! does not grow with the number of peers ever seen ([`BotLoopGuard::evict_idle_peers`] offers the
//! same sweep explicitly).

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use super::error::{ChannelError, Result};
use super::token::now_ms;

/// Tunables for [`BotLoopGuard`]. All bounds must be strictly positive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BotLoopGuardConfig {
    /// Maximum number of events allowed inside one window before the peer is cooled down.
    pub max_events_per_window: usize,
    /// Sliding-window length in milliseconds.
    pub window_ms: i64,
    /// Cooldown length in milliseconds once the window budget is exceeded.
    pub cooldown_ms: i64,
}

impl Default for BotLoopGuardConfig {
    fn default() -> Self {
        Self {
            max_events_per_window: 20,
            window_ms: 60_000,
            cooldown_ms: 60_000,
        }
    }
}

#[derive(Debug, Default)]
struct PeerState {
    events: VecDeque<i64>,
    cooldown_until_ms: i64,
    /// Wall-clock stamp of the last recorded event (creation time before that), used by the
    /// opportunistic idle sweep so a freshly created entry can never be mistaken for idle.
    last_event_ms: i64,
}

#[derive(Debug, Default)]
struct Inner {
    states: HashMap<String, PeerState>,
    last_sweep_ms: i64,
}

/// Per-peer sliding-window throttle.
#[derive(Debug)]
pub struct BotLoopGuard {
    config: BotLoopGuardConfig,
    inner: Mutex<Inner>,
}

impl Default for BotLoopGuard {
    fn default() -> Self {
        Self {
            config: BotLoopGuardConfig::default(),
            inner: Mutex::new(Inner::default()),
        }
    }
}

impl BotLoopGuard {
    /// Guard with the OpenClaw defaults (20 events / 60 s / 60 s).
    pub fn with_defaults() -> Self {
        Self::default()
    }

    /// Guard with explicit bounds. Mirrors the Java constructor's positivity validation.
    pub fn new(max_events_per_window: usize, window_ms: i64, cooldown_ms: i64) -> Result<Self> {
        if max_events_per_window == 0 || window_ms <= 0 || cooldown_ms <= 0 {
            return Err(ChannelError::Config(
                "bot loop guard bounds must all be positive".to_string(),
            ));
        }
        Ok(Self {
            config: BotLoopGuardConfig {
                max_events_per_window,
                window_ms,
                cooldown_ms,
            },
            inner: Mutex::new(Inner::default()),
        })
    }

    /// Record an event for `peer_key` and report whether the caller may proceed.
    pub fn allow(&self, peer_key: &str) -> bool {
        self.allow_at(peer_key, now_ms())
    }

    /// [`BotLoopGuard::allow`] with an injected clock (used by offline tests).
    pub fn allow_at(&self, peer_key: &str, now_ms: i64) -> bool {
        if peer_key.trim().is_empty() {
            return true;
        }
        let mut inner = self.inner.lock().expect("bot loop guard mutex poisoned");
        let idle_threshold = self.config.window_ms + self.config.cooldown_ms;
        inner.maybe_sweep(now_ms, idle_threshold);
        let state = inner
            .states
            .entry(peer_key.to_string())
            .or_insert_with(|| PeerState {
                last_event_ms: now_ms,
                ..Default::default()
            });

        if state.cooldown_until_ms > now_ms {
            return false;
        }
        // Drop events that fell out of the window.
        while let Some(front) = state.events.front().copied() {
            if now_ms - front > self.config.window_ms {
                state.events.pop_front();
            } else {
                break;
            }
        }
        if state.events.len() >= self.config.max_events_per_window {
            state.cooldown_until_ms = now_ms + self.config.cooldown_ms;
            state.events.clear();
            state.last_event_ms = now_ms;
            return false;
        }
        state.events.push_back(now_ms);
        state.last_event_ms = now_ms;
        true
    }

    /// Whether `peer_key` is currently serving a cooldown.
    pub fn is_cooling_down(&self, peer_key: &str) -> bool {
        self.is_cooling_down_at(peer_key, now_ms())
    }

    /// [`BotLoopGuard::is_cooling_down`] with an injected clock.
    pub fn is_cooling_down_at(&self, peer_key: &str, now_ms: i64) -> bool {
        let inner = self.inner.lock().expect("bot loop guard mutex poisoned");
        inner
            .states
            .get(peer_key)
            .map(|s| s.cooldown_until_ms > now_ms)
            .unwrap_or(false)
    }

    /// Number of currently tracked peers (observability / tests).
    pub fn tracked_peers(&self) -> usize {
        self.inner
            .lock()
            .expect("bot loop guard mutex poisoned")
            .states
            .len()
    }

    /// Drop peers that recorded no event for longer than `window + cooldown`; returns the number
    /// removed. By then the sliding window is empty and any cooldown has expired, so the entry
    /// carries no information and the peer restarts with a fresh window on its next event.
    pub fn evict_idle_peers(&self) -> usize {
        self.evict_idle_peers_at(now_ms())
    }

    /// [`BotLoopGuard::evict_idle_peers`] with an injected clock.
    pub fn evict_idle_peers_at(&self, now_ms: i64) -> usize {
        let mut inner = self.inner.lock().expect("bot loop guard mutex poisoned");
        inner.evict_idle(now_ms, self.config.window_ms + self.config.cooldown_ms)
    }
}

impl Inner {
    /// Opportunistic sweep piggybacked on `allow_at`: the first call after each
    /// `window + cooldown` period evicts idle peers. No background thread is started.
    fn maybe_sweep(&mut self, now_ms: i64, period_ms: i64) {
        if now_ms - self.last_sweep_ms >= period_ms {
            self.last_sweep_ms = now_ms;
            self.evict_idle(now_ms, period_ms);
        }
    }

    fn evict_idle(&mut self, now_ms: i64, idle_threshold_ms: i64) -> usize {
        let before = self.states.len();
        self.states
            .retain(|_, state| now_ms - state.last_event_ms <= idle_threshold_ms);
        before - self.states.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_bounds_are_rejected() {
        assert!(BotLoopGuard::new(0, 1_000, 1_000).is_err());
        assert!(BotLoopGuard::new(1, 0, 1_000).is_err());
        assert!(BotLoopGuard::new(1, 1_000, 0).is_err());
        assert!(BotLoopGuard::new(1, 1_000, 1_000).is_ok());
    }

    #[test]
    fn blank_peer_is_always_allowed_and_not_tracked() {
        let guard = BotLoopGuard::new(1, 1_000, 1_000).unwrap();
        for _ in 0..5 {
            assert!(guard.allow_at("  ", 0));
        }
        assert!(!guard.is_cooling_down_at("  ", 0));
    }

    #[test]
    fn exceeding_window_budget_triggers_cooldown() {
        let guard = BotLoopGuard::new(3, 1_000, 5_000).unwrap();
        assert!(guard.allow_at("peer", 0));
        assert!(guard.allow_at("peer", 1));
        assert!(guard.allow_at("peer", 2));
        // Fourth event exceeds the budget -> cooldown, request rejected.
        assert!(!guard.allow_at("peer", 3));
        assert!(guard.is_cooling_down_at("peer", 3));
        // Still cooling down well inside the cooldown window.
        assert!(!guard.allow_at("peer", 4_000));
        // Cooldown elapsed -> allowed again.
        assert!(guard.allow_at("peer", 6_000));
        assert!(!guard.is_cooling_down_at("peer", 6_000));
    }

    #[test]
    fn events_outside_window_are_evicted() {
        let guard = BotLoopGuard::new(2, 1_000, 10_000).unwrap();
        assert!(guard.allow_at("peer", 0));
        assert!(guard.allow_at("peer", 500));
        // The first two events are outside the 1s window now, so the budget resets.
        assert!(guard.allow_at("peer", 2_000));
        assert!(guard.allow_at("peer", 2_100));
        assert!(!guard.allow_at("peer", 2_200));
    }

    #[test]
    fn peers_are_tracked_independently() {
        let guard = BotLoopGuard::new(1, 10_000, 10_000).unwrap();
        assert!(guard.allow_at("a", 0));
        assert!(!guard.allow_at("a", 1));
        assert!(guard.allow_at("b", 1));
    }

    #[test]
    fn idle_peers_are_evicted_and_counted() {
        let guard = BotLoopGuard::new(5, 1_000, 2_000).unwrap();
        assert!(guard.allow_at("a", 0));
        assert!(guard.allow_at("b", 500));
        assert_eq!(guard.tracked_peers(), 2);
        // Idle threshold is window + cooldown = 3s: nothing is idle yet.
        assert_eq!(guard.evict_idle_peers_at(3_000), 0);
        assert_eq!(guard.tracked_peers(), 2);
        // "a" (last event at 0) is idle at 3_001, "b" (500) is not.
        assert_eq!(guard.evict_idle_peers_at(3_001), 1);
        assert_eq!(guard.tracked_peers(), 1);
        // An evicted peer restarts with a fresh window on its next event.
        assert!(guard.allow_at("a", 3_002));
        assert_eq!(guard.tracked_peers(), 2);
    }

    #[test]
    fn allow_sweeps_idle_peers_opportunistically() {
        let guard = BotLoopGuard::new(5, 1_000, 2_000).unwrap();
        assert!(guard.allow_at("a", 0));
        // Inside the first sweep period: the idle peer is retained.
        assert!(guard.allow_at("b", 2_000));
        assert_eq!(guard.tracked_peers(), 2);
        // The first call after `window + cooldown` sweeps "a" (idle since 0).
        assert!(guard.allow_at("c", 3_001));
        assert_eq!(guard.tracked_peers(), 2);
        assert!(!guard.is_cooling_down_at("a", 3_001));
    }
}
