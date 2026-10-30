//! Guards for the *resolution* semantics of `agentscope_core::ModelRegistry`,
//! the registry every provider in this crate is registered into.
//!
//! Java commit `9e716cb27` ("evict ModelRegistry cache on registerFactory and
//! share concurrent first creation") fixed two defects in
//! `io.agentscope.core.model.ModelRegistry`:
//!
//! 1. `registerFactory` never invalidated the factory-resolution cache, so an id
//!    that had already been resolved kept returning the *older* factory's
//!    instance;
//! 2. concurrent first resolution was `get` → create → `put`, so N racing callers
//!    each ran the factory and N-1 instances were orphaned.
//!
//! The Rust registry (`crates/agentscope-core/src/model/provider.rs`) is
//! **uncached**: `register` takes `&mut self` and replaces by `provider_id`,
//! while `resolve` builds a fresh instance on every call. Both defects are
//! therefore structurally absent — there is no cache to invalidate and no
//! creation to share. The tests below pin the two observable invariants a cached
//! registry would have to preserve (a re-registration wins on the very next
//! resolution, a failed creation is never retained), plus the current
//! no-cache contract itself, so that adding a resolution cache upstream cannot
//! silently reintroduce the Java bug.
//!
//! The "share one creation across concurrent first callers" half has no Rust
//! counterpart to port; it is recorded as a cross-module request in
//! `docs/extensions/MODEL__GAP_TRACKER.md` §4.2 #11.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use agentscope_core::{
    ChatResponse, Error, GenerateOptions, Model, ModelCreationContext, ModelProvider,
    ModelRegistry, Msg, Result, ToolSchema,
};
use futures::stream::BoxStream;

/// A model that only exposes its name; it never touches the network.
struct StubModel {
    name: String,
}

impl Model for StubModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> BoxStream<'_, Result<ChatResponse>> {
        Box::pin(futures::stream::empty())
    }

    fn model_name(&self) -> &str {
        &self.name
    }
}

/// Records every `create` call so a test can tell "reused instance" from
/// "the provider ran again".
struct StubProvider {
    id: &'static str,
    prefix: &'static str,
    label: &'static str,
    calls: Arc<AtomicUsize>,
    /// Fail the very first creation, then succeed (mirrors Java
    /// `resolve_failedCreation_isNotCached`).
    fail_first: bool,
}

impl StubProvider {
    fn new(id: &'static str, prefix: &'static str, label: &'static str) -> Self {
        Self {
            id,
            prefix,
            label,
            calls: Arc::new(AtomicUsize::new(0)),
            fail_first: false,
        }
    }
}

impl ModelProvider for StubProvider {
    fn provider_id(&self) -> &str {
        self.id
    }

    fn supports(&self, model: &str) -> bool {
        model.trim().starts_with(self.prefix)
    }

    fn create(&self, ctx: ModelCreationContext) -> Result<Arc<dyn Model>> {
        let nth = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if self.fail_first && nth == 1 {
            return Err(Error::Model(format!("{} factory boom", self.label)));
        }
        Ok(Arc::new(StubModel {
            name: format!("{}:{}", self.label, ctx.model),
        }))
    }
}

/// Registers `provider` and returns its creation counter.
fn register(registry: &mut ModelRegistry, provider: Arc<StubProvider>) -> Arc<AtomicUsize> {
    let calls = provider.calls.clone();
    let provider: Arc<dyn ModelProvider> = provider;
    registry.register(provider);
    calls
}

