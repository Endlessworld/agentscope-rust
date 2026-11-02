//! Pluggable media resolution.
//!
//! Providers must inline media as base64 (Gemini `inlineData`, Claude
//! `source.base64`, Ollama `images`), but a [`Source`] may point at a remote URL
//! whose bytes are only obtainable asynchronously. This module defines a single
//! [`MediaResolver`] seam every provider shares, instead of three divergent
//! behaviours (Claude passes the URL through, Gemini/Ollama reject it).
//!
//! The default resolver handles base64 and local files, and serves remote URLs
//! from the process-wide [`remote_media_cache`] (**explicitly rejecting** URLs that
//! were never [`RemoteMediaCache::prefetch`]ed, so nothing performs hidden network
//! I/O). A host that wants a different download policy registers its own resolver
//! via [`set_media_resolver`] (mirrors new-api's `SetMediaResolver`).

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, RwLock};

use agentscope_core::{determine_media_type, is_local_file, Error, Result, Source};

use super::transport::HttpTransport;

/// Inlined media bytes plus the MIME type they should be labelled with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMedia {
    /// Base64 payload **without** a `data:` prefix.
    pub base64: String,
    /// MIME type (e.g. `image/png`).
    pub mime_type: String,
}

/// Resolves a [`Source`] to inline base64 bytes.
///
/// Implementations must be `Send + Sync` because the resolver is stored
/// globally. Resolution is deliberately synchronous: providers stream
/// synchronously and an async hop here would infect every converter signature.
/// A host needing remote downloads should resolve those upfront (async) and pass
/// [`Source::Base64`] through, or register a resolver with a pre-populated cache.
pub trait MediaResolver: Send + Sync {
    /// Resolves `source`, tagging the result for the given `purpose`
    /// (e.g. `formatting image for Gemini`).
    fn resolve(&self, source: &Source, purpose: &str) -> Result<ResolvedMedia>;
}

/// Default resolver: base64 passes through, local files are read, remote URLs
/// are served from [`remote_media_cache`] (or rejected with an actionable
/// message when they were never prefetched).
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultMediaResolver;

impl MediaResolver for DefaultMediaResolver {
    fn resolve(&self, source: &Source, purpose: &str) -> Result<ResolvedMedia> {
        match source {
            Source::Base64(b) => {
                let mime_type = if b.media_type.trim().is_empty() {
                    "application/octet-stream".to_string()
                } else {
                    b.media_type.clone()
                };
                Ok(ResolvedMedia {
                    base64: b.data.clone(),
                    mime_type,
                })
            }
            Source::Url(u) => {
                if is_local_file(&u.url) {
                    let path = u.url.strip_prefix("file://").unwrap_or(&u.url);
                    let mime_type = u
                        .mime_type
                        .clone()
                        .filter(|m| !m.trim().is_empty())
                        .unwrap_or_else(|| determine_media_type(path));
                    Ok(ResolvedMedia {
                        base64: agentscope_core::file_to_base64(path)?,
                        mime_type,
                    })
                } else {
                    match remote_media_cache().get(&u.url) {
                        Some(mut resolved) => {
                            if let Some(mime) = u.mime_type.clone().filter(|m| !m.trim().is_empty())
                            {
                                resolved.mime_type = mime;
                            }
                            Ok(resolved)
                        }
                        None => Err(Error::Model(format!(
                            "remote media URL '{}' has not been fetched; prefetch it with \
                             agentscope_extensions_model::common::remote_media_cache().prefetch(...) \
                             or register a MediaResolver; purpose={purpose}",
                            u.url
                        ))),
                    }
                }
            }
            Source::FileId(file) => Err(Error::Other(format!(
                "provider file id '{}' has no inline bytes; {purpose} needs a URL or base64 source",
                file.file_id
            ))),
        }
    }
}

/// In-memory cache of remote media bytes.
///
/// The [`MediaResolver`] seam is synchronous because providers stream
/// synchronously and an async hop would infect every converter signature.
/// Downloading is therefore a separate, **async** step: the host calls
/// [`RemoteMediaCache::prefetch`] (which uses the shared
/// [`HttpTransport`]), and every provider then resolves that URL through the
/// process-wide cache. A prefetched URL is inlined as base64 exactly like a
/// `data:` source, so Gemini/Ollama stop rejecting remote images.
#[derive(Debug, Default)]
pub struct RemoteMediaCache {
    entries: RwLock<BTreeMap<String, ResolvedMedia>>,
}

