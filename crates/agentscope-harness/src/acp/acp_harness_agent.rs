//! ACP CodingAgent over stdio — mirrors the Java `HarnessCodingAgent` /
//! Kotlin `AgiHarnessAgent`. Wires a full `HarnessAgent` (filesystem tools,
//! shell, compaction, plan mode) + skills behind an [`AcpCodingAgent`] that
//! speaks the Agent Client Protocol over stdio.
//!
//! Run:
//!   echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}' \
//!     | cargo run -p agentscope-harness --bin acp_harness_agent
//!
//! A full interactive ACP client (IDE/CLI) is the intended peer.
//!
//! # 子 Agent（subagents）
//!
//! `HarnessAgent` 在构建时会自动接线子 agent 编排（对齐 Java `SubagentsMiddleware`）：
//! 内置一个 `general-purpose` 子 agent（与主 agent 相同的模型/工具集，静态 leaf，
//! 不能再 spawn），并把 `agent_spawn` / `agent_send` / `agent_list` 与
//! `task_output` / `task_cancel` / `task_list` 注册进主 agent 的工具箱。
//!
//! 额外的子 agent 来自三处：
//! - `<workspace>/subagents/*.md`（每次推理重新扫描，含 front-matter：tools / steps /
//!   workspace.mode / model 等）
//! - builder 上的 `.subagent(decl)` / `.subagents(vec)`（程序化声明）
//! - builder 上的 `.subagent_factory(name, fn)`（完全自定义工厂）
//!
//! # 模型配置文件
//!
//! 所有模型通过 `$HOME/.agentscope/models.json` 配置文件管理。
//! 首次运行时若该文件不存在，会自动写入包含以下默认配置的 JSON 文件：
//!
//! 解析采用**宽容模式**：允许 `//` 行注释、`/* */` 块注释、尾随逗号，
//! 以及任意换行 / 缩进 / 空行（换行空白 serde_json 本就支持），方便手工编辑；
//! 文件已存在时永远不会被覆写（即使语法出错）。
//!
//! ```json
//! {
//!   "default_provider": "opencode",
//!   "default_model": "mimo-v2.5-free",
//!   "providers": {
//!     "opencode": {
//!       "base_url": "https://opencode.ai/zen/v1",
//!       "api_key": "public",
//!       "models": {
//!         "completions": [
//!           "mimo-v2.5-free",
//!           "ling-3.0-flash-fin-free",
//!           "nemotron-3-ultra-free",
//!           "nemotron-3.5-lightning-free"
//!         ],
//!         "responses": [
//!           "muse-spark-1.3-contributor-free",
//!           "muse-spark-1.2-contributor-free"
//!         ]
//!       }
//!     }
//!   }
//! }
//! ```
//!
//! `models` 也支持**扁平数组**写法（按模型命名自动推断协议分组）：
//!
//! ```json
//! "models": ["glm-5.3", "kimi-k3", "minimax-m3", "qwen3.8-max", "grok-4.6"]
//! ```
//!
//! # 端点协议分组
//!
//! 同一供应商下不同模型可能走不同端点，因此 `models` 推荐按协议分组，
//! 三个 key 与端点后缀一一对应（缺省组可省略）：
//!
//! | key | 端点 | 适用模型 |
//! | --- | --- | --- |
//! | `completions` | `POST {base_url}/chat/completions` | glm / kimi / deepseek / mimo / hy / longcat |
//! | `messages` | `POST {base_url}/messages` | minimax / qwen（Anthropic 协议） |
//! | `responses` | `POST {base_url}/responses` | grok / gpt / muse-spark |
//!
//! 写成扁平数组时，会按上表前缀自动归入对应分组。
//!
//! `base_url` 始终只写到版本前缀（如 `https://opencode.ai/zen/go/v1`），
//! 端点后缀由协议决定，不要写进 `base_url`。
//!
//! 用户可在此基础上自由增加新的模型服务提供者（provider）和对应的模型分组。
//!
//! 环境变量可临时覆盖配置文件中的值：
//! - `CODING_PROVIDER`  — 覆盖 default_provider
//! - `CODING_MODEL_ID`  — 覆盖 default_model
//! - `OPENAI_API_KEY`   — 覆盖 provider.api_key
//! - `OPENAI_BASE_URL`  — 覆盖 provider.base_url

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentscope_core::model::provider::{ApiProtocol, ModelCreationContext};
use agentscope_core::model::{GenerateOptions, Model};
use agentscope_extensions_model::{
    AnthropicMessagesModel, OpenAiChatModel, OpenAiResponsesModel,
};
use agentscope_core::state::JsonFileAgentStateStore;
use agentscope_harness::acp::{config::default_modes, AcpAgentSession, AcpCodingAgent};
use agentscope_harness::acp::ACP_CONTEXT_WINDOW_TOKENS;
use agentscope_harness::acp::config::ModelChoice;
use agentscope_harness::agent::{
    CompactionConfig, CompactionMiddlewareConfig, FileSystemSkillRepository, HarnessAgent,
    ToolResultEvictionConfig,
};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// 模型配置 (JSON)
// ---------------------------------------------------------------------------

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
    #[serde(default)]
    pub completions: Vec<String>,
    /// `POST {base_url}/messages`（Anthropic Messages）。
    #[serde(default)]
    pub messages: Vec<String>,
    /// `POST {base_url}/responses`（OpenAI Responses）。
    #[serde(default)]
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
            ModelProviderConfig {
                base_url: "https://opencode.ai/zen/v1".to_string(),
                api_key: "public".to_string(),
                models: ModelsSpec {
                    completions: vec![
                        "big-pickle".to_string(),
                        "mimo-v2.5-free".to_string(),
                        "ling-3.0-flash-fin-free".to_string(),
                        "nemotron-3-ultra-free".to_string(),
                        "nemotron-3.5-lightning-free".to_string(),
                    ],
                    messages: vec![],
                    responses: vec![
                        "muse-spark-1.3-contributor-free".to_string(),
                    ],
                },
            },
        );
        Self {
            default_provider: "Zen".to_string(),
            default_model: "mimo-v2.5-free".to_string(),
            providers,
        }
    }
}

