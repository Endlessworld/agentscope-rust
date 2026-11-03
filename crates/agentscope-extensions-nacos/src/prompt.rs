//! Nacos prompt listening, mirroring `io.agentscope.core.nacos.prompt`.
//!
//! Each `(name, version, label)` selection owns its cached template and subscription: Nacos
//! 3.3.0 routes prompt events per selection, so different versions/labels of the same prompt
//! never overwrite each other. Rendering combines the template's variable defaults with the
//! caller's args (caller wins). Missing/deleted templates use the caller's default, or an empty
//! string. Different selections load concurrently; initialization is serialized only for the
//! same selection. [`NacosPromptListener::close`] waits for in-flight reads before cancelling
//! the subscriptions it created (the shared service stays open).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{Mutex as AsyncMutex, RwLock};
use tracing::warn;

use crate::service::{
    normalize_selectors, render_template, AiServiceError, AiServiceResult, NacosAiService,
    NacosPrompt, PromptListener,
};

/// Sentinel for "prompt not found in Nacos", mirroring Java's `EMPTY_SENTINEL`.
fn empty_sentinel() -> NacosPrompt {
    NacosPrompt::new("")
}

/// One `(name, version, label)` prompt selection.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PromptSelection {
    prompt_key: String,
    version: Option<String>,
    label: Option<String>,
}

impl PromptSelection {
    /// Builds a selection with trimmed selectors, rejecting a blank key or a version *and* label
    /// at once (Java: `IllegalArgumentException`).
    fn new(prompt_key: &str, version: Option<&str>, label: Option<&str>) -> AiServiceResult<Self> {
        if prompt_key.trim().is_empty() {
            return Err(AiServiceError::new(
                AiServiceError::INVALID_PARAM,
                "prompt key cannot be blank",
            ));
        }
        let (version, label) = normalize_selectors(version, label)?;
        Ok(Self {
            prompt_key: prompt_key.to_string(),
            version,
            label,
        })
    }
}

/// Subscribes to Nacos prompts and caches their templates per selection, mirroring
/// `NacosPromptListener`.
///
/// The listener registers itself with the Nacos AI service so that push
/// updates refresh the local cache; `get_prompt*` serves from that cache.
pub struct NacosPromptListener {
    service: Arc<dyn NacosAiService>,
    /// Current template per selection. Written by the (synchronous) push callbacks, mirroring
    /// Java's `ConcurrentHashMap`; never held across an await.
    prompts: Arc<Mutex<HashMap<PromptSelection, NacosPrompt>>>,
    /// One async lock per selection: initialization is serialized **per selection**, so a slow
    /// selection never blocks another (Java: `synchronized (subscription)`).
    init_locks: Mutex<HashMap<PromptSelection, Arc<AsyncMutex<()>>>>,
    /// Selections holding a live subscription, cancelled by [`Self::close`].
    subscriptions: Mutex<Vec<PromptSelection>>,
    /// Closed flag. `close` takes the write lock so it waits for in-flight reads (Java: the
    /// `lifecycle` rw lock).
    closed: RwLock<bool>,
}

impl NacosPromptListener {
    pub fn new(service: Arc<dyn NacosAiService>) -> Self {
        Self {
            service,
            prompts: Arc::new(Mutex::new(HashMap::new())),
            init_locks: Mutex::new(HashMap::new()),
            subscriptions: Mutex::new(Vec::new()),
            closed: RwLock::new(false),
        }
    }

    /// Returns the rendered prompt with no args and no default.
    pub async fn get_prompt(&self, prompt_key: &str) -> AiServiceResult<String> {
        self.get_prompt_full(prompt_key, None, None, None, None)
            .await
    }

    /// Returns the rendered prompt with args and no default.
    pub async fn get_prompt_with_args(
        &self,
        prompt_key: &str,
        args: Option<&HashMap<String, String>>,
    ) -> AiServiceResult<String> {
        self.get_prompt_full(prompt_key, None, None, args, None)
            .await
    }

    /// Returns the rendered prompt, falling back to `default_value` when the prompt is
    /// missing or empty. Mirrors
    /// `getPrompt(promptKey, args, defaultValue)`.
    pub async fn get_prompt_with_default(
        &self,
        prompt_key: &str,
        args: Option<&HashMap<String, String>>,
        default_value: Option<&str>,
    ) -> AiServiceResult<String> {
        self.get_prompt_full(prompt_key, None, None, args, default_value)
            .await
    }

