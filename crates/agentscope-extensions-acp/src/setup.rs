//! Data-driven provider setup (`--setup`).
//!
//! models.dev already knows every provider's base URL, credential environment
//! variables and model list, so the wizard never asks the user to type a model
//! id or an endpoint: it renders [`CatalogProvider`]s out of the catalog
//! snapshot and only asks for an API key. An already exported key needs no
//! answers at all — [`detect_env`] finds it and the agent starts configured.
//!
//! The module is pure: everything that touches the terminal goes through
//! [`SetupIo`], and the decision logic ([`run`]) is testable without one.

use std::collections::BTreeMap;

use agentscope_core::model::catalog::{CatalogSnapshot, ModelDescriptor, ModelKind, ModelStatus};
use agentscope_core::model::provider::ApiProtocol;

/// Base URLs for providers whose models.dev entry carries no `api` field.
///
/// models.dev omits `api` exactly for the providers that are the AI SDK's own
/// default endpoint, which includes the majors (`openai`, `anthropic`, …).
/// Without an entry here the wizard could not build a working endpoint for
/// them, so the handful we can actually drive are listed explicitly. `google`
/// points at its OpenAI-compatible surface because that is the only wire
/// protocol we speak for Gemini.
const FALLBACK_BASE_URLS: &[(&str, &str)] = &[
    ("openai", "https://api.openai.com/v1"),
    ("anthropic", "https://api.anthropic.com/v1"),
    (
        "google",
        "https://generativelanguage.googleapis.com/v1beta/openai",
    ),
    ("xai", "https://api.x.ai/v1"),
    ("groq", "https://api.groq.com/openai/v1"),
    ("mistral", "https://api.mistral.ai/v1"),
    ("cerebras", "https://api.cerebras.ai/v1"),
    ("togetherai", "https://api.together.xyz/v1"),
    ("deepinfra", "https://api.deepinfra.com/v1/openai"),
];

/// How many rows a single listing prints before asking the user to narrow down.
const MAX_LISTED: usize = 30;

/// One provider the user can pick, with everything needed to talk to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogProvider {
    /// Catalog provider id, for example `deepseek`.
    pub id: String,
    /// Endpoint prefix the protocol suffix is appended to.
    pub base_url: String,
    /// Environment variables that carry this provider's credentials.
    pub env: Vec<String>,
    /// Chat model ids, sorted, deprecations removed.
    pub models: Vec<String>,
    /// Wire protocol shared by every model of this provider.
    pub protocol: ApiProtocol,
}

/// The wire protocol every model of `provider_id` is configured under.
///
/// Anthropic Messages is the only non-OpenAI shape among the catalog providers;
/// everything else (including OpenAI-compatible gateways) speaks
/// `POST {base_url}/chat/completions`.
pub fn provider_protocol(provider_id: &str) -> ApiProtocol {
    if provider_id.to_ascii_lowercase().starts_with("anthropic") {
        ApiProtocol::Messages
    } else {
        ApiProtocol::ChatCompletions
    }
}

/// Renders the provider list a wizard can offer, straight from the catalog.
///
/// Providers without a reachable base URL (no `api` in models.dev and no
/// [`FALLBACK_BASE_URLS`] entry) are skipped: writing them out would produce a
/// configuration that cannot connect. Non-chat models (embeddings, images, …)
/// are skipped for the same reason: the wizard configures a chat agent.
pub fn catalog_providers(snapshot: &CatalogSnapshot) -> Vec<CatalogProvider> {
    let mut grouped: BTreeMap<&str, Vec<&ModelDescriptor>> = BTreeMap::new();
    for descriptor in snapshot.entries.values() {
        if descriptor.kind != ModelKind::Chat || descriptor.status == ModelStatus::Deprecated {
            continue;
        }
        grouped
            .entry(descriptor.provider.as_str())
            .or_default()
            .push(descriptor);
    }

    let mut providers = Vec::with_capacity(grouped.len());
    for (id, mut models) in grouped {
        let Some(base_url) = snapshot
            .provider_api
            .get(id)
            .cloned()
            .or_else(|| fallback_base_url(id))
        else {
            continue;
        };
        models.sort_by(|left, right| left.id.cmp(&right.id));
        providers.push(CatalogProvider {
            id: id.to_string(),
            base_url,
            env: snapshot.provider_env.get(id).cloned().unwrap_or_default(),
            models: models.into_iter().map(|model| model.id.clone()).collect(),
            protocol: provider_protocol(id),
        });
    }
    providers
}

