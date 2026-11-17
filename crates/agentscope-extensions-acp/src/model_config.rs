//! 模型供应商配置：`$HOME/.agentscope/models.json` 的解析、落盘与增删改。
//!
//! 从 `acp_harness_agent` 二进制里上收，因为 `providers/*` 的 ACP 处理器（库侧）需要
//! 读写同一份配置。文件格式与宽容解析口径保持不变，新增字段全部可选：
//!
//! - `label`：展示名（缺省用 provider key）
//! - `disabled`：被 `providers/disable` 停用（保留条目，便于再次启用）
//!
//! 环境变量 `AGENTSCOPE_MODELS_JSON` 可覆盖配置文件路径（测试与多配置并存用）。

use std::collections::HashMap;
use std::path::PathBuf;

use agentscope_core::model::catalog::ModelCatalog;
use agentscope_core::model::provider::ApiProtocol;
use serde::{Deserialize, Serialize};

use crate::config::ModelChoice;
use crate::setup::{self, DetectedProvider, SetupPlan};

/// 按端点协议分组的模型列表。
///
/// 支持两种写法（`models` 字段）：
///
/// 1. **分组对象**（显式指定协议，推荐）：
/// ```json
/// "models": {
///   "completions": ["glm-5.3", "kimi-k3"],
///   "messages":    ["minimax-m3", "qwen3.8-max"],
///   "responses":   ["grok-4.6", "gpt-5.6-luna"]
/// }
/// ```
///
/// 2. **扁平数组**（按模型命名自动推断协议）：
/// ```json
/// "models": ["glm-5.3", "kimi-k3", "minimax-m3", "qwen3.8-max", "grok-4.6"]
/// ```
///
/// 三个 key 与端点后缀一一对应，缺省组可省略。
#[derive(Debug, Clone, Default, Serialize)]
pub struct ModelsSpec {
    /// `POST {base_url}/chat/completions`（OpenAI Chat Completions）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completions: Vec<String>,
    /// `POST {base_url}/messages`（Anthropic Messages）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<String>,
    /// `POST {base_url}/responses`（OpenAI Responses）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub responses: Vec<String>,
}

impl ModelsSpec {
    /// Iterates every configured model together with its wire protocol.
    pub fn iter(&self) -> impl Iterator<Item = (&str, ApiProtocol)> {
        self.completions
            .iter()
            .map(|m| (m.as_str(), ApiProtocol::ChatCompletions))
            .chain(
                self.messages
                    .iter()
                    .map(|m| (m.as_str(), ApiProtocol::Messages)),
            )
            .chain(
                self.responses
                    .iter()
                    .map(|m| (m.as_str(), ApiProtocol::Responses)),
            )
    }

    /// Whether the given model id is listed under any protocol.
    pub fn contains(&self, model: &str) -> bool {
        self.iter().any(|(m, _)| m == model)
    }

    /// Whether no model is configured at all.
    pub fn is_empty(&self) -> bool {
        self.completions.is_empty() && self.messages.is_empty() && self.responses.is_empty()
    }

    /// 按模型命名推断其端点协议，用于扁平数组写法。
    ///
    /// - `minimax*` / `qwen*` → Anthropic `messages` 端点
    /// - `grok*` / `gpt*` / `muse-spark*` → OpenAI `responses` 端点
    /// - 其余（glm / kimi / deepseek / mimo / hy / longcat …）→ Chat Completions
    ///
    /// 带 provider 前缀的 ID（如 `Go/deepseek-v4-flash`）按 `/` 取尾段再匹配。
    pub fn infer_protocol(model: &str) -> ApiProtocol {
        let lower = model.to_ascii_lowercase();
        let bare = lower.rsplit('/').next().unwrap_or(lower.as_str());
        if bare.starts_with("minimax") || bare.starts_with("qwen") {
            ApiProtocol::Messages
        } else if bare.starts_with("grok")
            || bare.starts_with("gpt")
            || bare.starts_with("muse-spark")
        {
            ApiProtocol::Responses
        } else {
            ApiProtocol::ChatCompletions
        }
    }

    /// 按推断出的协议把一个模型追加到对应分组。
    pub fn push_inferred(&mut self, model: String) {
        match Self::infer_protocol(&model) {
            ApiProtocol::Messages => self.messages.push(model),
            ApiProtocol::Responses => self.responses.push(model),
            ApiProtocol::ChatCompletions => self.completions.push(model),
        }
    }
}

/// 兼容两种 `models` 写法的反序列化：分组对象或扁平数组。
///
/// serde 的 derive 只能处理对象形式，因此这里手写 visitor：
/// - 数组：逐个按 [`ModelsSpec::infer_protocol`] 归组；
/// - 对象：读取 `completions` / `messages` / `responses`，未知 key 忽略（向前兼容）。
impl<'de> Deserialize<'de> for ModelsSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{MapAccess, SeqAccess, Visitor};
        use std::fmt;

        struct ModelsSpecVisitor;

        impl<'de> Visitor<'de> for ModelsSpecVisitor {
            type Value = ModelsSpec;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("模型 ID 数组或按协议分组的对象")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut spec = ModelsSpec::default();
                while let Some(model) = seq.next_element::<String>()? {
                    spec.push_inferred(model);
                }
                Ok(spec)
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut spec = ModelsSpec::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.trim().to_ascii_lowercase().as_str() {
                        "completions" | "chat_completions" => {
                            spec.completions.extend(map.next_value::<Vec<String>>()?)
                        }
                        "messages" => spec.messages.extend(map.next_value::<Vec<String>>()?),
                        "responses" => spec.responses.extend(map.next_value::<Vec<String>>()?),
                        _ => {
                            // 未知分组：消费值后忽略，保持向前兼容。
                            let _: serde_json::Value = map.next_value()?;
                            tracing::warn!("models: ignoring unknown protocol group '{key}'");
                        }
                    }
                }
                Ok(spec)
            }
        }

        deserializer.deserialize_any(ModelsSpecVisitor)
    }
}

