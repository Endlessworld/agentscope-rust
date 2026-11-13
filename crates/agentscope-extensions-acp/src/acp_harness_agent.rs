//! ACP CodingAgent over stdio — mirrors the Java `HarnessCodingAgent` /
//! Kotlin `AgiHarnessAgent`. Wires a full `HarnessAgent` (filesystem tools,
//! shell, compaction, plan mode) + skills behind an [`AcpCodingAgent`] that
//! speaks the Agent Client Protocol over stdio.
//!
//! Run:
//!   echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}' \
//!     | cargo run -p agentscope-extensions-acp --bin acp_harness_agent
//!
//! 配置模型（二选一，都只要一个 API key）：
//!   - 导出 provider 的密钥（如 `DEEPSEEK_API_KEY`），启动时自动启用对应 provider；
//!   - 跑 `acp_harness_agent --setup`，从 models.dev 目录里选 provider + 填 key。
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

// The session journal serializes a full `stateJson` per checkpoint (megabyte scale). On
// Linux/glibc the default allocator keeps that peak in its arena, so mimalloc (which returns it)
// is worth installing. On Windows the CRT already `VirtualFree`s large blocks on `free`, and
// mimalloc made the same workload *worse* -- one heap per thread, purged on a 1 s timer: measured
// 54 MB peak RSS across 32 threads versus 4.6 MB with the system allocator. So it stays a unix
// choice. Installing it here -- the binary's crate root -- leaves every library consumer untouched.
#[cfg(all(unix, not(feature = "alloc-stats")))]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

// Diagnostic build (`--features alloc-stats`): replaces the platform allocator so `[mem]` lines can
// report live heap bytes -- the number that separates real data from pages the allocator kept.
#[cfg(feature = "alloc-stats")]
#[global_allocator]
static COUNTING_ALLOCATOR: agentscope_extensions_acp::mem::counting::CountingAlloc =
    agentscope_extensions_acp::mem::counting::CountingAlloc;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentscope_core::model::catalog::{CatalogAwareModel, ModelCatalog};
use agentscope_core::model::provider::{ApiProtocol, ModelCreationContext};
use agentscope_core::model::{GenerateOptions, Model};
use agentscope_extensions_acp::compaction_trigger_for;
use agentscope_extensions_acp::config::ModelChoice;
use agentscope_extensions_acp::setup::{self, DetectedProvider, SetupIo, SetupPlan};
use agentscope_extensions_acp::{
    config::default_modes, AcpAgentSession, AcpCodingAgent, ImageSupport,
};
use agentscope_extensions_acp::{AskUserTool, SessionBuild};
use agentscope_harness::agent::context::TaskContextOptions;
use agentscope_harness::agent::{
    CompactionConfig, FileSystemSkillRepository, HarnessAgent, ToolResultEvictionConfig,
};
use serde::{Deserialize, Serialize};

