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

use std::path::PathBuf;
use std::sync::Arc;

use agentscope_core::model::{Model, OpenAiChatModel};
use agentscope_harness::acp::{config::default_modes, AcpCodingAgent};
use agentscope_harness::acp::config::ModelChoice;
use agentscope_harness::agent::{
    CompactionConfig, CompactionMiddlewareConfig, FileSystemSkillRepository, HarnessAgent,
    ToolResultEvictionConfig,
};

/// User-level skills directory (`$HOME/.agents/skills`).
fn user_skills_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)?;
    Some(home.join(".agents").join("skills"))
}

/// 默认系统提示词（对齐 Java `HarnessCodingAgent`）。
const DEFAULT_SYSTEM_PROMPT: &str = "You are a coding agent operating inside a workspace. \
Use the provided filesystem tools (read_file, write_file, edit_file, grep_files, glob_files, list_files) \
to inspect and modify code, and the execute tool for terminal operations (git, build, test). \
Prefer dedicated tools over shell commands whenever they exist. \
Work step by step and verify your changes.";

/// 构建一个绑定指定 cwd 的完整 `HarnessAgent`（文件系统工具 + shell + 压缩 + skills）。
///
/// Skills are wired through the builder API (mirroring the Java `HarnessAgent`
/// builder): the harness composes the user / workspace / default skill repositories,
/// injects the `<available_skills>` block into the system prompt, registers the
/// `load_skill_through_path` tool, and (when enabled) the self-learning
/// `skill_manage` / `propose_skill` tools.
fn build_agent(cwd: PathBuf) -> HarnessAgent {
    let model_id = std::env::var("CODING_MODEL_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "mimo-v2.5".to_string());
    let api_key = std::env::var("OPENAI_API_KEY").ok();
    let base_url = std::env::var("OPENAI_BASE_URL").ok();
    let model: Arc<dyn Model> = Arc::new(
        OpenAiChatModel::new(model_id, api_key, base_url)
            .expect("failed to build OpenAiChatModel"),
    );

    let workspace = cwd.join(".agentscope").join("workspace");
    let mut builder = HarnessAgent::builder()
        .name("agentscope-coding-agent")
        .model(model)
        .cwd(cwd.clone())
        .workspace(workspace)
        .sys_prompt(DEFAULT_SYSTEM_PROMPT.to_string())
        .register_filesystem_tools(true)
        .register_shell_tool(true)
        .compaction(CompactionMiddlewareConfig {
            enabled: true,
            max_tokens: 16_000,
        }).disable_memory_hooks()
        // 对话压缩：历史超过触发阈值时由 LLM 总结早期对话并保留最近 tail，
        // 控制 AgentState.context 的持续增长（保留 tail 不破坏前缀 KV 缓存）。
        .conversation_compaction(CompactionConfig::builder()
            .trigger_messages(100)
            .trigger_tokens(0)
            .keep_messages(12)
            .keep_tokens(-1)
            .keep_tokens_min(2_000)
            .keep_tokens_max(8_000)
            .keep_tokens_ratio(0.25)
            .flush_before_compact(false)
            .offload_before_compact(false)
            .build())
        // 工具结果外置：超过 80k 字符的工具输出（如构建日志）写入磁盘，
        // context 只保留预览占位符，避免大输出驻留内存。
        .tool_result_eviction(ToolResultEvictionConfig::defaults())
        .max_iters(50);

    // Skill support: register the user-level skill repository. The harness composes
    // the workspace + default repositories and wires the skill prompt + the
    // `load_skill_through_path` tool automatically.
    if let Some(root) = user_skills_root() {
        builder = builder.skill_repository(Arc::new(FileSystemSkillRepository::new(root, "user")));
    }

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
    let models = vec![
        ModelChoice { id: "deepseek-v4-flash".to_string(), name: "deepseek-v4-flash".to_string() },
        ModelChoice { id: "mimo-v2.5".to_string(), name: "mimo-v2.5".to_string() },
    ];

    let agent = AcpCodingAgent::builder()
        .name("agentscope-coding-agent")
        .model_factory(build_agent)
        .modes(modes)
        .models(models)
        .max_sessions(3)
        .build()?;

    tracing::info!("ACP CodingAgent starting over stdio...");
    agent.run_stdio().await?;
    Ok(())
}
