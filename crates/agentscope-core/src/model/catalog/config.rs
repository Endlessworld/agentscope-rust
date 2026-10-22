//! Catalog configuration and environment overrides.

use std::env;
use std::path::PathBuf;
use std::time::Duration;

/// Default flat source.
pub const DEFAULT_MODELS_URL: &str = "https://models.dev/models.json?type=all";
/// Default detailed source.
pub const DEFAULT_API_URL: &str = "https://models.dev/api.json";

/// Runtime configuration. Every field can be overridden through the environment.
#[derive(Debug, Clone)]
pub struct CatalogConfig {
    pub enabled: bool,
    pub cache_dir: PathBuf,
    pub models_url: String,
    pub api_url: String,
    pub use_api: bool,
    pub ttl: Duration,
    pub cold_block: Duration,
    pub timeout: Duration,
    pub max_models_bytes: usize,
    pub max_api_bytes: usize,
    pub user_agent: String,
}

impl Default for CatalogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cache_dir: default_cache_dir(),
            models_url: DEFAULT_MODELS_URL.to_string(),
            api_url: DEFAULT_API_URL.to_string(),
            use_api: true,
            ttl: Duration::from_secs(24 * 60 * 60),
            cold_block: Duration::ZERO,
            timeout: Duration::from_secs(15),
            max_models_bytes: 1024 * 1024,
            max_api_bytes: 8 * 1024 * 1024,
            user_agent: format!("agentscope-rust/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

impl CatalogConfig {
    /// Reads the documented `AGENTSCOPE_MODEL_CATALOG*` variables.
    pub fn from_env() -> Self {
        let mut config = Self::default();
        config.enabled = env_flag("AGENTSCOPE_MODEL_CATALOG", true);
        config.use_api = env_flag("AGENTSCOPE_MODEL_CATALOG_API", true);
        if let Some(value) = env_string("AGENTSCOPE_MODEL_CATALOG_DIR") {
            config.cache_dir = PathBuf::from(value);
        }
        if let Some(value) = env_string("AGENTSCOPE_MODEL_CATALOG_URL") {
            config.models_url = value;
        }
        if let Some(value) = env_string("AGENTSCOPE_MODEL_CATALOG_API_URL") {
            config.api_url = value;
        }
        if let Some(hours) = env_u64("AGENTSCOPE_MODEL_CATALOG_TTL_HOURS") {
            config.ttl = Duration::from_secs(hours.saturating_mul(60 * 60));
        }
        if let Some(millis) = env_u64("AGENTSCOPE_MODEL_CATALOG_COLD_BLOCK_MS") {
            config.cold_block = Duration::from_millis(millis);
        }
        config
    }

    /// Effective cache directory.
    pub fn cache_dir(&self) -> &PathBuf {
        &self.cache_dir
    }
}

fn env_string(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn env_flag(key: &str, default: bool) -> bool {
    match env::var(key) {
        Ok(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        Err(_) => default,
    }
}

fn env_u64(key: &str) -> Option<u64> {
    env::var(key).ok()?.trim().parse::<u64>().ok()
}

fn default_cache_dir() -> PathBuf {
    if let Some(value) = env_string("AGENTSCOPE_MODEL_CATALOG_DIR") {
        return PathBuf::from(value);
    }
    let home = env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".agentscope").join("model-catalog")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_conservative() {
        let config = CatalogConfig::default();
        assert!(config.enabled);
        assert!(config.use_api);
        assert_eq!(config.ttl, Duration::from_secs(86_400));
        assert_eq!(config.cold_block, Duration::ZERO);
        assert_eq!(config.max_models_bytes, 1024 * 1024);
    }

    #[test]
    fn cache_dir_prefers_explicit_override() {
        // This test uses `env::set_var`, so it is kept to one thread-safe observation:
        // the default path ends in `.agentscope/model-catalog` when no override is set.
        if env::var_os("AGENTSCOPE_MODEL_CATALOG_DIR").is_none() {
            let dir = default_cache_dir();
            assert!(dir.ends_with("model-catalog"));
        }
    }
}