impl ModelConfig {
    /// 返回配置文件的默认路径：`$HOME/.agentscope/models.json`。
    fn default_path() -> Option<PathBuf> {
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

    /// 将当前配置写入磁盘（覆盖已有文件）。目录不存在时自动创建。
    fn save(&self) {
        if let Some(path) = Self::default_path() {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match serde_json::to_string_pretty(self) {
                Ok(json) => {
                    if let Err(e) = std::fs::write(&path, &json) {
                        eprintln!(
                            "[model-config] warning: failed to write default config to {}: {}",
                            path.display(),
                            e
                        );
                    } else {
                        eprintln!(
                            "[model-config] default config written to {}",
                            path.display()
                        );
                    }
                }
                Err(e) => {
                    eprintln!("[model-config] warning: failed to serialize default config: {}", e);
                }
            }
        }
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
            let api_key = std::env::var("OPENAI_API_KEY")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| provider.api_key.clone());

            let base_url = std::env::var("OPENAI_BASE_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| provider.base_url.clone());

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
            if provider.models.contains(bare) || provider.models.contains(model_id) {
                let api_key = std::env::var("OPENAI_API_KEY")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| provider.api_key.clone());
                let base_url = std::env::var("OPENAI_BASE_URL")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| provider.base_url.clone());
                let protocol = protocol_for(&provider.models, bare);
                tracing::info!(
                    "model-config: resolve_model '{model_id}' -> provider='{key}' \
                     model='{bare}' protocol={} base_url='{base_url}'",
                    protocol.label()
                );
                return Ok(ResolvedModel {
                    model_id: bare.to_string(),
                    base_url,
                    api_key,
                    protocol,
                });
            }
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
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        let mut keys: Vec<&String> = self.providers.keys().collect();
        keys.sort();
        for provider_key in keys {
            let provider = &self.providers[provider_key];
            let mut per_provider = 0usize;
            for (id, protocol) in provider.models.iter() {
                if seen.insert(id.to_string()) {
                    per_provider += 1;
                    tracing::debug!(
                        "model-config: provider='{provider_key}' model='{id}' protocol={}",
                        protocol.label()
                    );
                    out.push(ModelChoice {
                        id: id.to_string(),
                        name: format!("{}/{}", provider_key, id),
                    });
                }
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

// ---------------------------------------------------------------------------
// Agent 构建
// ---------------------------------------------------------------------------

/// User-level skills directory (`$HOME/.agents/skills`).
fn user_skills_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)?;
    Some(home.join(".agents").join("skills"))
}

/// 默认系统提示词模板（对齐 Java `LocalAgent.SYSTEM_PROMPT_TEMPLATE`）。
const DEFAULT_SYSTEM_PROMPT_TEMPLATE: &str = r#"你是一个编码智能体 XAgent
通过文件/内容查找、读取、文件创建、编辑等工具进行项目代码编辑
The current working directory is：{cwd} 所有文件操作仅限于工作目录之内
当前系统：{os_name}
当前系统换行符：{line_separator}
当前系统语言：{language}
您只能执行当前系统平台默认存在的命令
请使用当前系统语言：{language} 回复用户
<编码指南>
## 1.写代码前先思考
    **别假设。不要掩饰困惑。表面权衡。**
    在实施之前：
    - 明确陈述你的假设。如果不确定，可以问。
    - 如果存在多种解读，就提出来——不要默默选择。
    - 如果存在更简单的方法，请说明。必要时反驳。
    - 如果有什么不清楚的，就停。说出什么让人困惑。问吧。
## 2.简洁优先
    **解决问题的最低代码。没有任何推测性内容。**
    - 没有超出要求的特征。
    - 一次性代码不进行抽象。
    - 没有"灵活性"或"可配置性"，除非是被要求的。
    - 不处理不可能的错误处理。
    - 如果你写了200行，可能有50行能解决问题，就重写。
    - 遵循极致的高内聚 低耦合原则
    问问自己："高级工程师会说这太复杂了吗？"如果是，那就简化。
## 3.原子更改遵循最小改动
    **只触碰你必须触碰的。只收拾你自己的烂摊子。**
    编辑现有代码时：
    - 不要"改进"相邻的代码、注释或格式。
    - 不要重构没坏掉的东西。
    - 要符合现有风格，即使你会用不同的方式。
    - 如果你发现了无关的死代码，要提及——不要删除。
    - 使用批量编辑 一次修改多处进行高效修改
    当你的更改产生孤儿时：
        - 移除你的更改导致未使用的导入/变量/函数。
        - 除非被要求，不要删除已有的死代码。
    测试：每一行更改的线条都应直接追踪到用户的请求。
## 4.目标驱动执行
    **定义成功标准。循环直到确认。**
    将任务转化为可验证的目标：
    - "添加验证"→"为无效输入写测试，然后使其通过"
    - "修复漏洞"→"编写一个复现该漏洞的测试，然后使其通过"
    - "重构X"→"确保测试在之前和之后通过"
    对于多步骤任务，请提出简要计划：
    ```
    1. [步骤] → 验证：[检查] 更新todo
    2. [步骤] → 验证：[检查] 更新todo
    3. [步骤] → 验证：[检查] 更新todo
    ```
## 5.代码库探索与加载
    - 如果工作目录下存在 AGENTS.md 或 README.md 可以通过它们快速了解当前项目
    - 使用codegraph加载代码仓库 : 适用于任何代码库的代码智能与知识图谱
    -  codegraph -h
    Usage: codegraph [options] [command]
    Code intelligence and knowledge graph for any codebase
    Options:
      -h, --help                     display help for command
    Commands:
      init [options] [path]          Initialize CodeGraph in a project directory and build the initial index
      uninit [options] [path]        Remove CodeGraph from a project (deletes .codegraph/ directory)
      index [options] [path]         Rebuild the full index from scratch (same result as a fresh init)
      sync [options] [path]          Sync changes since last index
      status [options] [path]        Show index status and statistics
      query [options] <search>       Search for symbols in the codebase
      explore [options] <query...>   Explore an area: relevant symbols' source + call paths in one shot (same output as the codegraph_explore MCP tool)
      node [options] [name]          One symbol's source + caller/callee trail, or read a file with line numbers + dependents (same output as the codegraph_node MCP tool)
      files [options]                Show project file structure from the index
      daemon|daemons                 Manage running CodeGraph background daemons — pick one and press enter to stop it
      unlock [path]                  Remove a stale lock file that is blocking indexing
      callers [options] <symbol>     Find all functions/methods that call a specific symbol
      callees [options] <symbol>     Find all functions/methods that a specific symbol calls
      impact [options] <symbol>      Analyze what code is affected by changing a symbol
      affected [options] [files...]  Find test files affected by changed source files
      install [options]              Install codegraph MCP server into one or more agents (Claude Code, Cursor, Codex CLI, opencode, Hermes Agent)
      uninstall [options]            Remove codegraph from your agents (Claude Code, Cursor, Codex CLI, opencode, Hermes Agent)
      telemetry [action]             Show or change anonymous usage telemetry (status, on, off)
      version                        Print the installed CodeGraph version (also: -v, --version)
      help [command]                 display help for command
    - 使用rg命令代替grep glob命令
    - 无论何时优先使用codegraph进行代码查找与读取
**这些指南有效条件是：**
    差异中不必要的更改减少，因过度复杂而减少重写，澄清问题应在实施前而非错误之后。
</编码指南>
"#;

/// 渲染默认系统提示词：注入 cwd / 操作系统 / 换行符 / 语言等运行时信息
/// （对齐 Java `LocalAgent.getInstruction`）。
fn build_system_prompt(cwd: &std::path::Path) -> String {
    let os_name = std::env::consts::OS.to_lowercase();
    let line_separator = if cfg!(windows) { "\\r\\n" } else { "\\n" };
    let language = std::env::var("LANG")
        .ok()
        .map(|l| l.split('.').next().unwrap_or("").to_string())
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| "zh".to_string());
    DEFAULT_SYSTEM_PROMPT_TEMPLATE
        .replace("{cwd}", &cwd.to_string_lossy())
        .replace("{os_name}", &os_name)
        .replace("{line_separator}", line_separator)
        .replace("{language}", &language)
}

