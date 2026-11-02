//! Alibaba Cloud OSS backed [`BaseStore`], mirroring
//! `io.agentscope.extensions.oss.OssBaseStore`.
//!
//! Object key layout (all under `{keyPrefix}{namespace[0]}/{namespace[1]}/.../`):
//!
//! ```text
//! {key}.json     — item data (JSON object)
//! {key}.version  — version counter (decimal text)
//! ```
//!
//! Version semantics follow the harness definition
//! (`agentscope_harness::filesystem::remote::store`): version starts at 1 and
//! increments on every successful write; `put_if_version` with expected `0`
//! writes only when the item does not yet exist.
//!
//! # Compare-and-set
//!
//! With a client that implements conditional writes
//! ([`OssClient::put_conditional`]) the `.version` sidecar doubles as the CAS anchor:
//! the backend evaluates `If-Match`/`If-None-Match` on it, so of two writers racing on
//! the same expected version exactly one wins and the payload is written by the winner
//! only. [`BaseStore::supports_atomic_session_storage`] is only `true` in that case —
//! the fallback for plain clients is the Java read-check-write sequence, which cannot
//! fence concurrent writers.
//!
//! # Search error semantics
//!
//! [`OssStore::search_strict`] mirrors the Java error behaviour of
//! `OssBaseStore.search`: a listed object key that falls outside the requested
//! namespace prefix (or malformed item JSON) is a backend inconsistency and
//! surfaces as [`OssError`] (Java throws `RuntimeException` from its unchecked
//! `substring`). The harness [`BaseStore::search`] signature has no error channel,
//! so it delegates to `search_strict` and returns an empty page on error instead
//! of fabricating item keys.
//!
//! The same split exists for the other CRUD methods, because Java `OssBaseStore`
//! throws on every failure while the synchronous [`BaseStore`] trait cannot:
//! [`OssStore::get_strict`], [`OssStore::put_strict`],
//! [`OssStore::put_if_version_strict`] and [`OssStore::delete_strict`] return
//! [`OssResult`], while the trait methods delegate to them and only log.

use std::sync::Arc;

use agentscope_harness::filesystem::remote::store::{BaseStore, StoreItem, StoreValue};

use crate::client::{normalize_prefix, strip_leading_slashes, OssClient, PutCondition, PutOutcome};
use crate::error::{OssError, OssResult};

/// Default key prefix for base-store objects.
pub const DEFAULT_KEY_PREFIX: &str = "agentscope/store/";
const JSON_SUFFIX: &str = ".json";
const VERSION_SUFFIX: &str = ".version";

/// Keys requested per listing call in [`OssStore::search_strict`], relative to the number
/// of items still needed (an item is two objects: `{key}.json` + `{key}.version`).
const LIST_OVERSCAN: usize = 2;

/// Upper bound of a single listing request (S3/OSS page limit).
const MAX_KEYS_PER_LIST: usize = 1000;

/// OSS-backed namespace key-value store for the harness remote filesystem.
#[derive(Debug, Clone)]
pub struct OssStore {
    client: Arc<dyn OssClient>,
    bucket_name: String,
    key_prefix: String,
}

// `BaseStore` is not `async_trait`; the trait is synchronous, so blocking OSS
// operations happen inline, matching Java `OssBaseStore`.
impl BaseStore for OssStore {
    fn get(&self, namespace: &[String], key: &str) -> Option<StoreItem> {
        match self.get_strict(namespace, key) {
            Ok(item) => item,
            Err(e) => {
                tracing::error!(error = %e, key, "failed to get item");
                None
            }
        }
    }

    fn put(&self, namespace: &[String], key: &str, value: StoreValue) {
        if let Err(e) = self.put_strict(namespace, key, value) {
            tracing::error!(error = %e, key, "failed to put item");
        }
    }

    fn put_if_version(
        &self,
        namespace: &[String],
        key: &str,
        value: StoreValue,
        expected_version: u64,
    ) -> bool {
        match self.put_if_version_strict(namespace, key, value, expected_version) {
            Ok(written) => written,
            Err(e) => {
                tracing::error!(error = %e, key, "failed to put_if_version item");
                false
            }
        }
    }