#[test]
fn re_registering_a_provider_wins_on_the_next_resolution() {
    // Java `registerFactory_evictsCachedInstancesMatchingNewPattern`, Rust shape:
    // `register` replaces the entry with the same `provider_id`, and because the
    // registry holds no instances, the next `resolve` must consult the new
    // provider instead of handing back the previously built model.
    let mut registry = ModelRegistry::new();
    register(
        &mut registry,
        Arc::new(StubProvider::new("acme", "m", "old")),
    );
    register(
        &mut registry,
        Arc::new(StubProvider::new("other", "m", "other")),
    );

    let before = registry
        .resolve("acme:m1")
        .expect("resolve before re-registration");
    assert_eq!(before.model_name(), "old:m1");
    assert_eq!(
        registry
            .resolve("other:m1")
            .expect("resolve an unrelated provider")
            .model_name(),
        "other:m1"
    );

    register(
        &mut registry,
        Arc::new(StubProvider::new("acme", "m", "new")),
    );

    let after = registry
        .resolve("acme:m1")
        .expect("resolve after re-registration");
    assert_eq!(
        after.model_name(),
        "new:m1",
        "a re-registered provider must not be shadowed by a stale resolution"
    );
    // Replacing one provider must not disturb the others.
    assert_eq!(
        registry
            .resolve("other:m1")
            .expect("resolve the unrelated provider again")
            .model_name(),
        "other:m1"
    );
}

#[test]
fn resolution_is_uncached_so_repeat_calls_build_independent_instances() {
    // This is the Rust contract the Java fix presumes: `resolve` is a factory
    // call, not a cache lookup. If core ever grows a resolution cache, this test
    // must be revisited together with `docs/extensions/MODEL__GAP_TRACKER.md`
    // §4.2 #11 (eviction + single-flight would then become mandatory).
    let mut registry = ModelRegistry::new();
    let calls = register(
        &mut registry,
        Arc::new(StubProvider::new("acme", "m", "only")),
    );

    let first = registry.resolve("acme:m1").expect("first resolution");
    let second = registry.resolve("acme:m1").expect("second resolution");

    assert_eq!(first.model_name(), second.model_name());
    assert!(
        !Arc::ptr_eq(&first, &second),
        "ModelRegistry must not retain resolved instances"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "each resolve must ask the provider, not a cache"
    );
}

#[test]
fn a_failed_creation_is_not_retained_and_the_next_resolution_retries() {
    // Java `resolve_failedCreation_isNotCached`: failures must not be cached in
    // any form, so a later caller retries the provider.
    let mut registry = ModelRegistry::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(StubProvider {
        id: "flaky",
        prefix: "m",
        label: "flaky",
        calls: calls.clone(),
        fail_first: true,
    });
    register(&mut registry, provider);

    let first = registry.resolve("flaky:m1");
    // `unwrap_err()` is unavailable: `Arc<dyn Model>` is not `Debug`.
    assert!(
        first.is_err(),
        "the first creation must surface its failure"
    );

    let recovered = registry
        .resolve("flaky:m1")
        .expect("the retry must reach the provider again");
    assert_eq!(recovered.model_name(), "flaky:m1");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn concurrent_resolutions_are_shared_safe_and_never_reuse_one_instance() {
    // Java `resolve_concurrentFirstResolution_createsSingleInstance` relies on a
    // `CompletableFuture` cache. In Rust the registry is uncached, so concurrent
    // first callers all succeed and each gets its own instance; the point of this
    // test is that sharing one registry across threads is safe (no interior
    // mutability, no deadlock) *and* that no hidden cache appears.
    let mut registry = ModelRegistry::new();
    let calls = register(
        &mut registry,
        Arc::new(StubProvider::new("acme", "m", "race")),
    );
    let registry = Arc::new(registry);

    let threads = 8;
    let barrier = Arc::new(Barrier::new(threads));
    let mut handles = Vec::with_capacity(threads);
    for _ in 0..threads {
        let registry = Arc::clone(&registry);
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            registry
                .resolve("acme:m1")
                .map(|model| model.model_name().to_string())
        }));
    }

    let names: Vec<String> = handles
        .into_iter()
        .map(|handle| {
            handle
                .join()
                .expect("resolution thread panicked")
                .expect("concurrent resolution failed")
        })
        .collect();

    assert_eq!(names.len(), threads);
    assert!(names.iter().all(|name| name == "race:m1"));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        threads,
        "without a cache there is no single-flight to share; see tracker §4.2 #11"
    );
}