/// A resolved model endpoint: model id + base URL + credentials + wire protocol.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    /// Model id to send on the wire.
    pub model_id: String,
    pub base_url: String,
    pub api_key: String,
    pub protocol: ApiProtocol,
}

/// 单个模型服务提供者（如 opencode、openai、custom）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProviderConfig {
    /// 该提供者的 API 基础地址（如 `https://opencode.ai/zen/v1`）。
    pub base_url: String,
    /// API 密钥（部分免费服务可填 `"public"`）。
    pub api_key: String,
    /// 该提供者下可用的模型：按端点协议分组的对象，或扁平数组（自动推断协议）。
    pub models: ModelsSpec,
    /// 展示名（设置页用；缺省时显示 provider key）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// 被 `providers/disable` 停用：条目保留（含密钥），可再次启用。
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: bool,
    /// 该条目由环境变量提供（仅内存态）：不落盘，也不允许通过 ACP 改写。
    #[serde(skip)]
    pub env_var: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl ModelProviderConfig {
    /// 新建一个可落盘的 provider 条目。
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        models: ModelsSpec,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            models,
            label: None,
            disabled: false,
            env_var: None,
        }
    }
}

/// 一次 provider 写入（`providers/set`）。`None` 字段表示保留原值。
pub struct ProviderUpsert {
    pub id: String,
    pub label: Option<String>,
    pub base_url: String,
    pub api_key: Option<String>,
    pub models: Option<ModelsSpec>,
    pub disabled: Option<bool>,
}

/// 顶层模型配置文件结构。
///
/// 配置文件路径：`$HOME/.agentscope/models.json`
///（Windows 下为 `%USERPROFILE%\.agentscope\models.json`）。
///
/// 首次运行时若不存在，会自动写入默认配置。用户可在此基础上自由
/// 增加新的模型服务提供者和对应的模型列表。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    /// 默认使用的提供者键名（对应 `providers` 中的 key）。
    pub default_provider: String,
    /// 默认使用的模型 ID（须存在于 default_provider 的 models 列表中）。
    pub default_model: String,
    /// 所有模型服务提供者，键名为用户自定义的别名。
    pub providers: HashMap<String, ModelProviderConfig>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        let mut providers = HashMap::new();
        providers.insert(
            "Zen".to_string(),
            ModelProviderConfig::new(
                "https://opencode.ai/zen/v1",
                "public",
                ModelsSpec {
                    completions: vec![
                        "big-pickle".to_string(),
                        "mimo-v2.5-free".to_string(),
                        "ling-3.0-flash-fin-free".to_string(),
                        "nemotron-3-ultra-free".to_string(),
                        "nemotron-3.5-lightning-free".to_string(),
                    ],
                    messages: vec![],
                    responses: vec!["muse-spark-1.3-contributor-free".to_string()],
                },
            ),
        );
        Self {
            default_provider: "Zen".to_string(),
            default_model: "mimo-v2.5-free".to_string(),
            providers,
        }
    }
}

