//! Loads `SubagentDeclaration` instances from Markdown files with YAML front
//! matter placed in the workspace `subagents/` directory, mirroring Java
//! `AgentSpecLoader`. The filename (without `.md`) becomes the agent name.
//! Only direct children are scanned (non-recursive).
use std::path::{Path, PathBuf};

use agentscope_core::agent::RuntimeContext;

use super::{SubagentDeclaration, SubagentMode, SubagentWorkspaceMode};
use crate::filesystem::Filesystem;

/// Workspace directory that holds `.md` subagent declarations.
pub const SUBAGENTS_DIR: &str = "subagents";
/// Glob pattern used to locate declaration files.
pub const SUBAGENT_GLOB: &str = "*.md";

/// Parses markdown content with YAML front matter into a `SubagentDeclaration`.
/// Returns `None` when the content is malformed or `description` is missing.
/// `name` is the agent id (derived from the filename, without `.md`).
pub fn parse(
    markdown: &str,
    name: &str,
    main_workspace: Option<&str>,
) -> Option<SubagentDeclaration> {
    let markdown = markdown.trim();
    if markdown.is_empty() || !markdown.starts_with("---") {
        return None;
    }
    let rest = &markdown[3..];
    let end_idx = rest.find("---")?;
    let front_matter = &rest[..end_idx];
    let body = rest[end_idx + 3..].trim();

    let fm: serde_json::Value = serde_yaml::from_str(front_matter).ok()?;
    let Some(obj) = fm.as_object() else {
        return None;
    };
    if obj.is_empty() {
        return None;
    }

    let description = match as_str(obj.get("description")) {
        Some(d) if !d.trim().is_empty() => d,
        _ => {
            tracing::warn!("subagent '{name}': missing 'description' in front matter, skipping");
            return None;
        }
    };

    // ---- workspace section ----
    let mut workspace_mode = SubagentWorkspaceMode::Isolated;
    let mut workspace_path: Option<String> = None;
    if let Some(ws) = obj.get("workspace").and_then(|v| v.as_object()) {
        if let Some(m) = as_str(ws.get("mode")) {
            if m.eq_ignore_ascii_case("shared") {
                workspace_mode = SubagentWorkspaceMode::Shared;
            } else if m.eq_ignore_ascii_case("isolated") {
                workspace_mode = SubagentWorkspaceMode::Isolated;
            } else {
                tracing::warn!(
                    "subagent '{name}': unknown workspace.mode '{m}', defaulting to isolated"
                );
            }
        }
        if let Some(p) = as_str(ws.get("path")) {
            if !p.is_empty() {
                workspace_path = Some(resolve_path(&p, main_workspace));
            }
        }
    }

    // ---- body / inline system-prompt validation ----
    // When `workspace.path` is set the body must be blank; the prompt is read
    // from `<workspace.path>/AGENTS.md` at runtime.
    let mut body = body.to_string();
    if workspace_path.is_some() && !body.trim().is_empty() {
        tracing::warn!(
            "subagent '{name}': 'workspace.path' is set, ignoring inline body (the prompt is \
             read from <workspace.path>/AGENTS.md at runtime)"
        );
        body = String::new();
    }

    // ---- optional fields ----
    let model = as_str(obj.get("model"));
    // `steps` (preferred) > `maxIters` (deprecated alias), defaulting to 10.
    let steps = Some(parse_steps(obj).unwrap_or(10));
    let temperature = as_f64(obj.get("temperature"));
    let top_p = as_f64(obj.get("top_p")).or_else(|| as_f64(obj.get("topP")));
    let variant = as_str(obj.get("variant"));
    let mode = parse_mode(as_str(obj.get("mode")));
    let hidden = as_bool(obj.get("hidden")).unwrap_or(false);
    let expose_to_user = as_bool(obj.get("expose_to_user"))
        .or_else(|| as_bool(obj.get("exposeToUser")));
    let tools = parse_tool_names(as_str(obj.get("tools")));
    let skills = parse_tool_names(as_str(obj.get("skills")));
    let enable_pending_tool_recovery = as_bool(obj.get("enable_pending_tool_recovery"))
        .or_else(|| as_bool(obj.get("enablePendingToolRecovery")));

    let mut decl = SubagentDeclaration::new(name, description, body);
    decl.workspace_mode = workspace_mode;
    decl.workspace_path = workspace_path;
    decl.model = model;
    decl.steps = steps;
    decl.temperature = temperature;
    decl.top_p = top_p;
    decl.variant = variant;
    decl.mode = mode;
    decl.hidden = hidden;
    decl.expose_to_user = expose_to_user;
    decl.enable_pending_tool_recovery = enable_pending_tool_recovery;
    decl.tools = tools;
    decl.skills = skills;
    Some(decl)
}

