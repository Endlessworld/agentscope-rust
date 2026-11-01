//! Process-wide `rustls` crypto provider selection.
//!
//! The workspace builds `reqwest` 0.13 with the `rustls-no-provider` feature so
//! that only **one** crypto backend (`ring`) ends up in the binary: `reqwest`
//! 0.12 — pulled in through `rust-mcp-transport` / `async-anthropic` — already
//! defaults to `ring`, and compiling `aws-lc-rs` as well cost ~0.8 MB in the
//! embedded build for no functional gain.
//!
//! With `rustls-no-provider`, `reqwest` panics while building a `Client` unless a
//! process-wide provider has been installed. Two layers guarantee that:
//!
//! 1. every client factory calls [`ensure_crypto_provider`] before building a
//!    `Client` (grep `ensure_crypto_provider` to see them);
//! 2. [`auto_install`] installs it before `main` in any binary that links this
//!    crate, covering paths that build a `Client` without going through one of
//!    those factories (e.g. `#[derive(Default)]` wrappers).

use std::sync::Once;

static INSTALL: Once = Once::new();

/// Installs the `ring` provider once per process.
///
/// Idempotent: a provider installed earlier by the host wins and the call is a
/// no-op. Cheap enough to call from every `reqwest::Client` factory.
pub fn ensure_crypto_provider() {
    INSTALL.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Runs [`ensure_crypto_provider`] as a pre-`main` initializer.
///
/// The static lives in the platform's constructor table, so it cannot be dropped
/// as dead code by LTO.
mod auto_install {
    use super::ensure_crypto_provider;

    extern "C" fn install() {
        ensure_crypto_provider();
    }

    #[cfg(target_os = "windows")]
    #[used]
    #[link_section = ".CRT$XCU"]
    static INIT: extern "C" fn() = install;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[used]
    #[link_section = ".init_array"]
    static INIT: extern "C" fn() = install;

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    #[used]
    #[link_section = "__DATA,__mod_init_func"]
    static INIT: extern "C" fn() = install;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The initializer must have run before the test harness starts: building a
    /// `reqwest` client panics when no provider is installed.
    #[test]
    fn initializer_installs_the_provider_before_main() {
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
        reqwest::Client::builder().build().expect("client builds");
    }

    #[test]
    fn installing_twice_is_a_no_op() {
        ensure_crypto_provider();
        ensure_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }
}