fn fallback_base_url(provider_id: &str) -> Option<String> {
    FALLBACK_BASE_URLS
        .iter()
        .find(|(id, _)| *id == provider_id)
        .map(|(_, url)| (*url).to_string())
}

/// A provider whose API key is already present in the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedProvider {
    pub provider: CatalogProvider,
    /// The variable the key came from, for example `DEEPSEEK_API_KEY`.
    pub var: String,
    pub key: String,
}

/// Finds providers that are usable without any configuration.
///
/// `lookup` is `std::env::var` in production and a map in tests. The first set
/// variable of a provider wins, matching the order models.dev lists them in
/// (most specific first).
pub fn detect_env(
    providers: &[CatalogProvider],
    lookup: impl Fn(&str) -> Option<String>,
) -> Vec<DetectedProvider> {
    providers
        .iter()
        .filter_map(|provider| {
            provider.env.iter().find_map(|var| {
                let key = lookup(var)?;
                let key = key.trim();
                if key.is_empty() {
                    return None;
                }
                Some(DetectedProvider {
                    provider: provider.clone(),
                    var: var.clone(),
                    key: key.to_string(),
                })
            })
        })
        .collect()
}

/// What the wizard decided: a provider, its key and the model to default to.
#[derive(Debug, Clone)]
pub struct SetupPlan {
    pub provider: CatalogProvider,
    pub model: String,
    pub api_key: String,
    /// Set when the key came from the environment instead of being typed in.
    pub key_source: Option<String>,
}

/// Terminal the wizard talks to.
pub trait SetupIo {
    /// Prints one line of output.
    fn say(&mut self, line: &str);
    /// Prints a prompt and reads one line. `None` means end of input.
    fn ask(&mut self, prompt: &str) -> Option<String>;
}

/// Runs the wizard: reuse a detected provider, or pick one and enter its key.
pub fn run<IO: SetupIo>(
    io: &mut IO,
    providers: &[CatalogProvider],
    detected: &[DetectedProvider],
) -> Result<SetupPlan, String> {
    if providers.is_empty() {
        return Err(
            "模型目录为空：请先联网运行一次 acp_harness_agent，让 models.dev 目录下载完成"
                .to_string(),
        );
    }

    // A key that is already exported is the whole configuration: confirm and stop.
    if let Some(found) = detected.first() {
        let answer = io
            .ask(&format!(
                "检测到 {}，使用 {}（{} 个模型）? [Y/n]: ",
                found.var,
                found.provider.id,
                found.provider.models.len()
            ))
            .ok_or_else(end_of_input)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "n" | "no") {
            return Ok(SetupPlan {
                model: default_model(&found.provider)?,
                api_key: found.key.clone(),
                key_source: Some(found.var.clone()),
                provider: found.provider.clone(),
            });
        }
    }

    let provider = choose_provider(io, providers)?;
    let (api_key, key_source) = choose_key(io, &provider, detected);
    let model = choose_model(io, &provider)?;
    Ok(SetupPlan {
        provider,
        model,
        api_key,
        key_source,
    })
}

fn default_model(provider: &CatalogProvider) -> Result<String, String> {
    provider
        .models
        .first()
        .cloned()
        .ok_or_else(|| format!("provider '{}' 没有可用模型", provider.id))
}

fn end_of_input() -> String {
    "输入已结束，未写入任何配置".to_string()
}

