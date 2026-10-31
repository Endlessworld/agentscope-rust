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
//!
//! Missed firings follow Quartz's `SMART` misfire policy: a slot that elapsed
//! while the previous execution was still in flight (or while the task was
//! paused) fires once immediately and the cadence then resumes from the current
//! time, instead of replaying the whole backlog (`tokio`'s default `Burst`).
//! `FIXED_RATE` keeps the original grid and simply drops the missed ticks.

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

/// One planned cron firing.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CronFiring {
    /// The planned fire instant; also the cross-instance lock slot, so two nodes
    /// that share the schedule derive the same key for the same occurrence.
    pub(crate) at: DateTime<Utc>,
    /// The slot already elapsed (the previous execution overran it, or the task
    /// was paused across it) — Quartz calls this a misfire.
    pub(crate) missed: bool,
}

/// The next firing of a cron loop, computed from the slot the loop is working
/// towards (`cursor`) rather than from `now`.
///
/// Using the cursor is what makes a misfire visible: with a plain
/// `next_fire_time(now)` a slot that elapsed during a slow run is silently
/// dropped, while Quartz's `SMART` policy (`FIRE_ONCE_NOW`) fires it once late
/// and then resumes the cadence.
pub(crate) fn next_firing(
    plan: &CronPlan,
    cursor: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Option<CronFiring> {
    let at = plan.next_fire_time(cursor)?;
    Some(CronFiring {
        at,
        missed: at <= now,
    })
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
                // The slot the loop is working towards: every next firing is
                // computed from it (not from `now`), which is what makes a misfire
                // detectable (SCHED-03).
                let mut cursor = Utc::now();
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
                    let now = Utc::now();
                    let Some(firing) = next_firing(&plan, cursor, now) else {
                        // Quartz marks a trigger with no future fire time
                        // `COMPLETE`.
                        tracing::info!(
                            task = %task.name(),
                            "cron expression has no future fire time; completing task"
                        );
                        task.mark_completed();
                        return;
                    };
                    if firing.missed {
                        // Quartz `SMART` policy = `FIRE_ONCE_NOW`: the slot elapsed
                        // while the previous run was still in flight (or while the
                        // task was paused), so fire it right away instead of
                        // dropping it silently.
                        tracing::warn!(
                            task = %task.name(),
                            slot = %firing.at,
                            "missed cron firing: firing it once immediately"
                        );
                    } else {
                        let wait = (firing.at - now).to_std().unwrap_or(Duration::ZERO);
                        tokio::select! {
                            _ = tokio::time::sleep(wait) => {}
                            _ = task.cancelled() => return,
                        }
                        if task.is_cancelled() {
                            return;
                        }
                    }
                    // Quartz skips a firing whose trigger is paused at fire time;
                    // the cursor keeps the slot, so resuming catches it up once.
                    if task.is_paused() {
                        continue;
                    }
                    // The slot is the planned fire instant, so two nodes that share
                    // the schedule derive the same lock key for the same occurrence.
                    let slot = format!("cron:{}", firing.at.timestamp_millis());
                    // A CRON trigger keeps firing after an interrupt (only the
                    // FIXED_DELAY chain is rescheduled in Java).
                    let _ = fire(&ctx, &task, &slot).await;
                    // The next slot is computed from the one just fired; after a
                    // catch-up the timeline restarts from *now*, so a run that is
                    // always too slow cannot turn into an unbounded burst.
                    cursor = if firing.missed { Utc::now() } else { firing.at };
                }
            }
            ScheduleMode::FIXED_RATE => {
                let period = Duration::from_millis(schedule_config.fixed_rate().unwrap_or(1));
                let period_ms = period.as_millis().max(1) as i64;
                // The cadence grid is kept explicitly instead of with
                // `tokio::time::interval`: an `interval` whose deadline already
                // elapsed yields immediately (`Burst` and `Skip` alike), so a run
                // that outlasts the period makes the loop fire back to back with no
                // cadence at all. Quartz's `SMART` misfire policy instead drops the
                // occurrences that elapsed during the run and stays on the grid. The
                // grid starts when the task starts, i.e. at `now + initialDelay`
                // (slept above), so the first firing is not a period late either.
                let mut next = tokio::time::Instant::now();
                loop {
                    if task.is_cancelled() {
                        return;
                    }
                    task.wait_if_paused().await;
                    if task.is_cancelled() {
                        return;
                    }
                    // FIXED_RATE has no shared start instant across instances, so the
                    // slot is the wall-clock period bucket: occurrences that fall in
                    // the same period share one lock key.
                    let slot = format!("rate:{}", Utc::now().timestamp_millis() / period_ms);
                    let _ = fire(&ctx, &task, &slot).await;
                    // First grid point strictly after this run finished; the
                    // occurrences that elapsed meanwhile are dropped, not replayed.
                    next += period;
                    let now = tokio::time::Instant::now();
                    while next <= now {
                        next += period;
                    }
                    // Only the wait for the next slot is raced against cancel: an
                    // execution in flight is never dropped, so `shutdown(true)` /
                    // `deleteJob` (which never interrupt a running job in Java) can
                    // wait for it to finish.
                    tokio::select! {
                        _ = tokio::time::sleep_until(next) => {}
                        _ = task.cancelled() => return,
                    }
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
                    // FIXED_DELAY has no shared schedule, so the slot is the
                    // wall-clock instant of this firing.
                    let slot = format!("delay:{}", Utc::now().timestamp_millis());
                    if fire(&ctx, &task, &slot).await {
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
///
/// Two host seams run before the execution itself:
/// * [`crate::common::scheduler::SchedulerLock`] (SCHED-05) — a denied slot means
///   another instance owns this firing, so nothing runs here;
/// * the scheduler-wide semaphore (SCHED-09) — a firing waits for a permit, the
///   way a Quartz trigger waits for a worker thread.
async fn fire(ctx: &JobContext, task: &Arc<QuartzScheduleAgentTask>, slot: &str) -> bool {
    let lock_key = format!("{}:{}:{slot}", ctx.scheduler_id, task.name());
    if !ctx.lock.try_acquire(&lock_key).await {
        tracing::debug!(
            task = %task.name(),
            slot = %slot,
            "another instance owns this firing slot; skipping"
        );
        return false;
    }

    // A cancel while waiting for a permit must not leave the lock behind.
    let permit = tokio::select! {
        biased;
        _ = task.cancelled() => None,
        permit = ctx.semaphore.acquire() => permit.ok(),
    };
    let Some(permit) = permit else {
        ctx.lock.release(&lock_key).await;
        return true;
    };

    let interrupted = match task.execute().await {
        Ok(_) => false,
        Err(Error::Interrupted(_)) => true,
        Err(e) => {
            tracing::warn!(task = %task.name(), error = %e, "scheduled execution failed");
            false
        }
    };
    drop(permit);
    ctx.lock.release(&lock_key).await;
    interrupted
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
    fn a_slot_that_elapsed_during_a_run_is_reported_as_missed() {
        // Every second. The loop is working towards 10:00:01 but only looks again
        // at 10:00:03 (the previous run overran), so that slot is a misfire: Quartz
        // `SMART` fires it once now instead of silently dropping it.
        let plan = plan("* * * * * ?", 0);
        let cursor = Utc.with_ymd_and_hms(2026, 10, 31, 10, 0, 0).unwrap();
        let firing = next_firing(&plan, cursor, cursor).unwrap();
        assert_eq!(
            firing.at,
            Utc.with_ymd_and_hms(2026, 10, 31, 10, 0, 1).unwrap()
        );
        assert!(!firing.missed);

        let late = Utc.with_ymd_and_hms(2026, 10, 31, 10, 0, 3).unwrap();
        let firing = next_firing(&plan, cursor, late).unwrap();
        assert_eq!(
            firing.at,
            Utc.with_ymd_and_hms(2026, 10, 31, 10, 0, 1).unwrap()
        );
        assert!(firing.missed);

        // After a catch-up the loop restarts from *now*, so the next slot is a
        // future one and the whole backlog is never replayed.
        let resumed = next_firing(&plan, late, late).unwrap();
        assert_eq!(
            resumed.at,
            Utc.with_ymd_and_hms(2026, 10, 31, 10, 0, 4).unwrap()
        );
        assert!(!resumed.missed);
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