impl ModelConfig {
    /// 配置文件的默认路径：`$HOME/.agentscope/models.json`。
    ///
    /// `AGENTSCOPE_MODELS_JSON` 可覆盖它（测试、多配置并存）。
    pub fn default_path() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("AGENTSCOPE_MODELS_JSON").filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(path));
        }
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)?;
        Some(home.join(".agentscope").join("models.json"))
    }

    /// 加载配置文件；仅当文件**不存在**时才创建默认配置并写入磁盘。
    ///
    /// 注意：文件已存在但内容无法解析时（例如编辑时引入的语法错误，或
    /// Windows 编辑器写入的 UTF-8 BOM）**不会覆写原文件**——本次运行回退
    /// 到内置默认值并打印告警，避免用户的自定义配置被静默丢弃。
    ///
    /// 解析前会做宽容预处理（剥离注释与尾随逗号），因此配置文件中可以
    /// 写 `//` / `/* */` 注释和多余逗号。
    pub fn load_or_create() -> Self {
        let Some(path) = Self::default_path() else {
            tracing::warn!("model-config: cannot resolve $HOME; using built-in defaults");
            eprintln!("[model-config] warning: cannot resolve $HOME; using built-in defaults");
            return Self::default();
        };
        tracing::debug!("model-config: config path {}", path.display());
        if path.exists() {
            if let Some(cfg) = Self::try_load(&path) {
                tracing::info!(
                    "model-config: loaded {} (default_provider='{}', default_model='{}', providers={}, models={})",
                    path.display(),
                    cfg.default_provider,
                    cfg.default_model,
                    cfg.providers.len(),
                    cfg.model_count(),
                );
                return cfg;
            }
            tracing::error!(
                "model-config: failed to parse {}; keeping file and falling back to built-in defaults",
                path.display()
            );
            eprintln!(
                "[model-config] warning: failed to parse {}; keeping the file \
                 unchanged and using built-in defaults for this run",
                path.display()
            );
            return Self::default();
        }
        let cfg = Self::default();
        cfg.save();
        cfg
    }

    /// 尝试从磁盘加载配置文件，读取或解析失败时返回 `None`。
    ///
    /// 先按严格 JSON 解析；失败时再做一次宽容预处理（剥离注释与尾随逗号）
    /// 后重试，使手工编辑的配置允许 `//`、`/* */` 注释和多余逗号。
    /// 任意换行 / 缩进 / 空行 serde_json 本就支持，无需特殊处理。
    fn try_load(path: &std::path::Path) -> Option<Self> {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("model-config: failed to read {}: {e}", path.display());
                return None;
            }
        };
        tracing::debug!("model-config: read {} bytes", content.len());
        // 容忍编辑器（尤其 Windows 记事本）写入的 UTF-8 BOM，
        // 否则 serde_json 会解析失败并被误判为“配置文件无效”。
        let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
        match serde_json::from_str::<Self>(content) {
            Ok(cfg) => return Some(cfg),
            Err(e) => {
                tracing::debug!(
                    "model-config: strict JSON parse failed ({e}); retrying lenient pass"
                );
            }
        }
        let sanitized = sanitize_json(content);
        match serde_json::from_str::<Self>(&sanitized) {
            Ok(cfg) => {
                tracing::debug!("model-config: lenient pass parsed successfully");
                Some(cfg)
            }
            Err(e) => {
                tracing::error!("model-config: lenient pass still failed: {e}");
                None
            }
        }
    }

    /// 将当前配置写入磁盘（覆盖已有文件）。
    ///
    /// 环境变量提供的 provider（`env_var`）不落盘：它们是运行环境的东西，
    /// 写进文件反而会把一次性密钥固定下来。先写临时文件再改名，
    /// 写到一半被杀死也不会留下半份 JSON。
    pub fn save(&self) {
        let Some(path) = Self::default_path() else {
            return;
        };
        let mut persisted = self.clone();
        persisted.providers.retain(|_, p| p.env_var.is_none());
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(&persisted) {
            Ok(json) => {
                let tmp = path.with_extension("json.tmp");
                let written =
                    std::fs::write(&tmp, &json).and_then(|()| std::fs::rename(&tmp, &path));
                match written {
                    Ok(()) => eprintln!("[model-config] config written to {}", path.display()),
                    Err(e) => {
                        let _ = std::fs::remove_file(&tmp);
                        eprintln!(
                            "[model-config] warning: failed to write {}: {}",
                            path.display(),
                            e
                        );
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "[model-config] warning: failed to serialize model config: {}",
                    e
                );
            }
        }
    }

    /// 新增或替换一个 provider（不落盘时调用失败会返回错误）。
    ///
    /// 新增时 `models` 缺省为空列表，`api_key` 缺省为空；编辑时 `None` 字段保留原值。
    pub fn upsert_provider(&mut self, upsert: ProviderUpsert) -> Result<(), String> {
        if upsert.id.trim().is_empty() {
            return Err("provider id must not be empty".to_string());
        }
        let is_new = !self.providers.contains_key(&upsert.id);
        if let Some(existing) = self.providers.get(&upsert.id) {
            if let Some(var) = &existing.env_var {
                return Err(format!(
                    "provider '{}' comes from the environment variable {var}; change it there",
                    upsert.id
                ));
            }
        }
        // 新建的 provider 没有任何模型时，至少把默认模型指过去，避免下拉为空。
        let needs_default =
            self.default_model.is_empty() || !self.providers.contains_key(&self.default_provider);
        // 空地址表示「不改地址」（启用/停用等局部更新）；新建条目则必须给地址，
        // 否则会落下一份连不通、也无法 probe 的配置。
        if is_new && upsert.base_url.trim().is_empty() {
            return Err(format!("provider '{}' needs a base URL", upsert.id));
        }
        let entry = self.providers.entry(upsert.id.clone()).or_insert_with(|| {
            ModelProviderConfig::new(String::new(), String::new(), ModelsSpec::default())
        });
        // 空地址表示「不改地址」（启用/停用等局部更新），新建时由调用方必须给地址。
        let base_url = upsert.base_url.trim().trim_end_matches('/');
        if !base_url.is_empty() {
            entry.base_url = base_url.to_string();
        }
        if let Some(models) = upsert.models {
            entry.models = models;
        }
        if let Some(key) = upsert.api_key {
            entry.api_key = key;
        }
        if let Some(label) = upsert.label {
            entry.label = Some(label).filter(|label| !label.trim().is_empty());
        }
        if let Some(disabled) = upsert.disabled {
            entry.disabled = disabled;
        }
        if needs_default {
            if let Some(first) = entry.models.iter().next().map(|(id, _)| id.to_string()) {
                self.default_provider = upsert.id.clone();
                self.default_model = first;
            }
        }
        Ok(())
    }

    /// 停用/启用一个 provider。停用只打标记，不动密钥，可再次启用。
    pub fn set_disabled(&mut self, id: &str, disabled: bool) -> Result<(), String> {
        match self.providers.get_mut(id) {
            Some(provider) => {
                if let Some(var) = &provider.env_var {
                    return Err(format!(
                        "provider '{id}' comes from the environment variable {var}; change it there"
                    ));
                }
                provider.disabled = disabled;
                Ok(())
            }
            None => Err(format!("unknown provider '{id}'")),
        }
    }

    /// 删除一个 provider，并修正默认值（指向它时回退到第一个可用条目）。
    pub fn remove_provider(&mut self, id: &str) -> Result<(), String> {
        let Some(provider) = self.providers.get(id) else {
            return Err(format!("unknown provider '{id}'"));
        };
        if let Some(var) = &provider.env_var {
            return Err(format!(
                "provider '{id}' comes from the environment variable {var}; change it there"
            ));
        }
        self.providers.remove(id);
        if self.default_provider == id {
            let fallback = self.sorted_keys().into_iter().next().ok_or_else(|| {
                "the last provider was removed; add one before using the agent".to_string()
            })?;
            self.default_provider = fallback.clone();
            self.default_model = self.providers[&fallback]
                .models
                .iter()
                .next()
                .map(|(id, _)| id.to_string())
                .unwrap_or_default();
        }
        Ok(())
    }

    /// 设置默认 provider / 模型。`model` 缺省时用该 provider 的第一个模型。
    pub fn set_default(&mut self, provider: &str, model: Option<&str>) -> Result<(), String> {
        let entry = self
            .providers
            .get(provider)
            .ok_or_else(|| format!("unknown provider '{provider}'"))?;
        let model = match model.filter(|m| !m.trim().is_empty()) {
            Some(model) => {
                if !entry.models.contains(model) {
                    return Err(format!(
                        "model '{model}' is not listed under provider '{provider}'"
                    ));
                }
                model.to_string()
            }
            None => entry
                .models
                .iter()
                .next()
                .map(|(id, _)| id.to_string())
                .ok_or_else(|| format!("provider '{provider}' has no models"))?,
        };
        self.default_provider = provider.to_string();
        self.default_model = model;
        Ok(())
    }

    /// provider key 排序后的列表（稳定输出用）。
    pub fn sorted_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.providers.keys().cloned().collect();
        keys.sort();
        keys
    }

    /// 加载配置，并用已导出的 provider 密钥补齐配置（“0 配置”使用）。
    ///
    /// 只补配置里还没有的 provider（用户手写的 base_url / 模型列表优先），且不写盘：
    /// 环境变量随时可加可删，落盘反而会把一次性密钥写死。默认值仅当当前默认 provider
    /// 没有真实凭据时才切到检测到的第一个。
    pub fn load_or_create_with_env() -> Self {
        let mut config = Self::load_or_create();
        let catalog = ModelCatalog::global();
        catalog.ensure_loaded();
        let providers = setup::catalog_providers(&catalog.snapshot());
        let detected = setup::detect_env(&providers, |name| std::env::var(name).ok());
        config.enable_detected_providers(&detected);
        config
    }

    /// 把带环境变量密钥的 provider 并入内存配置。
    fn enable_detected_providers(&mut self, detected: &[DetectedProvider]) {
        for found in detected {
            if self.providers.contains_key(&found.provider.id) {
                continue;
            }
            tracing::info!(
                "model-config: {} is configured via {}; enabling it",
                found.provider.id,
                found.var
            );
            self.providers.insert(
                found.provider.id.clone(),
                ModelProviderConfig {
                    base_url: found.provider.base_url.clone(),
                    api_key: found.key.clone(),
                    models: models_for(&found.provider),
                    label: None,
                    disabled: false,
                    env_var: Some(found.var.clone()),
                },
            );
        }
        let placeholder = self
            .providers
            .get(&self.default_provider)
            .is_none_or(|provider| is_placeholder_key(&provider.api_key));
        if placeholder {
            if let Some((provider_id, model)) = detected.first().and_then(|found| {
                found
                    .provider
                    .models
                    .first()
                    .map(|model| (found.provider.id.clone(), model.clone()))
            }) {
                self.default_provider = provider_id;
                self.default_model = model;
            }
        }
    }

    /// 把 `--setup` 的结果并入配置：只动被选中的 provider 和两个默认值，其它 provider 原样保留。
    pub fn apply_plan(&mut self, plan: &SetupPlan) {
        self.providers.insert(
            plan.provider.id.clone(),
            ModelProviderConfig::new(
                plan.provider.base_url.clone(),
                plan.api_key.clone(),
                models_for(&plan.provider),
            ),
        );
        self.default_provider = plan.provider.id.clone();
        self.default_model = plan.model.clone();
    }

    /// 是否配置了任何凭据（只要有非空的 api_key 就算：免费服务的 `"public"` 也是凭据）。
    pub fn has_credential(&self) -> bool {
        self.providers
            .values()
            .any(|provider| !provider.api_key.trim().is_empty())
    }

    /// 读取已存在的配置文件；文件不存在或无法解析时返回 `None`。
    ///
    /// 与 [`load_or_create`](Self::load_or_create) 的区别：不写盘，也不把“解析失败”
    /// 当成默认值——`--setup` 要靠这一点避免覆盖用户手写但暂时写错的配置。
    pub fn load_existing() -> Option<Self> {
        let path = Self::default_path()?;
        path.exists().then(|| Self::try_load(&path)).flatten()
    }

    /// 根据 provider 名称和模型 ID 解析出 `(base_url, api_key, model_id)`。
    ///
    /// 查找优先级：
    /// 1. 环境变量 `CODING_PROVIDER` + `CODING_MODEL_ID`（覆盖配置文件）
    /// 2. 配置文件中的 default_provider / default_model
    pub fn resolve(&self) -> ResolvedModel {
        // 1. 环境变量覆盖
        let provider_name = std::env::var("CODING_PROVIDER")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| self.default_provider.clone());

        let model_id = std::env::var("CODING_MODEL_ID")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| self.default_model.clone());

        // 2. 从 providers 中查找
        if let Some(provider) = self.providers.get(&provider_name) {
            let api_key = resolve_api_key(&provider_name, &provider.api_key);
            let base_url = resolve_base_url(&provider_name, &provider.base_url);

            let protocol = protocol_for(&provider.models, &model_id);
            tracing::info!(
                "model-config: resolve default provider='{provider_name}' model='{model_id}' \
                 protocol={} base_url='{base_url}'",
                protocol.label()
            );
            ResolvedModel {
                model_id,
                base_url,
                api_key,
                protocol,
            }
        } else {
            tracing::warn!(
                "model-config: provider '{provider_name}' not found; falling back to env/defaults"
            );
            eprintln!(
                "[model-config] warning: provider '{}' not found in config, \
                 falling back to environment variables or defaults",
                provider_name
            );
            let base_url = std::env::var("OPENAI_BASE_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "https://opencode.ai/zen/v1".to_string());
            let api_key = std::env::var("OPENAI_API_KEY")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "public".to_string());
            ResolvedModel {
                model_id,
                base_url,
                api_key,
                protocol: ApiProtocol::ChatCompletions,
            }
        }
    }

    /// 根据模型 ID 查找其所属的 provider，返回 `(base_url, api_key)`。
    ///
    /// 兼容两种 ID 形式：裸 ID（`deepseek-v4-flash`）与带 provider 前缀的
    /// ID（`Go/deepseek-v4-flash`，即客户端下拉里展示的 `provider/model`
    /// 格式）——后者优先在指定 provider 下查找，找不到再退回全量查找。
    ///
    /// provider 按 key 排序后确定性遍历，避免同一模型 ID 出现在多个 provider
    /// 时（HashMap 迭代顺序随机）解析到不稳定的 base_url / api_key。
    ///
    /// 环境变量 `OPENAI_API_KEY` / `OPENAI_BASE_URL` 可覆盖查找结果。
    pub fn resolve_model(&self, model_id: &str) -> Result<ResolvedModel, String> {
        let bare = model_id.rsplit('/').next().unwrap_or(model_id);
        // 带前缀时（如 `Go/deepseek-v4-flash`）优先精确匹配该 provider。
        let prefix = model_id
            .split_once('/')
            .map(|(p, _)| p)
            .filter(|p| !p.is_empty());
        // 下拉列表在同名模型出现在多个 provider 下时会合成 `ProviderKey/model`
        // （见 `all_models`）。剥掉这段本地 provider key 后才是配置文件的原始值；
        // 配置项本身带 `/`（如 `workbuddy/auto`）时不会被剥，因为 `workbuddy`
        // 不是 provider key。
        let configured = match model_id.split_once('/') {
            Some((p, rest)) if self.providers.keys().any(|k| k.eq_ignore_ascii_case(p)) => rest,
            _ => model_id,
        };

        let mut keys: Vec<&String> = self.providers.keys().collect();
        keys.sort();
        if let Some(p) = prefix {
            if let Some(pos) = keys.iter().position(|k| k.eq_ignore_ascii_case(p)) {
                let key = keys.remove(pos);
                keys.insert(0, key);
            }
        }

        for key in keys {
            let provider = &self.providers[key];
            // 请求上送的 model 必须与配置文件里的写法一致：优先用原始值（其中可能
            // 自带 `/`，如网关要求的 `workbuddy/deepseek-v4.1-flash`），只有合成
            // 前缀形态才退到 `configured` / `bare`。
            let matched = if provider.models.contains(model_id) {
                model_id
            } else if provider.models.contains(configured) {
                configured
            } else if provider.models.contains(bare) {
                bare
            } else {
                continue;
            };
            let api_key = resolve_api_key(key, &provider.api_key);
            let base_url = resolve_base_url(key, &provider.base_url);
            // 协议按实际命中的写法查表，否则带前缀的项会退化成 ChatCompletions。
            let protocol = protocol_for(&provider.models, matched);
            tracing::info!(
                "model-config: resolve_model '{model_id}' -> provider='{key}' \
                 model='{matched}' protocol={} base_url='{base_url}'",
                protocol.label()
            );
            return Ok(ResolvedModel {
                model_id: matched.to_string(),
                base_url,
                api_key,
                protocol,
            });
        }
        tracing::error!("model-config: model '{model_id}' not found in any provider");
        Err(format!(
            "model '{model_id}' not found in any provider of ~/.agentscope/models.json"
        ))
    }

    /// 统计去重后的模型总数（不产生日志，供 `load_or_create` 打印摘要用）。
    pub fn model_count(&self) -> usize {
        let mut seen = std::collections::HashSet::new();
        for provider in self.providers.values() {
            for (id, _) in provider.models.iter() {
                seen.insert(id);
            }
        }
        seen.len()
    }

    /// 汇总所有 provider 下的模型列表，生成 ACP 的 `ModelChoice` 列表。
    ///
    /// 遍历 `providers` 中每个 provider 的 `models` 列表，去重后生成
    /// `ModelChoice { id, name }`。其中 `name` 使用 `provider_key/model_id`
    /// 格式以区分不同供应商，`id` 保持为模型 ID。用于动态填充 ACP
    /// `setModel` 下拉可选项，无需在代码中硬编码。
    ///
    /// provider 按 key 排序后确定性遍历，保证下拉顺序稳定。
    pub fn all_models(&self) -> Vec<ModelChoice> {
        let mut keys: Vec<&String> = self
            .providers
            .iter()
            .filter(|(_, provider)| !provider.disabled)
            .map(|(key, _)| key)
            .collect();
        keys.sort();

        // Count bare ids first so only genuinely ambiguous ids get a provider prefix.
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for provider_key in &keys {
            for id in self.providers[*provider_key]
                .models
                .iter()
                .map(|(id, _)| id)
            {
                *counts.entry(id.to_string()).or_default() += 1;
            }
        }

        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for provider_key in keys {
            let provider = &self.providers[provider_key];
            let mut per_provider = 0usize;
            for (id, protocol) in provider.models.iter() {
                let dedupe_key = (provider_key.to_string(), id.to_string());
                if !seen.insert(dedupe_key) {
                    continue;
                }
                per_provider += 1;
                let ambiguous = counts.get(id).copied().unwrap_or(0) > 1;
                let value = if ambiguous {
                    format!("{provider_key}/{id}")
                } else {
                    id.to_string()
                };
                tracing::debug!(
                    "model-config: provider='{provider_key}' model='{id}' protocol={}",
                    protocol.label()
                );
                out.push(ModelChoice {
                    id: value,
                    name: format!("{provider_key}/{id}"),
                    provider: Some(provider_key.clone()),
                    catalog_key: None,
                    base_url: Some(provider.base_url.clone()),
                });
            }
            tracing::info!(
                "model-config: provider '{provider_key}' provides {per_provider} models \
                 (completions={}, messages={}, responses={})",
                provider.models.completions.len(),
                provider.models.messages.len(),
                provider.models.responses.len(),
            );
        }
        tracing::info!("model-config: {} selectable models in total", out.len());
        out
    }
}

