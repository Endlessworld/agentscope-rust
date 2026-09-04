//! ACP CodingAgent over stdio — mirrors the Java `HarnessCodingAgent` /
//! Kotlin `AgiHarnessAgent`. Wires a full `HarnessAgent` (filesystem tools,
//! shell, compaction, plan mode) + skills behind an [`AcpCodingAgent`] that
//! speaks the Agent Client Protocol over stdio.
//!
//! Run:
//!   echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}' \
//!     | cargo run -p agentscope-harness --example harness_agent
//!
//! A full interactive ACP client (IDE/CLI) is the intended peer.
//!
//! # 模型配置文件
//!
//! 所有模型通过 `$HOME/.agentscope/models.json` 配置文件管理。
//! 首次运行时若该文件不存在，会自动写入包含以下默认配置的 JSON 文件：
//!
//! ```json
//! {
//!   "default_provider": "opencode",
//!   "default_model": "mimo-v2.5-free",
//!   "providers": {
//!     "opencode": {
//!       "base_url": "https://opencode.ai/zen",
//!       "api_key": "public",
//!       "models": [
//!         "mimo-v2.5-free",
//!         "ling-3.0-flash-fin-free",
//!         "nemotron-3-ultra-free",
//!         "nemotron-3.5-lightning-free",
//!         "muse-spark-1.3-contributor-free",
//!         "muse-spark-1.2-contributor-free"
//!       ]
//!     }
//!   }
//! }
//! ```
//!
//! 用户可在此基础上自由增加新的模型服务提供者（provider）和对应的模型列表。
//!
//! 环境变量可临时覆盖配置文件中的值：
//! - `CODING_PROVIDER`  — 覆盖 default_provider
//! - `CODING_MODEL_ID`  — 覆盖 default_model
//! - `OPENAI_API_KEY`   — 覆盖 provider.api_key
//! - `OPENAI_BASE_URL`  — 覆盖 provider.base_url

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use agentscope_core::model::{Model, OpenAiChatModel};
use agentscope_harness::acp::{config::default_modes, AcpCodingAgent};
use agentscope_harness::acp::config::ModelChoice;
use agentscope_harness::agent::{
    CompactionConfig, CompactionMiddlewareConfig, FileSystemSkillRepository, HarnessAgent,
    ToolResultEvictionConfig,
};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// 模型配置 (JSON)
// ---------------------------------------------------------------------------

