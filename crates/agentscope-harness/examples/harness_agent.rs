//! CLI coding agent built on `HarnessAgent`, mirroring the Java `AgiHarnessAgentConsoleDemo`.
//!
//! Wires a `LocalFilesystem` (optionally sandboxed), the standard filesystem tools and a
//! shell-execution tool around a `ReActAgent`. Supports both a one-shot task from the command
//! line and an interactive REPL (`--interactive`) with colored, streaming-style rendering of
//! thinking / agent text / tool calls / results and a final token-usage summary.
//!
//! Run:
//!   cargo run -p agentscope-harness --example harness_agent -- "list files"
//!   cargo run -p agentscope-harness --example harness_agent -- --interactive

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;

use agentscope_core::agent::{ObservableAgent, ReActAgent};
use agentscope_core::event::{AgentEvent, AgentEventType};
use agentscope_core::message::{ChatUsage, Msg, JsonMap, ToolResultBlock};
use agentscope_core::model::{Model, OpenAiChatModel};
use agentscope_core::tool::{Tool, ToolExecutionContext, Toolkit};
use agentscope_harness::{
    Filesystem, HarnessAgent, HarnessSkillEntry, LocalFilesystem, LocalFsMode,
    SandboxBackedFilesystem, SandboxFilesystem, ShellExecuteTool, SkillCatalog,
    SkillPromptBuilder, SkillRuntime,
};

// ── Skill 支持 ─────────────────────────────────────────────────────
//
// 默认从两个目录加载 skill（每个子目录即一个 skill，内含 SKILL.md 与资源文件）：
//   1. 用户级：$HOME/.agents/skills（例如 Windows: C:\Users\<user>\.agents\skills）
//   2. 项目级：<workspace>/.agents/skills
// 加载后构建 <available_skills> 系统提示块，并注册 load_skill_through_path 工具，
// 供 agent 在运行时按需加载 skill 文档/脚本。

/// Skill 来源标记（用于区分同名 skill 的来源，形成 skillId = `<name>_<source>`）。
const SKILL_SOURCE_USER: &str = "user";
const SKILL_SOURCE_WORKSPACE: &str = "workspace";

/// 返回用户级 skills 根目录（$HOME/.agents/skills）。
fn user_skills_root() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)?;
    Some(home.join(".agents").join("skills"))
}

/// 返回项目级 skills 根目录（<workspace>/.agents/skills）。
fn workspace_skills_root(workspace: &std::path::Path) -> std::path::PathBuf {
    workspace.join(".agents").join("skills")
}

/// 递归收集目录下的所有文件，返回 (相对路径, 内容)。
fn collect_resources(dir: &std::path::Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut stack = vec![std::path::PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let abs = dir.join(&rel);
        let Ok(entries) = std::fs::read_dir(&abs) else { continue };
        for entry in entries.flatten() {
            let entry_rel = rel.join(entry.file_name());
            let entry_abs = dir.join(&entry_rel);
            if entry_abs.is_dir() {
                stack.push(entry_rel);
            } else if entry_abs.is_file() {
                if let Ok(content) = std::fs::read_to_string(&entry_abs) {
                    let rel_str = entry_rel.to_string_lossy().replace('\\', "/");
                    out.push((rel_str, content));
                } else if let Ok(bytes) = std::fs::read(&entry_abs) {
                    use base64::Engine as _;
                    let b64 = {
                        use base64::engine::general_purpose::STANDARD;
                        STANDARD.encode(bytes)
                    };
                    let rel_str = entry_rel.to_string_lossy().replace('\\', "/");
                    out.push((rel_str, format!("base64:{b64}")));
                }
            }
        }
    }
    out.sort();
    out
}

/// 从 SKILL.md 的第一行标题提取 description（找不到则用目录名兜底）。
fn skill_description(name: &str, resources: &[(String, String)]) -> String {
    for (rel, content) in resources {
        if rel.ends_with("SKILL.md") {
            for line in content.lines() {
                let l = line.trim();
                if !l.is_empty() {
                    return l.trim_start_matches('#').trim().to_string();
                }
            }
        }
    }
    format!("Skill: {name}")
}