/// Looks up the wire protocol a model id is configured under.
///
/// Falls back to [`ApiProtocol::ChatCompletions`] when the model is not listed
/// (e.g. an env-var override pointing at an unlisted model).
fn protocol_for(models: &ModelsSpec, model_id: &str) -> ApiProtocol {
    models
        .iter()
        .find(|(m, _)| *m == model_id)
        .map(|(_, p)| p)
        .unwrap_or(ApiProtocol::ChatCompletions)
}

/// Reads an override from the environment, treating unset/blank as absent.
fn env_override(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Masks a secret for logs (`oc_s...FBM5`), keeping only enough to identify it.
pub fn mask_secret(secret: &str) -> String {
    if secret.trim().is_empty() {
        return String::new();
    }
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= 8 {
        return "***".to_string();
    }
    let head: String = chars.iter().take(4).collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}...{tail}")
}

/// Resolves a provider's api key, letting `OPENAI_API_KEY` override the config file.
///
/// A globally exported key silently taking precedence over `models.json` is a common
/// source of confusing 401s, so the override is called out in the log (and stderr).
/// 把目录里的模型按协议归到 `ModelsSpec` 的对应分组。
fn models_for(provider: &setup::CatalogProvider) -> ModelsSpec {
    let mut models = ModelsSpec::default();
    match provider.protocol {
        ApiProtocol::Messages => models.messages.extend(provider.models.iter().cloned()),
        ApiProtocol::Responses => models.responses.extend(provider.models.iter().cloned()),
        ApiProtocol::ChatCompletions => models.completions.extend(provider.models.iter().cloned()),
    }
    models
}

