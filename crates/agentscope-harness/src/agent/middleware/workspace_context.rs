//! Middleware that appends workspace context (session info, AGENTS.md, MEMORY.md,
//! knowledge, additional files) to the leading system message, mirroring
//! `io.agentscope.harness.agent.middleware.WorkspaceContextMiddleware`.
//!
//! Runs once per agent call (like the Java `onSystemPrompt` hook fired at call
//! time). Because the core `Middleware::on_system_prompt` hook is synchronous and
//! this middleware needs asynchronous filesystem reads, the section is built in
//! `on_agent` and appended to the leading SYSTEM message instead.

use std::path::Path;
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::middleware::{AgentInput, EventStream, Middleware, Next};
use futures::StreamExt;

use super::subagents::prepend_to_system_message;
use crate::agent::workspace::WorkspaceManager;

const SESSION_CONTEXT_SECTION_TEMPLATE: &str = "\
## AgentStateStore Context
This is the {agent}. We are setting up the context for our chat.
Today's date is {date}.
My operating system is: {os}
The workspace directory is: {workspace}
The project's temporary directory is: {temp}
{dynamic}
";

const GUIDANCE_TEMPLATE: &str = "\
## Domain Knowledge
The workspace `knowledge/` tree holds many detailed reference documents (not only a single summary file). When the task needs specs, procedures, schemas, or domain facts, treat that directory as the source of truth.
Below, `<domain_knowledge_context>` already includes what you need to navigate it: injected `knowledge/KNOWLEDGE.md` (if present) plus a **full list of knowledge file paths** under `knowledge/` — use that as the catalog of what exists and where.
For content not inlined here, open only the paths you need with read_file, grep, or glob (prefer targeted reads over loading entire trees into the reply).

