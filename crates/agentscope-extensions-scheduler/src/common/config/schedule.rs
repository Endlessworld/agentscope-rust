//! `ScheduleMode` / `ScheduleConfig`, mirroring
//! `io.agentscope.extensions.scheduler.config.ScheduleMode` and `ScheduleConfig`.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Timing strategy used to schedule agent execution.
///
/// Mirrors the Java `ScheduleMode` enum. Serialized with the same variant names
/// so persisted configs remain interchangeable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[allow(non_camel_case_types)]
pub enum ScheduleMode {
    /// No automatic scheduling (default); manual execution only.
    #[default]
    NONE,
    /// Cron-expression based scheduling.
    CRON,
    /// Fixed-rate scheduling (constant interval between execution starts).
    FIXED_RATE,
    /// Fixed-delay scheduling (constant delay between completion and next start).
    FIXED_DELAY,
}

impl ScheduleMode {
    /// Java-compatible wire name (`NONE`/`CRON`/...).
    pub fn as_str(&self) -> &'static str {
        match self {
            ScheduleMode::NONE => "NONE",
            ScheduleMode::CRON => "CRON",
            ScheduleMode::FIXED_RATE => "FIXED_RATE",
            ScheduleMode::FIXED_DELAY => "FIXED_DELAY",
        }
    }
}

/// Validation error raised while building a [`ScheduleConfig`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ScheduleConfigError {
    /// Schedule mode was not provided (never happens due to `Default`, kept for parity).
    #[error("Schedule mode must not be null")]
    MissingMode,
    /// `CRON` mode requires a non-empty expression.
    #[error("Cron expression must not be null or empty for CRON mode")]
    EmptyCronExpression,
    /// `FIXED_RATE` mode requires a positive value.
    #[error("Fixed rate must be a positive value for FIXED_RATE mode")]
    InvalidFixedRate,
    /// `FIXED_DELAY` mode requires a positive value.
    #[error("Fixed delay must be a positive value for FIXED_DELAY mode")]
    InvalidFixedDelay,
}

/// Scheduling parameters for a scheduled agent task.
///
/// Durations are expressed in **milliseconds**, matching the Java `Long` fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleConfig {
    /// The schedule mode.
    pub schedule_mode: ScheduleMode,
    /// Cron expression (only meaningful for `CRON`).
    pub cron_expression: Option<String>,
    /// Fixed rate in milliseconds (only meaningful for `FIXED_RATE`).
    pub fixed_rate: Option<u64>,
    /// Fixed delay in milliseconds (only meaningful for `FIXED_DELAY`).
    pub fixed_delay: Option<u64>,
    /// Initial delay in milliseconds before the first execution.
    pub initial_delay: Option<u64>,
    /// Time zone id used to evaluate the cron expression (e.g. `"Asia/Shanghai"`).
    pub zone_id: Option<String>,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            schedule_mode: ScheduleMode::NONE,
            cron_expression: None,
            fixed_rate: None,
            fixed_delay: None,
            initial_delay: None,
            zone_id: None,
        }
    }
}

impl ScheduleConfig {
    /// Creates a new builder.
    pub fn builder() -> ScheduleConfigBuilder {
        ScheduleConfigBuilder::default()
    }

    /// Creates a `NONE`-mode schedule config.
    pub fn none() -> Self {
        Self::default()
    }

    /// Accessor mirroring Java `getScheduleMode`.
    pub fn schedule_mode(&self) -> ScheduleMode {
        self.schedule_mode
    }
    /// Accessor mirroring Java `getCronExpression`.
    pub fn cron_expression(&self) -> Option<&str> {
        self.cron_expression.as_deref()
    }
    /// Accessor mirroring Java `getFixedRate`.
    pub fn fixed_rate(&self) -> Option<u64> {
        self.fixed_rate
    }
    /// Accessor mirroring Java `getFixedDelay`.
    pub fn fixed_delay(&self) -> Option<u64> {
        self.fixed_delay
    }
    /// Accessor mirroring Java `getInitialDelay`.
    pub fn initial_delay(&self) -> Option<u64> {
        self.initial_delay
    }
    /// Accessor mirroring Java `getZoneId`.
    pub fn zone_id(&self) -> Option<&str> {
        self.zone_id.as_deref()
    }

