//! Pluggable media resolution.
//!
//! Providers must inline media as base64 (Gemini `inlineData`, Claude
//! `source.base64`, Ollama `images`), but a [`Source`] may point at a remote URL
//! whose bytes are only obtainable asynchronously. This module defines a single
//! [`MediaResolver`] seam every provider shares, instead of three divergent
//! behaviours (Claude passes the URL through, Gemini/Ollama reject it).
//!
//! The default resolver handles base64 and local files and **explicitly rejects**
//! remote URLs; a host that wants remote downloads registers its own resolver
//! via [`set_media_resolver`] (mirrors new-api's `SetMediaResolver`).

use std::sync::{Arc, OnceLock};

use agentscope_core::{determine_media_type, is_local_file, Error, Result, Source};

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
/// are rejected with an actionable message.
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
                    Err(Error::Model(format!(
                        "remote media URL '{}' requires a registered MediaResolver \
                         (call agentscope_extensions_model::common::set_media_resolver); purpose={purpose}",
                        u.url
                    )))
                }
            }
        }
    }
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
        let source = Source::Url(URLSource::new("https://example.com/a.png"));
        let err = DefaultMediaResolver.resolve(&source, "test").unwrap_err();
        assert!(err
            .to_string()
            .contains("requires a registered MediaResolver"));
    }
}
