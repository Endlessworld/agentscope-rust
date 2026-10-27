//! Configuration for AgentScope Studio integration, mirroring `StudioConfig`.
use std::time::Duration;

use chrono::Utc;
use uuid::Uuid;

/// Configuration for AgentScope Studio integration.
///
/// Holds all parameters needed to connect to and communicate with Studio,
/// including HTTP and WebSocket settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StudioConfig {
    pub(crate) studio_url: String,
    pub(crate) tracing_url: String,
    pub(crate) project: String,
    pub(crate) run_name: String,
    pub(crate) run_id: String,
    pub(crate) max_retries: u32,
    pub(crate) reconnect_attempts: u32,
    pub(crate) reconnect_delay: Duration,
    pub(crate) reconnect_max_delay: Duration,
}

impl StudioConfig {
    /// Creates a new builder with Java-compatible defaults.
    pub fn builder() -> StudioConfigBuilder {
        StudioConfigBuilder::default()
    }

    /// The Studio server base URL (default `http://localhost:3000`).
    pub fn studio_url(&self) -> &str {
        &self.studio_url
    }

    /// The tracing endpoint URL for pushing messages.
    ///
    /// Defaults to `studioUrl + "/v1/traces"` when not set explicitly.
    pub fn tracing_url(&self) -> &str {
        &self.tracing_url
    }

    /// The project name for this run (default `UnnamedProject`).
    pub fn project(&self) -> &str {
        &self.project
    }

    /// The human-readable run name (default `run_<millis>`).
    pub fn run_name(&self) -> &str {
        &self.run_name
    }

    /// The unique run identifier (default random UUID).
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Maximum number of HTTP request retries (default 3).
    pub fn max_retries(&self) -> u32 {
        self.max_retries
    }

    /// Maximum number of WebSocket reconnection attempts (default 3).
    pub fn reconnect_attempts(&self) -> u32 {
        self.reconnect_attempts
    }

    /// Initial delay between reconnection attempts (default 1s).
    pub fn reconnect_delay(&self) -> Duration {
        self.reconnect_delay
    }

    /// Maximum delay between reconnection attempts (default 5s).
    pub fn reconnect_max_delay(&self) -> Duration {
        self.reconnect_max_delay
    }

    /// The Socket.IO/WebSocket endpoint derived from `studio_url`.
    pub fn socket_url(&self) -> String {
        self.studio_url.trim_end_matches('/').to_string()
    }
}

/// Builder for [`StudioConfig`].
#[derive(Debug, Clone)]
pub struct StudioConfigBuilder {
    studio_url: String,
    tracing_url: Option<String>,
    project: String,
    run_name: String,
    run_id: String,
    max_retries: u32,
    reconnect_attempts: u32,
    reconnect_delay: Duration,
    reconnect_max_delay: Duration,
}

impl Default for StudioConfigBuilder {
    fn default() -> Self {
        Self {
            studio_url: "http://localhost:3000".to_string(),
            tracing_url: None,
            project: "UnnamedProject".to_string(),
            run_name: format!("run_{}", Utc::now().timestamp_millis()),
            run_id: Uuid::new_v4().to_string(),
            max_retries: 3,
            reconnect_attempts: 3,
            reconnect_delay: Duration::from_secs(1),
            reconnect_max_delay: Duration::from_secs(5),
        }
    }
}

impl StudioConfigBuilder {
    /// Sets the Studio server base URL.
    pub fn studio_url(mut self, studio_url: impl Into<String>) -> Self {
        self.studio_url = studio_url.into();
        self
    }

    /// Sets the tracing URL explicitly (defaults to `{studioUrl}/v1/traces`).
    pub fn tracing_url(mut self, tracing_url: impl Into<String>) -> Self {
        self.tracing_url = Some(tracing_url.into());
        self
    }

    /// Sets the project name.
    pub fn project(mut self, project: impl Into<String>) -> Self {
        self.project = project.into();
        self
    }

    /// Sets the run name.
    pub fn run_name(mut self, run_name: impl Into<String>) -> Self {
        self.run_name = run_name.into();
        self
    }

    /// Sets the run id explicitly.
    pub fn run_id(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = run_id.into();
        self
    }

    /// Sets the maximum number of HTTP retries.
    pub fn max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Sets the number of WebSocket reconnection attempts.
    pub fn reconnect_attempts(mut self, reconnect_attempts: u32) -> Self {
        self.reconnect_attempts = reconnect_attempts;
        self
    }

    /// Sets the initial reconnect delay.
    pub fn reconnect_delay(mut self, reconnect_delay: Duration) -> Self {
        self.reconnect_delay = reconnect_delay;
        self
    }

    /// Sets the maximum reconnect delay.
    pub fn reconnect_max_delay(mut self, reconnect_max_delay: Duration) -> Self {
        self.reconnect_max_delay = reconnect_max_delay;
        self
    }

    /// Builds the [`StudioConfig`], deriving `tracing_url` when unset.
    pub fn build(self) -> StudioConfig {
        let base = self.studio_url.trim_end_matches('/').to_string();
        let tracing_url = self
            .tracing_url
            .unwrap_or_else(|| format!("{base}/v1/traces"));
        StudioConfig {
            studio_url: base,
            tracing_url,
            project: self.project,
            run_name: self.run_name,
            run_id: self.run_id,
            max_retries: self.max_retries,
            reconnect_attempts: self.reconnect_attempts,
            reconnect_delay: self.reconnect_delay,
            reconnect_max_delay: self.reconnect_max_delay,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_java() {
        let cfg = StudioConfig::builder().build();
        assert_eq!(cfg.studio_url(), "http://localhost:3000");
        assert_eq!(cfg.tracing_url(), "http://localhost:3000/v1/traces");
        assert_eq!(cfg.project(), "UnnamedProject");
        assert_eq!(cfg.max_retries(), 3);
        assert_eq!(cfg.reconnect_attempts(), 3);
        assert_eq!(cfg.reconnect_delay(), Duration::from_secs(1));
        assert_eq!(cfg.reconnect_max_delay(), Duration::from_secs(5));
        assert!(!cfg.run_id().is_empty());
        assert!(cfg.run_name().starts_with("run_"));
    }

    #[test]
    fn studio_url_is_trimmed_and_tracing_derived() {
        let cfg = StudioConfig::builder()
            .studio_url("http://host:8080/")
            .project("proj")
            .build();
        assert_eq!(cfg.studio_url(), "http://host:8080");
        assert_eq!(cfg.socket_url(), "http://host:8080");
        assert_eq!(cfg.tracing_url(), "http://host:8080/v1/traces");
        assert_eq!(cfg.project(), "proj");
    }

    #[test]
    fn explicit_tracing_url_wins() {
        let cfg = StudioConfig::builder()
            .studio_url("http://host")
            .tracing_url("http://collector/v1/traces")
            .build();
        assert_eq!(cfg.tracing_url(), "http://collector/v1/traces");
    }
}