/// 单个模型服务提供者（如 opencode、openai、custom）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProviderConfig {
    /// 该提供者的 API 基础地址（如 `https://opencode.ai/zen/v1`）。
    pub base_url: String,
    /// API 密钥（部分免费服务可填 `"public"`）。
    pub api_key: String,
    /// 该提供者下可用的模型 ID 列表。
    pub models: Vec<String>,
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
            "opencode".to_string(),
            ModelProviderConfig {
                base_url: "https://opencode.ai/zen/v1".to_string(),
                api_key: "public".to_string(),
                models: vec![
                    "mimo-v2.5-free".to_string(),
                    "ling-3.0-flash-fin-free".to_string(),
                    "nemotron-3-ultra-free".to_string(),
                    "nemotron-3.5-lightning-free".to_string(),
                    "muse-spark-1.3-contributor-free".to_string(),
                    "muse-spark-1.2-contributor-free".to_string(),
                ],
            },
        );
        Self {
            default_provider: "opencode".to_string(),
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

    /// 加载配置文件；若不存在则创建默认配置并写入磁盘。
    pub fn load_or_create() -> Self {
        match Self::try_load() {
            Some(cfg) => cfg,
            None => {
                let cfg = Self::default();
                cfg.save();
                cfg
            }
        }
    }

    /// 尝试从磁盘加载配置文件，文件不存在或解析失败时返回 `None`。
    fn try_load() -> Option<Self> {
        let path = Self::default_path()?;
        if !path.exists() {
            return None;
        }
        let content = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
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
    pub fn resolve(&self) -> (String, String, String) {
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

            (base_url, api_key, model_id)
        } else {
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
            (base_url, api_key, model_id)
        }
    }

    /// 根据模型 ID 查找其所属的 provider，返回 `(base_url, api_key)`。
    ///
    /// 环境变量 `OPENAI_API_KEY` / `OPENAI_BASE_URL` 可覆盖查找结果。
    pub fn resolve_model(&self, model_id: &str) -> Result<(String, String), String> {
        for (_key, provider) in &self.providers {
            if provider.models.iter().any(|m| m == model_id) {
                let api_key = std::env::var("OPENAI_API_KEY")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| provider.api_key.clone());
                let base_url = std::env::var("OPENAI_BASE_URL")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| provider.base_url.clone());
                return Ok((base_url, api_key));
            }
        }
        Err(format!("model '{model_id}' not found in any provider"))
    }

    /// 汇总所有 provider 下的模型列表，生成 ACP 的 `ModelChoice` 列表。
    ///
    /// 遍历 `providers` 中每个 provider 的 `models` 列表，去重后生成
    /// `ModelChoice { id, name }`。其中 `name` 使用 `provider_key/model_id`
    /// 格式以区分不同供应商，`id` 保持为模型 ID。用于动态填充 ACP
    /// `setModel` 下拉可选项，无需在代码中硬编码。
    pub fn all_models(&self) -> Vec<ModelChoice> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for (provider_key, provider) in &self.providers {
            for id in &provider.models {
                if seen.insert(id.clone()) {
                    out.push(ModelChoice {
                        id: id.clone(),
                        name: format!("{}/{}", provider_key, id),
                    });
                }
            }
        }
        out
    }
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
- 使用批量编辑 一次修改多处进行高效修改
- 如果工作目录下存在 AGENTS.md 或 README.md 可以通过它们快速了解当前项目
- 优先使用rg命令代替grep glob命令
- 优先使用 codegraph: Code intelligence and knowledge graph for any codebase 使用codegraph -h查看用法
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
fn build_agent(cwd: PathBuf) -> HarnessAgent {
    let model_config = ModelConfig::load_or_create();
    let (base_url, api_key, model_id) = model_config.resolve();

    eprintln!(
        "[model-config] using provider model='{}', base_url='{}', api_key='{}...'",
        model_id,
        base_url,
        &api_key[..api_key.len().min(8)],
    );

    let model: Arc<dyn Model> = Arc::new(
        OpenAiChatModel::new(model_id, Some(api_key), Some(base_url))
            .expect("failed to build OpenAiChatModel"),
    );

    let workspace = cwd.join(".agentscope").join("workspace");
    let mut builder = HarnessAgent::builder()
        .name("agentscope-coding-agent")
        .model(model)
        .cwd(cwd.clone())
        .workspace(workspace)
        .sys_prompt(build_system_prompt(&cwd))
        .register_filesystem_tools(true)
        .register_shell_tool(true)
        .compaction(CompactionMiddlewareConfig {
            enabled: true,
            max_tokens: 16_000,
        })
        .disable_memory_hooks()
        // 对话压缩：历史超过触发阈值时由 LLM 总结早期对话并保留最近 tail，
        // 控制 AgentState.context 的持续增长（保留 tail 不破坏前缀 KV 缓存）。
        .conversation_compaction(
            CompactionConfig::builder()
                .trigger_messages(100)
                .trigger_tokens(0)
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
        .max_iters(50);

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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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
        .model_factory(build_agent)
        // Dynamic model switching: resolve the selected model id against the
        // config file (provider lookup) and build a fresh OpenAiChatModel.
        // Takes effect on the next prompt; conversation state is preserved.
        .model_builder(move |model_id| {
            let cfg = ModelConfig::load_or_create();
            let (base_url, api_key) = cfg.resolve_model(model_id)?;
            let model: Arc<dyn Model> = Arc::new(
                OpenAiChatModel::new(model_id.to_string(), Some(api_key), Some(base_url))
                    .map_err(|e| format!("failed to build model '{model_id}': {e}"))?,
            );
            Ok(model)
        })
        .modes(modes)
        .models(models)
        .max_sessions(3)
        .build()?;

    tracing::info!("ACP CodingAgent starting over stdio...");
    agent.run_stdio().await?;
    Ok(())
}
