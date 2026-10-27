//! Loads `SubagentDeclaration` instances from Markdown files with YAML front
//! matter placed in the workspace `subagents/` directory, mirroring Java
//! `AgentSpecLoader`. The filename (without `.md`) becomes the agent name.
//! Only direct children are scanned (non-recursive).
use std::path::{Path, PathBuf};

use agentscope_core::agent::RuntimeContext;

use super::{SubagentDeclaration, SubagentMode, SubagentWorkspaceMode};
use crate::agent::memory::compaction::CompactionConfig;
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
    // Java `AgentSpecLoader.applyCompaction`: absent/null inherits, `false` disables,
    // `true` uses the defaults, a mapping builds a full config.
    let (compaction, disable_compaction) = match apply_compaction(obj.get("compaction"), name) {
        Some(CompactionSetting::Disabled) => (None, true),
        Some(CompactionSetting::Config(config)) => (Some(config), false),
        None => (None, as_bool(obj.get("disable_compaction"))
            .or_else(|| as_bool(obj.get("disableCompaction")))
            .unwrap_or(false)),
    };

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
    decl.compaction = compaction;
    decl.disable_compaction = disable_compaction;
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

/// The outcome of a front-matter `compaction:` key (Java `applyCompaction`).
enum CompactionSetting {
    /// `compaction: false` — Java `Builder.disableCompaction()`.
    Disabled,
    /// `compaction: true` (defaults) or a mapping of compaction fields.
    Config(CompactionConfig),
}

/// Parses the front-matter `compaction:` key. Returns `None` when the key is
/// absent or null (the declaration then inherits the parent setting), and `None`
/// plus a warning when the value is malformed — Java logs and ignores it too.
fn apply_compaction(
    value: Option<&serde_json::Value>,
    name: &str,
) -> Option<CompactionSetting> {
    let value = value?;
    if value.is_null() {
        return None;
    }
    if let Some(b) = value.as_bool() {
        return Some(if b {
            CompactionSetting::Config(CompactionConfig::builder().build())
        } else {
            CompactionSetting::Disabled
        });
    }
    let Some(fields) = value.as_object() else {
        tracing::warn!("subagent '{name}': compaction must be a boolean or mapping, ignoring");
        return None;
    };
    let mut config = CompactionConfig::builder();
    let mut invalid: Option<String> = None;
    for (key, setting) in fields {
        match key.as_str() {
            "triggerMessages" | "trigger_messages" => match compaction_int(key, setting) {
                Ok(v) => config = config.trigger_messages(v as usize),
                Err(e) => invalid = Some(e),
            },
            "triggerTokens" | "trigger_tokens" => match compaction_int(key, setting) {
                Ok(v) => config = config.trigger_tokens(v as usize),
                Err(e) => invalid = Some(e),
            },
            "reserved" => match compaction_int(key, setting) {
                Ok(v) => config = config.reserved(v as usize),
                Err(e) => invalid = Some(e),
            },
            "keepMessages" | "keep_messages" => match compaction_int(key, setting) {
                Ok(v) => config = config.keep_messages(v as usize),
                Err(e) => invalid = Some(e),
            },
            "keepTokens" | "keep_tokens" => match compaction_int(key, setting) {
                Ok(v) => config = config.keep_tokens(v),
                Err(e) => invalid = Some(e),
            },
            "keepTokensMin" | "keep_tokens_min" => match compaction_int(key, setting) {
                Ok(v) => config = config.keep_tokens_min(v as usize),
                Err(e) => invalid = Some(e),
            },
            "keepTokensMax" | "keep_tokens_max" => match compaction_int(key, setting) {
                Ok(v) => config = config.keep_tokens_max(v as usize),
                Err(e) => invalid = Some(e),
            },
            "keepTokensRatio" | "keep_tokens_ratio" => match setting.as_f64() {
                Some(ratio) if ratio.is_finite() && ratio > 0.0 && ratio <= 1.0 => {
                    config = config.keep_tokens_ratio(ratio);
                }
                _ => invalid = Some(format!("{key} must be greater than 0 and at most 1")),
            },
            "summaryPrompt" | "summary_prompt" => match setting.as_str() {
                Some(prompt) if prompt.contains("{messages}") => {
                    config = config.summary_prompt(prompt);
                }
                Some(_) => {
                    invalid = Some(format!("{key} must contain the {{messages}} placeholder"));
                }
                None => invalid = Some(format!("{key} must be a string")),
            },
            "flushBeforeCompact" | "flush_before_compact" => match setting.as_bool() {
                Some(enabled) => config = config.flush_before_compact(enabled),
                None => invalid = Some(format!("{key} must be a boolean")),
            },
            other => invalid = Some(format!("Unknown compaction field: {other}")),
        }
        if invalid.is_some() {
            break;
        }
    }
    if let Some(message) = invalid {
        tracing::warn!("subagent '{name}': ignoring invalid compaction config: {message}");
        return None;
    }
    let built = config.build();
    if built.keep_tokens_min > built.keep_tokens_max {
        tracing::warn!("subagent '{name}': ignoring invalid compaction config: keepTokensMin must not exceed keepTokensMax");
        return None;
    }
    Some(CompactionSetting::Config(built))
}

