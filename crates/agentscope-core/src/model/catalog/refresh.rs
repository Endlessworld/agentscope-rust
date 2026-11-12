//! Background refresh: HTTP fetching, source caching, merging, and generation diffing.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use reqwest::header::{ACCEPT, ETAG, IF_NONE_MATCH, LAST_MODIFIED};
use reqwest::StatusCode;

use super::config::CatalogConfig;
use super::source::{merge_descriptors, parse_api_json, parse_models_json, ParsedSource};
use super::store::{sha256_hex, CatalogStorage};
use super::types::{CatalogSnapshot, ModelDescriptor, SourceInfo};
use crate::Result;

const MODELS_SOURCE: &str = "models";
const API_SOURCE: &str = "api";

/// Result of one HTTP fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchStatus {
    Ok,
    NotModified,
}

/// One fetched payload.
#[derive(Debug)]
pub struct FetchResponse {
    pub status: FetchStatus,
    pub body: Vec<u8>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// Abstraction that lets tests run the full refresh without network access.
#[async_trait]
pub trait CatalogFetcher: Send + Sync {
    async fn fetch(&self, url: &str, etag: Option<&str>, max_bytes: usize)
        -> Result<FetchResponse>;
}

/// Reqwest-backed fetcher. The client is shared across refreshes.
pub struct HttpFetcher {
    /// `None` = 客户端构造失败（已打印原因），本次进程不刷新目录
    client: Option<reqwest::Client>,
}

impl HttpFetcher {
    pub fn new(timeout: Duration) -> Self {
        crate::tls::ensure_crypto_provider();
        let built = crate::tls::apply_tls_defaults(reqwest::Client::builder())
            .timeout(timeout)
            .user_agent(concat!("agentscope-rust/", env!("CARGO_PKG_VERSION")))
            .build();
        match built {
            Ok(client) => Self {
                client: Some(client),
            },
            Err(error) => {
                // 不能回落到 `Client::new()`：它走默认 TLS 后端，在 Android 上是
                // rustls-platform-verifier，首次握手就 panic（嵌入式 profile 下直接 abort）。
                eprintln!("[model-catalog] 无法创建 HTTP 客户端：{error:?}；本次不刷新模型目录");
                Self { client: None }
            }
        }
    }
}

#[async_trait]
impl CatalogFetcher for HttpFetcher {
    async fn fetch(
        &self,
        url: &str,
        etag: Option<&str>,
        max_bytes: usize,
    ) -> Result<FetchResponse> {
        let Some(client) = &self.client else {
            return Err(crate::Error::Http(
                "model catalog: HTTP client unavailable".to_string(),
            ));
        };
        let mut request = client.get(url).header(ACCEPT, "application/json");
        if let Some(etag) = etag {
            request = request.header(IF_NONE_MATCH, etag);
        }
        let response = request
            .send()
            .await
            .map_err(|error| crate::Error::Http(error.to_string()))?;
        if response.status() == StatusCode::NOT_MODIFIED {
            return Ok(FetchResponse {
                status: FetchStatus::NotModified,
                body: Vec::new(),
                etag: header(&response, ETAG),
                last_modified: header(&response, LAST_MODIFIED),
            });
        }
        if !response.status().is_success() {
            return Err(crate::Error::http_status(
                response.status().as_u16(),
                format!("catalog: GET {url} returned {}", response.status()),
            ));
        }
        let content_type = header(&response, reqwest::header::CONTENT_TYPE)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !content_type.starts_with("application/json") {
            return Err(crate::Error::Http(format!(
                "catalog: GET {url} returned content-type '{content_type}'"
            )));
        }
        if response
            .content_length()
            .is_some_and(|len| len > max_bytes as u64)
        {
            return Err(crate::Error::Http(format!(
                "catalog: GET {url} exceeds {max_bytes} bytes"
            )));
        }
        let etag = header(&response, ETAG);
        let last_modified = header(&response, LAST_MODIFIED);
        let body = response
            .bytes()
            .await
            .map_err(|error| crate::Error::Http(error.to_string()))?
            .to_vec();
        if body.len() > max_bytes {
            return Err(crate::Error::Http(format!(
                "catalog: GET {url} exceeds {max_bytes} bytes"
            )));
        }
        Ok(FetchResponse {
            status: FetchStatus::Ok,
            body,
            etag,
            last_modified,
        })
    }
}

fn header(response: &reqwest::Response, name: reqwest::header::HeaderName) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string)
}