/// 从单个 skills 根目录加载所有 skill。
/// 返回 (AgentSkill, 该 skill 的宿主盘绝对路径) 列表。
fn load_skills_from_dir(
    root: &std::path::Path,
    source: &str,
) -> Vec<(agentscope_harness::AgentSkill, std::path::PathBuf)> {
    let mut skills = Vec::new();
    if !root.is_dir() {
        return skills;
    }
    let Ok(entries) = std::fs::read_dir(root) else { return skills };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue; // 仅把子目录视为 skill
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "_drafts" {
            continue;
        }
        // 需要一个 SKILL.md 才视为有效 skill
        if !dir.join("SKILL.md").is_file() {
            continue;
        }
        let resources = collect_resources(&dir);
        let description = skill_description(&name, &resources);
        let mut builder = agentscope_harness::AgentSkillBuilder::new()
            .name(&name)
            .source(source)
            .description(description);
        for (rel, content) in &resources {
            builder = builder.add_resource(rel.clone(), content.clone());
        }
        skills.push((builder.build(), dir));
    }
    skills.sort_by(|a, b| a.0.name().cmp(b.0.name()));
    skills
}

/// 从用户级与项目级两个目录加载全部 skills，构建 SkillCatalog。
/// 每个 skill 的 files_root 指向其宿主盘绝对路径，便于 shell 执行脚本。
fn build_skill_catalog(workspace: &std::path::Path) -> (SkillCatalog, Vec<(String, std::path::PathBuf)>) {
    let mut entries = Vec::new();
    let mut id_to_dir: Vec<(String, std::path::PathBuf)> = Vec::new();
    let mut push = |source: &str, skills: Vec<(agentscope_harness::AgentSkill, std::path::PathBuf)>| {
        for (skill, dir) in skills {
            let id = skill.skill_id();
            let abs = dir
                .canonicalize()
                .unwrap_or_else(|_| dir.clone())
                .to_string_lossy()
                .replace('\\', "/");
            // 仅 Shell 可用时暴露 files_root，否则交给 load_skill_through_path。
            entries.push(HarnessSkillEntry::new(skill).with_files_root(&abs));
            id_to_dir.push((id, dir));
        }
    };

    let mut ws_skills = load_skills_from_dir(&workspace_skills_root(workspace), SKILL_SOURCE_WORKSPACE);
    let mut user_skills = match user_skills_root() {
        Some(root) => load_skills_from_dir(&root, SKILL_SOURCE_USER),
        None => Vec::new(),
    };
    // 项目级 skill 优先，重名时用户级不再加入（避免 skillId 冲突）。
    let ws_names: std::collections::HashSet<String> =
        ws_skills.iter().map(|(s, _)| s.name().to_string()).collect();
    user_skills.retain(|(s, _)| !ws_names.contains(s.name()));

    push(SKILL_SOURCE_WORKSPACE, std::mem::take(&mut ws_skills));
    push(SKILL_SOURCE_USER, user_skills);

    (SkillCatalog::of(entries), id_to_dir)
}

/// 渲染 <available_skills> 系统提示块（无 skill 时返回空串）。
fn render_skills_prompt(catalog: &SkillCatalog) -> String {
    let runtime = SkillRuntime::new();
    runtime.render(&catalog)
}

/// 把加载到的 skills 拼接到系统提示词尾部。
fn system_prompt_with_skills(base: &str, skills_block: &str) -> String {
    if skills_block.trim().is_empty() {
        base.to_string()
    } else {
        format!("{base}\n\n{skills_block}")
    }
}

/// `load_skill_through_path(skillId=..., path=...)` 工具：按 skillId 解析该
/// skill 的宿主目录，并读取其内部相对路径（默认 SKILL.md），返回文件内容。
struct LoadSkillTool {
    /// skillId -> 宿主盘绝对目录
    index: std::collections::HashMap<String, std::path::PathBuf>,
}

#[async_trait::async_trait]
impl Tool for LoadSkillTool {
    fn name(&self) -> &str {
        "load_skill_through_path"
    }