    fn search(&self, namespace: &[String], limit: usize, offset: usize) -> Vec<StoreItem> {
        // `BaseStore::search` cannot report failures, so the strict variant is
        // used and errors degrade to an empty page *with a log* — never to
        // silently wrong item keys.
        match self.search_strict(namespace, limit, offset) {
            Ok(items) => items,
            Err(e) => {
                tracing::error!(error = %e, "failed to search namespace");
                Vec::new()
            }
        }
    }

    fn delete(&self, namespace: &[String], key: &str) {
        if let Err(e) = self.delete_strict(namespace, key) {
            tracing::error!(error = %e, key, "failed to delete item");
        }
    }

    /// Declares real CAS support when the client can do ETag-guarded writes.
    ///
    /// Without the precondition the Java `putIfVersion` is a read-check-write sequence, so
    /// two writers can both "win"; only a backend-evaluated precondition may back a session
    /// journal (harness `StoreSessionStorage` refuses anything else).
    fn supports_atomic_session_storage(&self) -> bool {
        self.client.supports_conditional_writes()
    }
}

impl OssStore {
    /// Creates a builder.
    pub fn builder() -> OssStoreBuilder {
        OssStoreBuilder::default()
    }

    /// Loads an item, distinguishing "absent" (`Ok(None)`) from a backend/decode failure.
    ///
    /// Java `OssBaseStore.get` throws `RuntimeException` when the listing/read fails, which
    /// the harness [`BaseStore::get`] signature cannot express; this inherent twin restores
    /// the distinction.
    pub fn get_strict(&self, namespace: &[String], key: &str) -> OssResult<Option<StoreItem>> {
        let data_key = self.data_object_key(namespace, key);
        let version_key = self.version_object_key(namespace, key);
        let Some(json) = self.get_string(&data_key)? else {
            return Ok(None);
        };
        let value = serde_json::from_str::<StoreValue>(&json)?;
        let version = self.read_version_strict(&version_key)?;
        Ok(Some(StoreItem::new(key, value, version)))
    }

    /// Writes an item unconditionally (`version + 1`), propagating backend failures.
    pub fn put_strict(&self, namespace: &[String], key: &str, value: StoreValue) -> OssResult<()> {
        let data_key = self.data_object_key(namespace, key);
        let version_key = self.version_object_key(namespace, key);
        let json = serde_json::to_string(&value)?;
        let current = self.read_version_strict(&version_key)?;
        self.put_string(&data_key, &json)?;
        self.put_string(&version_key, &(current + 1).to_string())
    }

    /// CAS write with an error channel: `Ok(false)` is a failed precondition, `Err` is a
    /// backend/decode failure — the distinction Java gets from "returns false" vs
    /// `RuntimeException`.
    ///
    /// Two objects make up an item (`{key}.json` + `{key}.version`), and object storage has
    /// no multi-object transaction, so the **`.version` sidecar is the CAS anchor**: the
    /// write is decided by one backend-evaluated `If-Match` (or `If-None-Match: *`) on that
    /// object, and the payload is written only by the writer that won. Clients without
    /// conditional writes fall back to the Java read-check-write sequence.
    pub fn put_if_version_strict(
        &self,
        namespace: &[String],
        key: &str,
        value: StoreValue,
        expected_version: u64,
    ) -> OssResult<bool> {
        let data_key = self.data_object_key(namespace, key);
        let version_key = self.version_object_key(namespace, key);
        let json = serde_json::to_string(&value)?;

        if !self.client.supports_conditional_writes() {
            return self.put_if_version_read_check_write(
                &data_key,
                &version_key,
                &json,
                expected_version,
            );
        }

        // Version 0 means "write only if the item does not exist yet"; the data object
        // itself is the thing that must be absent.
        if expected_version == 0 {
            return match self.client.put_conditional(
                &data_key,
                json.as_bytes(),
                &PutCondition::IfNoneMatchAny,
            )? {
                PutOutcome::PreconditionFailed => Ok(false),
                PutOutcome::Written { .. } => {
                    self.put_string(&version_key, "1")?;
                    Ok(true)
                }
            };
        }

        let Some(current_object) = self.client.get_with_etag(&version_key)? else {
            // No version sidecar ⇒ stored version 0, which cannot match `expected_version`.
            return Ok(false);
        };
        let current = parse_version_object(&version_key, &current_object.content)?;
        if current != expected_version {
            return Ok(false);
        }
        match self.client.put_conditional(
            &version_key,
            (current + 1).to_string().as_bytes(),
            &PutCondition::IfMatch(current_object.etag),
        )? {
            PutOutcome::PreconditionFailed => Ok(false),
            PutOutcome::Written { .. } => {
                // The version bump is the commit point: a failure here leaves the new
                // version visible with the previous payload, which is still detectable
                // (and is the only window a two-object layout can offer).
                self.put_string(&data_key, &json)?;
                Ok(true)
            }
        }
    }