## Memory Recall
Before answering questions about prior work, decisions, dates, people, or preferences: run memory_search on MEMORY.md + memory/*.md, then memory_get for needed lines. Include Source: <path#line> citations when helpful.

## Memory Persistence
You have a persistent MEMORY.md. Update it proactively when:
- User shares preferences, project context, or decisions
- Important outcomes or action items are established
Use the **memory_save** tool to persist memories — it atomically updates both MEMORY.md and the daily ledger. Do NOT use write_file or edit_file on MEMORY.md or any path under memory/ — always use memory_save instead. Memory is also automatically extracted at conversation end.
";

const WORKSPACE_FILES_NOTICE: &str = "\
## Workspace Files (Injected)
The following <loaded_context> was loaded in from files in your workspace.
These files (for example, `AGENTS.md`, `MEMORY.md`, and `knowledge/KNOWLEDGE.md`) contain memory, facts, preferences, guidelines, and user-specific details learned from prior interactions with user.
";

const TRUNCATION_NOTICE: &str =
    "\n\n... (memory truncated — use memory_search for older entries) ...\n";

const DEFAULT_MAX_CONTEXT_TOKENS: usize = 8000;

/// Middleware that appends workspace context to the leading system message once per call.
pub struct WorkspaceContextMiddleware {
    workspace_manager: Arc<WorkspaceManager>,
    agent_name: String,
    environment_memory: Option<String>,
    max_context_tokens: usize,
    additional_context_files: Vec<String>,
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
        }
    }

    /// Sets the additional context files (workspace-relative) to inline.
    pub fn set_additional_context_files(&mut self, files: Vec<String>) {
        self.additional_context_files = files;
    }
}

impl Middleware for WorkspaceContextMiddleware {
    fn on_agent(&self, ctx: &RuntimeContext, input: AgentInput, next: Next<AgentInput>) -> EventStream {
        let wm = Arc::clone(&self.workspace_manager);
        let agent_name = self.agent_name.clone();
        let environment_memory = self.environment_memory.clone();
        let max_context_tokens = self.max_context_tokens;
        let additional_context_files = self.additional_context_files.clone();
        let ctx = ctx.clone();

        let fut = async move {
            let mut input = input;
            let section = build_workspace_section(
                &wm, &ctx, &agent_name, environment_memory.as_deref(), max_context_tokens,
                &additional_context_files,
            ).await;
            if !section.is_empty() {
                input.messages = prepend_to_system_message(input.messages, &section);
            }
            next(input)
        };

        Box::pin(futures::stream::once(fut).flatten())
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
) -> String {
    let agents_content = wm.read_agents_md(rc).await.trim().to_string();
    let memory_content = wm.read_memory_md(rc).await.trim().to_string();
    let knowledge_content = wm.read_knowledge_md(rc).await.trim().to_string();
    let workspace = wm.workspace();
    let session_context = build_session_context_section(workspace, rc, agent_name, environment_memory);

    let knowledge_block = build_knowledge_block(wm, rc, &knowledge_content, workspace).await;
    let additional_block = build_additional_context_block(wm, rc, additional_context_files).await;

    let fixed_tokens = estimate_tokens(&session_context)
        + estimate_tokens(&agents_content)
        + estimate_tokens(&knowledge_block)
        + estimate_tokens(&additional_block);
    let memory_tokens = estimate_tokens(&memory_content);
    let available = max_context_tokens.saturating_sub(fixed_tokens);
    let memory_content = if available > 0 && memory_tokens > available {
        truncate_to_token_budget(&memory_content, available)
    } else {
        memory_content
    };

    let workspace_paragraph = build_workspace_paragraph(workspace);
    let loaded_context = build_loaded_context_section(
        &agents_content, &memory_content, &knowledge_block, &additional_block,
    );
    assemble_section(&session_context, &workspace_paragraph, &loaded_context)
}

fn assemble_section(
    session_context: &str,
    workspace_paragraph: &str,
    loaded_context_section: &str,
) -> String {
    let mut sb = String::new();
    if !session_context.trim().is_empty() {
        sb.push_str(session_context);
        sb.push_str("\n\n");
    }
    sb.push_str(GUIDANCE_TEMPLATE);
    if !workspace_paragraph.is_empty() {
        sb.push('\n');
        sb.push_str(workspace_paragraph);
    }
    sb.push('\n');
    sb.push_str(WORKSPACE_FILES_NOTICE);
    sb.push('\n');
    sb.push_str(loaded_context_section);
    sb
}

/// Builds the `## Workspace` paragraph. In Rust the filesystem is a `dyn Filesystem`
/// with no downcasting, so this renders the single-line legacy form.
fn build_workspace_paragraph(workspace: &Path) -> String {
    let mut sb = String::from("## Workspace\n");
    sb.push_str("Your working directory is: ");
    sb.push_str(&workspace.to_string_lossy());
    sb.push_str("\n");
    sb.push_str(
        "Treat this directory as the single global workspace for file operations \
unless explicitly instructed otherwise.\n",
    );
    sb.push_str(
        "AGENTS.md defines persona and local conventions — honor them when consistent \
with safety and policy.\n",
    );
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
) -> String {
    let mut sb = String::from("<loaded_context>\n");
    sb.push_str(&build_xml_context("agents_context", agents_content));
    sb.push_str(&build_xml_context("memory_context", memory_content));
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

fn truncate_to_token_budget(text: &str, max_tokens: usize) -> String {
    let max_chars = max_tokens * 4;
    if text.len() <= max_chars {
        return text.to_string();
    }
    let prefix: String = text.chars().take(max_chars).collect();
    format!("{prefix}{TRUNCATION_NOTICE}")
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
        assert_eq!(truncate_to_token_budget(text, 100), text);
    }

    #[test]
    fn truncate_long_text_appends_notice() {
        let text = "a".repeat(200);
        let out = truncate_to_token_budget(&text, 10);
        assert!(out.ends_with(TRUNCATION_NOTICE));
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
        let out = build_workspace_paragraph(p);
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

    #[tokio::test]
    async fn build_section_includes_agents_and_guidance() {
        let ws = std::env::temp_dir().join(format!("wctx-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("AGENTS.md"), "# Persona\nFriendly.").unwrap();
        let wm = WorkspaceManager::new(ws.clone());
        let rc = RuntimeContext::new().with_session_id("s1");
        let section = build_workspace_section(&wm, &rc, "HarnessAgent", None, 8000, &[]).await;
        assert!(section.contains("AgentStateStore Context"));
        assert!(section.contains("Persona"));
        assert!(section.contains("<loaded_context>"));
        assert!(section.contains("<agents_context>"));
        std::fs::remove_dir_all(ws).ok();
    }
}