struct FetchedSource {
    bytes: Vec<u8>,
    meta: SourceInfo,
    not_modified: bool,
}

/// One refresh result.
pub(crate) struct RefreshResult {
    pub snapshot: Arc<CatalogSnapshot>,
    pub changed_keys: HashSet<String>,
}

/// Refreshes both sources and builds a new snapshot.
///
/// A flat-source failure aborts the refresh and leaves the previous snapshot in place.
/// A detailed-source failure degrades to the flat source (or its last cached raw payload)
/// so a single endpoint outage does not lose context windows.
pub(crate) async fn refresh(
    config: &CatalogConfig,
    storage: &CatalogStorage,
    fetcher: &dyn CatalogFetcher,
    previous: &CatalogSnapshot,
) -> Result<RefreshResult> {
    let models = fetch_source(
        fetcher,
        storage,
        MODELS_SOURCE,
        &config.models_url,
        config.max_models_bytes,
        true,
    )
    .await?;

    let api = if config.use_api {
        match fetch_source(
            fetcher,
            storage,
            API_SOURCE,
            &config.api_url,
            config.max_api_bytes,
            false,
        )
        .await
        {
            Ok(source) => Some(source),
            Err(error) => {
                tracing::warn!(error = %error, "model catalog: detailed source unavailable; using flat source");
                storage.read_raw(API_SOURCE).map(|bytes| FetchedSource {
                    meta: storage.read_meta(API_SOURCE).unwrap_or_else(|| {
                        source_info(API_SOURCE, &config.api_url, None, None, &bytes)
                    }),
                    bytes,
                    not_modified: true,
                })
            }
        }
    } else {
        None
    };

    let flat = parse_models_json(&models.bytes)?;
    let detailed = api
        .as_ref()
        .map(|source| parse_api_json(&source.bytes))
        .transpose()?;
    // 详细源不可用时（移动网络下 6 MB 的 api.json 经常超时）必须沿用上一份快照的
    // provider 表：`catalog_providers` 靠 `provider_api` 才认得出 provider（否则只剩
    // `FALLBACK_BASE_URLS` 里那几家），靠 `provider_env` 才认得出环境变量里的密钥。
    // 两张表被清空等于把目录废掉，而且会连累条目：扁平源没有明细字段。
    let (provider_api, provider_env) = match &detailed {
        Some(parsed) => (parsed.provider_api.clone(), parsed.provider_env.clone()),
        None => (previous.provider_api.clone(), previous.provider_env.clone()),
    };
    let entries = match detailed {
        Some(detailed) => merge_sources(flat, Some(detailed)),
        None => merge_with_previous(flat, &previous.entries),
    };

    let not_modified = models.not_modified
        && api
            .as_ref()
            .map(|source| source.not_modified)
            .unwrap_or(true);
    let mut sources = vec![models.meta];
    if let Some(api) = &api {
        sources.push(api.meta.clone());
    }
    let mut snapshot = CatalogSnapshot::empty();
    snapshot.entries = entries;
    snapshot.provider_api = provider_api;
    snapshot.provider_env = provider_env;
    snapshot.sources = sources;
    snapshot.rebuild_indices();

    let changed_keys = diff_keys(previous, &snapshot);
    snapshot.generation = if !changed_keys.is_empty() {
        previous.generation + 1
    } else {
        previous.generation
    };
    if !not_modified {
        storage.write_snapshot(&snapshot)?;
    } else if snapshot.provider_env != previous.provider_env
        || snapshot.provider_api != previous.provider_api
    {
        // A `304` normally skips the write, but the derived tables can still gain data
        // from the raw sources (a new field, e.g. `provider_env`): write then, otherwise
        // the next process keeps reading a cache that lacks them forever.
        storage.write_snapshot(&snapshot)?;
    }
    Ok(RefreshResult {
        snapshot: Arc::new(snapshot),
        changed_keys,
    })
}

