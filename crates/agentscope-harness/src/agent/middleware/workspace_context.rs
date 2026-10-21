//! Middleware that appends workspace context (session info, AGENTS.md, MEMORY.md,
//! knowledge, additional files) to the request-scoped system prompt, mirroring
//! `io.agentscope.harness.agent.middleware.WorkspaceContextMiddleware`.
//!
//! Runs once per agent call in the async `on_system_prompt` hook, mirroring Java
//! `WorkspaceContextMiddleware.onSystemPrompt`. The core freezes the result and
//! prepends a fresh copy to every reasoning request without persisting it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::middleware::Middleware;

use crate::agent::workspace::WorkspaceManager;
use crate::filesystem::{
    Filesystem, FilesystemKind, LocalFilesystem, LocalFsMode, OverlayFilesystem,
    SandboxBackedFilesystem,
};

const SESSION_CONTEXT_SECTION_TEMPLATE: &str = "\
## AgentStateStore Context
This is the {agent}. We are setting up the context for our chat.
Today's date is {date}.
My operating system is: {os}
The workspace directory is: {workspace}
The project's temporary directory is: {temp}
{dynamic}
";

const DOMAIN_KNOWLEDGE_GUIDANCE: &str = "\
## Domain Knowledge
The workspace `knowledge/` tree holds many detailed reference documents (not only a single summary file). When the task needs specs, procedures, schemas, or domain facts, treat that directory as the source of truth.
Below, `<domain_knowledge_context>` already includes what you need to navigate it: injected `knowledge/KNOWLEDGE.md` (if present) plus a **full list of knowledge file paths** under `knowledge/` — use that as the catalog of what exists and where.
For content not inlined here, open only the paths you need with read_file, grep, or glob (prefer targeted reads over loading entire trees into the reply).
";