/// 占位凭据：空值，或免费服务的 `"public"`。
fn is_placeholder_key(api_key: &str) -> bool {
    let key = api_key.trim();
    key.is_empty() || key.eq_ignore_ascii_case("public")
}

fn resolve_api_key(provider_name: &str, configured: &str) -> String {
    let Some(key) = env_override("OPENAI_API_KEY") else {
        return configured.to_string();
    };
    if key != configured {
        tracing::warn!(
            "model-config: OPENAI_API_KEY overrides provider '{provider_name}' api_key \
             (env={}, config={})",
            mask_secret(&key),
            mask_secret(configured),
        );
        eprintln!(
            "[model-config] warning: OPENAI_API_KEY is set and overrides the api_key \
             configured for provider '{provider_name}' (using env key {}); unset it or \
             update it to use the provider key",
            mask_secret(&key),
        );
    }
    key
}

/// Resolves a provider's base URL, letting `OPENAI_BASE_URL` override the config file.
fn resolve_base_url(provider_name: &str, configured: &str) -> String {
    let Some(url) = env_override("OPENAI_BASE_URL") else {
        return configured.to_string();
    };
    if url.trim_end_matches('/') != configured.trim_end_matches('/') {
        tracing::warn!(
            "model-config: OPENAI_BASE_URL overrides provider '{provider_name}' base_url \
             (env='{url}', config='{configured}')"
        );
    }
    url
}

