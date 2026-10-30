//! Job loop for the Quartz backend, mirroring `AgentQuartzJob`.
//!
//! Spawns a tokio task that fires the scheduled task according to the
//! [`ScheduleMode`]:
//! * `NONE` — never fires automatically (manual `trigger` only).
//! * `CRON` — sleeps until the next fire time computed by
//!   [`CronExpression::next_after`] on the wall clock of the configured time
//!   zone, then fires.
//! * `FIXED_RATE` — fires every `fixed_rate` ms regardless of duration.
//! * `FIXED_DELAY` — fires, waits for completion, then sleeps `fixed_delay` ms.
//!
//! Interrupting an execution (`InterruptableJob#interrupt` in Java) aborts the
//! in-flight run; for `FIXED_DELAY` it also stops the reschedule chain, matching
//! `AgentQuartzJob`.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, FixedOffset, Utc};
use tokio::task::JoinHandle;

use agentscope_core::error::Error;

use crate::common::config::{ScheduleConfig, ScheduleMode};
use crate::common::scheduler::task::ScheduleAgentTask;
use crate::quartz::cron::CronExpression;
use crate::quartz::task::QuartzScheduleAgentTask;
use crate::quartz::JobContext;

/// A validated cron schedule: the parsed expression plus the fixed offset it is
/// evaluated in (from `ScheduleConfig::zone_id`, UTC when unset).
///
/// Both parts are resolved by
/// `QuartzAgentScheduler::schedule_runtime` *before* the task is registered, so
/// an invalid expression or zone id fails the `schedule*` call instead of
/// leaving behind a task that silently never fires.
#[derive(Debug, Clone)]
pub(crate) struct CronPlan {
    pub(crate) expression: CronExpression,
    pub(crate) offset: FixedOffset,
}

impl CronPlan {
    /// Whether the expression matches the wall clock in its zone at `now`.
    ///
    /// Only used by the unit tests: the job loop sleeps until
    /// [`next_fire_time`](Self::next_fire_time) instead of polling, so a late wake
    /// up still fires instead of silently dropping the occurrence.
    #[cfg(test)]
    pub(crate) fn matches_at(&self, now: DateTime<Utc>) -> bool {
        self.expression.matches(now.with_timezone(&self.offset))
    }

    /// The next fire time strictly after `now`, as an absolute instant.
    pub(crate) fn next_fire_time(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.expression
            .next_after(now.with_timezone(&self.offset))
            .map(|next| next.with_timezone(&Utc))
    }
}

/// Spawns the job loop for `task` and returns its handle.
pub(crate) fn spawn_job(
    ctx: JobContext,
    task: Arc<QuartzScheduleAgentTask>,
    schedule_config: ScheduleConfig,
) -> JoinHandle<()> {
    let initial_delay = schedule_config.initial_delay().unwrap_or(0);
    let mode = schedule_config.schedule_mode();
    tracing::debug!(
        scheduler_id = %ctx.scheduler_id,
        task = %task.name(),
        mode = mode.as_str(),
        "job loop started"
    );

    tokio::spawn(async move {
        if initial_delay > 0 {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(initial_delay)) => {}
                _ = task.cancelled() => return,
            }
        }

        match mode {
            ScheduleMode::NONE => {
                // No automatic firing; keep the task alive until cancelled.
                task.cancelled().await;
            }
            ScheduleMode::CRON => {
                // Only reachable with a plan: `schedule_runtime` parses the cron
                // expression before registering the task.
                let Some(plan) = task.cron_plan().cloned() else {
                    tracing::error!("CRON schedule without a parsed expression");
                    task.cancelled().await;
                    return;
                };
                loop {
                    if task.is_cancelled() {
                        return;
                    }
                    task.wait_if_paused().await;
                    if task.is_cancelled() {
                        return;
                    }
                    // Sleep exactly until the next firing instead of polling every
                    // second, so a slow execution or a scheduler hiccup cannot
                    // silently skip a whole minute (`next_after` is now on the
                    // production path, not only in tests).
                    let Some(next) = plan.next_fire_time(Utc::now()) else {
                        // Quartz marks a trigger with no future fire time
                        // `COMPLETE`.
                        tracing::info!(
                            task = %task.name(),
                            "cron expression has no future fire time; completing task"
                        );
                        task.mark_completed();
                        return;
                    };
                    let wait = (next - Utc::now()).to_std().unwrap_or(Duration::ZERO);
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {}
                        _ = task.cancelled() => return,
                    }
                    if task.is_cancelled() {
                        return;
                    }
                    // Quartz skips a firing whose trigger is paused at fire time.
                    if task.is_paused() {
                        continue;
                    }
                    // A CRON trigger keeps firing after an interrupt (only the
                    // FIXED_DELAY chain is rescheduled in Java).
                    let _ = fire(&task).await;
                }
            }
            ScheduleMode::FIXED_RATE => {
                let period = Duration::from_millis(schedule_config.fixed_rate().unwrap_or(1));
                let mut ticker = tokio::time::interval(period);
                // First tick fires immediately; consume it so we honour the
                // "start after initial delay" semantics.
                ticker.tick().await;
                loop {
                    // Only the wait for the next tick is raced against cancel: an
                    // execution in flight is never dropped, so `shutdown(true)` /
                    // `deleteJob` (which never interrupt a running job in Java) can
                    // wait for it to finish.
                    tokio::select! {
                        _ = ticker.tick() => {}
                        _ = task.cancelled() => return,
                    }
                    if task.is_cancelled() {
                        return;
                    }
                    task.wait_if_paused().await;
                    if task.is_cancelled() {
                        return;
                    }
                    let _ = fire(&task).await;
                }
            }
            ScheduleMode::FIXED_DELAY => {
                let delay = Duration::from_millis(schedule_config.fixed_delay().unwrap_or(1));
                loop {
                    if task.is_cancelled() {
                        return;
                    }
                    task.wait_if_paused().await;
                    if task.is_cancelled() {
                        return;
                    }
                    // `AgentQuartzJob` skips `rescheduleNextFixedDelay` when the
                    // job was interrupted during the run, which stops the chain.
                    if fire(&task).await {
                        tracing::info!(
                            task = %task.name(),
                            "fixed-delay task interrupted; skipping reschedule"
                        );
                        return;
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = task.cancelled() => return,
                    }
                }
            }
        }
    })
}