/// 通过 `ModelCreationContext` 构建模型，并注入外部指定的附加请求头
///（如 `x-opencode-session`，用于 OpenCode Go 网关的 prompt-cache 会话路由）。
/// 会话头取当前 ACP sessionId，同一会话内保持稳定（利于 prompt-cache 命中）。
fn build_model(
    model_id: &str,
    base_url: &str,
    api_key: &str,
    protocol: ApiProtocol,
    session_id: &str,
) -> Arc<dyn Model> {
    let ctx = ModelCreationContext::new("openai", model_id)
        .api_key(api_key)
        .base_url(base_url)
        .protocol(protocol)
        .options(
            GenerateOptions::builder()
                .additional_header("x-opencode-session", session_id)
                .build(),
        );
    build_model_for_protocol(&ctx)
        .unwrap_or_else(|e| panic!("failed to build model for protocol {protocol:?}: {e}"))
}

/// Dispatches to the concrete [`Model`] implementation matching the context's
/// wire protocol. `base_url` always stops at the version prefix (e.g.
/// `https://opencode.ai/zen/go/v1`); the protocol decides the endpoint suffix.
fn build_model_for_protocol(ctx: &ModelCreationContext) -> Result<Arc<dyn Model>, String> {
    match ctx.protocol {
        ApiProtocol::ChatCompletions => OpenAiChatModel::from_context(ctx)
            .map(|m| Arc::new(m) as Arc<dyn Model>)
            .map_err(|e| e.to_string()),
        ApiProtocol::Responses => OpenAiResponsesModel::from_context(ctx)
            .map(|m| Arc::new(m) as Arc<dyn Model>)
            .map_err(|e| e.to_string()),
        ApiProtocol::Messages => AnthropicMessagesModel::from_context(ctx)
            .map(|m| Arc::new(m) as Arc<dyn Model>)
            .map_err(|e| e.to_string()),
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

/// 构建一个绑定指定 cwd 的完整 `HarnessAgent`（文件系统工具 + shell + 压缩 + skills）。
///
/// 模型配置通过 `$HOME/.agentscope/models.json` 管理：
/// - 首次运行时自动生成默认配置文件（opencode 提供者 + 免费模型列表）
/// - 用户可手动编辑该文件添加新的模型服务和模型
/// - 环境变量 `CODING_PROVIDER` / `CODING_MODEL_ID` / `OPENAI_API_KEY` / `OPENAI_BASE_URL`
///   可临时覆盖配置文件中的对应值
///
/// Skills are wired through the builder API (mirroring the Java `HarnessAgent`
/// builder): the harness composes the user / workspace / default skill repositories,
/// injects the `<available_skills>` block into the system prompt, registers the
/// `load_skill_through_path` tool, and (when enabled) the self-learning
/// `skill_manage` / `propose_skill` tools.
fn build_agent(cwd: PathBuf, session_id: &str) -> HarnessAgent {
    let model_config = ModelConfig::load_or_create();
    let resolved = model_config.resolve();
    let (base_url, api_key, model_id, protocol) = (
        resolved.base_url,
        resolved.api_key,
        resolved.model_id,
        resolved.protocol,
    );

    eprintln!(
        "[model-config] using provider model='{}', base_url='{}', api_key='{}...', protocol='{}'",
        model_id,
        base_url,
        &api_key[..api_key.len().min(8)],
        protocol.label(),
    );
    tracing::info!(
        "model-config: build_agent model='{}' base_url='{}' protocol={}",
        model_id,
        base_url,
        protocol.label()
    );

    // 初始模型即绑定当前 ACP sessionId（x-opencode-session）。
    let model: Arc<dyn Model> =
        build_model(&model_id, &base_url, &api_key, protocol, session_id);
    // 子 agent 的 front-matter `model:` 覆盖：按 `~/.agentscope/models.json` 解析 provider 后
    // 新建一个模型（对齐 Java `Builder.modelResolver`）。解析失败则回退到父模型。
    let resolver_session = session_id.to_string();

    let workspace = cwd.join(".agentscope").join("workspace");
    let mut builder = HarnessAgent::builder()
        .name("agentscope-coding-agent")
        .model(model)
        // 子 agent 的 front-matter `model:` 覆盖：按 `~/.agentscope/models.json` 解析 provider 后
        // 新建一个模型（对齐 Java `Builder.modelResolver`）。解析失败则回退到父模型。
        .model_resolver(move |model_id: &str| {
            let cfg = ModelConfig::load_or_create();
            let resolved = cfg.resolve_model(model_id).ok()?;
            Some(build_model(
                model_id,
                &resolved.base_url,
                &resolved.api_key,
                resolved.protocol,
                &resolver_session,
            ))
        })
        .cwd(cwd.clone())
        .workspace(workspace)
        // 会话状态持久化：以 ACP sessionId 为恢复键，每次运行后把完整 AgentState
        // （对话历史 / task / plan 状态）原子写入 {cwd}/.agentscope/state。进程重启后
        // 客户端用同一 sessionId 发 session/load|resume，即可续接之前的对话。
        .session_id(session_id)
        .state_store(Arc::new(JsonFileAgentStateStore::new(
            cwd.join(".agentscope").join("state"),
        )))
        .sys_prompt(build_system_prompt(&cwd))
        .register_filesystem_tools(true)
        .register_shell_tool(true)
        .compaction(CompactionMiddlewareConfig {
            enabled: true,
            max_tokens: ACP_CONTEXT_WINDOW_TOKENS,
        })
        .disable_memory_hooks()
        // 对话压缩：上下文 token 数达到 256k 窗口上限时由 LLM 总结早期对话并保留最近 tail，
        // 控制 AgentState.context 的持续增长（保留 tail 不破坏前缀 KV 缓存）。
        .conversation_compaction(
            CompactionConfig::builder()
                .trigger_messages(0)
                .trigger_tokens(ACP_CONTEXT_WINDOW_TOKENS)
                .keep_messages(12)
                .keep_tokens(-1)
                .keep_tokens_min(2_000)
                .keep_tokens_max(8_000)
                .keep_tokens_ratio(0.25)
                .flush_before_compact(false)
                .offload_before_compact(false)
                .build(),
        )
        // 工具结果外置：超过 80k 字符的工具输出（如构建日志）写入磁盘，
        // context 只保留预览占位符，避免大输出驻留内存。
        .tool_result_eviction(ToolResultEvictionConfig::defaults())
        .max_iters(9999)
        // 被中断/崩溃打断后，把悬挂的 tool_use 补成 error result 再续跑，避免
        // ToolCallPairSanitizer 静默丢弃整条 assistant 消息（对齐 Java maybePatchPendingToolCalls）。
        .enable_pending_tool_recovery(true);

    // Skill support (Open Agent Skills protocol, mirrors Java commit b906d1b):
    // - global:   $HOME/.agents/skills   (installed via `npx skill` globally)
    // - project:  <cwd>/.agents/skills   (installed via `npx skill` in the project)
    // The harness also composes the workspace-level repositories automatically and
    // wires the `<available_skills>` prompt block + the `load_skill_through_path` tool.
    if let Some(root) = user_skills_root() {
        builder = builder
            .skill_repository(Arc::new(FileSystemSkillRepository::new(root, "user")));
    }
    builder = builder.project_global_skills_dir(cwd.join(".agents").join("skills"));

    // Enable the self-learning loop (skill_manage + propose_skill tools). Drafts are
    // promoted only through an explicit promotion gate (auto-promote off).
    builder = builder.enable_skill_manage_tool_auto_promote(false);

    builder.build()
}

/// Initializes tracing to a log file under `<cwd>/.agentscope/log/`.
///
/// The ACP transport is stdio: anything written to stdout corrupts the
/// JSON-RPC stream, so logs MUST go to a file. Level defaults to `info` for
/// third-party crates plus `debug` for this agent's own modules (config
/// parsing / model resolution), and can be overridden with `RUST_LOG`
/// (e.g. `RUST_LOG=trace`).
fn init_logging() {
    use tracing_subscriber::EnvFilter;

    // Prefer an explicitly injected log dir (stable across launch cwds);
    // fall back to <cwd>/.agentscope/log.
    let log_dir = std::env::var_os("AGENTSCOPE_LOG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("../../../.."))
                .join(".agentscope")
                .join("log")
        });
    let _ = std::fs::create_dir_all(&log_dir);
    let log_file = log_dir.join("acp-agent.log");
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file)
    {
        Ok(f) => f,
        Err(e) => {
            // Last resort: stderr (never stdout) so the transport survives.
            eprintln!("[log] cannot open {}: {e}; falling back to stderr", log_file.display());
            return;
        }
    };
    // 本 agent 自身模块（配置解析 / 模型解析）默认 debug，便于排障；
    // 依赖保持 info 以免刷屏。RUST_LOG 可完全覆盖。
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info,acp_harness_agent=debug,agentscope_harness=debug")
    });
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .with_target(false)
        .try_init();
    tracing::info!("logging to {}", log_file.display());
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();

    // 三种 ACP 模式，分别映射到框架的权限模式（切换即时生效，作用于下一个请求）：
    // - plan (Plan)        -> PermissionMode::Explore  ：只读，写/执行类工具一律拒绝
    // - accept_edits (Accept Edits) -> PermissionMode::AcceptEdits：文件读写自动批准，
    //                           shell (execute) 需要人工确认（走 ACP request_permission）
    // - yolo (YOLO)        -> PermissionMode::Bypass  ：跳过所有权限检查
    let modes = default_modes();

    // 模型列表从配置文件动态加载：汇总所有 provider 下的模型 ID（去重后）。
    let model_config = ModelConfig::load_or_create();
    let models = model_config.all_models();

    let agent = AcpCodingAgent::builder()
        .name("agentscope-coding-agent")
        .model_factory(|cwd, sid| build_agent(cwd, sid))
        // Dynamic model switching: resolve the selected model id against the
        // config file (provider lookup + protocol) and build a fresh model.
        // Takes effect on the next prompt; conversation state is preserved.
        .model_builder(move |model_id, session_id| {
            let cfg = ModelConfig::load_or_create();
            let resolved = cfg.resolve_model(model_id)?;
            Ok(build_model(
                model_id,
                &resolved.base_url,
                &resolved.api_key,
                resolved.protocol,
                session_id,
            ))
        })
        .modes(modes)
        .models(models)
        .max_sessions(3)
        .build()?;

    // Shutdown handling: on Ctrl-C / SIGTERM, interrupt every live session so its
    // in-flight turn unwinds through the normal path — interrupt reconciliation
    // plus the state checkpoint — before the process exits, instead of being
    // killed mid-turn and losing the iteration.
    //
    // `kill -9`, OOM kills and power loss cannot run any handler at all; those rely
    // on the per-iteration checkpoint (`StateCheckpointMiddleware::on_reasoning`)
    // to cap the loss at the in-flight iteration.
    spawn_shutdown_handler(agent.session_registry());

    tracing::info!("ACP CodingAgent starting over stdio...");
    agent.run_stdio().await?;
    Ok(())
}

/// Waits for a shutdown signal: Ctrl-C on every platform, plus SIGTERM on unix.
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Interrupts every live session on shutdown so in-flight turns unwind through the
/// normal path and get checkpointed before the process exits.
///
/// Polls the shared registry at signal time because sessions can be created after
/// startup.
fn spawn_shutdown_handler(sessions: Arc<Mutex<HashMap<String, AcpAgentSession>>>) {
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        eprintln!("\n[shutdown] interrupt received; saving session state...");
        {
            let guard = sessions.lock().unwrap();
            for session in guard.values() {
                session.agent().interrupt();
            }
        }
        // `ExecutionGuard` tracks live calls, so poll that rather than guessing a
        // fixed delay: exit only after the interrupted turns have unwound and their
        // state has been checkpointed.
        let manager = agentscope_core::GracefulShutdownManager::instance();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while manager.get_active_request_count() > 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        eprintln!("[shutdown] state saved; exiting");
        std::process::exit(130);
    });
}