async fn fetch_source(
    fetcher: &dyn CatalogFetcher,
    storage: &CatalogStorage,
    name: &str,
    url: &str,
    max_bytes: usize,
    required: bool,
) -> Result<FetchedSource> {
    let old_meta = storage.read_meta(name);
    let etag = old_meta.as_ref().and_then(|meta| meta.etag.as_deref());
    let response = fetcher.fetch(url, etag, max_bytes).await;
    let response = match response {
        Ok(response) => response,
        Err(error) if required => return Err(error),
        Err(error) => {
            tracing::warn!(source = name, error = %error, "model catalog: fetch failed");
            let bytes = storage.read_raw(name).ok_or(error)?;
            return Ok(FetchedSource {
                meta: old_meta.unwrap_or_else(|| source_info(name, url, None, None, &bytes)),
                bytes,
                not_modified: true,
            });
        }
    };
    match response.status {
        FetchStatus::NotModified => {
            let bytes = storage.read_raw(name).ok_or_else(|| {
                crate::Error::Model(format!(
                    "catalog: {name} returned 304 but no cached raw payload exists"
                ))
            })?;
            let mut meta = source_info(name, url, response.etag, response.last_modified, &bytes);
            if let Some(old) = &old_meta {
                meta.fetched_at = old.fetched_at;
                if meta.etag.is_none() {
                    meta.etag = old.etag.clone();
                }
                if meta.last_modified.is_none() {
                    meta.last_modified = old.last_modified.clone();
                }
            }
            storage.write_meta(name, &meta)?;
            Ok(FetchedSource {
                bytes,
                meta,
                not_modified: true,
            })
        }
        FetchStatus::Ok => {
            let meta = source_info(
                name,
                url,
                response.etag,
                response.last_modified,
                &response.body,
            );
            storage.write_raw(name, &response.body)?;
            storage.write_meta(name, &meta)?;
            Ok(FetchedSource {
                bytes: response.body,
                meta,
                not_modified: false,
            })
        }
    }
}

fn source_info(
    name: &str,
    url: &str,
    etag: Option<String>,
    last_modified: Option<String>,
    bytes: &[u8],
) -> SourceInfo {
    let now = Utc::now();
    SourceInfo {
        name: name.to_string(),
        url: url.to_string(),
        etag,
        last_modified,
        fetched_at: now,
        checked_at: now,
        bytes: bytes.len() as u64,
        sha256: Some(sha256_hex(bytes)),
    }
}

fn merge_sources(
    flat: ParsedSource,
    detailed: Option<ParsedSource>,
) -> HashMap<String, Arc<ModelDescriptor>> {
    let mut entries: HashMap<String, ModelDescriptor> = flat
        .entries
        .into_iter()
        .map(|entry| (entry.key.clone(), entry))
        .collect();
    if let Some(detailed) = detailed {
        for enhanced in detailed.entries {
            if let Some(base) = entries.remove(&enhanced.key) {
                entries.insert(enhanced.key.clone(), merge_descriptors(base, enhanced));
            } else {
                entries.insert(enhanced.key.clone(), enhanced);
            }
        }
    }
    entries
        .into_iter()
        .map(|(key, mut descriptor)| {
            descriptor.recompute_fingerprint();
            (key, Arc::new(descriptor))
        })
        .collect()
}

/// 只有扁平源可用时，以既有快照为底合并。
///
/// 扁平源负责更新它认识的 key，旧快照里其余的条目原样保留；同 key 时把旧条目当 base、
/// 扁平条目当 enhanced（扁平源没有 context/cost 等明细，不能反过来把旧条目的字段抹掉）。
fn merge_with_previous(
    flat: ParsedSource,
    previous: &HashMap<String, Arc<ModelDescriptor>>,
) -> HashMap<String, Arc<ModelDescriptor>> {
    let mut entries: HashMap<String, ModelDescriptor> = previous
        .iter()
        .map(|(key, descriptor)| (key.clone(), (**descriptor).clone()))
        .collect();
    for enhanced in flat.entries {
        match entries.remove(&enhanced.key) {
            Some(base) => {
                entries.insert(enhanced.key.clone(), merge_descriptors(base, enhanced));
            }
            None => {
                entries.insert(enhanced.key.clone(), enhanced);
            }
        }
    }
    entries
        .into_iter()
        .map(|(key, mut descriptor)| {
            descriptor.recompute_fingerprint();
            (key, Arc::new(descriptor))
        })
        .collect()
}

