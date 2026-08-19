//! Logging / tracing helpers, mirroring the SLF4J → tracing mapping.
use tracing_subscriber::EnvFilter;

/// Initializes a global `tracing` subscriber with a filter read from the
/// `RUST_LOG` environment variable (defaulting to `info`).
pub fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

/// Initializes a global `tracing` subscriber with the given filter string.
pub fn init_tracing_with_filter(filter: &str) {
    let f = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(f).try_init();
}
