//! Job loop for the Quartz backend, mirroring `AgentQuartzJob`.
//!
//! Spawns a tokio task that fires the scheduled task according to the
//! [`ScheduleMode`]:
//! * `NONE`        — never fires automatically (manual `trigger` only).
//! * `CRON`        — ticks every second and fires when the expression matches.
//! * `FIXED_RATE`  — fires every `fixed_rate` ms regardless of duration.
//! * `FIXED_DELAY` — fires, waits for completion, then sleeps `fixed_delay` ms.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::task::JoinHandle;

use crate::common::config::{ScheduleConfig, ScheduleMode};
use crate::common::scheduler::task::ScheduleAgentTask;
use crate::quartz::cron::CronExpression;
use crate::quartz::task::QuartzScheduleAgentTask;
use crate::quartz::JobContext;

/// Spawns the job loop for `task` and returns its handle.
pub(crate) fn spawn_job(
    ctx: JobContext,
    task: Arc<QuartzScheduleAgentTask>,
    schedule_config: ScheduleConfig,
) -> JoinHandle<()> {
    let _ = ctx;
    let initial_delay = schedule_config.initial_delay().unwrap_or(0);
    let mode = schedule_config.schedule_mode();

    // Parse cron up-front so a bad expression surfaces as a job that never fires
    // (schedule() already validated presence; parsing failure is logged).
    let cron = match mode {
        ScheduleMode::CRON => {
            match CronExpression::parse(schedule_config.cron_expression().unwrap_or("")) {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::error!(error = %e, "invalid cron expression; job will not fire");
                    None
                }
            }
        }
        _ => None,
    };

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
                let Some(cron) = cron else { return };
                loop {
                    if task.is_cancelled() {
                        return;
                    }
                    task.wait_if_paused().await;
                    if task.is_cancelled() {
                        return;
                    }
                    let now = Utc::now();
                    if cron.matches(now) {
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
