//! Base-URL resolution shared by every provider adapter.
//!
//! Precedence (mirrors the Ollama provider's existing behaviour):
//! 1. an explicit value from the creation context;
//! 2. the provider-specific `*_BASE_URL` environment variable;
//! 3. the provider default.
//!
//! The environment lookup is injectable ([`resolve_base_url_with`]) so tests can
//! verify the fallback without mutating process-wide state (which would race
//! with the other tests running in parallel).

/// Resolves a provider base URL using the real process environment.
pub fn resolve_base_url(explicit: Option<String>, env_key: &str, default: &str) -> String {
    resolve_base_url_with(explicit, env_key, default, |key| std::env::var(key).ok())
}

/// Resolves a provider base URL using an injectable environment lookup.
///
/// Blank (whitespace-only) explicit values and blank environment values are
/// treated as absent, matching the rest of the crate's "empty means unset"
/// convention.
pub fn resolve_base_url_with<F>(
    explicit: Option<String>,
    env_key: &str,
    default: &str,
    lookup: F,
) -> String
where
    F: Fn(&str) -> Option<String>,
{
    let non_blank = |u: String| {
        let trimmed = u.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    };
    explicit
        .and_then(non_blank)
        .or_else(|| lookup(env_key).and_then(non_blank))
        .unwrap_or_else(|| default.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate process-wide environment variables.
    ///
    /// `cargo test` runs tests on parallel threads within one process, so any
    /// test that sets/removes an env var must hold this lock to avoid racing
    /// the other env-based tests in this crate.
    pub(crate) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn explicit_value_wins_over_env_and_default() {
        let resolved = resolve_base_url_with(
            Some("https://explicit.test".to_string()),
            "SOME_BASE_URL",
            "https://default.test",
            |_| Some("https://env.test".to_string()),
        );
        assert_eq!(resolved, "https://explicit.test");
    }

    #[test]
    fn env_value_used_when_explicit_absent() {
        let resolved = resolve_base_url_with(None, "SOME_BASE_URL", "https://default.test", |k| {
            (k == "SOME_BASE_URL").then(|| "https://env.test".to_string())
        });
        assert_eq!(resolved, "https://env.test");
    }

    #[test]
    fn default_used_when_neither_present() {
        let resolved =
            resolve_base_url_with(None, "SOME_BASE_URL", "https://default.test", |_| None);
        assert_eq!(resolved, "https://default.test");
    }

    #[test]
    fn blank_values_are_treated_as_absent() {
        let resolved = resolve_base_url_with(
            Some("   ".to_string()),
            "SOME_BASE_URL",
            "https://default.test",
            |_| Some("  ".to_string()),
        );
        assert_eq!(resolved, "https://default.test");
    }
}
