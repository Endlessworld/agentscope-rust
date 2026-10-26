//! Job loop for the Quartz backend, mirroring `AgentQuartzJob`.
//!
//! Spawns a tokio task that fires the scheduled task according to the
//! [`ScheduleMode`]:
//! * `NONE`        — never fires automatically (manual `trigger` only).
//! * `CRON`        — ticks every second and fires when the expression matches
//!                   the wall clock of the configured time zone.
//! * `FIXED_RATE`  — fires every `fixed_rate` ms regardless of duration.
//! * `FIXED_DELAY` — fires, waits for completion, then sleeps `fixed_delay` ms.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, FixedOffset, Utc};
use tokio::task::JoinHandle;

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
    pub(crate) fn matches_at(&self, now: DateTime<Utc>) -> bool {
        self.expression.matches(now.with_timezone(&self.offset))
    }
}

/// Spawns the job loop for `task` and returns its handle.
pub(crate) fn spawn_job(
    ctx: JobContext,
    task: Arc<QuartzScheduleAgentTask>,
    schedule_config: ScheduleConfig,
    cron: Option<CronPlan>,
) -> JoinHandle<()> {
    let _ = ctx;
    let initial_delay = schedule_config.initial_delay().unwrap_or(0);
    let mode = schedule_config.schedule_mode();

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
                let Some(plan) = cron else {
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
                    if plan.matches_at(Utc::now()) {
                        let _ = fire(&task).await;
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                        _ = task.cancelled() => return,
                    }
                }
            }
            ScheduleMode::FIXED_RATE => {
                let period = Duration::from_millis(schedule_config.fixed_rate().unwrap_or(1));
                let mut ticker = tokio::time::interval(period);
                // First tick fires immediately; consume it so we honour the
                // "start after initial delay" semantics.
                ticker.tick().await;
                loop {
                    tokio::select! {
                        _ = ticker.tick() => {
                            if task.is_cancelled() {
                                return;
                            }
                            task.wait_if_paused().await;
                            if task.is_cancelled() {
                                return;
                            }
                            let _ = fire(&task).await;
                        }
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
                    let _ = fire(&task).await;
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = task.cancelled() => return,
                    }
                }
            }
        }
    })
}

async fn fire(task: &Arc<QuartzScheduleAgentTask>) {
    if let Err(e) = task.execute().await {
        tracing::warn!(task = %task.name(), error = %e, "scheduled execution failed");
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
}