impl RemoteMediaCache {
    /// Creates an empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the cached media for `url`, if any.
    pub fn get(&self, url: &str) -> Option<ResolvedMedia> {
        self.entries.read().ok()?.get(url).cloned()
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        self.entries.read().map(|e| e.len()).unwrap_or(0)
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Stores an already-resolved media payload for `url`.
    pub fn insert(&self, url: impl Into<String>, media: ResolvedMedia) {
        if let Ok(mut entries) = self.entries.write() {
            entries.insert(url.into(), media);
        }
    }

    /// Downloads `url` through `transport` and caches the base64 payload.
    ///
    /// The MIME type comes from the response's `content-type`, falling back to
    /// the URL's extension. A non-success status surfaces through the shared
    /// [`crate::common::ModelError`] normalisation.
    pub async fn prefetch(
        &self,
        transport: &dyn HttpTransport,
        url: &str,
    ) -> Result<ResolvedMedia> {
        if let Some(cached) = self.get(url) {
            return Ok(cached);
        }
        let (bytes, content_type) = transport.get_bytes(url, &[], "media").await?;
        let mime_type = content_type
            .filter(|value| !value.trim().is_empty())
            .or_else(|| Some(determine_media_type(url)))
            .unwrap_or_else(|| "application/octet-stream".to_string());
        use base64::Engine as _;
        let resolved = ResolvedMedia {
            base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            mime_type,
        };
        self.insert(url, resolved.clone());
        Ok(resolved)
    }
}

static REMOTE_CACHE: OnceLock<Arc<RemoteMediaCache>> = OnceLock::new();

/// The process-wide remote media cache used by [`DefaultMediaResolver`].
pub fn remote_media_cache() -> &'static Arc<RemoteMediaCache> {
    REMOTE_CACHE.get_or_init(|| Arc::new(RemoteMediaCache::new()))
}

static RESOLVER: OnceLock<Arc<dyn MediaResolver>> = OnceLock::new();

/// Installs the process-wide media resolver.
///
/// Returns `Err` with the previously installed resolver if one was already set,
/// so callers can detect double-registration.
pub fn set_media_resolver(
    resolver: Arc<dyn MediaResolver>,
) -> std::result::Result<(), Arc<dyn MediaResolver>> {
    RESOLVER.set(resolver)
}

/// Installs the resolver unless one is already set (idempotent).
pub fn ensure_media_resolver(resolver: Arc<dyn MediaResolver>) {
    let _ = RESOLVER.set(resolver);
}

/// The active resolver, or [`DefaultMediaResolver`] when none was registered.
pub fn media_resolver() -> Arc<dyn MediaResolver> {
    RESOLVER
        .get()
        .cloned()
        .unwrap_or_else(|| Arc::new(DefaultMediaResolver))
}

/// Resolves `source` through the active resolver.
pub fn resolve_media(source: &Source, purpose: &str) -> Result<ResolvedMedia> {
    media_resolver().resolve(source, purpose)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::{Base64Source, URLSource};

    #[test]
    fn default_resolver_passes_base64_through() {
        let source = Source::Base64(Base64Source::new("image/png", "AAAA"));
        let resolved = DefaultMediaResolver.resolve(&source, "test").unwrap();
        assert_eq!(resolved.base64, "AAAA");
        assert_eq!(resolved.mime_type, "image/png");
    }

    #[test]
    fn default_resolver_rejects_remote_url() {
        let source = Source::Url(URLSource::new("https://example.invalid/never-fetched.png"));
        let err = DefaultMediaResolver.resolve(&source, "test").unwrap_err();
        assert!(err.to_string().contains("has not been fetched"));
    }

    #[test]
    fn default_resolver_serves_prefetched_remote_media() {
        let cache = RemoteMediaCache::new();
        cache.insert(
            "https://example.invalid/cached.png",
            ResolvedMedia {
                base64: "AAAA".to_string(),
                mime_type: "image/png".to_string(),
            },
        );
        assert_eq!(cache.len(), 1);
        assert!(cache.get("https://example.invalid/cached.png").is_some());
        assert!(cache.get("https://example.invalid/other.png").is_none());
        assert!(RemoteMediaCache::default().is_empty());
    }

    #[tokio::test]
    async fn prefetch_downloads_through_the_transport_and_caches() {
        let transport = crate::common::MemoryTransport {
            bytes_response: Some((vec![1, 2, 3], Some("image/png".to_string()))),
            ..Default::default()
        };
        let cache = RemoteMediaCache::new();
        let resolved = cache
            .prefetch(&transport, "https://example.invalid/a.png")
            .await
            .unwrap();
        assert_eq!(resolved.mime_type, "image/png");
        assert_eq!(resolved.base64, "AQID");
        assert!(transport.last_get_url().unwrap().ends_with("a.png"));

        // A second prefetch is served from the cache: the transport would
        // otherwise fail (only one response configured).
        let again = cache
            .prefetch(&transport, "https://example.invalid/a.png")
            .await
            .unwrap();
        assert_eq!(again, resolved);
    }

    #[tokio::test]
    async fn prefetch_reports_transports_without_get_support() {
        let transport = crate::common::MemoryTransport::default();
        let err = RemoteMediaCache::new()
            .prefetch(&transport, "https://example.invalid/a.png")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no GET bytes configured"));
    }
}