    fn description(&self) -> &str {
        "Load the documentation or a resource file of an available skill. \
         Provide skillId (from <available_skills>) and an optional relative path \
         inside that skill (defaults to SKILL.md). Returns the file content."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skillId": {"type": "string", "description": "The skill id, e.g. <name>_<source> as shown in <available_skills>"},
                "path": {"type": "string", "description": "Relative path inside the skill directory (default: SKILL.md)"}
            },
            "required": ["skillId"]
        })
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> agentscope_core::error::Result<ToolResultBlock> {
        use std::io::Read;
        let skill_id = args
            .get("skillId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| agentscope_core::Error::Tool("Missing string argument: skillId".into()))?;
        let rel = args
            .get("path")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "SKILL.md".to_string());

        let Some(dir) = self.index.get(&skill_id) else {
            return Ok(ToolResultBlock::error(format!(
                "Unknown skillId '{skill_id}'. Available skills: {}",
                self.index.keys().cloned().collect::<Vec<_>>().join(", ")
            )));
        };
        let rel_normalized = rel.replace('\\', "/");
        if rel_normalized.starts_with('/') || rel_normalized.contains("..") {
            return Ok(ToolResultBlock::error("path must be a relative path within the skill; '..' and absolute paths are not allowed."));
        }
        let target = dir.join(&rel_normalized);
        if !target.starts_with(dir) {
            return Ok(ToolResultBlock::error("path escapes the skill directory."));
        }
        if target.is_dir() {
            // 目录：列出内容，便于模型浏览
            let mut listing = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&target) {
                for e in entries.flatten() {
                    let n = e.file_name().to_string_lossy().to_string();
                    listing.push(if e.path().is_dir() {
                        format!("{n}/")
                    } else {
                        n
                    });
                }
            }
            listing.sort();
            return Ok(ToolResultBlock::text(format!(
                "Directory listing of {rel_normalized}:\n{}",
                listing.join("\n")
            )));
        }
        if !target.is_file() {
            return Ok(ToolResultBlock::error(format!(
                "No such file: {rel_normalized} in skill '{skill_id}'"
            )));
        }
        let mut handle = match std::fs::File::open(&target) {
            Ok(f) => f,
            Err(e) => return Ok(ToolResultBlock::error(e.to_string())),
        };
        let mut bytes = Vec::new();
        if handle.read_to_end(&mut bytes).is_err() {
            return Ok(ToolResultBlock::error(format!(
                "Failed to read {rel_normalized} in skill '{skill_id}'"
            )));
        }
        // 尽量按 UTF-8 输出；二进制则给出大小提示。
        match String::from_utf8(bytes) {
            Ok(text) => Ok(ToolResultBlock::text(text)),
            Err(_) => Ok(ToolResultBlock::text(format!(
                "[binary file {rel_normalized} in skill '{skill_id}', {} bytes — use the shell/filesystem tools to process it]",
                bytes.len()
            ))),
        }
    }
}

// ── ANSI 颜色工具（与 Kotlin Demo 的 Color 对象对应）───────────────────
struct Color;
impl Color {
    const RESET: &'static str = "\x1B[0m";
    const BOLD: &'static str = "\x1B[1m";
    const DIM: &'static str = "\x1B[2m";
    const CYAN: &'static str = "\x1B[36m";
    const GREEN: &'static str = "\x1B[32m";
    const YELLOW: &'static str = "\x1B[33m";
    const MAGENTA: &'static str = "\x1B[35m";
    const RED: &'static str = "\x1B[31m";
    /// 组合：加粗 + 青色（用于工具区域边框）。
    const BOLD_CYAN: &'static str = "\x1B[1m\x1B[36m";
}

fn cprint(color: &str, msg: &str) {
    print!("{color}{msg}{}", Color::RESET);
    let _ = std::io::stdout().flush();
}
fn cprintln(color: &str, msg: &str) {
    println!("{color}{msg}{}", Color::RESET);
}
fn ok(msg: &str) {
    println!("  {}{}{msg}{}", Color::GREEN, "ok", Color::RESET);
}
fn info(msg: &str) {
    println!("  {}{msg}{}", Color::DIM, Color::RESET);
}
fn err(msg: &str) {
    cprintln(Color::RED, &format!("  error: {msg}"));
}

/// 在 Windows 上尽量保证 UTF-8 输出。ANSI VT 处理在现代 Windows 终端
/// （Windows Terminal / VS Code 集成终端）默认开启，无需额外配置。
fn enable_console_utf8_and_vt() {
    // 若环境变量缺失则显式设置 UTF-8（写入 stdout 前调用）
    if std::env::var_os("RUST_LOG").is_none() {
        let _ = std::env::set_var("RUST_LOG", "info");
    }
}

