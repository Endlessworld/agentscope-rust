//! models.dev-backed model catalog.
//!
//! The catalog is a process-wide, read-mostly snapshot. Requests only read an
//! `Arc<CatalogSnapshot>`; refresh runs in the background and swaps the snapshot after an
//! atomic on-disk write. All lookup APIs are synchronous and never perform I/O.

pub mod config;
pub mod refresh;
pub mod resolve;
pub mod source;
pub mod store;
pub mod types;
pub mod wrap;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::Utc;
use tokio::sync::{watch, Mutex};

use crate::Result;

pub use config::CatalogConfig;
pub use refresh::{CatalogFetcher, FetchResponse, FetchStatus, HttpFetcher};
pub use resolve::{resolve, ResolveRequest};
pub use store::CatalogStorage;
pub use types::{
    CatalogConfidence, CatalogEvent, CatalogSnapshot, Cost, MatchKind, ModalitySet,
    ModelDescriptor, ModelKind, ModelStatus, ReasoningOption, Resolution, SourceInfo, UseCaseSet,
    CATALOG_SCHEMA_VERSION, DEFAULT_CONTEXT_WINDOW, DEFAULT_RESERVED_OUTPUT, MAX_SAFETY_MARGIN,
};
pub use wrap::{descriptor_for, CatalogAwareModel};

/// Result of one refresh attempt.
#[derive(Debug, Clone)]
pub enum RefreshOutcome {
    Updated {
        generation: u64,
        changed_keys: Arc<HashSet<String>>,
    },
    Unchanged,
    /// Another refresh is already running.
    Skipped,
    Disabled,
}