fn diff_keys(previous: &CatalogSnapshot, next: &CatalogSnapshot) -> HashSet<String> {
    let mut changed = HashSet::new();
    for (key, descriptor) in &next.entries {
        match previous.entries.get(key) {
            None => {
                changed.insert(key.clone());
            }
            Some(old) if old.fingerprint != descriptor.fingerprint => {
                changed.insert(key.clone());
            }
            Some(_) => {}
        }
    }
    for key in previous.entries.keys() {
        if !next.entries.contains_key(key) {
            changed.insert(key.clone());
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct MemoryFetcher {
        responses: Mutex<HashMap<String, std::result::Result<(Vec<u8>, Option<String>), String>>>,
        not_modified: Mutex<std::collections::HashSet<String>>,
        calls: Mutex<Vec<String>>,
    }

    impl MemoryFetcher {
        fn new() -> Self {
            Self {
                responses: Mutex::new(HashMap::new()),
                not_modified: Mutex::new(std::collections::HashSet::new()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn insert(&self, url: &str, body: impl Into<Vec<u8>>, etag: Option<&str>) {
            self.responses.lock().unwrap().insert(
                url.to_string(),
                Ok((body.into(), etag.map(ToString::to_string))),
            );
        }

        fn fail(&self, url: &str) {
            self.responses
                .lock()
                .unwrap()
                .insert(url.to_string(), Err("offline".to_string()));
        }

        /// Answers every request for `url` with `304 Not Modified`.
        fn not_modified(&self, url: &str) {
            self.not_modified.lock().unwrap().insert(url.to_string());
        }
    }

    #[async_trait]
    impl CatalogFetcher for MemoryFetcher {
        async fn fetch(
            &self,
            url: &str,
            _etag: Option<&str>,
            _max_bytes: usize,
        ) -> Result<FetchResponse> {
            self.calls.lock().unwrap().push(url.to_string());
            if self.not_modified.lock().unwrap().contains(url) {
                return Ok(FetchResponse {
                    status: FetchStatus::NotModified,
                    body: Vec::new(),
                    etag: None,
                    last_modified: None,
                });
            }
            match self.responses.lock().unwrap().get(url).cloned() {
                Some(Ok((body, etag))) => Ok(FetchResponse {
                    status: FetchStatus::Ok,
                    body,
                    etag,
                    last_modified: None,
                }),
                Some(Err(error)) => Err(crate::Error::Http(error)),
                None => Err(crate::Error::Http(format!("unexpected URL {url}"))),
            }
        }
    }

    fn config() -> CatalogConfig {
        CatalogConfig {
            models_url: "mem://models".into(),
            api_url: "mem://api".into(),
            ttl: Duration::from_secs(3600),
            ..CatalogConfig::default()
        }
    }

    #[tokio::test]
    async fn refresh_builds_snapshot_and_increments_generation() {
        let storage = CatalogStorage::new(
            std::env::temp_dir().join(format!("agentscope-refresh-test-{}", uuid::Uuid::new_v4())),
        );
        let fetcher = MemoryFetcher::new();
        fetcher.insert(
            "mem://models",
            br#"{"p/m": {"limit": {"context": 100000}}}"#.to_vec(),
            Some("v1"),
        );
        fetcher.insert(
            "mem://api",
            br#"{"p": {"api": "https://example.test/v1", "models": {"m": {"reasoning": true}}}}"#
                .to_vec(),
            Some("v1"),
        );
        let previous = CatalogSnapshot::empty();
        let result = refresh(&config(), &storage, &fetcher, &previous)
            .await
            .unwrap();
        assert_eq!(result.snapshot.generation, 1);
        assert!(!result.changed_keys.is_empty());
        assert_eq!(result.snapshot.entries.len(), 1);
        let loaded = storage.read_snapshot().unwrap();
        assert_eq!(loaded.generation, 1);
        let _ = std::fs::remove_dir_all(storage.root());
    }

    /// A `304` still re-parses the raw sources, so the derived tables can grow (a new
    /// normalised field). That growth has to reach the cache, or every later process keeps
    /// reading a snapshot that lacks it.
    #[tokio::test]
    async fn a_not_modified_refresh_persists_newly_derived_tables() {
        let storage = CatalogStorage::new(
            std::env::temp_dir().join(format!("agentscope-refresh-test-{}", uuid::Uuid::new_v4())),
        );
        let fetcher = MemoryFetcher::new();
        fetcher.insert(
            "mem://models",
            br#"{"p/m": {"limit": {"context": 100000}}}"#.to_vec(),
            Some("v1"),
        );
        fetcher.insert(
            "mem://api",
            br#"{"p": {"api": "https://example.test/v1", "env": ["P_API_KEY"], "models": {}}}"#
                .to_vec(),
            Some("v1"),
        );
        refresh(&config(), &storage, &fetcher, &CatalogSnapshot::empty())
            .await
            .unwrap();

        // Simulate a cache written before `provider_env` existed.
        let mut stale = storage.read_snapshot().unwrap();
        stale.provider_env.clear();
        storage.write_snapshot(&stale).unwrap();

        fetcher.not_modified("mem://models");
        fetcher.not_modified("mem://api");
        let result = refresh(&config(), &storage, &fetcher, &stale)
            .await
            .unwrap();
        assert_eq!(result.snapshot.provider_env["p"], ["P_API_KEY"]);
        let persisted = storage.read_snapshot().unwrap();
        assert_eq!(
            persisted.provider_env["p"],
            ["P_API_KEY"],
            "a 304 must not strand the newly derived table on this process only"
        );
        let _ = std::fs::remove_dir_all(storage.root());
    }

    #[tokio::test]
    async fn detailed_failure_degrades_to_flat_source() {
        let storage = CatalogStorage::new(
            std::env::temp_dir().join(format!("agentscope-refresh-test-{}", uuid::Uuid::new_v4())),
        );
        let fetcher = MemoryFetcher::new();
        fetcher.insert(
            "mem://models",
            br#"{"p/m": {"limit": {"context": 100000}}}"#.to_vec(),
            None,
        );
        fetcher.fail("mem://api");
        let result = refresh(&config(), &storage, &fetcher, &CatalogSnapshot::empty())
            .await
            .unwrap();
        assert_eq!(result.snapshot.entries.len(), 1);
        assert_eq!(result.snapshot.entries["p/m"].context_window, 100_000);
        let _ = std::fs::remove_dir_all(storage.root());
    }

    /// 移动网络下 6 MB 的 `api.json` 经常超时。降级到扁平源时**不能**丢掉上一份快照
    /// 的派表：`provider_api` 空了会让 `catalog_providers` 只剩下 `FALLBACK_BASE_URLS`
    /// 里那几家，`provider_env` 空了则环境变量里的密钥再也对不上 provider。
    #[tokio::test]
    async fn detailed_failure_keeps_provider_tables_and_previous_entries() {
        let root =
            std::env::temp_dir().join(format!("agentscope-refresh-test-{}", uuid::Uuid::new_v4()));
        let fetcher = MemoryFetcher::new();
        fetcher.insert(
            "mem://models",
            br#"{"p/m": {"limit": {"context": 100000}}}"#.to_vec(),
            Some("v1"),
        );
        fetcher.insert(
            "mem://api",
            br#"{"p": {"api": "https://example.test/v1", "env": ["P_API_KEY"], "models": {"m": {"reasoning": true}}}}"#
                .to_vec(),
            Some("v1"),
        );
        let first = refresh(
            &config(),
            &CatalogStorage::new(root.join("first")),
            &fetcher,
            &CatalogSnapshot::empty(),
        )
        .await
        .unwrap()
        .snapshot;
        assert_eq!(first.provider_api["p"], "https://example.test/v1");

        // 新缓存目录：没有可回退的 api raw，详细源这次彻底不可用。
        fetcher.fail("mem://api");
        fetcher.insert(
            "mem://models",
            br#"{"p/m": {"limit": {"context": 200000}}, "q/n": {"limit": {"context": 8000}}}"#
                .to_vec(),
            Some("v2"),
        );
        let second = refresh(
            &config(),
            &CatalogStorage::new(root.join("second")),
            &fetcher,
            &first,
        )
        .await
        .unwrap()
        .snapshot;

        assert_eq!(
            second.provider_api["p"], "https://example.test/v1",
            "provider_api must survive a detailed-source failure"
        );
        assert_eq!(second.provider_env["p"], ["P_API_KEY"]);
        assert!(
            second.entries.contains_key("q/n"),
            "new flat entries must still be added"
        );
        let pm = &second.entries["p/m"];
        assert_eq!(pm.context_window, 200_000, "flat source updates the detail");
        assert!(
            pm.reasoning,
            "flat source must not erase an api-only detail"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    #[ignore = "hits models.dev; validates compressed HTTP fetch"]
    async fn fetches_real_models_dev_payload() {
        let fetcher = HttpFetcher::new(Duration::from_secs(30));
        let response = fetcher
            .fetch(
                super::super::config::DEFAULT_MODELS_URL,
                None,
                4 * 1024 * 1024,
            )
            .await
            .expect("models.dev fetch");
        assert_eq!(response.status, FetchStatus::Ok);
        let parsed = super::super::source::parse_models_json(&response.body).unwrap();
        assert!(
            parsed.entries.len() > 500,
            "entries={}",
            parsed.entries.len()
        );
    }
}