// ── CLI 参数 ─────────────────────────────────────────────────────────
struct Cli {
    task: Option<String>,
    model: String,
    workspace: PathBuf,
    sandbox: bool,
    max_iters: i32,
    shell: bool,
    quiet: bool,
    system: String,
    interactive: bool,
    type_delay: u64,
}

fn usage() -> ! {
    eprintln!(
        "Usage: harness_agent [OPTIONS] [TASK]\n\
         \n\
         Options:\n\
         \x20 --interactive       start an interactive REPL (default when no TASK)\n\
         \x20 --model <NAME>      model id (env: AGENTSCOPE_MODEL, default gpt-4o)\n\
         \x20 --workspace <DIR>   working directory (default: current dir)\n\
         \x20 --sandbox           restrict filesystem ops to the workspace\n\
         \x20 --max-iters <N>     max reasoning iterations (default 10)\n\
         \x20 --no-shell          do not register the shell execution tool\n\
         \x20 --quiet             suppress progress output\n\
         \x20 --system <PROMPT>   custom system prompt\n\
         \x20 --type-delay <MS>   typewriter delay per char in ms (default 5, 0 = instant)\n\
         \x20 --help              show this help"
    );
    std::process::exit(2);
}

fn parse_args() -> Cli {
    let mut cli = Cli {
        task: None,
        model: std::env::var("AGENTSCOPE_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".to_string()),
        workspace: std::env::current_dir().unwrap_or_default(),
        sandbox: false,
        max_iters: 10,
        shell: true,
        quiet: false,
        system: DEFAULT_SYSTEM_PROMPT.to_string(),
        interactive: false,
        type_delay: 5,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let mut next = || {
            i += 1;
            args.get(i).cloned()
        };
        match a.as_str() {
            "--model" => cli.model = next().unwrap_or_else(|| usage()),
            "--workspace" => cli.workspace = PathBuf::from(next().unwrap_or_else(|| usage())),
            "--max-iters" => {
                cli.max_iters = next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| usage())
            }
            "--system" => cli.system = next().unwrap_or_else(|| usage()),
            "--type-delay" => {
                cli.type_delay = next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| usage())
            }
            "--interactive" => cli.interactive = true,
            "--sandbox" => cli.sandbox = true,
            "--no-shell" => cli.shell = false,
            "--quiet" => cli.quiet = true,
            "--help" | "-h" => usage(),
            _ if a.starts_with('-') => usage(),
            _ => cli.task = Some(a.clone()),
        }
        i += 1;
    }
    // 未提供 TASK 且未显式 --interactive 时，默认进入交互模式
    if cli.task.is_none() {
        cli.interactive = true;
    }
    cli
}

const DEFAULT_SYSTEM_PROMPT: &str = "You are a coding agent operating inside a workspace. \
Use the provided filesystem tools (read_file, write_file, edit_file, grep_files, glob_files, list_files) \
to inspect and modify code, and the execute tool for terminal operations (git, build, test). \
Prefer dedicated tools over shell commands whenever they exist. \
Work step by step and verify your changes.";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    enable_console_utf8_and_vt();
    let cli = parse_args();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run(cli))
}

/// 构建一个绑定指定模型与 toolkit 的 ReActAgent。
fn build_agent(
    model: Arc<dyn Model>,
    toolkit: agentscope_core::tool::Toolkit,
    system: &str,
    max_iters: i32,
) -> Result<ReActAgent, Box<dyn std::error::Error>> {
    let agent = ReActAgent::builder()
        .name("coding-agent")
        .model(model)
        .toolkit(toolkit)
        .sys_prompt(system)
        .max_iters(max_iters)
        .build()?;
    Ok(agent)
}