impl RefreshOutcome {
    pub fn generation(&self, fallback: u64) -> u64 {
        match self {
            Self::Updated { generation, .. } => *generation,
            _ => fallback,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ResolveCacheKey {
    generation: u64,
    provider_key: Option<String>,
    model_id: String,
    base_url: Option<String>,
}

struct CatalogInner {
    config: CatalogConfig,
    storage: CatalogStorage,
    fetcher: Arc<dyn CatalogFetcher>,
    snapshot: RwLock<Arc<CatalogSnapshot>>,
    resolve_cache: RwLock<HashMap<ResolveCacheKey, Resolution>>,
    events: watch::Sender<Arc<CatalogEvent>>,
    refresh_lock: Mutex<()>,
    last_error: RwLock<Option<String>>,
    background_started: std::sync::atomic::AtomicBool,
}

/// Cheap, cloneable catalog handle.
#[derive(Clone)]
pub struct ModelCatalog {
    inner: Arc<CatalogInner>,
}

impl ModelCatalog {
    /// Builds a catalog and loads the normalized disk snapshot, if present.
    pub fn new(config: CatalogConfig) -> Self {
        let storage = CatalogStorage::new(config.cache_dir.clone());
        let snapshot = if config.enabled {
            storage.read_snapshot()
        } else {
            None
        };
        let fetcher = Arc::new(HttpFetcher::new(config.timeout));
        Self::build(config, storage, fetcher, snapshot)
    }

    /// Builds a catalog with an injected fetcher (tests, enterprise mirrors, fixtures).
    pub fn with_fetcher(config: CatalogConfig, fetcher: Arc<dyn CatalogFetcher>) -> Self {
        let storage = CatalogStorage::new(config.cache_dir.clone());
        let snapshot = if config.enabled {
            storage.read_snapshot()
        } else {
            None
        };
        Self::build(config, storage, fetcher, snapshot)
    }

    /// Builds a catalog from an already-normalized snapshot.
    pub fn from_snapshot(config: CatalogConfig, mut snapshot: CatalogSnapshot) -> Self {
        snapshot.rebuild_indices();
        let storage = CatalogStorage::new(config.cache_dir.clone());
        let fetcher = Arc::new(HttpFetcher::new(config.timeout));
        Self::build(config, storage, fetcher, Some(snapshot))
    }

    fn build(
        config: CatalogConfig,
        storage: CatalogStorage,
        fetcher: Arc<dyn CatalogFetcher>,
        snapshot: Option<CatalogSnapshot>,
    ) -> Self {
        let snapshot = Arc::new(snapshot.unwrap_or_else(CatalogSnapshot::empty));
        let (events, _) = watch::channel(Arc::new(CatalogEvent {
            generation: snapshot.generation,
            changed_keys: Arc::new(HashSet::new()),
        }));
        Self {
            inner: Arc::new(CatalogInner {
                config,
                storage,
                fetcher,
                snapshot: RwLock::new(snapshot),
                resolve_cache: RwLock::new(HashMap::new()),
                events,
                refresh_lock: Mutex::new(()),
                last_error: RwLock::new(None),
                background_started: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }

    /// Process-wide default catalog.
    pub fn global() -> &'static ModelCatalog {
        static GLOBAL: OnceLock<ModelCatalog> = OnceLock::new();
        GLOBAL.get_or_init(|| {
            let catalog = ModelCatalog::new(CatalogConfig::from_env());
            // The application owns the async runtime; call
            // `spawn_background_refresh()` from its startup path so tests and
            // library users do not accidentally start network I/O.
            catalog
        })
    }

    /// Current immutable snapshot.
    pub fn snapshot(&self) -> Arc<CatalogSnapshot> {
        self.inner.snapshot.read().unwrap().clone()
    }

    /// Current generation. `0` means no snapshot has ever been loaded.
    pub fn generation(&self) -> u64 {
        self.snapshot().generation
    }

    /// Subscribes to content-change events. The initial value is the current generation
    /// with an empty change set.
    pub fn subscribe(&self) -> watch::Receiver<Arc<CatalogEvent>> {
        self.inner.events.subscribe()
    }

    /// Effective configuration.
    pub fn config(&self) -> &CatalogConfig {
        &self.inner.config
    }

    /// Last refresh error, if any.
    pub fn last_error(&self) -> Option<String> {
        self.inner.last_error.read().unwrap().clone()
    }

    /// Whether the snapshot is older than the configured TTL.
    pub fn is_stale(&self) -> bool {
        let snapshot = self.snapshot();
        if snapshot.entries.is_empty() {
            return true;
        }
        let ttl = self.inner.config.ttl;
        let age = Utc::now()
            .signed_duration_since(snapshot.fetched_at)
            .to_std()
            .unwrap_or_default();
        age >= ttl
    }

    /// Resolves a model id against the current snapshot, using a small generation-scoped
    /// result cache.
    pub fn resolve(&self, request: &ResolveRequest<'_>) -> Resolution {
        let snapshot = self.snapshot();
        let key = ResolveCacheKey {
            generation: snapshot.generation,
            provider_key: request.provider_key.map(ToString::to_string),
            model_id: request.model_id.to_string(),
            base_url: request.base_url.map(ToString::to_string),
        };
        if let Some(cached) = self.inner.resolve_cache.read().unwrap().get(&key).cloned() {
            return cached;
        }
        let resolution = resolve::resolve(&snapshot, request);
        tracing::debug!(
            model = request.model_id,
            provider = request.provider_key.unwrap_or(""),
            base_url = request.base_url.unwrap_or(""),
            generation = snapshot.generation,
            kind = ?resolution.kind,
            confidence = resolution.confidence,
            catalog_key = resolution
                .descriptor
                .as_ref()
                .map(|descriptor| descriptor.key.as_str())
                .unwrap_or(""),
            context_window = resolution
                .descriptor
                .as_ref()
                .map(|descriptor| descriptor.context_window)
                .unwrap_or(0),
            "model catalog resolved"
        );
        let mut cache = self.inner.resolve_cache.write().unwrap();
        if cache.len() >= 1024 {
            cache.clear();
        }
        cache.insert(key, resolution.clone());
        resolution
    }

    /// Runs one refresh. Concurrent callers are skipped rather than queued.
    pub async fn refresh(&self) -> Result<RefreshOutcome> {
        if !self.inner.config.enabled {
            return Ok(RefreshOutcome::Disabled);
        }
        let _guard = match self.inner.refresh_lock.try_lock() {
            Ok(guard) => guard,
            Err(_) => return Ok(RefreshOutcome::Skipped),
        };
        let previous = self.snapshot();
        let result = refresh::refresh(
            &self.inner.config,
            &self.inner.storage,
            self.inner.fetcher.as_ref(),
            previous.as_ref(),
        )
        .await;
        match result {
            Ok(result) => {
                let generation = result.snapshot.generation;
                *self.inner.snapshot.write().unwrap() = Arc::clone(&result.snapshot);
                self.inner.resolve_cache.write().unwrap().clear();
                *self.inner.last_error.write().unwrap() = None;
                let changed = !result.changed_keys.is_empty();
                let changed_keys = Arc::new(result.changed_keys);
                let _ = self.inner.events.send_replace(Arc::new(CatalogEvent {
                    generation,
                    changed_keys: Arc::clone(&changed_keys),
                }));
                if changed {
                    Ok(RefreshOutcome::Updated {
                        generation,
                        changed_keys,
                    })
                } else {
                    Ok(RefreshOutcome::Unchanged)
                }
            }
            Err(error) => {
                *self.inner.last_error.write().unwrap() = Some(error.to_string());
                Err(error)
            }
        }
    }

    /// Starts the background refresh loop once. Safe to call from any thread.
    pub fn spawn_background_refresh(&self) {
        use std::sync::atomic::Ordering;
        if !self.inner.config.enabled {
            return;
        }
        if self.inner.background_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.inner.background_started.store(false, Ordering::SeqCst);
            return;
        };
        let catalog = self.clone();
        handle.spawn(async move { catalog.refresh_loop().await });
    }

    async fn refresh_loop(&self) {
        if self.is_stale() {
            if let Err(error) = self.refresh().await {
                tracing::warn!(error = %error, "model catalog: initial refresh failed");
            }
        }
        loop {
            let ttl = self.inner.config.ttl;
            let sleep_for = if ttl.is_zero() {
                Duration::from_secs(60 * 60)
            } else {
                ttl + jitter(self.generation())
            };
            tokio::time::sleep(sleep_for).await;
            if let Err(error) = self.refresh().await {
                tracing::warn!(error = %error, "model catalog: scheduled refresh failed");
            }
        }
    }
}

fn jitter(seed: u64) -> Duration {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos() as u64)
        .unwrap_or(0);
    Duration::from_secs(((nanos ^ seed.rotate_left(7)) % 600).max(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FixtureFetcher {
        calls: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl CatalogFetcher for FixtureFetcher {
        async fn fetch(
            &self,
            url: &str,
            _etag: Option<&str>,
            _max_bytes: usize,
        ) -> Result<FetchResponse> {
            self.calls.lock().unwrap().push(url.to_string());
            let body = if url.contains("models") {
                br#"{"p/m": {"limit": {"context": 100000}}}"#.to_vec()
            } else {
                br#"{"p": {"api": "https://example.test/v1", "models": {"m": {"reasoning": true}}}}"#
                    .to_vec()
            };
            Ok(FetchResponse {
                status: FetchStatus::Ok,
                body,
                etag: None,
                last_modified: None,
            })
        }
    }

    fn config() -> CatalogConfig {
        CatalogConfig {
            models_url: "mem://models".into(),
            api_url: "mem://api".into(),
            cache_dir: std::env::temp_dir()
                .join(format!("agentscope-catalog-unit-{}", uuid::Uuid::new_v4())),
            ..CatalogConfig::default()
        }
    }

    #[test]
    fn empty_catalog_defaults_to_256k() {
        let catalog = ModelCatalog::with_fetcher(
            config(),
            Arc::new(FixtureFetcher {
                calls: Mutex::new(Vec::new()),
            }),
        );
        let result = catalog.resolve(&ResolveRequest::new("unknown"));
        assert!(result.descriptor.is_none());
        assert_eq!(result.kind, MatchKind::Default);
        assert_eq!(catalog.generation(), 0);
    }

    #[tokio::test]
    async fn refresh_swaps_snapshot_and_bumps_generation() {
        let fetcher = Arc::new(FixtureFetcher {
            calls: Mutex::new(Vec::new()),
        });
        let catalog = ModelCatalog::with_fetcher(config(), fetcher);
        let outcome = catalog.refresh().await.unwrap();
        assert!(matches!(
            outcome,
            RefreshOutcome::Updated { generation: 1, .. }
        ));
        assert_eq!(catalog.generation(), 1);
        let result = catalog.resolve(&ResolveRequest::new("m"));
        assert_eq!(result.kind, MatchKind::ExactBare);
        assert!(result.descriptor.unwrap().reasoning);
    }

    #[tokio::test]
    async fn concurrent_refresh_is_skipped_not_queued() {
        let fetcher = Arc::new(FixtureFetcher {
            calls: Mutex::new(Vec::new()),
        });
        let catalog = ModelCatalog::with_fetcher(config(), fetcher);
        let guard = catalog.inner.refresh_lock.lock().await;
        let outcome = catalog.refresh().await.unwrap();
        assert!(matches!(outcome, RefreshOutcome::Skipped));
        drop(guard);
    }
}