    /// Returns the rendered prompt with version/label targeting and an optional
    /// default. Mirrors `getPrompt(promptKey, version, label, args, defaultValue)`.
    ///
    /// `version` and `label` are trimmed and mutually exclusive; `None`/blank means
    /// unspecified.
    pub async fn get_prompt_full(
        &self,
        prompt_key: &str,
        version: Option<&str>,
        label: Option<&str>,
        args: Option<&HashMap<String, String>>,
        default_value: Option<&str>,
    ) -> AiServiceResult<String> {
        // Held for the whole read, so `close` waits for it.
        let closed = self.closed.read().await;
        if *closed {
            return Err(AiServiceError::new(
                AiServiceError::INVALID_PARAM,
                "prompt listener is closed",
            ));
        }
        // Selectors are validated on every call, cache hit or not.
        let selection = PromptSelection::new(prompt_key, version, label)?;

        let prompt = match self.cached(&selection) {
            Some(prompt) => prompt,
            None => match self.subscribe_and_load(&selection).await {
                Ok(prompt) => prompt,
                Err(error) => {
                    if let Some(default_value) = default_value {
                        return Ok(render_default(default_value, args));
                    }
                    return Err(error);
                }
            },
        };

        if prompt.template.is_empty() {
            return Ok(match default_value {
                Some(default_value) => render_default(default_value, args),
                None => String::new(),
            });
        }

        Ok(render_prompt(&prompt, args))
    }

    /// Cancels every prompt subscription created by this listener.
    ///
    /// Waits for in-flight reads, so a selection being initialized concurrently is cancelled
    /// before this returns. A selection whose cancellation fails stays queued for a later
    /// `close`. The shared service is not shut down; later reads are rejected.
    pub async fn close(&self) -> AiServiceResult<()> {
        let mut closed = self.closed.write().await;
        *closed = true;
        let selections: Vec<PromptSelection> =
            std::mem::take(&mut *self.subscriptions.lock().unwrap());
        let mut failure: Option<AiServiceError> = None;
        let mut remaining = Vec::new();
        for selection in selections {
            match self
                .service
                .unsubscribe_prompt(
                    &selection.prompt_key,
                    selection.version.as_deref(),
                    selection.label.as_deref(),
                )
                .await
            {
                Ok(()) => {
                    self.prompts.lock().unwrap().remove(&selection);
                }
                Err(error) => {
                    remaining.push(selection);
                    failure.get_or_insert(error);
                }
            }
        }
        *self.subscriptions.lock().unwrap() = remaining;
        failure.map_or(Ok(()), Err)
    }

    fn cached(&self, selection: &PromptSelection) -> Option<NacosPrompt> {
        self.prompts.lock().unwrap().get(selection).cloned()
    }

    /// Initializes the selection's subscription, reusing the cache when another task got there
    /// first (exactly one subscription per selection is created).
    async fn subscribe_and_load(
        &self,
        selection: &PromptSelection,
    ) -> AiServiceResult<NacosPrompt> {
        let init_lock = self
            .init_locks
            .lock()
            .unwrap()
            .entry(selection.clone())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone();
        let _initializing = init_lock.lock().await;
        if let Some(prompt) = self.cached(selection) {
            return Ok(prompt);
        }

        // Listener keeps the local cache current on Nacos push updates.
        let cache = self.prompts.clone();
        let pushed = selection.clone();
        let listener: PromptListener = Arc::new(move |key, prompt| {
            let mut guard = cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            apply_prompt_push(&mut guard, &pushed, key, prompt);
        });

        let loaded = self
            .service
            .subscribe_prompt(
                &selection.prompt_key,
                selection.version.as_deref(),
                selection.label.as_deref(),
                listener,
            )
            .await?;

        self.subscriptions.lock().unwrap().push(selection.clone());
        // A push (including a deletion) can arrive before the initial answer, so a value the
        // callback already stored wins (Java: `compareAndSet(null, initial)`).
        let loaded = loaded.unwrap_or_else(empty_sentinel);
        let mut guard = self.prompts.lock().unwrap();
        Ok(guard.entry(selection.clone()).or_insert(loaded).clone())
    }
}

