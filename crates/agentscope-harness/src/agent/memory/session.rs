//! Session freshness evaluation, mirroring
//! `io.agentscope.harness.agent.memory.SessionFreshnessEvaluator`.
//!
//! The retired `SessionEntry` / `SessionTree` JSONL pair was deleted together with the rest of
//! the old session design: committed history now lives in the native journal
//! (`agentscope_core::session`). Only the staleness policy survives, because it decides when a
//! conversation should be reset rather than how history is stored.

use chrono::{DateTime, Duration as ChronoDuration, Local, NaiveTime};

/// Evaluates whether a session is stale and should be reset.
///
/// Supports two policies: daily reset after a configured local hour, and an idle timeout.
#[derive(Debug, Clone)]
pub struct SessionFreshnessEvaluator {
    daily_reset_hour: i32,
    idle_timeout: Option<ChronoDuration>,
}

impl SessionFreshnessEvaluator {
    /// Creates an evaluator with a daily reset hour and optional idle timeout.
    pub fn new(daily_reset_hour: i32, idle_timeout: Option<ChronoDuration>) -> Self {
        Self {
            daily_reset_hour,
            idle_timeout,
        }
    }

    /// Defaults: daily reset at 4 AM, idle timeout of 2 hours.
    pub fn defaults() -> Self {
        Self::new(4, Some(ChronoDuration::hours(2)))
    }

    pub fn daily_reset_hour(&self) -> i32 {
        self.daily_reset_hour
    }

    pub fn idle_timeout(&self) -> Option<ChronoDuration> {
        self.idle_timeout
    }

    /// Returns true when the session should be considered stale and reset.
    pub fn is_stale(&self, last_activity: Option<DateTime<Local>>) -> bool {
        let Some(last) = last_activity else {
            return true;
        };
        let now = Local::now();

        if let Some(timeout) = self.idle_timeout {
            if now - last > timeout {
                return true;
            }
        }

        if self.daily_reset_hour >= 0 {
            let reset_time = NaiveTime::from_hms_opt(self.daily_reset_hour as u32, 0, 0)
                .expect("valid reset hour");
            let today_reset = now.date_naive().and_time(reset_time);
            if now.naive_local() > today_reset && last.naive_local() < today_reset {
                return true;
            }
        }

        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_previous_activity_is_stale() {
        assert!(SessionFreshnessEvaluator::defaults().is_stale(None));
    }

    #[test]
    fn recent_activity_is_fresh() {
        let evaluator = SessionFreshnessEvaluator::new(-1, Some(ChronoDuration::hours(2)));
        assert!(!evaluator.is_stale(Some(Local::now())));
    }

    #[test]
    fn idle_past_the_timeout_is_stale() {
        let evaluator = SessionFreshnessEvaluator::new(-1, Some(ChronoDuration::hours(2)));
        assert!(evaluator.is_stale(Some(Local::now() - ChronoDuration::hours(3))));
    }
}