    /// Java-compatible non-atomic CAS used when the client has no conditional writes.
    fn put_if_version_read_check_write(
        &self,
        data_key: &str,
        version_key: &str,
        json: &str,
        expected_version: u64,
    ) -> OssResult<bool> {
        let current = self.read_version_strict(version_key)?;
        let matches = if expected_version == 0 {
            !self.client.exists(data_key)?
        } else {
            current == expected_version
        };
        if !matches {
            return Ok(false);
        }
        self.put_string(data_key, json)?;
        self.put_string(version_key, &(current + 1).to_string())?;
        Ok(true)
    }

    /// Deletes the item and its `.version` sidecar, propagating the first failure.
    pub fn delete_strict(&self, namespace: &[String], key: &str) -> OssResult<()> {
        let data_key = self.data_object_key(namespace, key);
        let version_key = self.version_object_key(namespace, key);
        self.client.delete(&data_key)?;
        self.client.delete(&version_key)
    }

    /// Searches `namespace` and returns a page of items, **propagating backend
    /// errors**.
    ///
    /// This mirrors Java `OssBaseStore.search`, which throws `RuntimeException`
    /// when listing/reading fails or when a listed object key does not live
    /// under the requested namespace prefix (Java uses an unchecked
    /// `substring(prefix.length(), …)` there). The harness [`BaseStore::search`]
    /// signature has no error channel, so it delegates to this method and returns
    /// an empty page on error; callers that need the failure should call this
    /// inherent method directly.
    ///
    /// Listing stops as soon as `offset + limit` items are collected and every
    /// [`OssClient::list_page`] call asks for a bounded number of keys, so the work is
    /// bounded by the requested page instead of the namespace size.
    pub fn search_strict(
        &self,
        namespace: &[String],
        limit: usize,
        offset: usize,
    ) -> OssResult<Vec<StoreItem>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let prefix = self.namespace_prefix(namespace);
        // Stop listing as soon as `offset + limit` items are in hand: the backend has no
        // offset, so keys before `offset` must be walked, but there is no reason to list
        // (and keep) the whole namespace.
        let wanted = offset.saturating_add(limit);