/// 订阅粗粒度生命周期事件，输出进度（与 Kotlin Demo 的 subscribe 对应）。
fn subscribe_progress(agent: &ReActAgent) {
    agent.subscribe(Box::new(|ev: AgentEvent| match ev {
        AgentEvent::AgentStart(_) => println!("\n[run] agent started"),
        AgentEvent::ModelCallEnd(_) => println!("[run] model call finished"),
        AgentEvent::AgentEnd(_) => println!("[run] agent finished"),
        _ => {}
    }));
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    if !cli.workspace.is_dir() {
        std::fs::create_dir_all(&cli.workspace)?;
    }
    let workspace = cli.workspace.canonicalize()?;

    // 1. Model
    let api_key = std::env::var("OPENAI_API_KEY").ok();
    let base_url = std::env::var("OPENAI_BASE_URL").ok();
    if !cli.quiet {
        println!(
            "[agent] model={} base_url={} api_key={} workspace={}",
            cli.model,
            base_url.as_deref().unwrap_or("(not set)"),
            api_key.as_deref().unwrap_or("(not set)"),
            workspace.display()
        );
    }
    let model: Arc<dyn Model> = Arc::new(OpenAiChatModel::new(
        cli.model.clone(),
        api_key,
        base_url,
    )?);

    // 2. Filesystem (sandboxed or unrestricted over the workspace)
    let fs: Arc<dyn Filesystem> = if cli.sandbox {
        Arc::new(LocalFilesystem::with_mode(
            &workspace,
            LocalFsMode::Sandboxed,
            10,
        ))
    } else {
        Arc::new(LocalFilesystem::new(&workspace))
    };

    // 3. HarnessAgent wires filesystem tools + cwd around the model/toolkit
    let harness = HarnessAgent::builder()
        .name("coding-agent")
        .model(model.clone())
        .filesystem(fs.clone())
        .cwd(&workspace)
        .register_filesystem_tools(true)
        .build();

    // 4. Add the shell tool on top of the harness toolkit
    let toolkit = harness.toolkit().clone();
    if cli.shell {
        let sandbox: Arc<dyn SandboxFilesystem> =
            Arc::new(SandboxBackedFilesystem::new(fs, &workspace));
        ShellExecuteTool::register(&toolkit, sandbox);
    }

    // 4.5 Skills: load from 用户级 ($HOME/.agents/skills) 与 项目级 (<workspace>/.agents/skills),
    //     渲染 <available_skills> 提示块，并注册 load_skill_through_path 工具。
    let (skill_catalog, skill_dirs) = build_skill_catalog(&workspace);
    let skills_block = render_skills_prompt(&skill_catalog);
    if !cli.quiet {
        println!(
            "[agent] skills loaded: {}",
            if skill_catalog.is_empty() {
                "(none)".to_string()
            } else {
                skill_catalog.ids().join(", ")
            }
        );
    }
    if !skill_dirs.is_empty() {
        toolkit.register_tool(std::sync::Arc::new(LoadSkillTool {
            index: skill_dirs.into_iter().collect(),
        }));
    }

    if !cli.quiet {
        println!("[agent] tools: {}", toolkit.tool_names().join(", "));
    }

    // 5. ReAct agent driven by the harness-assembled toolkit
    let system_prompt = system_prompt_with_skills(&cli.system, &skills_block);
    let mut agent = build_agent(model.clone(), toolkit.clone(), &system_prompt, cli.max_iters)?;
    if !cli.quiet {
        subscribe_progress(&agent);
    }

    // 6. Dispatch: single task or interactive REPL
    if let Some(task) = cli.task {
        run_task(&agent, &task, cli.type_delay).await?;
    } else {
        run_repl(&mut agent, toolkit, &cli, &workspace, &system_prompt).await?;
    }
    Ok(())
}

/// 执行单个任务，边生成边渲染（流式打字机输出）。
async fn run_task(
    agent: &ReActAgent,
    task: &str,
    type_delay: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    // 订阅流式事件，边生成边打印，最后输出 token 汇总。
    let opts = agentscope_core::agent::stream_options::StreamOptions::defaults();
    let mut stream =
        agent.stream_events(vec![Msg::user(task)], opts);
    let mut render = StreamRenderer::new(type_delay);
    while let Some(ev) = stream.next().await {
        let ev = ev?;
        render.on_event(&ev).await;
    }
    render.finish().await;
    Ok(())
}