/// 把“类 JSON5”文本预处理为严格 JSON：字符串感知地剥离 `//` 行注释、
/// `/* */` 块注释，并删除 `}` / `]` 前的尾随逗号。
///
/// 仅做这两项变换——字符串内部的 `//`（如 URL `https://…`）与转义引号
/// 不会被误伤；单引号、无引号 key 等更强的 JSON5 语法不在支持范围。
fn sanitize_json(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
        } else if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
        } else if c == '/' && chars.get(i + 1) == Some(&'/') {
            // 行注释：跳到行尾（保留换行，便于报错定位）。
            i += 2;
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            // 块注释：跳到闭合的 `*/`。
            i += 2;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i = (i + 2).min(chars.len());
        } else if c == ',' {
            // 尾随逗号：逗号后跳过空白若是 `}` / `]` 则丢弃。
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if matches!(chars.get(j), Some(&'}') | Some(&']')) {
                i += 1;
            } else {
                out.push(c);
                i += 1;
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A provider whose configured model ids themselves contain `/` (a gateway that
    /// routes by `vendor/model`), plus one bare id that exists under two providers so
    /// the dropdown has to synthesize a `ProviderKey/model` value.
    fn config() -> ModelConfig {
        let mut providers = HashMap::new();
        providers.insert(
            "Gateway".to_string(),
            ModelProviderConfig::new(
                "http://127.0.0.1:3425/v1",
                "magpie",
                ModelsSpec {
                    completions: vec![
                        "workbuddy/auto".to_string(),
                        "deepseek/deepseek-v4-flash".to_string(),
                    ],
                    messages: vec!["workbuddy/minimax-m3.1-flash".to_string()],
                    responses: vec![],
                },
            ),
        );
        providers.insert(
            "Go".to_string(),
            ModelProviderConfig::new(
                "https://example.invalid/v1",
                "k",
                ModelsSpec {
                    completions: vec!["auto".to_string()],
                    messages: vec![],
                    responses: vec![],
                },
            ),
        );
        ModelConfig {
            default_provider: "Gateway".to_string(),
            default_model: "workbuddy/auto".to_string(),
            providers,
        }
    }

    /// `base_url` may be overridden by `OPENAI_BASE_URL`, so only assert when absent.
    fn assert_base_url(resolved: &ResolvedModel, expected: &str) {
        let overridden = std::env::var("OPENAI_BASE_URL")
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
        if !overridden {
            assert_eq!(resolved.base_url, expected);
        }
    }

    #[test]
    fn resolve_model_sends_the_configured_id_verbatim() {
        let cfg = config();
        for id in [
            "workbuddy/auto",
            "deepseek/deepseek-v4-flash",
            "workbuddy/minimax-m3.1-flash",
        ] {
            let resolved = cfg.resolve_model(id).unwrap();
            assert_eq!(resolved.model_id, id, "wire model must match models.json");
            assert_base_url(&resolved, "http://127.0.0.1:3425/v1");
        }
    }

    #[test]
    fn resolve_model_keeps_the_matched_protocol_group() {
        let cfg = config();
        let resolved = cfg.resolve_model("workbuddy/minimax-m3.1-flash").unwrap();
        assert_eq!(resolved.protocol, ApiProtocol::Messages);
        let resolved = cfg.resolve_model("workbuddy/auto").unwrap();
        assert_eq!(resolved.protocol, ApiProtocol::ChatCompletions);
    }

    #[test]
    fn resolve_model_strips_only_a_real_provider_prefix() {
        let cfg = config();
        // `Go/auto` is the disambiguated dropdown value; the configured id is `auto`.
        let resolved = cfg.resolve_model("Go/auto").unwrap();
        assert_eq!(resolved.model_id, "auto");
        assert_base_url(&resolved, "https://example.invalid/v1");
        // `Gateway/<configured id>` is the same form for an id that contains `/`.
        let resolved = cfg.resolve_model("Gateway/workbuddy/auto").unwrap();
        assert_eq!(resolved.model_id, "workbuddy/auto");
        assert_base_url(&resolved, "http://127.0.0.1:3425/v1");
    }

    #[test]
    fn resolve_model_rejects_unknown_ids() {
        let cfg = config();
        assert!(cfg.resolve_model("Gateway/nope").is_err());
    }

    fn catalog_provider(
        id: &str,
        models: &[&str],
        protocol: ApiProtocol,
    ) -> setup::CatalogProvider {
        setup::CatalogProvider {
            id: id.to_string(),
            base_url: format!("https://{id}.example/v1"),
            env: vec![format!("{}_API_KEY", id.to_ascii_uppercase())],
            models: models.iter().map(|model| model.to_string()).collect(),
            protocol,
        }
    }

    #[test]
    fn a_not_yet_configured_provider_is_enabled_from_its_environment_key() {
        let mut cfg = ModelConfig::default();
        // The built-in Zen key is the free-endpoint placeholder `"public"`: it counts as a
        // credential (the agent works out of the box), but it must not block env takeover.
        assert!(cfg.has_credential());
        cfg.enable_detected_providers(&[DetectedProvider {
            provider: catalog_provider(
                "anthropic",
                &["claude-sonnet", "claude-haiku"],
                ApiProtocol::Messages,
            ),
            var: "ANTHROPIC_API_KEY".to_string(),
            key: "sk-ant".to_string(),
        }]);

        let provider = cfg.providers.get("anthropic").expect("enabled");
        assert_eq!(provider.base_url, "https://anthropic.example/v1");
        assert_eq!(provider.api_key, "sk-ant");
        // The protocol decides the group, so the wire path is `/messages`.
        assert_eq!(provider.models.messages, ["claude-sonnet", "claude-haiku"]);
        assert!(provider.models.completions.is_empty());
        // A placeholder default is replaced; models keep their catalog order.
        assert_eq!(cfg.default_provider, "anthropic");
        assert_eq!(cfg.default_model, "claude-sonnet");
        assert!(cfg.has_credential());
        // Environment-backed entries are memory only: never written to the file.
        assert_eq!(provider.env_var.as_deref(), Some("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn a_configured_provider_is_never_overwritten_by_the_environment() {
        let mut cfg = config();
        cfg.default_provider = "Gateway".to_string();
        cfg.default_model = "workbuddy/auto".to_string();
        cfg.enable_detected_providers(&[DetectedProvider {
            provider: catalog_provider(
                "deepseek",
                &["deepseek-chat"],
                ApiProtocol::ChatCompletions,
            ),
            var: "DEEPSEEK_API_KEY".to_string(),
            key: "sk-deepseek".to_string(),
        }]);
        // New provider available, but the user's defaults stay untouched.
        assert!(cfg.providers.contains_key("deepseek"));
        assert_eq!(cfg.default_provider, "Gateway");
        assert_eq!(cfg.default_model, "workbuddy/auto");
    }

    #[test]
    fn setup_merges_the_chosen_provider_and_keeps_the_rest() {
        let mut cfg = config();
        let plan = SetupPlan {
            provider: catalog_provider(
                "deepseek",
                &["deepseek-chat"],
                ApiProtocol::ChatCompletions,
            ),
            model: "deepseek-chat".to_string(),
            api_key: "sk-typed".to_string(),
            key_source: None,
        };
        cfg.apply_plan(&plan);

        // The pre-existing providers are still there.
        assert!(cfg.providers.contains_key("Gateway"));
        assert!(cfg.providers.contains_key("Go"));
        let provider = cfg.providers.get("deepseek").expect("written");
        assert_eq!(provider.api_key, "sk-typed");
        assert_eq!(provider.models.completions, ["deepseek-chat"]);
        assert_eq!(cfg.default_provider, "deepseek");
        assert_eq!(cfg.default_model, "deepseek-chat");
        // ... and it resolves without the prefix synthesis the dropdown needs.
        let resolved = cfg.resolve_model("deepseek-chat").expect("resolves");
        assert_eq!(resolved.base_url, "https://deepseek.example/v1");
    }

    #[test]
    fn a_disabled_provider_leaves_the_picker_but_keeps_its_key() {
        let mut cfg = config();
        cfg.set_disabled("Go", true).unwrap();
        assert!(cfg.all_models().iter().all(|choice| choice.id != "auto"));
        // Disabling is not deleting: the entry (and its credentials) survive.
        assert_eq!(cfg.providers["Go"].api_key, "k");
        assert!(cfg.resolve_model("auto").is_ok());
        cfg.set_disabled("Go", false).unwrap();
        assert!(cfg.all_models().iter().any(|choice| choice.id == "auto"));
    }

    #[test]
    fn removing_the_default_provider_repoints_the_default() {
        let mut cfg = config();
        cfg.remove_provider("Gateway").unwrap();
        assert_eq!(cfg.default_provider, "Go");
        assert_eq!(cfg.default_model, "auto");
    }

    #[test]
    fn an_edit_keeps_the_key_the_client_did_not_send() {
        let mut cfg = config();
        cfg.upsert_provider(ProviderUpsert {
            id: "Gateway".to_string(),
            label: Some("My gateway".to_string()),
            base_url: "http://127.0.0.1:3425/v1/".to_string(),
            api_key: None,
            models: None,
            disabled: None,
        })
        .unwrap();
        let provider = &cfg.providers["Gateway"];
        assert_eq!(provider.api_key, "magpie");
        assert_eq!(provider.label.as_deref(), Some("My gateway"));
        // The trailing slash is normalised away: protocols append their own path.
        assert_eq!(provider.base_url, "http://127.0.0.1:3425/v1");
    }

    #[test]
    fn a_new_provider_needs_a_base_url_but_an_edit_does_not() {
        let mut cfg = config();
        // An empty address means "keep the current one" for an edit, but a new entry without
        // one is a config nobody can reach (and `probe` cannot test it either).
        assert!(cfg
            .upsert_provider(ProviderUpsert {
                id: "Blank".to_string(),
                label: None,
                base_url: "  ".to_string(),
                api_key: Some("k".to_string()),
                models: None,
                disabled: None,
            })
            .is_err());
        assert!(!cfg.providers.contains_key("Blank"));
        assert!(cfg
            .upsert_provider(ProviderUpsert {
                id: "Gateway".to_string(),
                label: Some("renamed".to_string()),
                base_url: String::new(),
                api_key: Some("magpie-2".to_string()),
                models: None,
                disabled: None,
            })
            .is_ok());
        assert_eq!(cfg.providers["Gateway"].api_key, "magpie-2");
        assert_eq!(
            cfg.providers["Gateway"].base_url,
            "http://127.0.0.1:3425/v1"
        );
    }

    #[test]
    fn an_environment_provider_cannot_be_edited_through_the_file() {
        let mut cfg = ModelConfig::default();
        cfg.enable_detected_providers(&[DetectedProvider {
            provider: catalog_provider(
                "deepseek",
                &["deepseek-chat"],
                ApiProtocol::ChatCompletions,
            ),
            var: "DEEPSEEK_API_KEY".to_string(),
            key: "sk-deepseek".to_string(),
        }]);
        assert!(cfg
            .upsert_provider(ProviderUpsert {
                id: "deepseek".to_string(),
                label: None,
                base_url: "https://elsewhere.example/v1".to_string(),
                api_key: Some("other".to_string()),
                models: None,
                disabled: None,
            })
            .is_err());
        assert!(cfg.remove_provider("deepseek").is_err());
    }
}
