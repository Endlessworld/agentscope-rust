//! Per-peer sliding-window throttle protecting the bot from runaway interaction loops.
//!
//! Rust port of `io.agentscope.extensions.channel.common.BotLoopGuard`. Defaults follow OpenClaw:
//! 20 events / 60 s window, then a 60 s cooldown during which [`BotLoopGuard::allow`] is `false`.

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
}

/// Per-peer sliding-window throttle.
#[derive(Debug)]
pub struct BotLoopGuard {
    config: BotLoopGuardConfig,
    states: Mutex<HashMap<String, PeerState>>,
}

impl Default for BotLoopGuard {
    fn default() -> Self {
        Self {
            config: BotLoopGuardConfig::default(),
            states: Mutex::new(HashMap::new()),
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
            states: Mutex::new(HashMap::new()),
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
        let mut states = self.states.lock().expect("bot loop guard mutex poisoned");
        let state = states.entry(peer_key.to_string()).or_default();

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
            return false;
        }
        state.events.push_back(now_ms);
        true
    }

    /// Whether `peer_key` is currently serving a cooldown.
    pub fn is_cooling_down(&self, peer_key: &str) -> bool {
        self.is_cooling_down_at(peer_key, now_ms())
    }

    /// [`BotLoopGuard::is_cooling_down`] with an injected clock.
    pub fn is_cooling_down_at(&self, peer_key: &str, now_ms: i64) -> bool {
        let states = self.states.lock().expect("bot loop guard mutex poisoned");
        states
            .get(peer_key)
            .map(|s| s.cooldown_until_ms > now_ms)
            .unwrap_or(false)
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
}