use agent_client_protocol::v1::schema::AuthState;

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
                    responses: vec!["muse-spark-1.3-contributor-free".to_string()],
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
                        eprintln!("[model-config] config written to {}", path.display());
                    }
                }
                Err(e) => {
                    eprintln!(
                        "[model-config] warning: failed to serialize default config: {}",
                        e
                    );
                }
            }
        }
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
            ModelProviderConfig {
                base_url: plan.provider.base_url.clone(),
                api_key: plan.api_key.clone(),
                models: models_for(&plan.provider),
            },
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
    fn load_existing() -> Option<Self> {
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
        let mut keys: Vec<&String> = self.providers.keys().collect();
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
    - 澄清问题应在实施前而非错误之后。
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
    - 使用跨文件批量编辑 一次对多个文件的多处内容进行高效修改
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
    先执行每一步 最后一起批量验证/修复验证
    对于rust项目 subagent不要求编译验证 由主agent验证和兜底
    ```
    1. [步骤1] →  更新todo
    2. [步骤2] →  更新todo
    3. [步骤3] →  更新todo
    4. 验证：[检查] → 修复验证
    5. 更新todo
    ```
## 5.代码库探索与加载
    - 如果工作目录下存在 AGENTS.md 或 README.md 可以通过它们快速了解当前项目
    - 使用codegraph查看当前代码仓库的知识图谱，获取调用链/方法实现等等，具体使用codegraph -h查看用法
    - 使用rg命令代替grep glob工具
    - 无论何时优先使用codegraph进行代码查找与读取
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

/// 把客户端通过 `systemPrompt`（`append` 模式，ACP-38）送来的文本接在默认提示词之后。
/// 默认提示词保留在前，所以编码规则与工具说明仍然生效，客户端只能追加约束。
fn with_client_system_prompt(base: String, client_prompt: Option<&str>) -> String {
    match client_prompt {
        Some(text) => format!(
            "{base}\n\n# Client-supplied system prompt\n\n\
             The client that started this session asked for the following to be appended \
             to your instructions. It does not replace anything above.\n\n{text}"
        ),
        None => base,
    }
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
    // MODEL-01: the protocol→implementation mapping lives in the model crate's
    // `assembly` seam, so the host and the providers cannot drift apart.
    let model = agentscope_extensions_model::assembly::build_model_for_protocol(
        ctx,
        agentscope_extensions_model::assembly::ProtocolImplementations::new(),
    )
    .map_err(|e| e.to_string())?;
    Ok(match &ctx.catalog {
        Some(descriptor) => CatalogAwareModel::from_descriptor(model, Arc::clone(descriptor)),
        None => CatalogAwareModel::wrap(
            model,
            Some(&ctx.provider_id),
            &ctx.model,
            ctx.base_url.as_deref(),
        ),
    })
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
fn mask_secret(secret: &str) -> String {
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
fn build_agent(ctx: SessionBuild) -> HarnessAgent {
    let SessionBuild {
        cwd,
        session_id,
        client_support,
        elicitation_slot,
        system_prompt,
        subagent_events,
        client_slot,
        permission_store,
        subagent_agents,
        subagent_mode,
        subagent_nesting,
    } = ctx;
    let session_id = session_id.as_str();
    let model_config = ModelConfig::load_or_create_with_env();
    let resolved = model_config.resolve();
    let (base_url, api_key, model_id, protocol) = (
        resolved.base_url,
        resolved.api_key,
        resolved.model_id,
        resolved.protocol,
    );

    eprintln!(
        "[model-config] using provider model='{}', base_url='{}', api_key='{}', protocol='{}'",
        model_id,
        base_url,
        mask_secret(&api_key),
        protocol.label(),
    );
    tracing::info!(
        "model-config: build_agent model='{}' base_url='{}' protocol={} api_key={}",
        model_id,
        base_url,
        protocol.label(),
        mask_secret(&api_key)
    );

    // 初始模型即绑定当前 ACP sessionId（x-opencode-session）。
    let model: Arc<dyn Model> = build_model(&model_id, &base_url, &api_key, protocol, session_id);
    let context_window = model.context_window_size();
    // 压缩阈值跟随模型自己的上下文窗口（catalog 已知 opencode-go/deepseek-v4.1-flash 为 1M），
    // 与编译器预算同源；`AGENTSCOPE_ACP_COMPACTION_WINDOW` 仍可显式封顶（见
    // `compaction_trigger_for`）。
    let compaction_trigger = compaction_trigger_for(context_window);
    tracing::info!(
        "compaction-config: model='{}' context_window={} trigger_tokens={}",
        model_id,
        context_window,
        compaction_trigger
    );
    // 子 agent 的 front-matter `model:` 覆盖：按 `~/.agentscope/models.json` 解析 provider 后
    // 新建一个模型（对齐 Java `Builder.modelResolver`）。解析失败则回退到父模型。
    let resolver_session = session_id.to_string();

    // 工具结果外置的阈值与豁免：Rust 的 read_file / grep / glob / list 返回的是完整内容
    // （工具本身不会分页截断），若继续按默认豁免，一次大文件读取就会永久驻留在 context、
    // checkpoint 和后续每轮请求里。因此它们也参与外置，context 只保留 head+tail 预览 +
    // 落盘路径；write_file / edit_file / memory_* 这类小输出仍保持豁免。
    let mut eviction = ToolResultEvictionConfig::defaults().scaled_for_window(context_window);
    for tool in [
        "read_file",
        "grep_files",
        "glob_files",
        "list_files",
        "session_search",
    ] {
        eviction.excluded_tool_names.remove(tool);
    }

    let workspace = cwd.join(".agentscope").join("workspace");
    let mut builder = HarnessAgent::builder()
        .name("agentscope-coding-agent")
        .model(model)
        // 仅 Anthropic Messages 端点接受 `cache_control` 断点；OpenAI 兼容网关可能
        // 拒绝未知字段，因此按解析出的协议闸门开启。
        .prompt_cache(matches!(protocol, ApiProtocol::Messages))
        // 子 agent 的 front-matter `model:` 覆盖：按 `~/.agentscope/models.json` 解析 provider 后
        // 新建一个模型（对齐 Java `Builder.modelResolver`）。解析失败则回退到父模型。
        .model_resolver(move |model_id: &str| {
            let cfg = ModelConfig::load_or_create();
            let resolved = cfg.resolve_model(model_id).ok()?;
            Some(build_model(
                &resolved.model_id,
                &resolved.base_url,
                &resolved.api_key,
                resolved.protocol,
                &resolver_session,
            ))
        })
        .cwd(cwd.clone())
        .workspace(workspace)
        // 会话历史以 **journal 为唯一权威**（对齐 Java native session）：`session_id` 既是
        // journal 的会话键，也是恢复键。进程重启后客户端用同一 sessionId 发
        // `session/load|resume`，运行时用 `SessionProjection::restore()` 从已提交事实恢复
        // 对话历史 / task / plan 状态，因此不再需要额外挂 `AgentStateStore`（那是 Java 的
        // legacy 模式和沙箱元数据才用的第二条轨）。
        .session_id(session_id)
        .sys_prompt(with_client_system_prompt(
            build_system_prompt(&cwd),
            system_prompt.as_deref(),
        ))
        // 任务投影：每步把已提交的 todo / 需求 / 已验证项投影成 `<TASK_STATE>` 视图，并让
        // 上下文编译器产出 manifest（预算、transforms、taskRevision）；没有注册 source，
        // 因此不会与 `WorkspaceContextMiddleware` 的 system prompt 注入重复。
        .task_context(TaskContextOptions {
            requirements: true,
            verification_results: true,
            requirement_proposals: false,
        })
        .register_filesystem_tools(true)
        .register_shell_tool(true)
        .disable_memory_hooks()
        // 对话压缩：上下文 token 数达到 256k 窗口上限时由 LLM 总结早期对话并保留最近 tail，
        // 控制 AgentState.context 的持续增长（保留 tail 不破坏前缀 KV 缓存）。
        .conversation_compaction(
            CompactionConfig::builder()
                .trigger_messages(0)
                .trigger_tokens(compaction_trigger)
                .keep_messages(25)
                .keep_tokens(-1)
                .keep_tokens_min(2_000)
                .keep_tokens_max(8_000)
                .keep_tokens_ratio(0.25)
                .flush_before_compact(false)
                .offload_before_compact(false)
                .build()
                .scaled_for_window(context_window),
        )
        // 工具结果外置：超过 80k 字符的工具输出（如构建日志）写入磁盘，
        // context 只保留预览占位符，避免大输出驻留内存。
        .tool_result_eviction(eviction)
        .max_iters(9999)
        // `request/prepared` 携带完整请求体，恢复路径不读它（恢复依赖 `state/checkpoint` +
        // `model/dispatch`），因此默认关闭以省内存/磁盘；
        // `AGENTSCOPE_ACP_RECORD_MODEL_REQUESTS=1` 可随时恢复完整记录，行为与旧版一致。
        .record_model_requests(agentscope_extensions_acp::recording::record_model_requests())
        // 被中断/崩溃打断后，把悬挂的 tool_use 补成 error result 再续跑，避免
        // ToolCallPairSanitizer 静默丢弃整条 assistant 消息（对齐 Java maybePatchPendingToolCalls）。
        .enable_pending_tool_recovery(true);

    // 工具执行策略：超过 240s 的调用转后台（给模型占位结果，跑完再回送结果并唤醒），
    // 单次执行上限 1800s。没有这一步时 core 的 300s 默认会直接取消长命令。
    builder = agentscope_extensions_acp::coding_agent::with_tool_execution_policy(builder);

    // Subagent progress: the harness streams each spawned subagent's events out of band (a
    // child keeps running after the parent's prompt returns), and the session forwards them to
    // the client as `subagent_update` + child-session updates.
    if let Some(events) = subagent_events {
        builder = builder.subagent_events(events);
    }
    // Nested delegation: a subagent may spawn its own worker (and report it as a card of its own
    // transcript). The configured depth bounds the recursion; the default of one level makes the
    // grandchild a leaf. A host that wants flat delegation sets `with_subagent_nesting(0)`.
    builder = builder.subagent_nesting(0, subagent_nesting);

    // A child runs in a background task under its own session id (`sub-<uuid>`): its `ask`
    // tools must raise `session/request_permission` on *that* id, otherwise the approval card
    // cannot be attributed to the subagent and the parent session blocks on it instead.
    // The child id only exists after the spawn tool minted it, so the routing is installed as a
    // runtime hook rather than baked into the agent factory. The mode slot lets the hook install
    // the *current* permission context on the child (without it every child would fall back to
    // the trivial context and run tools without asking).
    if let (Some(slot), Some(store), Some(agents), Some(mode)) = (
        client_slot,
        permission_store,
        subagent_agents,
        subagent_mode,
    ) {
        builder = builder.subagent_runtime_hook(Arc::new(
            agentscope_extensions_acp::session::SubagentPermissionHook::new(
                slot,
                store,
                agent_client_protocol::v1::schema::SessionId::new(session_id.to_string()),
                mode,
                agents,
            ),
        ));
    }

    // Skill support (Open Agent Skills protocol, mirrors Java commit b906d1b):
    // - global:   $HOME/.agents/skills   (installed via `npx skill` globally)
    // - project:  <cwd>/.agents/skills   (installed via `npx skill` in the project)
    // The harness also composes the workspace-level repositories automatically and
    // wires the `<available_skills>` prompt block + the `load_skill_through_path` tool.
    if let Some(root) = user_skills_root() {
        builder = builder.skill_repository(Arc::new(FileSystemSkillRepository::new(root, "user")));
    }
    builder = builder.project_global_skills_dir(cwd.join(".agents").join("skills"));

    // The self-learning loop (skill_manage + propose_skill tools). Drafts are
    // promoted only through an explicit promotion gate (auto-promote off).
    builder = builder.enable_skill_manage_tool_auto_promote(false);

    // `ask_user` pushes an ACP elicitation form. It is registered on the base
    // toolkit *before* `build()` because the subagent blueprint snapshots the
    // toolkit once at build time — registering later would leave subagents
    // without the tool. It is capability-gated: when the client did not
    // advertise `elicitation.form` the tool is not exposed at all (the protocol
    // forbids requesting an un-advertised mode).
    if client_support.supports_elicitation_form() {
        let toolkit = agentscope_core::tool::Toolkit::new();
        toolkit.register_tool(Arc::new(AskUserTool::new(
            elicitation_slot,
            agent_client_protocol::v1::schema::SessionId::new(session_id.to_string()),
        )));
        builder = builder.toolkit(toolkit);
    }

    let agent = builder.build();
    // One line per session build: the footprint the agent settles at before any turn runs. This is
    // the baseline the idle trim works on, not a per-turn number.
    agentscope_extensions_acp::mem::log_footprint("init");
    agent
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
            eprintln!(
                "[log] cannot open {}: {e}; falling back to stderr",
                log_file.display()
            );
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

/// `--setup`：交互式写入 `$HOME/.agentscope/models.json` 后退出。
///
/// 这是 `initialize` 广告的 terminal auth 方法的可执行部分：客户端用同一个二进制加
/// `--setup` 开一个终端，用户只需选一个 provider、填一个 API key；若密钥已在环境变量里，
/// 一个问题都不用回答。
fn run_setup() -> Result<(), Box<dyn std::error::Error>> {
    let catalog = ModelCatalog::global();
    let snapshot = agentscope_extensions_acp::mem::runtime().block_on(async {
        catalog.ensure_loaded();
        // Refresh unconditionally: a `304` still re-parses the raw sources, which is what
        // fills the provider tables (`provider_env`) on an install whose cache predates them.
        if let Err(error) = catalog.refresh().await {
            eprintln!("[setup] 无法更新模型目录（{error}）；使用本地缓存");
        }
        catalog.ensure_loaded();
        catalog.snapshot()
    });

    let providers = setup::catalog_providers(&snapshot);
    let detected = setup::detect_env(&providers, |name| std::env::var(name).ok());
    println!(
        "AgentScope Rust 模型配置：目录中 {} 个 provider，已从环境变量检测到 {} 个",
        providers.len(),
        detected.len()
    );

    let plan = setup::run(&mut TerminalIo, &providers, &detected)?;
    // `--setup` 是显式写配置，但仍不能默默吞掉一份解析失败的手写文件。
    let mut config = match ModelConfig::load_existing() {
        Some(config) => config,
        None if ModelConfig::default_path().is_some_and(|path| path.exists()) => {
            return Err(format!(
                "{} 无法解析；为免覆盖你手写的配置已中止，修好或删掉它再跑 --setup",
                ModelConfig::default_path()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default()
            )
            .into());
        }
        None => ModelConfig::default(),
    };
    config.apply_plan(&plan);
    config.save();
    match (&plan.key_source, plan.api_key.is_empty()) {
        (Some(var), _) => println!("使用环境变量 {var} 中的密钥"),
        (None, true) => println!("未写入密钥，稍后可在 models.json 里补上"),
        (None, false) => println!("已写入密钥"),
    }
    println!("默认模型：{}/{}", plan.provider.id, plan.model);
    println!("重启 agent（或重开会话）后生效。");
    Ok(())
}

/// `--setup` 使用的 stdin/stdout 终端。
struct TerminalIo;

impl SetupIo for TerminalIo {
    fn say(&mut self, line: &str) {
        println!("{line}");
    }

    fn ask(&mut self, prompt: &str) -> Option<String> {
        use std::io::Write as _;
        print!("{prompt}");
        std::io::stdout().flush().ok()?;
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line),
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `--setup` 是纯控制台向导：不能启动 ACP 传输，否则屏幕上会混进 stdout 协议帧。
    if std::env::args().skip(1).any(|arg| arg == "--setup") {
        return run_setup();
    }
    // Worker count is a knobs, not `ncpu`: this agent serves one conversation, and every extra
    // thread also costs a mimalloc heap that keeps its own freed pages resident.
    agentscope_extensions_acp::mem::runtime().block_on(run())
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    agentscope_extensions_acp::retention::apply_session_defaults();
    init_logging();
    agentscope_core::model::catalog::ModelCatalog::global().spawn_background_refresh();

    // 三种 ACP 模式，分别映射到框架的权限模式（切换即时生效，作用于下一个请求）：
    // - plan (Plan)        -> PermissionMode::Explore  ：只读，写/执行类工具一律拒绝
    // - accept_edits (Accept Edits) -> PermissionMode::AcceptEdits：文件读写自动批准，
    //                           shell (execute) 需要人工确认（走 ACP request_permission）
    // - yolo (YOLO)        -> PermissionMode::Bypass  ：跳过所有权限检查
    let modes = default_modes();

    // 模型列表从配置文件动态加载：汇总所有 provider 下的模型 ID（去重后）。
    // 已导出的 provider 密钥会被一并启用，所以环境变量配好后无需任何文件配置。
    let model_config = ModelConfig::load_or_create_with_env();
    let models = model_config.all_models();
    // 供转发器在目录常驻时重算 `prompt.image` 能力（见 `ImageSupport`）。
    let image_models = models.clone();

    let agent = AcpCodingAgent::builder()
        .name("agentscope-coding-agent")
        // Capability-aware factory: lets `ask_user` be registered at build time
        // (capability-gated on `elicitation.form`, and inherited by subagents).
        .session_factory(build_agent)
        // Dynamic model switching: resolve the selected model id against the
        // config file (provider lookup + protocol) and build a fresh model.
        // Takes effect on the next prompt; conversation state is preserved.
        .model_builder(move |model_id, session_id| {
            let cfg = ModelConfig::load_or_create_with_env();
            let resolved = cfg.resolve_model(model_id)?;
            Ok(build_model(
                &resolved.model_id,
                &resolved.base_url,
                &resolved.api_key,
                resolved.protocol,
                session_id,
            ))
        })
        .modes(modes)
        .models(models)
        .max_sessions(3)
        // `auth/status` 报告真实状态：有真实凭据（含环境变量）为 authenticated，
        // 一处凭据也没有时才是 unauthenticated（客户端据此提示 `--setup`）。
        .auth_probe(|| {
            if ModelConfig::load_or_create_with_env().has_credential() {
                AuthState::Authenticated
            } else {
                AuthState::Unauthenticated
            }
        })
        .build()?;

    // The footprint the process serves at, before any session exists: this is the baseline the
    // idle trim works on (measured ~26 MB RSS / 29 MB commit on Windows).
    agentscope_extensions_acp::mem::log_footprint("serving");

    // Shutdown handling: on Ctrl-C / SIGTERM, interrupt every live session so its
    // in-flight turn unwinds through the normal path — interrupt reconciliation
    // plus the state checkpoint — before the process exits, instead of being
    // killed mid-turn and losing the iteration.
    //
    // `kill -9`, OOM kills and power loss cannot run any handler at all; those rely
    // on the per-iteration checkpoint (`StateCheckpointMiddleware::on_reasoning`)
    // to cap the loss at the in-flight iteration.
    spawn_shutdown_handler(agent.session_registry());
    spawn_catalog_forwarder(
        agent.session_registry(),
        image_models,
        agent.image_support(),
    );

    tracing::info!("ACP CodingAgent starting over stdio...");
    let sessions = agent.session_registry();
    let result = agent.run_stdio().await;
    // stdin EOF (the IDE closed the pipe) or a transport error ends the stdio loop just like
    // a signal does: without draining here the runtime is dropped while a turn is still in
    // flight, so the recorder's `Drop` release and the tool abort paths never run and the
    // journal keeps a half-written turn. Always drain, then surface the loop's outcome.
    drain_sessions(&sessions, "transport closed").await;
    result?;
    Ok(())
}

/// Forwards catalog content changes to live sessions as
/// `ConfigOptionUpdate` + `UsageUpdate`, then hands the table back.
///
/// Every refresh (the startup one and the TTL one) installs a freshly parsed table; sessions that
/// already bound their model do not need it, so the memory goes back to the OS right here instead of
/// waiting for the next `session/new`. Without this a refresh that finished after `session/new`
/// would keep ~15 MB resident for a whole TTL.
///
/// The same table answers the `prompt.image` capability, so the cached answer is refreshed here —
/// while the table is resident — before it is released. That keeps `initialize` from parsing it.
fn spawn_catalog_forwarder(
    sessions: Arc<Mutex<HashMap<String, AcpAgentSession>>>,
    models: Vec<ModelChoice>,
    image_support: ImageSupport,
) {
    let catalog = agentscope_core::model::catalog::ModelCatalog::global();
    let mut receiver = catalog.subscribe();
    tokio::spawn(async move {
        // The startup refresh may already have finished before we subscribed, in which case no
        // event is coming: refresh and release once here so an idle agent does not sit on the table.
        image_support.refresh(&models);
        agentscope_extensions_acp::release_catalog_if_unused(&sessions);
        while receiver.changed().await.is_ok() {
            let event = receiver.borrow_and_update().clone();
            if !event.changed_keys.is_empty() {
                let guard = sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                for session in guard.values() {
                    let relevant = session
                        .selected_catalog_key()
                        .map(|key| event.changed_keys.contains(&key))
                        .unwrap_or(true);
                    if relevant {
                        session.push_config_update();
                    }
                }
                drop(guard);
            }
            image_support.refresh(&models);
            agentscope_extensions_acp::release_catalog_if_unused(&sessions);
        }
    });
}

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
        drain_sessions(&sessions, "shutdown signal").await;
        eprintln!("[shutdown] state saved; exiting");
        std::process::exit(130);
    });
}

/// Interrupts every live session and waits for in-flight turns to publish their terminal
/// facts before returning.
///
/// Used by both the signal path and the stdin-EOF/transport-close path; neither may let the
/// process exit while a turn is still running.
async fn drain_sessions(sessions: &Arc<Mutex<HashMap<String, AcpAgentSession>>>, reason: &str) {
    // Interrupt each session by id, then wait for its prompt to unwind. `is_running` is
    // set by the prompt itself, so it is authoritative even when the shutdown count is
    // zero (the ACP path must also register with `ExecutionGuard`).
    let live: Vec<AcpAgentSession> = {
        let guard = sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.values().cloned().collect()
    };
    if live.is_empty() {
        return;
    }
    let manager = agentscope_core::GracefulShutdownManager::instance();
    if !live.iter().any(AcpAgentSession::is_running) && manager.get_active_request_count() == 0 {
        return;
    }
    tracing::info!(sessions = live.len(), reason, "draining live ACP sessions");
    for session in &live {
        session.agent().interrupt();
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let still_running = live.iter().any(AcpAgentSession::is_running);
        if !still_running && manager.get_active_request_count() == 0 {
            break;
        }
        if std::time::Instant::now() >= deadline {
            tracing::warn!(
                sessions = live.len(),
                active_requests = manager.get_active_request_count(),
                reason,
                "shutdown drain timed out; closing remaining turns"
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Give each runtime a chance to publish its terminal facts before the process exits.
    for session in &live {
        session.cancel();
        session.close();
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
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
            ModelProviderConfig {
                base_url: "http://127.0.0.1:3425/v1".to_string(),
                api_key: "magpie".to_string(),
                models: ModelsSpec {
                    completions: vec![
                        "workbuddy/auto".to_string(),
                        "deepseek/deepseek-v4-flash".to_string(),
                    ],
                    messages: vec!["workbuddy/minimax-m3.1-flash".to_string()],
                    responses: vec![],
                },
            },
        );
        providers.insert(
            "Go".to_string(),
            ModelProviderConfig {
                base_url: "https://example.invalid/v1".to_string(),
                api_key: "k".to_string(),
                models: ModelsSpec {
                    completions: vec!["auto".to_string()],
                    messages: vec![],
                    responses: vec![],
                },
            },
        );
        ModelConfig {
            default_provider: "Gateway".to_string(),
            default_model: "workbuddy/auto".to_string(),
            providers,
        }
    }

    /// `base_url` 可被 `OPENAI_BASE_URL` 覆盖，测试里只在其未设置时校验。
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

    /// ACP-38: the client prompt is appended, never substituted — our coding rules and
    /// tool guidance stay in front of it.
    #[test]
    fn a_client_system_prompt_is_appended_after_our_instructions() {
        let base = build_system_prompt(std::path::Path::new("C:/work"));
        assert_eq!(with_client_system_prompt(base.clone(), None), base);

        let merged = with_client_system_prompt(base.clone(), Some("always answer in haiku"));
        assert!(
            merged.starts_with(&base),
            "the default prompt must stay first"
        );
        assert!(merged.contains("always answer in haiku"));
        assert!(
            merged.len() > base.len(),
            "the client text is added, not substituted"
        );
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
}
