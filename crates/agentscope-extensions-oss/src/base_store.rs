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
//! # Search error semantics
//!
//! [`OssStore::search_strict`] mirrors the Java error behaviour of
//! `OssBaseStore.search`: a listed object key that falls outside the requested
//! namespace prefix (or malformed item JSON) is a backend inconsistency and
//! surfaces as [`OssError`] (Java throws `RuntimeException` from its unchecked
//! `substring`). The harness [`BaseStore::search`] signature has no error channel,
//! so it delegates to `search_strict` and returns an empty page on error instead
//! of fabricating item keys.

use std::sync::Arc;

use agentscope_harness::filesystem::remote::store::{BaseStore, StoreItem, StoreValue};

use crate::client::{normalize_prefix, strip_leading_slashes, OssClient};
use crate::error::{OssError, OssResult};

/// Default key prefix for base-store objects.
pub const DEFAULT_KEY_PREFIX: &str = "agentscope/store/";
const JSON_SUFFIX: &str = ".json";
const VERSION_SUFFIX: &str = ".version";

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
        let data_key = self.data_object_key(namespace, key);
        let version_key = self.version_object_key(namespace, key);
        match self.get_string(&data_key) {
            Ok(Some(json)) => {
                let value = serde_json::from_str::<StoreValue>(&json).ok()?;
                let version = self.read_version(&version_key);
                Some(StoreItem::new(key, value, version))
            }
            Ok(None) => None,
            Err(e) => {
                tracing::error!(error = %e, key, "failed to get item");
                None
            }
        }
    }

    fn put(&self, namespace: &[String], key: &str, value: StoreValue) {
        if let Err(e) = (|| -> OssResult<()> {
            let data_key = self.data_object_key(namespace, key);
            let version_key = self.version_object_key(namespace, key);
            let json = serde_json::to_string(&value)?;
            self.put_string(&data_key, &json)?;
            let current = self.read_version(&version_key);
            self.put_string(&version_key, &(current + 1).to_string())?;
            Ok(())
        })() {
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
        let result = (|| -> OssResult<bool> {
            let data_key = self.data_object_key(namespace, key);
            let version_key = self.version_object_key(namespace, key);
            let current = self.read_version(&version_key);

            // CAS: version 0 means "write only if not present".
            let matches = if expected_version == 0 {
                !self.client.exists(&data_key)?
            } else {
                current == expected_version
            };
            if !matches {
                return Ok(false);
            }
            let json = serde_json::to_string(&value)?;
            self.put_string(&data_key, &json)?;
            self.put_string(&version_key, &(current + 1).to_string())?;
            Ok(true)
        })();

        match result {
            Ok(v) => v,
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
        let data_key = self.data_object_key(namespace, key);
        let version_key = self.version_object_key(namespace, key);
        if let Err(e) = self.client.delete(&data_key) {
            tracing::error!(error = %e, key, "failed to delete item data");
        }
        let _ = self.client.delete(&version_key);
    }
}

impl OssStore {
    /// Creates a builder.
    pub fn builder() -> OssStoreBuilder {
        OssStoreBuilder::default()
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
    pub fn search_strict(
        &self,
        namespace: &[String],
        limit: usize,
        offset: usize,
    ) -> OssResult<Vec<StoreItem>> {
        let prefix = self.namespace_prefix(namespace);
        let mut data_keys: Vec<String> = self
            .client
            .list_all(&prefix)?
            .into_iter()
            // Only `.json` payloads are items; `.version` sidecars are excluded.
            // (A `.list.json` payload can only appear under a state-store prefix,
            // never under this store's namespace prefix.)
            .filter(|k| k.ends_with(JSON_SUFFIX) && !k.ends_with(VERSION_SUFFIX))
            .collect();
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
            let version = self.read_version(&version_key);
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

    fn read_version(&self, version_key: &str) -> u64 {
        match self.get_string(version_key) {
            Ok(Some(content)) => content.trim().parse::<u64>().unwrap_or(0),
            _ => 0,
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
        Ok(OssStore { client, bucket_name, key_prefix })
    }
}