const MEMORY_RECALL_GUIDANCE: &str = "\
## Memory Recall
Before answering questions about prior work, decisions, dates, people, or preferences: run memory_search on MEMORY.md + memory/*.md, then memory_get for needed lines. Include Source: <path#line> citations when helpful.
";

const MEMORY_PERSISTENCE_HEADER: &str = "\
## Memory Persistence
You have a persistent MEMORY.md. Update it proactively when:
- User shares preferences, project context, or decisions
- Important outcomes or action items are established
";

const MEMORY_PERSISTENCE_HEADER_HOOKS_ONLY: &str = "\
## Memory Persistence
You have a persistent MEMORY.md that the harness maintains automatically.
";

const MEMORY_SAVE_TOOL_GUIDANCE: &str = "\
Use the **memory_save** tool to persist memories — it atomically updates both MEMORY.md and the daily ledger. Do NOT use write_file or edit_file on MEMORY.md or any path under memory/ — always use memory_save instead.
";

const MEMORY_WRITE_FILE_GUARD: &str = "\
Do NOT use write_file or edit_file on MEMORY.md or any path under memory/ — the harness owns those files.
";

const MEMORY_AUTO_EXTRACT_GUIDANCE: &str =
    "Memory is also automatically extracted at conversation end.\n";

const WORKSPACE_FILES_NOTICE_WITH_MEMORY: &str = "\
## Workspace Files (Injected)
The following <loaded_context> was loaded in from files in your workspace.
These files (for example, `AGENTS.md`, `MEMORY.md`, and `knowledge/KNOWLEDGE.md`) contain memory, facts, preferences, guidelines, and user-specific details learned from prior interactions with user.
";

const WORKSPACE_FILES_NOTICE_WITHOUT_MEMORY: &str = "\
## Workspace Files (Injected)
The following <loaded_context> was loaded in from files in your workspace.
These files (for example, `AGENTS.md` and `knowledge/KNOWLEDGE.md`) contain guidelines and domain context for this agent.
";

const TRUNCATION_NOTICE_WITH_SEARCH: &str =
    "\n\n... (memory truncated — use memory_search for older entries) ...\n";

const TRUNCATION_NOTICE_PLAIN: &str = "\n\n... (memory truncated) ...\n";

const DEFAULT_MAX_CONTEXT_TOKENS: usize = 8000;

/// Middleware that appends workspace context to the request-scoped system prompt once
/// per call. Mirrors Java `WorkspaceContextMiddleware`.
pub struct WorkspaceContextMiddleware {
    workspace_manager: Arc<WorkspaceManager>,
    agent_name: String,
    environment_memory: Option<String>,
    max_context_tokens: usize,
    additional_context_files: Vec<String>,
    disable_memory_tools: bool,
    disable_memory_hooks: bool,
    artifact_delivery_enabled: bool,
}

impl WorkspaceContextMiddleware {
    /// Creates a middleware with default agent name, no environment memory, and the
    /// default 8000-token context budget.
    pub fn new(workspace_manager: Arc<WorkspaceManager>) -> Self {
        Self::new_with_config(
            workspace_manager,
            "HarnessAgent",
            None,
            DEFAULT_MAX_CONTEXT_TOKENS,
        )
    }

    /// Creates a middleware with a custom context token budget.
    pub fn with_max_tokens(workspace_manager: Arc<WorkspaceManager>, max_context_tokens: usize) -> Self {
        Self::new_with_config(workspace_manager, "HarnessAgent", None, max_context_tokens)
    }

    /// Creates a fully-configured middleware.
    pub fn new_with_config(
        workspace_manager: Arc<WorkspaceManager>,
        agent_name: &str,
        environment_memory: Option<String>,
        max_context_tokens: usize,
    ) -> Self {
        let agent_name = if agent_name.trim().is_empty() {
            "HarnessAgent".to_string()
        } else {
            agent_name.to_string()
        };
        Self {
            workspace_manager,
            agent_name,
            environment_memory,
            max_context_tokens,
            additional_context_files: Vec::new(),
            disable_memory_tools: false,
            disable_memory_hooks: false,
            artifact_delivery_enabled: false,
        }
    }

    /// Sets the additional context files (workspace-relative) to inline.
    pub fn set_additional_context_files(&mut self, files: Vec<String>) {
        self.additional_context_files = files;
    }

    /// Mirrors `HarnessAgent.Builder.disableMemoryTools` /
    /// `disableMemoryHooks`: the prompt must not advertise memory capabilities
    /// that the agent does not have.
    pub fn set_memory_flags(&mut self, disable_memory_tools: bool, disable_memory_hooks: bool) {
        self.disable_memory_tools = disable_memory_tools;
        self.disable_memory_hooks = disable_memory_hooks;
    }

    /// Mirrors Java `setArtifactDeliveryEnabled`: adds the sandbox File Isolation
    /// Notice only when a `deliver_artifact` target is configured.
    pub fn set_artifact_delivery_enabled(&mut self, enabled: bool) {
        self.artifact_delivery_enabled = enabled;
    }
}

#[async_trait::async_trait]
impl Middleware for WorkspaceContextMiddleware {
    async fn on_system_prompt(&self, ctx: &RuntimeContext, current_prompt: String) -> String {
        let section = build_workspace_section(
            &self.workspace_manager,
            ctx,
            &self.agent_name,
            self.environment_memory.as_deref(),
            self.max_context_tokens,
            &self.additional_context_files,
            self.disable_memory_tools,
            self.disable_memory_hooks,
            self.artifact_delivery_enabled,
        )
        .await;
        let separator = if current_prompt.is_empty() || current_prompt.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        format!("{current_prompt}{separator}{section}")
    }
}

/// Builds the full workspace section by reading AGENTS.md, MEMORY.md, and knowledge.
async fn build_workspace_section(
    wm: &WorkspaceManager,
    rc: &RuntimeContext,
    agent_name: &str,
    environment_memory: Option<&str>,
    max_context_tokens: usize,
    additional_context_files: &[String],
    disable_memory_tools: bool,
    disable_memory_hooks: bool,
    artifact_delivery_enabled: bool,
) -> String {
    // Mirrors Java `includeMemoryContext()`: MEMORY.md is hidden only when both the
    // memory tools and the background hooks are off.
    let include_memory_context = !(disable_memory_tools && disable_memory_hooks);

    let agents_content = wm.read_agents_md(rc).await.trim().to_string();
    let memory_content = if include_memory_context {
        wm.read_memory_md(rc).await.trim().to_string()
    } else {
        String::new()
    };
    let knowledge_content = wm.read_knowledge_md(rc).await.trim().to_string();
    let workspace = wm.workspace();
    let effective_workspace = effective_workspace(wm, rc, workspace);
    let session_context =
        build_session_context_section(&effective_workspace, rc, agent_name, environment_memory);

    let knowledge_block = build_knowledge_block(wm, rc, &knowledge_content, workspace).await;
    let additional_block = build_additional_context_block(wm, rc, additional_context_files).await;

    let fixed_tokens = estimate_tokens(&session_context)
        + estimate_tokens(&agents_content)
        + estimate_tokens(&knowledge_block)
        + estimate_tokens(&additional_block);
    let memory_tokens = estimate_tokens(&memory_content);
    let available = max_context_tokens.saturating_sub(fixed_tokens);
    let memory_content = if include_memory_context && available > 0 && memory_tokens > available {
        truncate_to_token_budget(&memory_content, available, disable_memory_tools)
    } else {
        memory_content
    };

    let guidance = build_guidance(disable_memory_tools, disable_memory_hooks);
    let workspace_paragraph = build_workspace_paragraph(
        workspace,
        &effective_workspace,
        wm.filesystem(),
        artifact_delivery_enabled,
    );
    let loaded_context = build_loaded_context_section(
        &agents_content,
        &memory_content,
        &knowledge_block,
        &additional_block,
        include_memory_context,
    );
    assemble_section(&session_context, &guidance, &workspace_paragraph, &loaded_context)
}

fn assemble_section(
    session_context: &str,
    guidance: &str,
    workspace_paragraph: &str,
    loaded_context_section: &str,
) -> String {
    let mut sb = String::new();
    if !session_context.trim().is_empty() {
        sb.push_str(session_context);
        sb.push_str("\n\n");
    }
    sb.push_str(guidance);
    if !workspace_paragraph.is_empty() {
        sb.push('\n');
        sb.push_str(workspace_paragraph);
    }
    sb.push('\n');
    sb.push_str(loaded_context_section);
    sb
}

fn build_guidance(disable_memory_tools: bool, disable_memory_hooks: bool) -> String {
    let mut sb = String::new();
    sb.push_str(DOMAIN_KNOWLEDGE_GUIDANCE.trim());
    sb.push_str("\n\n");

    if !disable_memory_tools {
        sb.push_str(MEMORY_RECALL_GUIDANCE.trim());
        sb.push_str("\n\n");
    }

    let persistence = build_memory_persistence_guidance(disable_memory_tools, disable_memory_hooks);
    if !persistence.is_empty() {
        sb.push_str(persistence.trim());
        sb.push_str("\n\n");
    }

    format!("{}\n", sb.trim_end())
}

fn build_memory_persistence_guidance(
    disable_memory_tools: bool,
    disable_memory_hooks: bool,
) -> String {
    if disable_memory_tools && disable_memory_hooks {
        return String::new();
    }

    let mut sb = String::new();
    if !disable_memory_tools {
        sb.push_str(MEMORY_PERSISTENCE_HEADER.trim());
        sb.push('\n');
        sb.push_str(MEMORY_SAVE_TOOL_GUIDANCE.trim());
        sb.push('\n');
    } else {
        sb.push_str(MEMORY_PERSISTENCE_HEADER_HOOKS_ONLY.trim());
        sb.push('\n');
        sb.push_str(MEMORY_WRITE_FILE_GUARD.trim());
        sb.push('\n');
    }

    if !disable_memory_hooks {
        sb.push_str(MEMORY_AUTO_EXTRACT_GUIDANCE.trim());
        sb.push('\n');
    }

    sb
}

/// Java's `effectiveWorkspace`: local overlays expose their runtime-data path (the
/// writable workspace layer) rather than the raw workspace root.
fn effective_workspace(wm: &WorkspaceManager, rc: &RuntimeContext, raw: &Path) -> PathBuf {
    let Some(filesystem) = wm.filesystem() else {
        return raw.to_path_buf();
    };
    if filesystem.kind() == FilesystemKind::Overlay {
        if let Some(overlay) = filesystem.as_any().downcast_ref::<OverlayFilesystem>() {
            if overlay.upper().kind() == FilesystemKind::Local {
                return wm.resolve_runtime_data_path(rc, "");
            }
        }
    }
    raw.to_path_buf()
}

fn local_overlay(workspace: &Path, effective: &Path, fs: Option<&dyn Filesystem>) -> Option<String> {
    let overlay = fs?.as_any().downcast_ref::<OverlayFilesystem>()?;
    let local = overlay.upper().as_any().downcast_ref::<LocalFilesystem>()?;
    let mut sb = String::from("## Workspace\n");
    sb.push_str("Project (the user's source tree you're assisting with): ");
    sb.push_str(&local.root().to_string_lossy());
    sb.push('\n');
    sb.push_str("Workspace (your home base — memory, sessions, skills, runtime data): ");
    sb.push_str(&effective.to_string_lossy());
    sb.push('\n');
    sb.push_str("Path access policy: ");
    sb.push_str(describe_mode(local.mode()));
    sb.push_str(". File tools reject absolute paths outside the roots above");
    sb.push_str(if local.mode() == LocalFsMode::Rooted {
        " with a security error"
    } else {
        ""
    });
    sb.push_str(".\n");
    sb.push_str(
        "Relative paths resolve under the workspace and read-fall-back to the project \
(overlay copy-on-write).\n",
    );
    sb.push_str("Treat this directory as the single global workspace for file operations \
unless explicitly instructed otherwise.\n");
    let _ = workspace;
    Some(sb)
}

fn sandbox_paragraph(effective: &Path, fs: &dyn Filesystem, artifact_enabled: bool) -> String {
    let cwd = fs
        .as_any()
        .downcast_ref::<SandboxBackedFilesystem>()
        .map(|sandbox| sandbox.cwd().to_string_lossy().to_string())
        .unwrap_or_else(|| effective.to_string_lossy().to_string());
    let mut sb = String::from("## Workspace\n");
    sb.push_str("Sandbox root: ");
    sb.push_str(&cwd);
    sb.push('\n');
    if artifact_enabled {
        sb.push_str(
            "Files are isolated inside this container. The host filesystem is not directly \
accessible — see the File Isolation Notice below for how to deliver files out of the sandbox.\n",
        );
    } else {
        sb.push_str(
            "Files are isolated inside this container. The host filesystem is not accessible \
and there is no mechanism for moving files across the boundary.\n",
        );
    }
    sb
}

fn distributed_paragraph(workspace: &Path) -> String {
    let mut sb = String::from("## Workspace\n");
    sb.push_str("Distributed workspace template root: ");
    sb.push_str(&workspace.to_string_lossy());
    sb.push('\n');
    sb.push_str(
        "Runtime data (MEMORY.md, sessions, tasks, skills) lives in a shared remote store, \
not on the local host. Reads of project-authored template files fall back to the workspace \
template root above.\n",
    );
    sb
}

fn legacy_paragraph(workspace: &Path) -> String {
    let mut sb = String::from("## Workspace\n");
    sb.push_str("Your working directory is: ");
    sb.push_str(&workspace.to_string_lossy());
    sb.push('\n');
    sb.push_str(
        "Treat this directory as the single global workspace for file operations \
unless explicitly instructed otherwise.\n",
    );
    sb
}

fn describe_mode(mode: LocalFsMode) -> &'static str {
    match mode {
        LocalFsMode::Sandboxed => "SANDBOXED (all paths anchored to the workspace; `..` blocked)",
        LocalFsMode::Rooted => "ROOTED (absolute paths accepted only inside the roots above)",
        LocalFsMode::Unrestricted => "UNRESTRICTED (absolute paths pass through unchanged)",
    }
}

/// Builds the `## Workspace` paragraph, branching on the concrete filesystem family.
fn build_workspace_paragraph(
    workspace: &Path,
    effective: &Path,
    fs: Option<&dyn Filesystem>,
    artifact_delivery_enabled: bool,
) -> String {
    let kind = fs.map(Filesystem::kind);
    let mut sb = match kind {
        Some(FilesystemKind::Overlay) => local_overlay(workspace, effective, fs)
            .unwrap_or_else(|| legacy_paragraph(workspace)),
        Some(FilesystemKind::Sandbox) => fs
            .map(|fs| sandbox_paragraph(effective, fs, artifact_delivery_enabled))
            .unwrap_or_else(|| legacy_paragraph(workspace)),
        Some(FilesystemKind::Composite) | Some(FilesystemKind::Remote) => {
            distributed_paragraph(workspace)
        }
        _ => legacy_paragraph(workspace),
    };
    sb.push_str(
        "AGENTS.md defines persona and local conventions — honor them when consistent \
with safety and policy.\n",
    );
    if matches!(kind, Some(FilesystemKind::Sandbox)) && artifact_delivery_enabled {
        sb.push_str(
            "**File Isolation Notice**\nFiles inside this container are isolated from the \
host filesystem and are not directly accessible from outside. If your work produces any \
final deliverables—such as documents, reports, images, spreadsheets, archives, audio/video \
files, code artifacts, or similar—you **must** call deliver_artifact automatically when you \
finish the task to export them to their configured external destination. Deliver it silently: \
do not ask the user whether they want it delivered; the tool call itself lets the user see and \
retrieve the artifact directly. Do not mention the delivery or the tool in your reply.\n",
        );
    }
    sb
}

/// Builds the `## AgentStateStore Context` section (date, OS, workspace, dynamic part).
fn build_session_context_section(
    workspace: &Path,
    rc: &RuntimeContext,
    agent_name: &str,
    environment_memory: Option<&str>,
) -> String {
    let today = chrono::Local::now().format("%A %b %-d, %Y").to_string();
    let platform = format!(
        "{} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let temp_dir = std::env::temp_dir().display().to_string();
    let dynamic_part = build_session_dynamic_part(rc, environment_memory);

    SESSION_CONTEXT_SECTION_TEMPLATE
        .replace("{agent}", agent_name)
        .replace("{date}", &today)
        .replace("{os}", &platform)
        .replace("{workspace}", &workspace.to_string_lossy())
        .replace("{temp}", &temp_dir)
        .replace("{dynamic}", &dynamic_part)
        .trim()
        .to_string()
}

fn build_session_dynamic_part(rc: &RuntimeContext, environment_memory: Option<&str>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(sid) = rc.session_id().filter(|s| !s.trim().is_empty()) {
        parts.push(format!("AgentStateStore ID: {sid}"));
    }
    if let Some(em) = environment_memory.filter(|s| !s.trim().is_empty()) {
        parts.push(em.to_string());
    }
    parts.join("\n")
}

fn build_loaded_context_section(
    agents_content: &str,
    memory_content: &str,
    knowledge_block: &str,
    additional_block: &str,
    include_memory_context: bool,
) -> String {
    let notice = if include_memory_context {
        WORKSPACE_FILES_NOTICE_WITH_MEMORY
    } else {
        WORKSPACE_FILES_NOTICE_WITHOUT_MEMORY
    };
    let mut sb = String::from(notice);
    sb.push('\n');
    sb.push_str("<loaded_context>\n");
    sb.push_str(&build_xml_context("agents_context", agents_content));
    if include_memory_context {
        sb.push_str(&build_xml_context("memory_context", memory_content));
    }
    sb.push_str(&build_xml_context("domain_knowledge_context", knowledge_block));
    if !additional_block.trim().is_empty() {
        sb.push_str(additional_block);
    }
    sb.push_str("</loaded_context>\n");
    sb
}

fn build_xml_context(tag_name: &str, content: &str) -> String {
    if content.trim().is_empty() {
        return format!("  <{tag_name}></{tag_name}>\n");
    }
    format!("  <{tag_name}>\n{}\n  </{tag_name}>\n", indent_by_two(content.trim()))
}

fn indent_by_two(text: &str) -> String {
    text.lines().map(|line| format!("  {line}")).collect::<Vec<_>>().join("\n")
}

async fn build_additional_context_block(
    wm: &WorkspaceManager,
    rc: &RuntimeContext,
    additional_context_files: &[String],
) -> String {
    if additional_context_files.is_empty() {
        return String::new();
    }
    let mut sb = String::new();
    for rel_path in additional_context_files {
        let content = wm.read_managed_workspace_file_utf8(rc, rel_path).await;
        if content.trim().is_empty() {
            continue;
        }
        let tag = rel_path
            .replace('/', "_")
            .replace('.', "_")
            .to_lowercase();
        sb.push_str(&format!("  <{tag}>\n"));
        sb.push_str(&indent_by_two(content.trim()));
        sb.push_str("\n");
        sb.push_str(&format!("  </{tag}>\n"));
    }
    sb
}

fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.len() / 4
    }
}

fn truncate_to_token_budget(
    text: &str,
    max_tokens: usize,
    disable_memory_tools: bool,
) -> String {
    let max_chars = max_tokens * 4;
    if text.len() <= max_chars {
        return text.to_string();
    }
    let prefix: String = text.chars().take(max_chars).collect();
    let notice = if disable_memory_tools {
        TRUNCATION_NOTICE_PLAIN
    } else {
        TRUNCATION_NOTICE_WITH_SEARCH
    };
    format!("{prefix}{notice}")
}

async fn build_knowledge_block(
    wm: &WorkspaceManager,
    rc: &RuntimeContext,
    knowledge_content: &str,
    workspace: &Path,
) -> String {
    let knowledge_files = wm.list_knowledge_files(rc).await;
    let mut sb = String::new();

    if !knowledge_content.trim().is_empty() {
        sb.push_str(knowledge_content.trim());
        sb.push('\n');
    }

    if !knowledge_files.is_empty() {
        if !sb.is_empty() {
            sb.push('\n');
        }
        sb.push_str("Knowledge files:\n");
        let mut first = true;
        for f in knowledge_files {
            if !first {
                sb.push('\n');
            }
            first = false;
            let rel = f.strip_prefix(workspace).unwrap_or(&f);
            sb.push_str(&format!("- {}", rel.to_string_lossy().replace('\\', "/")));
        }
        sb.push('\n');
    }

    sb
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn estimate_tokens_quarter_length() {
        assert_eq!(estimate_tokens("abcdefgh"), 2);
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn truncate_short_text_unchanged() {
        let text = "short";
        assert_eq!(truncate_to_token_budget(text, 100, false), text);
    }

    #[test]
    fn truncate_long_text_appends_search_aware_notice() {
        let text = "a".repeat(200);
        let out = truncate_to_token_budget(&text, 10, false);
        assert!(out.ends_with(TRUNCATION_NOTICE_WITH_SEARCH));
        assert!(out.len() < text.len());
    }

    #[test]
    fn truncate_long_text_uses_plain_notice_when_memory_tools_disabled() {
        let text = "a".repeat(200);
        let out = truncate_to_token_budget(&text, 10, true);
        assert!(out.ends_with(TRUNCATION_NOTICE_PLAIN));
        assert!(out.len() < text.len());
    }

    #[test]
    fn xml_context_empty_is_self_closing() {
        assert_eq!(build_xml_context("agents_context", "   "), "  <agents_context></agents_context>\n");
    }

    #[test]
    fn xml_context_indents_content() {
        let out = build_xml_context("memory", "line1\nline2");
        assert_eq!(out, "  <memory>\n  line1\n  line2\n  </memory>\n");
    }

    #[test]
    fn indent_by_two_prefixes_lines() {
        assert_eq!(indent_by_two("a\nb\nc"), "  a\n  b\n  c");
    }

    #[test]
    fn workspace_paragraph_mentions_directory() {
        let p = Path::new("/tmp/ws");
        let out = build_workspace_paragraph(p, p, None, false);
        assert!(out.starts_with("## Workspace\n"));
        assert!(out.contains("/tmp/ws"));
        assert!(out.contains("AGENTS.md"));
    }

    #[test]
    fn session_dynamic_part_includes_id() {
        let rc = RuntimeContext::new().with_session_id("sess-1");
        assert_eq!(build_session_dynamic_part(&rc, None), "AgentStateStore ID: sess-1");
    }

    #[test]
    fn session_dynamic_part_empty_when_absent() {
        let rc = RuntimeContext::new();
        assert_eq!(build_session_dynamic_part(&rc, None), "");
    }

    #[test]
    fn local_overlay_uses_the_runtime_data_workspace() {
        let ws = std::env::temp_dir().join(format!("wctx-overlay-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&ws).unwrap();
        let upper: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::new(ws.clone()));
        let lower: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::new(ws.clone()));
        let fs: Arc<dyn Filesystem> = Arc::new(OverlayFilesystem::new(upper, lower));
        let wm = WorkspaceManager::with_fs(ws.clone(), Some(fs));
        let rc = RuntimeContext::new();

        assert_eq!(
            effective_workspace(&wm, &rc, &ws),
            wm.resolve_runtime_data_path(&rc, "")
        );
        std::fs::remove_dir_all(ws).ok();
    }

    #[test]
    fn sandbox_prompt_includes_artifact_notice_only_when_enabled() {
        let ws = std::env::temp_dir().join(format!("wctx-sandbox-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&ws).unwrap();
        let fs = SandboxBackedFilesystem::new(
            Arc::new(LocalFilesystem::new(ws.clone())) as Arc<dyn Filesystem>,
            ws.clone(),
        );

        let enabled = build_workspace_paragraph(&ws, &ws, Some(&fs), true);
        assert!(enabled.contains("File Isolation Notice"), "{enabled}");
        assert!(enabled.contains("deliver_artifact"), "{enabled}");

        let disabled = build_workspace_paragraph(&ws, &ws, Some(&fs), false);
        assert!(!disabled.contains("File Isolation Notice"), "{disabled}");
        std::fs::remove_dir_all(ws).ok();
    }

    #[tokio::test]
    async fn build_section_includes_agents_and_guidance() {
        let ws = std::env::temp_dir().join(format!("wctx-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("AGENTS.md"), "# Persona\nFriendly.").unwrap();
        let wm = WorkspaceManager::new(ws.clone());
        let rc = RuntimeContext::new().with_session_id("s1");
        let section =
            build_workspace_section(&wm, &rc, "HarnessAgent", None, 8000, &[], false, false, false)
                .await;
        assert!(section.contains("AgentStateStore Context"));
        assert!(section.contains("Persona"));
        assert!(section.contains("<loaded_context>"));
        assert!(section.contains("<agents_context>"));
        std::fs::remove_dir_all(ws).ok();
    }

    #[tokio::test]
    async fn memory_prompt_respects_disable_flags() {
        let ws = std::env::temp_dir().join(format!("wctx-memory-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(ws.join("knowledge")).unwrap();
        std::fs::write(ws.join("AGENTS.md"), "# Persona\nFriendly.").unwrap();
        std::fs::write(ws.join("MEMORY.md"), "remember the project conventions").unwrap();
        std::fs::write(ws.join("knowledge/KNOWLEDGE.md"), "# Knowledge").unwrap();
        let wm = WorkspaceManager::new(ws.clone());
        let rc = RuntimeContext::new().with_session_id("s1");

        let both_enabled =
            build_workspace_section(&wm, &rc, "HarnessAgent", None, 8000, &[], false, false, false)
                .await;
        assert!(both_enabled.contains("## Memory Recall"));
        assert!(both_enabled.contains("memory_save"));
        assert!(both_enabled.contains("automatically extracted"));
        assert!(both_enabled.contains("<memory_context>"));
        assert!(both_enabled.contains("remember the project conventions"));

        let tools_disabled =
            build_workspace_section(&wm, &rc, "HarnessAgent", None, 8000, &[], true, false, false)
                .await;
        assert!(!tools_disabled.contains("## Memory Recall"));
        assert!(!tools_disabled.contains("memory_save"));
        assert!(tools_disabled.contains("harness maintains automatically"));
        assert!(tools_disabled.contains("Do NOT use write_file"));
        assert!(tools_disabled.contains("automatically extracted"));
        assert!(tools_disabled.contains("<memory_context>"));

        let hooks_disabled =
            build_workspace_section(&wm, &rc, "HarnessAgent", None, 8000, &[], false, true, false)
                .await;
        assert!(hooks_disabled.contains("## Memory Recall"));
        assert!(hooks_disabled.contains("memory_save"));
        assert!(!hooks_disabled.contains("automatically extracted"));
        assert!(hooks_disabled.contains("<memory_context>"));

        let both_disabled =
            build_workspace_section(&wm, &rc, "HarnessAgent", None, 8000, &[], true, true, false)
                .await;
        assert!(!both_disabled.contains("## Memory Recall"));
        assert!(!both_disabled.contains("memory_save"));
        assert!(!both_disabled.contains("automatically extracted"));
        assert!(!both_disabled.contains("<memory_context>"));
        assert!(!both_disabled.contains("remember the project conventions"));
        assert!(both_disabled.contains("AGENTS.md` and `knowledge/KNOWLEDGE.md`"));

        std::fs::remove_dir_all(ws).ok();
    }
}