fn choose_provider<IO: SetupIo>(
    io: &mut IO,
    providers: &[CatalogProvider],
) -> Result<CatalogProvider, String> {
    loop {
        let query = io
            .ask("provider（关键字过滤，直接回车=列出全部）: ")
            .ok_or_else(end_of_input)?;
        let query = query.trim().to_ascii_lowercase();
        let matched: Vec<&CatalogProvider> = if query.is_empty() {
            providers.iter().collect()
        } else {
            let found: Vec<&CatalogProvider> = providers
                .iter()
                .filter(|provider| provider.id.to_ascii_lowercase().contains(&query))
                .collect();
            // A single hit needs no second question.
            if found.len() == 1 {
                return Ok(found[0].clone());
            }
            found
        };
        if matched.is_empty() {
            io.say(&format!("没有 id 含 '{query}' 的 provider"));
            continue;
        }

        let listed = matched.len().min(MAX_LISTED);
        for (index, provider) in matched.iter().take(listed).enumerate() {
            io.say(&format!(
                "  {:>2}) {:<20} {:<48} {:>3} models  env: {}",
                index + 1,
                provider.id,
                provider.base_url,
                provider.models.len(),
                if provider.env.is_empty() {
                    "-".to_string()
                } else {
                    provider.env.join(" / ")
                }
            ));
        }
        if listed < matched.len() {
            io.say(&format!(
                "  …共 {} 个，只列出前 {listed} 个，输入关键字缩小范围",
                matched.len()
            ));
        }

        let answer = io
            .ask(&format!("选择编号 [1-{listed}]（回车=1）: "))
            .ok_or_else(end_of_input)?;
        let answer = answer.trim();
        let index = if answer.is_empty() {
            1
        } else {
            answer.parse::<usize>().unwrap_or(0)
        };
        if (1..=listed).contains(&index) {
            return Ok(matched[index - 1].clone());
        }
        io.say(&format!("编号 {answer} 无效"));
    }
}

fn choose_key<IO: SetupIo>(
    io: &mut IO,
    provider: &CatalogProvider,
    detected: &[DetectedProvider],
) -> (String, Option<String>) {
    let found = detected
        .iter()
        .find(|detected| detected.provider.id == provider.id);
    let hint = match found {
        Some(detected) => format!("（留空使用已检测到的 ${}）", detected.var),
        None if !provider.env.is_empty() => {
            format!(
                "（留空则不写密钥，之后可用环境变量 {}）",
                provider.env.join(" / ")
            )
        }
        None => String::new(),
    };
    let answer = io.ask(&format!("API key{hint}: ")).unwrap_or_default();
    let typed = answer.trim();
    if !typed.is_empty() {
        return (typed.to_string(), None);
    }
    match found {
        Some(detected) => (detected.key.clone(), Some(detected.var.clone())),
        None => {
            io.say(&format!(
                "已留空：稍后把 {} 的 api_key 填进 models.json 即可",
                provider.id
            ));
            (String::new(), None)
        }
    }
}

