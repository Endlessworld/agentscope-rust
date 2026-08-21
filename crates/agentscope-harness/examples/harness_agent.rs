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

use async_trait::async_trait;

use agentscope_core::model::{Model, OpenAiChatModel};
use agentscope_harness::acp::{config::ModeChoice, AcpCodingAgent};
use agentscope_harness::agent::{
    CompactionMiddlewareConfig, FileSystemSkillRepository, HarnessAgent,
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
        })
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
    let modes = vec![
        ModeChoice { id: "yolo".to_string(), name: "YOLO".to_string() },
        ModeChoice { id: "plan".to_string(), name: "Plan".to_string() },
    ];

    let agent = AcpCodingAgent::builder()
        .name("agentscope-coding-agent")
        .model_factory(build_agent)
        .modes(modes)
        .build()?;

    tracing::info!("ACP CodingAgent starting over stdio...");
    agent.run_stdio().await?;
    Ok(())
}
