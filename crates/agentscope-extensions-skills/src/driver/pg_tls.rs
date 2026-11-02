//! `rustls` TLS connector for [`tokio_postgres`] (feature `postgres-tls`).
//!
//! `tokio-postgres` ships no TLS backend of its own: the caller hands [`Config::connect`] a
//! [`MakeTlsConnect`]. The workspace has no `tokio-postgres-rustls` crate and does not need one —
//! the glue is the three small traits below on top of `tokio-rustls` (the same rustls stack the
//! rest of the workspace already builds).
//!
//! Trust roots come from the platform store (`rustls-native-certs`); when that yields nothing
//! usable the compiled-in `webpki-roots` bundle is used instead, so a handshake never fails
//! merely because the OS store could not be read.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_postgres::tls::{ChannelBinding, MakeTlsConnect, TlsStream as PgTlsStream};
use tokio_postgres::Socket;

use super::db_error;
use crate::error::SkillRepositoryError;

const BACKEND: &str = "postgres";

/// The process-wide rustls client configuration (built once: it parses every trust anchor).
pub(super) fn client_config() -> Result<Arc<rustls::ClientConfig>, SkillRepositoryError> {
    static CONFIG: OnceLock<Result<Arc<rustls::ClientConfig>, String>> = OnceLock::new();
    CONFIG
        .get_or_init(build_client_config)
        .clone()
        .map_err(|e| db_error(BACKEND, e))
}

fn build_client_config() -> Result<Arc<rustls::ClientConfig>, String> {
    // The provider is named explicitly instead of going through `ClientConfig::builder()`: the
    // workspace also compiles the `aws-lc-rs` provider, and the implicit "process default"
    // resolution panics when more than one provider is compiled in.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("failed to build the rustls configuration: {e}"))?
        .with_root_certificates(root_store())
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// The trust anchors for the handshake: the platform store when it yields at least one usable
/// certificate, the `webpki-roots` bundle otherwise.
fn root_store() -> rustls::RootCertStore {
    let native = rustls_native_certs::load_native_certs();
    if !native.errors.is_empty() {
        tracing::debug!(backend = BACKEND, errors = ?native.errors, "incomplete native certificate store");
    }
    root_store_from(native.certs)
}

fn root_store_from(
    certs: Vec<rustls::pki_types::CertificateDer<'static>>,
) -> rustls::RootCertStore {
    let mut store = rustls::RootCertStore::empty();
    let (added, ignored) = store.add_parsable_certificates(certs);
    if added == 0 {
        store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    } else if ignored > 0 {
        tracing::debug!(
            backend = BACKEND,
            ignored,
            "ignored unusable native certificates"
        );
    }
    store
}

/// [`MakeTlsConnect`] for PostgreSQL, cloning one shared [`rustls::ClientConfig`].
#[derive(Clone)]
pub(super) struct MakeRustlsConnect {
    config: Arc<rustls::ClientConfig>,
}

impl MakeRustlsConnect {
    pub(super) fn new() -> Result<Self, SkillRepositoryError> {
        Ok(Self {
            config: client_config()?,
        })
    }
}

/// The per-connection half of [`MakeRustlsConnect`] (bound to the server name for SNI).
pub(super) struct RustlsConnect {
    config: Arc<rustls::ClientConfig>,
    domain: rustls::pki_types::ServerName<'static>,
}

impl MakeTlsConnect<Socket> for MakeRustlsConnect {
    type Stream = RustlsStream;
    type TlsConnect = RustlsConnect;
    type Error = rustls::pki_types::InvalidDnsNameError;

    fn make_tls_connect(&mut self, domain: &str) -> Result<RustlsConnect, Self::Error> {
        Ok(RustlsConnect {
            config: Arc::clone(&self.config),
            domain: rustls::pki_types::ServerName::try_from(domain)?.to_owned(),
        })
    }
}

impl tokio_postgres::tls::TlsConnect<Socket> for RustlsConnect {
    type Stream = RustlsStream;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<RustlsStream>> + Send>>;

    fn connect(self, stream: Socket) -> Self::Future {
        let connector = tokio_rustls::TlsConnector::from(self.config);
        Box::pin(async move {
            let stream = connector.connect(self.domain, stream).await?;
            Ok(RustlsStream(stream))
        })
    }
}

/// The encrypted stream handed to `tokio-postgres`.
pub(super) struct RustlsStream(tokio_rustls::client::TlsStream<Socket>);

impl AsyncRead for RustlsStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for RustlsStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl PgTlsStream for RustlsStream {
    fn channel_binding(&self) -> ChannelBinding {
        // `tokio-rustls` does not expose the server certificate here, so `tls-server-end-point`
        // channel binding (`SCRAM-SHA-256-PLUS`) is unavailable; the server falls back to plain
        // SCRAM, which is what a `sslmode=require` deployment already relies on.
        ChannelBinding::none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_native_certs_fall_back_to_the_bundled_roots() {
        let store = root_store_from(Vec::new());
        assert_eq!(store.len(), webpki_roots::TLS_SERVER_ROOTS.len());
    }

    #[test]
    fn unusable_native_certs_fall_back_without_panicking() {
        let garbage = vec![rustls::pki_types::CertificateDer::from(vec![0, 1, 2, 3])];
        let store = root_store_from(garbage);
        assert_eq!(store.len(), webpki_roots::TLS_SERVER_ROOTS.len());
    }

    #[test]
    fn the_client_config_is_built_once_and_cached() {
        let first = client_config().expect("client config");
        let second = client_config().expect("client config");
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn dns_names_are_accepted_and_garbage_is_rejected() {
        let mut connector = MakeRustlsConnect::new().expect("connector");
        assert!(connector.make_tls_connect("db.example.com").is_ok());
        assert!(connector.make_tls_connect("not a dns name!").is_err());
    }
}