fn choose_model<IO: SetupIo>(io: &mut IO, provider: &CatalogProvider) -> Result<String, String> {
    let default = default_model(provider)?;
    let answer = io
        .ask(&format!("默认模型 [{default}]: "))
        .ok_or_else(end_of_input)?;
    let typed = answer.trim();
    if typed.is_empty() {
        return Ok(default);
    }
    if provider.models.iter().any(|model| model == typed) {
        return Ok(typed.to_string());
    }
    let prefixed: Vec<&String> = provider
        .models
        .iter()
        .filter(|model| model.starts_with(typed))
        .collect();
    match prefixed.len() {
        1 => Ok(prefixed[0].clone()),
        0 => {
            io.say(&format!("目录里没有 '{typed}'，按原样写入"));
            Ok(typed.to_string())
        }
        count => {
            io.say(&format!(
                "'{typed}' 匹配到 {count} 个模型，使用 {}",
                prefixed[0]
            ));
            Ok(prefixed[0].clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct FakeIo {
        answers: Vec<String>,
        output: Vec<String>,
    }

    impl FakeIo {
        fn new(answers: &[&str]) -> Self {
            Self {
                answers: answers.iter().map(|answer| answer.to_string()).collect(),
                output: Vec::new(),
            }
        }
    }

    impl SetupIo for FakeIo {
        fn say(&mut self, line: &str) {
            self.output.push(line.to_string());
        }

        fn ask(&mut self, _prompt: &str) -> Option<String> {
            if self.answers.is_empty() {
                None
            } else {
                Some(self.answers.remove(0))
            }
        }
    }

    fn descriptor(provider: &str, id: &str, kind: &str, status: &str) -> serde_json::Value {
        json!({
            "key": format!("{provider}/{id}"),
            "provider": provider,
            "id": id,
            "name": id,
            "description": null,
            "family": null,
            "kind": kind,
            "context_window": 128_000,
            "max_output": 8_192,
            "input_modalities": ["text"],
            "output_modalities": ["text"],
            "attachment": false,
            "reasoning": false,
            "reasoning_options": [],
            "interleaved_field": null,
            "tool_call": true,
            "structured_output": null,
            "temperature": true,
            "status": status,
            "open_weights": false,
            "cost": null,
            "canonical": null,
            "use_cases": [],
            "fingerprint": 0
        })
    }

    fn snapshot() -> CatalogSnapshot {
        let mut entries = serde_json::Map::new();
        for (provider, id, kind, status) in [
            ("deepseek", "deepseek-chat", "chat", "active"),
            ("deepseek", "deepseek-old", "chat", "deprecated"),
            ("deepseek", "deepseek-embed", "embedding", "active"),
            ("openai", "gpt-4.1", "chat", "active"),
            ("nano-gpt", "nano-x", "chat", "active"),
        ] {
            entries.insert(
                format!("{provider}/{id}"),
                descriptor(provider, id, kind, status),
            );
        }
        serde_json::from_value(json!({
            "schema_version": 2,
            "generation": 1,
            "fetched_at": "2026-01-01T00:00:00Z",
            "provider_api": { "deepseek": "https://api.deepseek.com" },
            "provider_env": {
                "deepseek": ["DEEPSEEK_API_KEY"],
                "openai": ["OPENAI_API_KEY"]
            },
            "entries": serde_json::Value::Object(entries)
        }))
        .expect("snapshot fixture")
    }

    #[test]
    fn catalog_providers_use_the_catalog_base_url_or_a_known_one() {
        let providers = catalog_providers(&snapshot());
        let ids: Vec<&str> = providers.iter().map(|p| p.id.as_str()).collect();
        // `nano-gpt` has neither a catalog `api` nor a fallback entry.
        assert_eq!(ids, ["deepseek", "openai"]);

        let deepseek = &providers[0];
        assert_eq!(deepseek.base_url, "https://api.deepseek.com");
        assert_eq!(deepseek.env, ["DEEPSEEK_API_KEY"]);
        // Deprecated and non-chat models are not offered.
        assert_eq!(deepseek.models, ["deepseek-chat"]);
        assert_eq!(deepseek.protocol, ApiProtocol::ChatCompletions);

        assert_eq!(providers[1].base_url, "https://api.openai.com/v1");
    }

    #[test]
    fn only_anthropic_speaks_the_messages_protocol() {
        assert_eq!(provider_protocol("anthropic"), ApiProtocol::Messages);
        assert_eq!(
            provider_protocol("anthropic-compatible"),
            ApiProtocol::Messages
        );
        assert_eq!(provider_protocol("openai"), ApiProtocol::ChatCompletions);
        assert_eq!(provider_protocol("deepseek"), ApiProtocol::ChatCompletions);
    }

    #[test]
    fn detect_env_reports_every_configured_provider_once() {
        let providers = catalog_providers(&snapshot());
        let detected = detect_env(&providers, |name| match name {
            "DEEPSEEK_API_KEY" => Some("  sk-deepseek  ".to_string()),
            "OPENAI_API_KEY" => Some("   ".to_string()),
            _ => None,
        });
        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0].provider.id, "deepseek");
        assert_eq!(detected[0].var, "DEEPSEEK_API_KEY");
        assert_eq!(detected[0].key, "sk-deepseek");
    }

    #[test]
    fn an_exported_key_needs_no_answers_at_all() {
        let providers = catalog_providers(&snapshot());
        let detected = detect_env(&providers, |name| {
            (name == "DEEPSEEK_API_KEY").then(|| "sk-deepseek".to_string())
        });
        let mut io = FakeIo::new(&[""]);
        let plan = run(&mut io, &providers, &detected).expect("plan");
        assert_eq!(plan.provider.id, "deepseek");
        assert_eq!(plan.model, "deepseek-chat");
        assert_eq!(plan.api_key, "sk-deepseek");
        assert_eq!(plan.key_source.as_deref(), Some("DEEPSEEK_API_KEY"));
        assert!(io.answers.is_empty(), "a detected key must not ask again");
    }

    #[test]
    fn declining_the_detected_key_falls_back_to_manual_selection() {
        let providers = catalog_providers(&snapshot());
        let detected = detect_env(&providers, |name| {
            (name == "DEEPSEEK_API_KEY").then(|| "sk-deepseek".to_string())
        });
        // Decline, filter to `openai`, type a key, accept the default model.
        let mut io = FakeIo::new(&["n", "openai", "sk-openai", ""]);
        let plan = run(&mut io, &providers, &detected).expect("plan");
        assert_eq!(plan.provider.id, "openai");
        assert_eq!(plan.api_key, "sk-openai");
        assert_eq!(plan.key_source, None);
        assert_eq!(plan.model, "gpt-4.1");
    }

    #[test]
    fn a_typed_key_wins_over_the_detected_one() {
        let providers = catalog_providers(&snapshot());
        let detected = detect_env(&providers, |name| {
            (name == "DEEPSEEK_API_KEY").then(|| "sk-deepseek".to_string())
        });
        // Decline detection, keep deepseek (single filtered hit), type another key.
        let mut io = FakeIo::new(&["no", "deepseek", "sk-other", "deepseek-chat"]);
        let plan = run(&mut io, &providers, &detected).expect("plan");
        assert_eq!(plan.api_key, "sk-other");
        assert_eq!(plan.key_source, None);
    }

    #[test]
    fn an_ambiguous_listing_is_picked_by_number() {
        let providers = catalog_providers(&snapshot());
        // Empty filter lists both providers; `2` selects openai.
        let mut io = FakeIo::new(&["", "2", "", ""]);
        let plan = run(&mut io, &providers, &[]).expect("plan");
        assert_eq!(plan.provider.id, "openai");
        assert_eq!(plan.api_key, "");
        assert!(io.output.iter().any(|line| line.contains("openai")));
    }

    #[test]
    fn an_out_of_range_number_asks_again() {
        let providers = catalog_providers(&snapshot());
        // List, answer 7 (out of range), list again, accept the default index, then the key and model prompts.
        let mut io = FakeIo::new(&["", "7", "", "", "", ""]);
        let plan = run(&mut io, &providers, &[]).expect("plan");
        assert_eq!(plan.provider.id, "deepseek");
        assert!(io.output.iter().any(|line| line.contains("无效")));
    }

    #[test]
    fn end_of_input_leaves_the_config_untouched() {
        let providers = catalog_providers(&snapshot());
        let mut io = FakeIo::new(&[]);
        assert!(run(&mut io, &providers, &[]).is_err());
    }

    #[test]
    fn an_empty_catalog_is_an_error_not_an_empty_config() {
        let mut io = FakeIo::new(&[]);
        let error = run(&mut io, &[], &[]).expect_err("must fail");
        assert!(error.contains("模型目录为空"), "{error}");
    }
}