/// Fires one execution; returns `true` when the execution was interrupted.
async fn fire(task: &Arc<QuartzScheduleAgentTask>) -> bool {
    match task.execute().await {
        Ok(_) => false,
        Err(Error::Interrupted(_)) => true,
        Err(e) => {
            tracing::warn!(task = %task.name(), error = %e, "scheduled execution failed");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn plan(expr: &str, offset_hours: i32) -> CronPlan {
        CronPlan {
            expression: CronExpression::parse(expr).unwrap(),
            offset: FixedOffset::east_opt(offset_hours * 3600).unwrap(),
        }
    }

    #[test]
    fn cron_matches_in_configured_zone() {
        // 08:00 Asia/Shanghai == 00:00 UTC.
        let plan = plan("0 0 8 * * ?", 8);
        assert!(plan.matches_at(Utc.with_ymd_and_hms(2026, 9, 18, 0, 0, 0).unwrap()));
        // 08:00 UTC is 16:00 in UTC+8: the pre-fix implementation fired here and
        // missed the real 08:00 local slot.
        assert!(!plan.matches_at(Utc.with_ymd_and_hms(2026, 9, 18, 8, 0, 0).unwrap()));
    }

    #[test]
    fn cron_matches_in_utc_by_default() {
        let plan = plan("0 0 8 * * ?", 0);
        assert!(plan.matches_at(Utc.with_ymd_and_hms(2026, 9, 18, 8, 0, 0).unwrap()));
        assert!(!plan.matches_at(Utc.with_ymd_and_hms(2026, 9, 18, 0, 0, 0).unwrap()));
    }

    #[test]
    fn negative_offset_shifts_weekday_boundary() {
        // 0 0 9 ? * 1 == Sunday 09:00 local. At UTC-5 that is Sunday 14:00 UTC.
        let plan = plan("0 0 9 ? * 1", -5);
        assert!(plan.matches_at(Utc.with_ymd_and_hms(2026, 9, 20, 14, 0, 0).unwrap()));
        assert!(!plan.matches_at(Utc.with_ymd_and_hms(2026, 9, 20, 9, 0, 0).unwrap()));
    }

    #[test]
    fn next_fire_time_honours_the_configured_zone() {
        // 08:00 Asia/Shanghai == 00:00 UTC, so the next fire after 2026-09-18
        // 01:00 UTC is 2026-09-19 00:00 UTC.
        let plan = plan("0 0 8 * * ?", 8);
        let next = plan
            .next_fire_time(Utc.with_ymd_and_hms(2026, 9, 18, 1, 0, 0).unwrap())
            .unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 9, 19, 0, 0, 0).unwrap());
    }

    #[test]
    fn next_fire_time_returns_none_when_years_are_exhausted() {
        let plan = plan("0 0 0 1 1 ? 2027", 0);
        assert_eq!(
            plan.next_fire_time(Utc.with_ymd_and_hms(2026, 10, 28, 0, 0, 0).unwrap()),
            Some(Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap())
        );
        assert_eq!(
            plan.next_fire_time(Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap()),
            None
        );
    }
}