    /// Validates the config against its mode, mirroring Java `ScheduleConfig.validate`.
    pub fn validate(&self) -> Result<(), ScheduleConfigError> {
        match self.schedule_mode {
            ScheduleMode::NONE => Ok(()),
            ScheduleMode::CRON => match &self.cron_expression {
                Some(e) if !e.trim().is_empty() => Ok(()),
                _ => Err(ScheduleConfigError::EmptyCronExpression),
            },
            ScheduleMode::FIXED_RATE => match self.fixed_rate {
                Some(v) if v > 0 => Ok(()),
                _ => Err(ScheduleConfigError::InvalidFixedRate),
            },
            ScheduleMode::FIXED_DELAY => match self.fixed_delay {
                Some(v) if v > 0 => Ok(()),
                _ => Err(ScheduleConfigError::InvalidFixedDelay),
            },
        }
    }
}

/// Builder for [`ScheduleConfig`].
#[derive(Debug, Clone, Default)]
pub struct ScheduleConfigBuilder {
    inner: ScheduleConfig,
}

impl ScheduleConfigBuilder {
    /// Configures a cron-based schedule.
    pub fn cron(mut self, expression: impl Into<String>) -> Self {
        self.inner.schedule_mode = ScheduleMode::CRON;
        self.inner.cron_expression = Some(expression.into());
        self
    }

    /// Configures a fixed-rate schedule (interval in milliseconds).
    pub fn fixed_rate(mut self, milliseconds: u64) -> Self {
        self.inner.schedule_mode = ScheduleMode::FIXED_RATE;
        self.inner.fixed_rate = Some(milliseconds);
        self
    }

    /// Configures a fixed-delay schedule (delay in milliseconds).
    pub fn fixed_delay(mut self, milliseconds: u64) -> Self {
        self.inner.schedule_mode = ScheduleMode::FIXED_DELAY;
        self.inner.fixed_delay = Some(milliseconds);
        self
    }

    /// Sets the initial delay in milliseconds.
    pub fn initial_delay(mut self, milliseconds: u64) -> Self {
        self.inner.initial_delay = Some(milliseconds);
        self
    }

    /// Sets the time zone id used to evaluate the cron expression.
    pub fn zone_id(mut self, zone: impl Into<String>) -> Self {
        self.inner.zone_id = Some(zone.into());
        self
    }

    /// Sets the schedule mode explicitly.
    pub fn schedule_mode(mut self, mode: ScheduleMode) -> Self {
        self.inner.schedule_mode = mode;
        self
    }

    /// Builds and validates the [`ScheduleConfig`].
    pub fn build(self) -> Result<ScheduleConfig, ScheduleConfigError> {
        self.inner.validate()?;
        Ok(self.inner)
    }

    /// Builds without validation (used by internal reconstruction paths).
    pub fn build_unchecked(self) -> ScheduleConfig {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_mode_is_default_and_valid() {
        let cfg = ScheduleConfig::builder().build().unwrap();
        assert_eq!(cfg.schedule_mode(), ScheduleMode::NONE);
        assert!(cfg.cron_expression().is_none());
    }

    #[test]
    fn cron_requires_expression() {
        let err = ScheduleConfig::builder()
            .schedule_mode(ScheduleMode::CRON)
            .build()
            .unwrap_err();
        assert_eq!(err, ScheduleConfigError::EmptyCronExpression);

        let cfg = ScheduleConfig::builder().cron("0 0 8 * * ?").build().unwrap();
        assert_eq!(cfg.schedule_mode(), ScheduleMode::CRON);
        assert_eq!(cfg.cron_expression(), Some("0 0 8 * * ?"));
    }

    #[test]
    fn fixed_rate_requires_positive() {
        assert_eq!(
            ScheduleConfig::builder().fixed_rate(0).build().unwrap_err(),
            ScheduleConfigError::InvalidFixedRate
        );
        let cfg = ScheduleConfig::builder().fixed_rate(300_000).initial_delay(10).build().unwrap();
        assert_eq!(cfg.fixed_rate(), Some(300_000));
        assert_eq!(cfg.initial_delay(), Some(10));
    }

    #[test]
    fn fixed_delay_requires_positive() {
        assert_eq!(
            ScheduleConfig::builder().fixed_delay(0).build().unwrap_err(),
            ScheduleConfigError::InvalidFixedDelay
        );
        let cfg = ScheduleConfig::builder().fixed_delay(5).build().unwrap();
        assert_eq!(cfg.fixed_delay(), Some(5));
    }

    #[test]
    fn serde_roundtrip_uses_java_enum_names() {
        let cfg = ScheduleConfig::builder()
            .cron("0 0 9 * * MON-FRI")
            .zone_id("Asia/Shanghai")
            .build()
            .unwrap();
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("\"CRON\""), "json was {json}");
        let back: ScheduleConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cfg);
    }
}