// ── 交互式 REPL（对应 Kotlin Demo 的命令循环）────────────────────────
/// REPL 持有 toolkit，以便 `/model` 切换模型时重建 agent 而保留工具。
async fn run_repl(
    agent: &mut ReActAgent,
    toolkit: agentscope_core::tool::Toolkit,
    cli: &Cli,
    workspace: &PathBuf,
    system_prompt: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    cprintln(Color::BOLD, "AgiHarnessAgent Console (Rust)");
    cprintln(Color::DIM, &"─".repeat(50));
    println!();
    info(&format!("model={}", cli.model));
    info(&format!("workspace={}", workspace.display()));
    info(&format!("tools: {}", toolkit.tool_names().join(", ")));
    println!("输入消息发送给 Agent，输入 /help 查看命令");
    cprintln(Color::DIM, &"─".repeat(50));
    println!();

    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    loop {
        cprint(Color::CYAN, "> ");
        print!(" ");
        let _ = std::io::stdout().flush();
        let line = match lines.next() {
            Some(Ok(l)) => l,
            _ => break,
        };
        let input = line.trim().to_string();
        if input.is_empty() {
            continue;
        }
        if input.starts_with('/') {
            if !handle_command(agent, &toolkit, &input, cli, system_prompt).await {
                break;
            }
            continue;
        }
        let opts = agentscope_core::agent::stream_options::StreamOptions::defaults();
        let mut stream = agent.stream_events(vec![Msg::user(&input)], opts);
        let mut render = StreamRenderer::new(cli.type_delay);
        let mut failed = false;
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(ev) => render.on_event(&ev).await,
                Err(e) => {
                    err(&e.to_string());
                    failed = true;
                    break;
                }
            }
        }
        if !failed {
            render.finish().await;
        }
    }
    cprintln(Color::DIM, "清理中...");
    cprintln(Color::GREEN, "再见！");
    Ok(())
}

/// 处理命令，返回 false 表示退出。
async fn handle_command(
    agent: &mut ReActAgent,
    toolkit: &agentscope_core::tool::Toolkit,
    input: &str,
    cli: &Cli,
) -> bool {
    let parts: Vec<&str> = input.splitn(2, char::is_whitespace).collect();
    let cmd = parts[0];
    let arg = parts.get(1).copied().unwrap_or("");
    match cmd {
        "/exit" | "/quit" => false,
        "/help" => {
            print_help();
            true
        }
        "/model" => {
            if arg.is_empty() {
                cprintln(Color::CYAN, &format!("当前模型: {}", cli.model));
            } else {
                // 切换模型：用新模型重建 agent（toolkit 复用，保留工具）
                let api_key = std::env::var("OPENAI_API_KEY").ok();
                let base_url = std::env::var("OPENAI_BASE_URL").ok();
                match OpenAiChatModel::new(arg.to_string(), api_key.clone(), base_url.clone()) {
                    Ok(m) => {
                        let model: Arc<dyn Model> = Arc::new(m);
                        match build_agent(model, toolkit.clone(), &cli.system, cli.max_iters) {
                            Ok(new_agent) => {
                                *agent = new_agent;
                                if !cli.quiet {
                                    subscribe_progress(agent);
                                }
                                ok(&format!(
                                    "已切换到模型: {} (base_url={}, api_key={})",
                                    arg,
                                    base_url.as_deref().unwrap_or("(not set)"),
                                    api_key.as_deref().unwrap_or("(not set)")
                                ));
                            }
                            Err(e) => err(&e.to_string()),
                        }
                    }
                    Err(e) => err(&e.to_string()),
                }
            }
            true
        }
        "/workspace" => {
            cprintln(Color::CYAN, &format!("当前工作目录: {}", cli.workspace.display()));
            true
        }
        other => {
            cprintln(Color::RED, &format!("未知命令: {other}"));
            print_help();
            true
        }
    }
}

fn print_help() {
    println!(
        "\
  命令
  /exit       退出 Demo
  /help       显示此帮助
  /model      查看当前模型
  /model <id> 切换到指定模型
  /workspace  查看当前工作目录
  其他内容作为 prompt 发送给 Agent"
    );
}
// ── 流式渲染器（对应 Kotlin Demo 的流式渲染，基于 stream_events 的增量事件）────
/// 消费 `stream_events` 的增量事件，边生成边用打字机逐字打印。
/// 文本/思考走 typewrite；工具调用/结果按行打印；结束后输出 token 汇总。
struct StreamRenderer {
    type_delay: u64,
    responding: bool,
    tool_area_open: bool,
    usage: Option<ChatUsage>,
    stop_reason: Option<String>,
}