/// Applies a Nacos prompt push event to the cache entry of `selection`.
///
/// Mirrors 3.3.0's `Subscription#onEvent`: an event for another prompt key is dropped (a
/// selection is updated only through its own subscription) and an **empty** template is stored
/// as-is — `get_prompt` is what substitutes the default later. A `null` payload cannot be
/// expressed by the Rust callback type, so only the key checks remain.
///
/// Returns whether the cache was updated.
fn apply_prompt_push(
    cache: &mut HashMap<PromptSelection, NacosPrompt>,
    selection: &PromptSelection,
    key: &str,
    prompt: &NacosPrompt,
) -> bool {
    if key.is_empty() {
        warn!("Received prompt event with null or empty promptKey");
        return false;
    }
    if key != selection.prompt_key {
        warn!(
            key = %key,
            selected = %selection.prompt_key,
            "Ignoring prompt event for another promptKey"
        );
        return false;
    }
    cache.insert(selection.clone(), prompt.clone());
    true
}

/// Renders a template with the prompt's variable defaults, overridden by the caller's args,
/// mirroring 3.3.0's `NacosPromptListener#render`.
fn render_prompt(prompt: &NacosPrompt, args: Option<&HashMap<String, String>>) -> String {
    let defaults: Vec<(String, String)> = prompt
        .variables
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter_map(|variable| {
            variable
                .default_value
                .as_ref()
                .map(|value| (variable.name.clone(), value.clone()))
        })
        .collect();
    if defaults.is_empty() {
        return prompt.render(args);
    }
    let mut values: HashMap<String, String> = defaults.into_iter().collect();
    if let Some(args) = args {
        values.extend(args.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    render_template(&prompt.template, Some(&values))
}

/// Renders a default value with args, mirroring `renderDefault`.
fn render_default(default_value: &str, args: Option<&HashMap<String, String>>) -> String {
    match args {
        None => default_value.to_string(),
        Some(a) if a.is_empty() => default_value.to_string(),
        _ => render_template(default_value, args),
    }
}

/// Convenience alias mirroring `NacosException` propagation.
pub type PromptError = AiServiceError;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::InMemoryNacosAiService;

    fn args(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[tokio::test]
    async fn returns_rendered_prompt() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        svc.set_prompt(
            "greet",
            NacosPrompt::new("Hello {{name}}, welcome to ${place}"),
        );
        let listener = NacosPromptListener::new(svc);
        let out = listener
            .get_prompt_with_args("greet", Some(&args(&[("name", "Ada"), ("place", "Nacos")])))
            .await
            .unwrap();
        assert_eq!(out, "Hello Ada, welcome to Nacos");
    }

    #[tokio::test]
    async fn falls_back_to_default_when_missing() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        let listener = NacosPromptListener::new(svc);
        let out = listener
            .get_prompt_with_default("missing", None, Some("fallback {{x}}"))
            .await
            .unwrap();
        assert_eq!(out, "fallback {{x}}");
    }

    #[tokio::test]
    async fn default_is_rendered_with_args() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        let listener = NacosPromptListener::new(svc);
        let out = listener
            .get_prompt_full(
                "missing",
                None,
                None,
                Some(&args(&[("x", "42")])),
                Some("value=${x}"),
            )
            .await
            .unwrap();
        assert_eq!(out, "value=42");
    }

    #[tokio::test]
    async fn cache_updates_on_push() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        svc.set_prompt("p", NacosPrompt::new("v1"));
        let listener = NacosPromptListener::new(svc.clone());
        assert_eq!(listener.get_prompt("p").await.unwrap(), "v1");

        svc.fire_prompt_update("p", NacosPrompt::new("v2"));
        // Cached, so still serves the old value until re-subscribed... unless the
        // push-updated cache is observed. The listener cache is updated in-place.
        assert_eq!(listener.get_prompt("p").await.unwrap(), "v2");
    }

    #[tokio::test]
    async fn empty_prompt_returns_empty_string() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        svc.set_prompt("empty", NacosPrompt::new(""));
        let listener = NacosPromptListener::new(svc);
        assert_eq!(listener.get_prompt("empty").await.unwrap(), "");
    }

    #[tokio::test]
    async fn version_and_label_are_passed_through() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        svc.set_prompt("p", NacosPrompt::new("v"));
        let listener = NacosPromptListener::new(svc.clone());
        listener
            .get_prompt_full("p", Some("1.0"), None, None, None)
            .await
            .unwrap();
        listener
            .get_prompt_full("p", None, Some("prod"), None, None)
            .await
            .unwrap();
        assert_eq!(
            svc.prompt_subscriptions(),
            vec![
                ("p".to_string(), Some("1.0".to_string()), None),
                ("p".to_string(), None, Some("prod".to_string())),
            ]
        );
    }

    /// Java 3.3.0 rejects a `version` *and* a `label` on the same read, even when the prompt is
    /// already cached, and never issues a subscription for it.
    #[tokio::test]
    async fn version_and_label_together_are_rejected() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        svc.set_prompt("p", NacosPrompt::new("v"));
        let listener = NacosPromptListener::new(svc.clone());
        assert_eq!(listener.get_prompt("p").await.unwrap(), "v");

        let error = listener
            .get_prompt_full("p", Some("1.0"), Some("prod"), None, None)
            .await
            .unwrap_err();
        assert_eq!(error.code(), AiServiceError::INVALID_PARAM);
        assert!(error.message().contains("mutually exclusive"), "{error}");
        assert_eq!(svc.prompt_subscriptions().len(), 1);
        assert!(svc.prompt_subscriptions()[0].1.is_none());
        assert!(svc.prompt_subscriptions()[0].2.is_none());
    }

    /// Same name, different version/label: each selection keeps its own cached template.
    #[tokio::test]
    async fn selections_are_cached_independently() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        svc.set_prompt_selection("shared", Some("1.0.0"), None, NacosPrompt::new("v1"));
        svc.set_prompt_selection("shared", Some("2.0.0"), None, NacosPrompt::new("v2"));
        svc.set_prompt_selection("shared", None, Some("prod"), NacosPrompt::new("prod"));
        svc.set_prompt("shared", NacosPrompt::new("default"));
        let listener = NacosPromptListener::new(svc.clone());

        assert_eq!(
            listener
                .get_prompt_full("shared", Some("1.0.0"), None, None, None)
                .await
                .unwrap(),
            "v1"
        );
        assert_eq!(
            listener
                .get_prompt_full("shared", Some("2.0.0"), None, None, None)
                .await
                .unwrap(),
            "v2"
        );
        assert_eq!(
            listener
                .get_prompt_full("shared", None, Some("prod"), None, None)
                .await
                .unwrap(),
            "prod"
        );
        assert_eq!(listener.get_prompt("shared").await.unwrap(), "default");
        // One subscription per selection, and the re-reads above did not add more.
        assert_eq!(svc.prompt_subscriptions().len(), 4);
    }

    /// A push reaches only the subscription it was routed to.
    #[tokio::test]
    async fn push_updates_only_its_selection() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        svc.set_prompt_selection("shared", None, Some("prod"), NacosPrompt::new("prod"));
        svc.set_prompt_selection("shared", None, Some("test"), NacosPrompt::new("test"));
        let listener = NacosPromptListener::new(svc.clone());
        assert_eq!(
            listener
                .get_prompt_full("shared", None, Some("prod"), None, None)
                .await
                .unwrap(),
            "prod"
        );
        assert_eq!(
            listener
                .get_prompt_full("shared", None, Some("test"), None, None)
                .await
                .unwrap(),
            "test"
        );

        assert!(svc.fire_prompt_update_for(
            "shared",
            None,
            Some("prod"),
            NacosPrompt::new("new prod")
        ));
        assert_eq!(
            listener
                .get_prompt_full("shared", None, Some("prod"), None, None)
                .await
                .unwrap(),
            "new prod"
        );
        assert_eq!(
            listener
                .get_prompt_full("shared", None, Some("test"), None, None)
                .await
                .unwrap(),
            "test"
        );
    }

    /// Blank selectors are "unspecified", so they share the default selection.
    #[tokio::test]
    async fn blank_selectors_normalize_to_the_same_subscription() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        svc.set_prompt("key", NacosPrompt::new("v"));
        let listener = NacosPromptListener::new(svc.clone());
        assert_eq!(
            listener
                .get_prompt_full("key", Some(" "), Some("\t"), None, None)
                .await
                .unwrap(),
            "v"
        );
        assert_eq!(listener.get_prompt("key").await.unwrap(), "v");
        assert_eq!(
            svc.prompt_subscriptions(),
            vec![("key".to_string(), None, None)]
        );

        let error = listener.get_prompt("   ").await.unwrap_err();
        assert_eq!(error.code(), AiServiceError::INVALID_PARAM);
    }

    /// Template variable defaults are applied, and a caller arg overrides its default; a variable
    /// without a default stays as a literal placeholder.
    #[tokio::test]
    async fn variable_defaults_are_applied_and_overridden() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        let mut prompt = NacosPrompt::new("{{name}} {{value}} {{missing}}");
        prompt.variables = Some(vec![
            crate::service::NacosPromptVariable {
                name: "name".to_string(),
                default_value: Some("default".to_string()),
            },
            crate::service::NacosPromptVariable {
                name: "missing".to_string(),
                default_value: None,
            },
        ]);
        svc.set_prompt("greeting", prompt);
        let listener = NacosPromptListener::new(svc);

        assert_eq!(
            listener
                .get_prompt_with_args("greeting", Some(&args(&[("value", "$1\\path")])))
                .await
                .unwrap(),
            "default $1\\path {{missing}}"
        );
        assert_eq!(
            listener
                .get_prompt_with_args(
                    "greeting",
                    Some(&args(&[("name", "override"), ("value", "override")]))
                )
                .await
                .unwrap(),
            "override override {{missing}}"
        );
    }

    /// A failed load is not cached, so the next read retries it (`fallback` is not sticky).
    #[tokio::test]
    async fn failed_load_is_retried_without_caching_the_fallback() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        svc.set_prompt("retry", NacosPrompt::new("recovered"));
        svc.fail_next_prompt_subscribe(AiServiceError::new(503, "unavailable"));
        let listener = NacosPromptListener::new(svc.clone());

        assert_eq!(
            listener
                .get_prompt_with_default("retry", None, Some("fallback"))
                .await
                .unwrap(),
            "fallback"
        );
        assert_eq!(listener.get_prompt("retry").await.unwrap(), "recovered");
    }

    /// Concurrent reads of the same selection share one subscription.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_loads_share_one_subscription() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        svc.set_prompt("concurrent", NacosPrompt::new("value"));
        let gate = svc.gate_next_prompt_subscribe();
        let listener = Arc::new(NacosPromptListener::new(svc.clone()));

        let mut reads = Vec::new();
        for _ in 0..4 {
            let listener = Arc::clone(&listener);
            reads.push(tokio::spawn(async move {
                listener.get_prompt("concurrent").await
            }));
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        gate.add_permits(1);
        for read in reads {
            assert_eq!(read.await.unwrap().unwrap(), "value");
        }
        assert_eq!(svc.prompt_subscriptions().len(), 1);
    }

    /// A slow subscription for one selection must not block another selection.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn slow_initialization_does_not_block_other_selections() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        svc.set_prompt_selection("shared", None, Some("prod"), NacosPrompt::new("cached"));
        let listener = Arc::new(NacosPromptListener::new(svc.clone()));
        assert_eq!(
            listener
                .get_prompt_full("shared", None, Some("prod"), None, None)
                .await
                .unwrap(),
            "cached"
        );

        let gate = svc.gate_next_prompt_subscribe();
        let slow = {
            let listener = Arc::clone(&listener);
            tokio::spawn(async move {
                listener
                    .get_prompt_full("shared", Some("slow"), None, None, None)
                    .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let cached = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            listener.get_prompt_full("shared", None, Some("prod"), None, None),
        )
        .await
        .expect("the cached selection must not wait for the slow one")
        .unwrap();
        assert_eq!(cached, "cached");
        assert!(!slow.is_finished());

        gate.add_permits(1);
        assert_eq!(slow.await.unwrap().unwrap(), "");
    }

    /// `close` cancels every selection, waits for in-flight reads and rejects later reads.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn close_waits_for_an_in_flight_read_and_cancels_its_subscription() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        svc.set_prompt("race", NacosPrompt::new("value"));
        let gate = svc.gate_next_prompt_subscribe();
        let listener = Arc::new(NacosPromptListener::new(svc.clone()));

        let read = {
            let listener = Arc::clone(&listener);
            tokio::spawn(async move { listener.get_prompt("race").await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let close = {
            let listener = Arc::clone(&listener);
            tokio::spawn(async move { listener.close().await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !close.is_finished(),
            "close must wait for the in-flight read"
        );

        gate.add_permits(1);
        assert_eq!(read.await.unwrap().unwrap(), "value");
        close.await.unwrap().unwrap();
        assert_eq!(
            svc.prompt_unsubscriptions(),
            vec![("race".to_string(), None, None)]
        );

        let error = listener.get_prompt("race").await.unwrap_err();
        assert_eq!(error.code(), AiServiceError::INVALID_PARAM);
        // Closing again is a no-op, not an error.
        listener.close().await.unwrap();
    }

    /// Every selection is unsubscribed, and the shared service is not shut down.
    #[tokio::test]
    async fn close_cancels_every_selection() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        svc.set_prompt_selection("shared", Some("1.0.0"), None, NacosPrompt::new("v1"));
        svc.set_prompt_selection("shared", None, Some("prod"), NacosPrompt::new("prod"));
        let listener = NacosPromptListener::new(svc.clone());
        listener
            .get_prompt_full("shared", Some("1.0.0"), None, None, None)
            .await
            .unwrap();
        listener
            .get_prompt_full("shared", None, Some("prod"), None, None)
            .await
            .unwrap();

        listener.close().await.unwrap();
        listener.close().await.unwrap();
        assert_eq!(
            svc.prompt_unsubscriptions(),
            vec![
                ("shared".to_string(), Some("1.0.0".to_string()), None),
                ("shared".to_string(), None, Some("prod".to_string())),
            ]
        );
    }

    #[tokio::test]
    async fn cache_hit_does_not_resubscribe() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        svc.set_prompt("p", NacosPrompt::new("v1"));
        let listener = NacosPromptListener::new(svc.clone());
        assert_eq!(listener.get_prompt("p").await.unwrap(), "v1");
        assert_eq!(listener.get_prompt("p").await.unwrap(), "v1");
        // One subscription despite two reads (mirrors Java's ConcurrentHashMap cache).
        assert_eq!(svc.prompt_subscriptions().len(), 1);
    }

    #[tokio::test]
    async fn default_is_used_when_loading_errors() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        // No prompt seeded: subscribe returns `None`, treated as empty -> default.
        let listener = NacosPromptListener::new(svc);
        let out = listener
            .get_prompt_with_default("missing", None, Some("fallback"))
            .await
            .unwrap();
        assert_eq!(out, "fallback");
    }

    #[tokio::test]
    async fn empty_template_push_is_stored_as_is() {
        // Java `onEvent`: `prompt != null && getTemplate() != null` -> put, even for
        // the empty template. Only `null` payloads are dropped.
        let svc = Arc::new(InMemoryNacosAiService::new());
        svc.set_prompt("p", NacosPrompt::new("v1"));
        let listener = NacosPromptListener::new(svc.clone());
        assert_eq!(listener.get_prompt("p").await.unwrap(), "v1");

        svc.fire_prompt_update("p", NacosPrompt::new(""));
        assert_eq!(listener.get_prompt("p").await.unwrap(), "");
    }

    #[test]
    fn push_with_empty_or_foreign_key_is_dropped() {
        let selection = PromptSelection::new("kept", None, None).unwrap();
        let mut cache = HashMap::new();
        cache.insert(selection.clone(), NacosPrompt::new("v1"));

        // An event without a key, or for another prompt, never touches this selection.
        assert!(!apply_prompt_push(
            &mut cache,
            &selection,
            "",
            &NacosPrompt::new("v2")
        ));
        assert!(!apply_prompt_push(
            &mut cache,
            &selection,
            "other",
            &NacosPrompt::new("v2")
        ));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache[&selection].template, "v1");

        assert!(apply_prompt_push(
            &mut cache,
            &selection,
            "kept",
            &NacosPrompt::new("")
        ));
        assert_eq!(cache[&selection].template, "");
    }
}
