//! Backend abstraction over the Alibaba Cloud OSS object API.
//!
//! The Java module talks to `com.aliyun.oss.OSS` directly. In Rust we keep the
//! same object-model semantics (put/get/exists/delete/list) behind the
//! [`OssClient`] trait so that:
//!
//! * object storage backends stay swappable: the in-memory
//!   [`InMemoryOssClient`] is always available, while the S3-compatible backend
//!   only compiles with the opt-in `s3` feature (see the `s3` module);
//! * tests can use [`InMemoryOssClient`] (optionally via
//!   [`InMemoryOssClient::with_page_size`]) mirroring the Mockito-mocked `OSS`
//!   in the Java tests, including multi-page listings.
//!
//! All methods are synchronous because the core/harness store traits
//! ([`agentscope_core::state::AgentStateStore`],
//! [`agentscope_harness::filesystem::remote::store::BaseStore`]) are synchronous;
//! concrete backends perform blocking I/O internally.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::error::{OssError, OssResult};

/// A single object listing entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OssObjectSummary {
    /// Full object key.
    pub key: String,
}

/// Result of a listing operation (single page).
#[derive(Debug, Clone, Default)]
pub struct OssListPage {
    /// Object summaries in this page.
    pub objects: Vec<OssObjectSummary>,
    /// Whether more pages are available.
    pub truncated: bool,
    /// Opaque continuation token for the next page.
    pub next_token: Option<String>,
}

/// Minimal object-store surface needed by the OSS-backed stores.
///
/// Object keys use `/` as separator, matching OSS conventions.
pub trait OssClient: Send + Sync + std::fmt::Debug {
    /// Writes `content` (UTF-8 bytes) to `object_key`, overwriting any existing object.
    fn put(&self, object_key: &str, content: &[u8]) -> OssResult<()>;

    /// Reads `object_key`, returning `None` when it does not exist.
    fn get(&self, object_key: &str) -> OssResult<Option<Vec<u8>>>;

    /// Returns whether `object_key` exists.
    fn exists(&self, object_key: &str) -> OssResult<bool>;

    /// Deletes `object_key` if present (no error if absent).
    fn delete(&self, object_key: &str) -> OssResult<()>;

    /// Lists up to `max_keys` objects under `prefix`.
    ///
    /// When `continuation_token` is `Some`, listing resumes after that token.
    /// `max_keys == 0` yields an empty, non-truncated page.
    fn list_page(
        &self,
        prefix: &str,
        max_keys: usize,
        continuation_token: Option<&str>,
    ) -> OssResult<OssListPage>;

    /// Lists every object key under `prefix`, following continuation tokens.
    ///
    /// Real backends cap a page (1000 keys for OSS/S3), so this helper is what
    /// makes store-level operations pagination-correct.
    fn list_all(&self, prefix: &str) -> OssResult<Vec<String>> {
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let page = self.list_page(prefix, 1000, token.as_deref())?;
            for obj in page.objects {
                keys.push(obj.key);
            }
            if page.truncated {
                token = page.next_token;
                if token.is_none() {
                    break;
                }
            } else {
                break;
            }
        }
        Ok(keys)
    }

    /// Shuts the underlying client down. Default: no-op.
    fn shutdown(&self) {}
}

/// In-memory [`OssClient`] for tests and local development.
///
/// Keys are kept in a `BTreeMap` so [`OssClient::list_all`] returns sorted keys,
/// matching OSS lexicographic ordering used by the real backend.
///
/// By default a single [`OssClient::list_page`] call may return every matching
/// key, which makes multi-page behaviour untestable. Use
/// [`InMemoryOssClient::with_page_size`] to emulate a backend page cap.
#[derive(Debug, Default)]
pub struct InMemoryOssClient {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
    shut_down: Mutex<bool>,
    /// Optional per-page object cap, emulating the backend `MaxKeys` limit.
    page_size: Option<usize>,
}

impl InMemoryOssClient {
    /// Creates an empty in-memory client.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an empty in-memory client that never returns more than
    /// `page_size` objects per [`OssClient::list_page`] call.
    ///
    /// This emulates a real backend page cap (OSS/S3 cap a page at 1000 keys and
    /// hand out continuation tokens), so tests can exercise multi-page listing.
    /// `page_size` is clamped to at least `1`.
    pub fn with_page_size(page_size: usize) -> Self {
        Self {
            objects: Mutex::new(BTreeMap::new()),
            shut_down: Mutex::new(false),
            page_size: Some(page_size.max(1)),
        }
    }