/// Loads a single declaration from a filesystem path, mirroring Java
/// `AgentSpecLoader#loadFromFile`. Returns `None` when the file is missing, unreadable, or its
/// front matter is malformed. The filename (without `.md`) becomes the agent name.
pub fn load_from_file(path: &Path, main_workspace: Option<&str>) -> Option<SubagentDeclaration> {
    let name = path.file_name().and_then(|f| f.to_str())?;
    if !name.ends_with(".md") {
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;
    parse(&content, strip_md_extension(name), main_workspace)
}

/// Non-recursively scans `subagents_dir` for `.md` files and parses each into a
/// declaration. Returns an empty list when the directory is missing.
pub fn load_from_directory(
    subagents_dir: &str,
    main_workspace: Option<&str>,
) -> Vec<SubagentDeclaration> {
    let dir = Path::new(subagents_dir);
    if !dir.is_dir() {
        return Vec::new();
    }
    let mut entries: Vec<_> = match std::fs::read_dir(dir) {
        Ok(e) => e.flatten().collect(),
        Err(_) => return Vec::new(),
    };
    entries.sort_by_key(|e| e.file_name());

    let mut decls = Vec::new();
    for entry in entries {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let fname = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
        if !fname.ends_with(".md") {
            continue;
        }
        let name = strip_md_extension(fname);
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(decl) = parse(&content, name, main_workspace) {
            decls.push(decl);
        }
    }
    decls
}

/// Loads declarations via the `Filesystem` layer (applies namespace isolation
/// transparently), listing the `subagents/` directory and reading each `.md`
/// file. Only direct children are considered (non-recursive), matching Java.
pub async fn load_from_filesystem(
    filesystem: &dyn Filesystem,
    ctx: &RuntimeContext,
    main_workspace: Option<&str>,
) -> Vec<SubagentDeclaration> {
    let Ok(files) = filesystem.ls(ctx, SUBAGENTS_DIR).await else {
        return Vec::new();
    };
    let mut sorted: Vec<_> = files.into_iter().filter(|f| !f.is_directory).collect();
    sorted.sort_by_key(|f| f.path.clone());

    let mut decls = Vec::new();
    for fi in sorted {
        let path = fi.path;
        if path.is_empty() {
            continue;
        }
        let fname = extract_file_name(&path);
        if !fname.ends_with(".md") {
            continue;
        }
        let name = strip_md_extension(&fname);
        if name.is_empty() {
            continue;
        }
        let Ok(rr) = filesystem.read(ctx, &path, 0, 0).await else {
            continue;
        };
        if rr.encoding == "base64" {
            continue;
        }
        if let Some(decl) = parse(&rr.content, name, main_workspace) {
            decls.push(decl);
        }
    }
    decls
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn as_str(v: Option<&serde_json::Value>) -> Option<String> {
    match v? {
        serde_json::Value::String(s) => Some(s.trim().to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        // YAML list representation "[a, b]" (mirrors Jackson's List#toString).
        serde_json::Value::Array(a) => Some(format!(
            "[{}]",
            a.iter()
                .map(|x| x.as_str().unwrap_or("").to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
        _ => None,
    }
}

fn as_f64(v: Option<&serde_json::Value>) -> Option<f64> {
    match v? {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn as_u32(v: Option<&serde_json::Value>) -> Option<u32> {
    match v? {
        serde_json::Value::Number(n) => n.as_u64().and_then(|x| u32::try_from(x).ok()),
        serde_json::Value::String(s) => s.trim().parse::<u32>().ok(),
        _ => None,
    }
}

fn as_bool(v: Option<&serde_json::Value>) -> Option<bool> {
    match v? {
        serde_json::Value::Bool(b) => Some(*b),
        serde_json::Value::String(s) => Some(s.trim().eq_ignore_ascii_case("true")),
        _ => None,
    }
}

fn parse_steps(obj: &serde_json::Map<String, serde_json::Value>) -> Option<u32> {
    as_u32(obj.get("steps")).or_else(|| as_u32(obj.get("maxIters")))
}

fn parse_mode(s: Option<String>) -> SubagentMode {
    match s.as_deref().map(str::to_lowercase).as_deref() {
        None | Some("all") => SubagentMode::All,
        Some("primary") => SubagentMode::Primary,
        Some("subagent") => SubagentMode::Subagent,
        Some(other) => {
            tracing::warn!("unknown subagent mode '{other}', defaulting to ALL");
            SubagentMode::All
        }
    }
}

fn parse_tool_names(s: Option<String>) -> Vec<String> {
    let Some(s) = s else {
        return Vec::new();
    };
    let s = s.trim();
    if s.is_empty() {
        return Vec::new();
    }
    // Support both comma-separated strings and YAML list representations "[a, b]"
    let cleaned = s
        .strip_prefix('[')
        .and_then(|x| x.strip_suffix(']'))
        .unwrap_or(s);
    cleaned
        .split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

fn strip_md_extension(filename: &str) -> &str {
    filename.strip_suffix(".md").unwrap_or(filename)
}

fn extract_file_name(path: &str) -> String {
    let idx = path
        .rfind('/')
        .or_else(|| path.rfind('\\'))
        .map(|i| i + 1)
        .unwrap_or(0);
    path[idx..].to_string()
}

fn resolve_path(path_str: &str, main_workspace: Option<&str>) -> String {
    let p = Path::new(path_str);
    if p.is_absolute() {
        return p.to_string_lossy().into_owned();
    }
    if let Some(mw) = main_workspace {
        let resolved = normalize_path(&Path::new(mw).join(p));
        return resolved.to_string_lossy().into_owned();
    }
    path_str.to_string()
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}