/// Java `compactionInt`: a 32-bit integer, `>= 0` (or `>= -1` for `keepTokens`).
fn compaction_int(key: &str, value: &serde_json::Value) -> Result<i64, String> {
    let number = value
        .as_i64()
        .or_else(|| value.as_f64().filter(|n| n.fract() == 0.0).map(|n| n as i64))
        .ok_or_else(|| format!("{key} must be a 32-bit integer"))?;
    let Ok(setting) = i32::try_from(number) else {
        return Err(format!("{key} must be a 32-bit integer"));
    };
    // Only `keepTokens` accepts -1 (Java: "no cap"); every other field is a plain count.
    let minimum = if key == "keepTokens" || key == "keep_tokens" {
        -1
    } else {
        0
    };
    if (setting as i64) < minimum {
        return Err(format!("{key} must be at least {minimum}"));
    }
    Ok(setting as i64)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses `compaction: <value>` front matter, mirroring Java's
    /// `loader_distinguishesInheritedEnabledAndDisabledCompaction`.
    fn parse_compaction(value: &str) -> Option<SubagentDeclaration> {
        parse(
            &format!("---\ndescription: worker\ncompaction: {value}\n---\nbody"),
            "worker",
            None,
        )
    }

    #[test]
    fn loader_parses_compaction_configuration() {
        let decl = parse(
            "---\ndescription: Custom compaction\ncompaction:\n  triggerMessages: 9\n  \
             triggerTokens: 12000\n  reserved: 1000\n  keepMessages: 3\n  keepTokens: 0\n  \
             keepTokensMin: 500\n  keepTokensMax: 1500\n  keepTokensRatio: 0.2\n  \
             summaryPrompt: 'Summarize {messages}'\n  flushBeforeCompact: false\n---\nbody",
            "worker",
            None,
        )
        .expect("a valid declaration parses");
        let config = decl.compaction.expect("compaction config");
        assert_eq!(config.trigger_messages, 9);
        assert_eq!(config.trigger_tokens, 12_000);
        assert_eq!(config.reserved, 1_000);
        assert_eq!(config.keep_messages, 3);
        assert_eq!(config.keep_tokens, 0);
        assert_eq!(config.keep_tokens_min, 500);
        assert_eq!(config.keep_tokens_max, 1_500);
        assert_eq!(config.keep_tokens_ratio, 0.2);
        assert_eq!(config.summary_prompt, "Summarize {messages}");
        assert!(!config.flush_before_compact);
        assert!(!decl.disable_compaction);
    }

    #[test]
    fn loader_distinguishes_inherited_enabled_and_disabled_compaction() {
        let inherited = parse_compaction("null").expect("null inherits");
        assert!(inherited.compaction.is_none());
        assert!(!inherited.disable_compaction);

        let enabled = parse_compaction("true").expect("true enables");
        assert!(enabled.compaction.is_some());
        assert!(!enabled.disable_compaction);

        let disabled = parse_compaction("false").expect("false disables");
        assert!(disabled.compaction.is_none());
        assert!(disabled.disable_compaction);

        let empty = parse_compaction("{}").expect("a mapping enables");
        assert!(empty.compaction.is_some());
        assert!(!empty.disable_compaction);
    }

    #[test]
    fn loader_ignores_malformed_compaction_without_dropping_the_agent() {
        for value in [
            "oops",
            "[]",
            "{unknown: 1}",
            "{keepTokens: 1.5}",
            "{keepTokens: 4294967296}",
            "{flushBeforeCompact: nope}",
            "{summaryPrompt: 42}",
            "{summaryPrompt: 'Summarize the conversation'}",
            "{keepMessages: -1}",
            "{triggerMessages: -1}",
            "{triggerTokens: -1}",
            "{reserved: -1}",
            "{keepTokens: -2}",
            "{keepTokensMin: -1}",
            "{keepTokensMax: -1}",
            "{keepTokensMin: 9000, keepTokensMax: 2000}",
            "{keepTokensRatio: 0}",
            "{keepTokensRatio: -0.1}",
            "{keepTokensRatio: 1.1}",
        ] {
            let decl = parse_compaction(value).unwrap_or_else(|| panic!("{value} must parse"));
            assert_eq!(decl.name, "worker", "{value}");
            assert!(decl.compaction.is_none(), "{value}");
            assert!(!decl.disable_compaction, "{value}");
        }
    }

    #[test]
    fn loader_supports_disable_compaction_flag() {
        let decl = parse(
            "---\ndescription: worker\ndisable_compaction: true\n---\nbody",
            "worker",
            None,
        )
        .expect("a valid declaration parses");
        assert!(decl.disable_compaction);
        assert!(decl.compaction.is_none());
    }

    #[test]
    fn declaration_without_compaction_inherits() {
        let decl = parse("---\ndescription: worker\n---\nbody", "worker", None)
            .expect("a valid declaration parses");
        assert!(decl.compaction.is_none());
        assert!(!decl.disable_compaction);
    }
}
