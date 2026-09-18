//! Nacos prompt listening, mirroring `io.agentscope.core.nacos.prompt`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::service::{
    render_template, AiServiceError, AiServiceResult, NacosAiService, NacosPrompt, PromptListener,
};

/// Sentinel for "prompt not found in Nacos", mirroring Java's `EMPTY_SENTINEL`.
fn empty_sentinel() -> NacosPrompt {
    NacosPrompt::new("")
}

/// Subscribes to Nacos prompts and caches their templates, mirroring
/// `NacosPromptListener`.
///
/// The listener registers itself with the Nacos AI service so that push
/// updates refresh the local cache; `get_prompt*` serves from that cache.
pub struct NacosPromptListener {
    service: Arc<dyn NacosAiService>,
    prompts: Arc<Mutex<HashMap<String, NacosPrompt>>>,
}

impl NacosPromptListener {
    pub fn new(service: Arc<dyn NacosAiService>) -> Self {
        Self {
            service,
            prompts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Returns the rendered prompt with no args and no default.
    pub async fn get_prompt(&self, prompt_key: &str) -> AiServiceResult<String> {
        self.get_prompt_full(prompt_key, None, None, None, None).await
    }

    /// Returns the rendered prompt with args and no default.
    pub async fn get_prompt_with_args(
        &self,
        prompt_key: &str,
        args: Option<&HashMap<String, String>>,
    ) -> AiServiceResult<String> {
        self.get_prompt_full(prompt_key, None, None, args, None).await
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
    pub async fn get_prompt_full(
        &self,
        prompt_key: &str,
        version: Option<&str>,
        label: Option<&str>,
        args: Option<&HashMap<String, String>>,
        default_value: Option<&str>,
    ) -> AiServiceResult<String> {
        let mut prompt = self.prompts.lock().unwrap().get(prompt_key).cloned();

        if prompt.is_none() {
            match self.subscribe_and_load(prompt_key, version, label).await {
                Ok(p) => prompt = Some(p),
                Err(e) => {
                    if let Some(default) = default_value {
                        return Ok(render_default(default, args));
                    }
                    return Err(e);
                }
            }
        }

        let prompt = prompt.unwrap_or_else(empty_sentinel);
        if prompt.template.is_empty() {
            if let Some(default) = default_value {
                return Ok(render_default(default, args));
            }
            return Ok(String::new());
        }

        Ok(prompt.render(args))
    }

    async fn subscribe_and_load(
        &self,
        prompt_key: &str,
        version: Option<&str>,
        label: Option<&str>,
    ) -> AiServiceResult<NacosPrompt> {
        // Listener keeps the local cache current on Nacos push updates.
        let cache = self.prompts.clone();
        let listener: PromptListener = Arc::new(move |key, prompt| {
            if prompt.template.is_empty() {
                return;
            }
            cache
                .lock()
                .unwrap()
                .insert(key.to_string(), prompt.clone());
        });

        let loaded = self
            .service
            .subscribe_prompt(prompt_key, version, label, listener)
            .await?;

        let prompt = loaded.unwrap_or_else(empty_sentinel);
        let mut guard = self.prompts.lock().unwrap();
        let chosen = guard.get(prompt_key).cloned().unwrap_or(prompt);
        guard.insert(prompt_key.to_string(), chosen.clone());
        Ok(chosen)
    }
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
        svc.set_prompt("greet", NacosPrompt::new("Hello {{name}}, welcome to ${place}"));
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
            .get_prompt_full("p", Some("1.0"), Some("prod"), None, None)
            .await
            .unwrap();
        assert_eq!(
            svc.prompt_subscriptions(),
            vec![("p".to_string(), Some("1.0".to_string()), Some("prod".to_string()))]
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
}