    /// Returns the configured page cap, or `None` when a page may contain every
    /// matching key.
    pub fn page_size(&self) -> Option<usize> {
        self.page_size
    }

    /// Returns the number of stored objects.
    pub fn len(&self) -> usize {
        self.objects.lock().unwrap().len()
    }

    /// Returns whether no objects are stored.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns whether [`OssClient::shutdown`] has been called.
    pub fn is_shut_down(&self) -> bool {
        *self.shut_down.lock().unwrap()
    }
}

impl OssClient for InMemoryOssClient {
    fn put(&self, object_key: &str, content: &[u8]) -> OssResult<()> {
        self.objects
            .lock()
            .unwrap()
            .insert(object_key.to_string(), content.to_vec());
        Ok(())
    }

    fn get(&self, object_key: &str) -> OssResult<Option<Vec<u8>>> {
        Ok(self.objects.lock().unwrap().get(object_key).cloned())
    }

    fn exists(&self, object_key: &str) -> OssResult<bool> {
        Ok(self.objects.lock().unwrap().contains_key(object_key))
    }

    fn delete(&self, object_key: &str) -> OssResult<()> {
        self.objects.lock().unwrap().remove(object_key);
        Ok(())
    }

    fn list_page(
        &self,
        prefix: &str,
        max_keys: usize,
        continuation_token: Option<&str>,
    ) -> OssResult<OssListPage> {
        let guard = self.objects.lock().unwrap();
        let mut matched: Vec<&String> = guard.keys().filter(|k| k.starts_with(prefix)).collect();
        matched.sort();

        let start = match continuation_token {
            Some(tok) => matched
                .iter()
                .position(|k| k.as_str() > tok)
                .unwrap_or(matched.len()),
            None => 0,
        };
        // Emulate the backend page cap when the client was built with one.
        let effective_max = match self.page_size {
            Some(cap) => max_keys.min(cap),
            None => max_keys,
        };
        // `max_keys == 0` is an explicit request for an empty page (S3/OSS
        // semantics); it must not report `truncated` without a token.
        if effective_max == 0 {
            return Ok(OssListPage::default());
        }
        let end = (start + effective_max).min(matched.len());
        let objects = matched[start..end]
            .iter()
            .map(|k| OssObjectSummary { key: (*k).clone() })
            .collect::<Vec<_>>();
        let truncated = end < matched.len();
        let next_token = if truncated {
            objects.last().map(|o| o.key.clone())
        } else {
            None
        };
        Ok(OssListPage {
            objects,
            truncated,
            next_token,
        })
    }

    fn shutdown(&self) {
        *self.shut_down.lock().unwrap() = true;
    }
}

/// Validates a snapshot id is a safe single path segment.
///
/// Mirrors `agentscope_harness::agent::sandbox::snapshot::validate_snapshot_id`
/// behaviour (blank / separators / `..` / NUL rejected) so the OSS spec fails the
/// same way as harness built-ins.
pub(crate) fn validate_snapshot_id(id: &str) -> OssResult<()> {
    if id.trim().is_empty() {
        return Err(OssError::BlankIdentifier("snapshotId"));
    }
    if id.contains('/') || id.contains('\\') || id.contains("..") || id.contains('\0') {
        return Err(OssError::UnsafeSnapshotId(id.to_string()));
    }
    Ok(())
}

/// Normalizes an OSS key prefix: backslashes to slashes, strip leading slashes,
/// append a trailing slash when non-empty.
///
/// `fallback` is returned when the prefix is `None`/blank.
pub(crate) fn normalize_prefix(prefix: Option<&str>, fallback: &str) -> String {
    let Some(prefix) = prefix else {
        return fallback.to_string();
    };
    if prefix.trim().is_empty() {
        return fallback.to_string();
    }
    let mut p = prefix.replace('\\', "/");
    while p.starts_with('/') {
        p.remove(0);
    }
    if !p.is_empty() && !p.ends_with('/') {
        p.push('/');
    }
    if p.is_empty() {
        fallback.to_string()
    } else {
        p
    }
}

/// Strips leading `/` characters from a key (Java `stripLeadingSlashes`).
pub(crate) fn strip_leading_slashes(s: &str) -> String {
    s.trim_start_matches('/').to_string()
}