impl StreamRenderer {
    fn new(type_delay: u64) -> Self {
        Self {
            type_delay,
            responding: false,
            tool_area_open: false,
            usage: None,
            stop_reason: None,
        }
    }

    fn field_str(data: &agentscope_core::event::EventData, key: &str) -> Option<String> {
        data.fields
            .get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    async fn on_event(&mut self, ev: &AgentEvent) {
        match ev.event_type() {
            AgentEventType::ThinkingBlockDelta => {
                if !self.responding {
                    cprintln(Color::MAGENTA, "  ─ thinking ─ ");
                }
                if let Some(d) = Self::field_str(ev.data(), "delta") {
                    typewrite(&d, self.type_delay).await;
                }
            }
            AgentEventType::TextBlockDelta => {
                if let Some(d) = Self::field_str(ev.data(), "delta") {
                    if !d.trim().is_empty() && !self.responding {
                        cprintln(
                            Color::GREEN,
                            &format!("  {}{}{}:", Color::BOLD, "agent", Color::RESET),
                        );
                        self.responding = true;
                    }
                    typewrite(&d, self.type_delay).await;
                    if d.ends_with('\n') {
                        self.responding = false;
                    }
                }
            }
            AgentEventType::AgentResult => {
                if self.responding {
                    println!();
                    self.responding = false;
                }
            }
            AgentEventType::ToolCallStart => {
                if self.responding {
                    println!();
                    self.responding = false;
                }
                if !self.tool_area_open {
                    cprintln(Color::BOLD_CYAN, "  ┌─ tools ────────────────────────────");
                    self.tool_area_open = true;
                }
                let name = Self::field_str(ev.data(), "toolCallName")
                    .unwrap_or_else(|| "tool".to_string());
                let input = Self::field_str(ev.data(), "input")
                    .unwrap_or_default();
                println!(
                    "  │ {}{}{}  {}in:{}  {}",
                    Color::YELLOW,
                    name,
                    Color::RESET,
                    Color::BOLD,
                    Color::RESET,
                    input
                );
                let _ = std::io::stdout().flush();
            }
            AgentEventType::ToolResultTextDelta => {
                if self.responding {
                    println!();
                    self.responding = false;
                }
                if !self.tool_area_open {
                    cprintln(Color::BOLD_CYAN, "  ┌─ tools ────────────────────────────");
                    self.tool_area_open = true;
                }
                if let Some(d) = Self::field_str(ev.data(), "delta") {
                    print!("  │ {}{}", Color::YELLOW, d);
                    let _ = std::io::stdout().flush();
                }
            }
            AgentEventType::ExceedMaxIters => {
                self.stop_reason = Some("max_iterations".to_string());
            }
            _ => {}
        }
    }

    async fn finish(self) {
        if self.responding {
            println!();
        }
        if self.tool_area_open {
            cprintln(Color::BOLD_CYAN, "  └──────────────────────────────────");
            println!();
        }
        let reason = self.stop_reason.unwrap_or_else(|| "stop".to_string());
        println!();
        cprintln(Color::BOLD, "  ┌─ summary ──────────────────────────");
        info(&format!("stopReason: {reason}"));
        if let Some(u) = self.usage {
            info(&format!(
                "tokens: in={}  out={}  total={}",
                u.input_tokens,
                u.output_tokens,
                u.total_tokens()
            ));
        }
        cprintln(Color::BOLD, "  └──────────────────────────────────");
        println!();
        println!();
    }
}

// ── 回复渲染 ──────────────────────────────────────────────────────
/// 以打字机节奏逐字打印文本（type_delay 毫秒/字符；0 表示即时输出）。
/// 换行符即时刷新，避免整行卡顿，营造自然的“打字机”效果。
async fn typewrite(text: &str, type_delay: u64) {
    if type_delay == 0 {
        print!("{text}");
        let _ = std::io::stdout().flush();
        return;
    }
    for ch in text.chars() {
        print!("{ch}");
        let _ = std::io::stdout().flush();
        if ch == '\n' {
            continue; // 换行不额外延迟
        }
        tokio::time::sleep(std::time::Duration::from_millis(type_delay)).await;
    }
}