        let mut data_keys: Vec<String> = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let remaining = wanted.saturating_sub(data_keys.len());
            // Each item is two objects (`{key}.json` + `{key}.version`), so overscanning
            // by the item count keeps the usual case down to one request.
            let max_keys = remaining
                .saturating_mul(LIST_OVERSCAN)
                .clamp(1, MAX_KEYS_PER_LIST);
            let page = self.client.list_page(&prefix, max_keys, token.as_deref())?;
            data_keys.extend(
                page.objects
                    .into_iter()
                    .map(|object| object.key)
                    // Only `.json` payloads are items; `.version` sidecars are excluded.
                    // (A `.list.json` payload can only appear under a state-store prefix,
                    // never under this store's namespace prefix.)
                    .filter(|key| key.ends_with(JSON_SUFFIX) && !key.ends_with(VERSION_SUFFIX)),
            );
            if !page.truncated || data_keys.len() >= wanted {
                break;
            }
            match page.next_token {
                Some(next) => token = Some(next),
                None => break,
            }
        }
        data_keys.sort();

        let start = offset.min(data_keys.len());
        let end = (start + limit).min(data_keys.len());
        let page = &data_keys[start..end];

        let mut items = Vec::with_capacity(page.len());
        for data_key in page {
            let item_key = data_key
                .strip_prefix(prefix.as_str())
                .and_then(|k| k.strip_suffix(JSON_SUFFIX))
                .ok_or_else(|| {
                    OssError::Backend(format!(
                        "listed object key '{data_key}' is outside namespace prefix '{prefix}'"
                    ))
                })?
                .to_string();
            let Some(json) = self.get_string(data_key)? else {
                continue;
            };
            let value = serde_json::from_str::<StoreValue>(&json)?;
            let version_key = format!(
                "{}{}",
                data_key.strip_suffix(JSON_SUFFIX).unwrap_or(data_key),
                VERSION_SUFFIX
            );
            let version = self.read_version_strict(&version_key)?;
            items.push(StoreItem::new(item_key, value, version));
        }
        Ok(items)
    }

    /// Returns the configured bucket name.
    pub fn bucket_name(&self) -> &str {
        &self.bucket_name
    }

    /// Returns the normalized key prefix.
    pub fn key_prefix(&self) -> &str {
        &self.key_prefix
    }

    // ---- internal helpers ----

    fn namespace_prefix(&self, namespace: &[String]) -> String {
        let mut sb = String::from(&self.key_prefix);
        for component in namespace {
            sb.push_str(component);
            sb.push('/');
        }
        sb
    }

    fn data_object_key(&self, namespace: &[String], key: &str) -> String {
        format!(
            "{}{}{}",
            self.namespace_prefix(namespace),
            strip_leading_slashes(key),
            JSON_SUFFIX
        )
    }

    fn version_object_key(&self, namespace: &[String], key: &str) -> String {
        format!(
            "{}{}{}",
            self.namespace_prefix(namespace),
            strip_leading_slashes(key),
            VERSION_SUFFIX
        )
    }

    /// Reads the `.version` sidecar, treating a malformed value as a backend inconsistency
    /// (Java `Long.parseLong` throws `NumberFormatException` → `RuntimeException`).
    fn read_version_strict(&self, version_key: &str) -> OssResult<u64> {
        match self.client.get(version_key)? {
            None => Ok(0),
            Some(bytes) => parse_version_object(version_key, &bytes),
        }
    }

    fn put_string(&self, object_key: &str, content: &str) -> OssResult<()> {
        self.client.put(object_key, content.as_bytes())
    }

    fn get_string(&self, object_key: &str) -> OssResult<Option<String>> {
        match self.client.get(object_key)? {
            Some(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
            None => Ok(None),
        }
    }
}

/// Parses a `.version` sidecar payload.
///
/// A malformed value is a backend inconsistency (Java `Long.parseLong` throws
/// `NumberFormatException` → `RuntimeException`), which must not be read as version 0.
fn parse_version_object(version_key: &str, content: &[u8]) -> OssResult<u64> {
    let content = String::from_utf8_lossy(content);
    content.trim().parse::<u64>().map_err(|_| {
        OssError::Backend(format!(
            "version object '{version_key}' is not a number: '{content}'"
        ))
    })
}

/// Builder for [`OssStore`].
#[derive(Default)]
pub struct OssStoreBuilder {
    client: Option<Arc<dyn OssClient>>,
    bucket_name: Option<String>,
    key_prefix: Option<String>,
}

impl OssStoreBuilder {
    /// Sets the OSS client (required).
    pub fn client(mut self, client: Arc<dyn OssClient>) -> Self {
        self.client = Some(client);
        self
    }

    /// Sets the target bucket name (required, must not be blank).
    pub fn bucket_name(mut self, bucket: impl Into<String>) -> Self {
        self.bucket_name = Some(bucket.into());
        self
    }

    /// Sets the object key prefix (defaults to [`DEFAULT_KEY_PREFIX`]).
    pub fn key_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.key_prefix = Some(prefix.into());
        self
    }

    /// Builds the store, validating required fields.
    pub fn build(self) -> OssResult<OssStore> {
        let client = self.client.ok_or(OssError::MissingClient)?;
        let bucket_name = self.bucket_name.ok_or(OssError::BlankBucket)?;
        if bucket_name.trim().is_empty() {
            return Err(OssError::BlankBucket);
        }
        let key_prefix = normalize_prefix(self.key_prefix.as_deref(), DEFAULT_KEY_PREFIX);
        Ok(OssStore {
            client,
            bucket_name,
            key_prefix,
        })
    }
}
