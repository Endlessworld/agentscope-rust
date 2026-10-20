//! Argument parsing and output shaping for the `grep_files` tool.
//!
//! `Filesystem::grep` only takes `(pattern, path, glob)`; the flags rg exposes on top
//! of that (`-i`, `-c`, `-l`, `-t`, multiple paths) are resolved here so the
//! `Filesystem` trait stays untouched.

use agentscope_core::error::{Error, Result};
use agentscope_core::message::JsonMap;

use crate::filesystem::GrepMatch;

/// Extensions per `-t` type name. Deliberately small: only names a coding agent
/// realistically filters on.
const TYPES: &[(&str, &[&str])] = &[
    ("rust", &["rs"]),
    ("python", &["py", "pyi"]),
    ("js", &["js", "mjs", "cjs", "jsx"]),
    ("ts", &["ts", "tsx", "mts", "cts"]),
    ("go", &["go"]),
    ("java", &["java"]),
    ("kotlin", &["kt", "kts"]),
    ("c", &["c", "h"]),
    ("cpp", &["cc", "cpp", "cxx", "hpp", "hh"]),
    ("csharp", &["cs"]),
    ("ruby", &["rb"]),
    ("php", &["php"]),
    ("markdown", &["md", "markdown"]),
    ("json", &["json"]),
    ("toml", &["toml"]),
    ("yaml", &["yml", "yaml"]),
    ("xml", &["xml"]),
    ("html", &["html", "htm"]),
    ("css", &["css"]),
    ("sql", &["sql"]),
    ("shell", &["sh", "bash", "zsh"]),
];

/// Builds the regex passed to `Filesystem::grep`, folding in `ignore_case`.
pub(crate) fn pattern(args: &JsonMap) -> Result<String> {
    let raw = args
        .get("pattern")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Tool("Missing string argument: pattern".into()))?;
    match args.get("ignore_case").and_then(|v| v.as_bool()) {
        Some(true) if !raw.starts_with("(?i)") => Ok(format!("(?i){raw}")),
        _ => Ok(raw.to_string()),
    }
}

/// Resolves the `glob` / `type` arguments into a single glob filter.
pub(crate) fn glob(args: &JsonMap) -> Result<Option<String>> {
    let glob = args.get("glob").and_then(|v| v.as_str());
    let ty = args.get("type").and_then(|v| v.as_str());
    match (glob, ty) {
        (Some(_), Some(_)) => Err(Error::Tool(
            "Specify either glob or type, not both".into(),
        )),
        (Some(g), None) => Ok(Some(g.to_string())),
        (None, Some(t)) => {
            let exts = TYPES
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(t.trim()))
                .map(|(_, exts)| *exts)
                .ok_or_else(|| {
                    let known = TYPES.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ");
                    Error::Tool(format!("Unknown type: {t}. Known types: {known}"))
                })?;
            Ok(Some(match exts {
                [single] => format!("*.{single}"),
                _ => {
                    let braces = exts.join(",");
                    format!("*.{{{braces}}}")
                }
            }))
        }
        (None, None) => Ok(None),
    }
}

/// Search roots: `paths` when present, otherwise `path`, otherwise the fs root.
pub(crate) fn targets(args: &JsonMap) -> Vec<String> {
    let from_list = args
        .get("paths")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !from_list.is_empty() {
        return from_list;
    }
    match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => vec![p.to_string()],
        None => vec!["/".to_string()],
    }
}

/// Renders matches according to `output_mode` (`content` | `files_with_matches` | `count`).
pub(crate) fn format_matches(matches: &[GrepMatch], limit: usize, mode: &str) -> String {
    if matches.is_empty() {
        return "No matches found".to_string();
    }
    match mode {
        "files_with_matches" => {
            let mut paths: Vec<&str> = matches.iter().map(|m| m.path.as_str()).collect();
            paths.dedup();
            let total = paths.len();
            let shown = paths.iter().take(limit).copied().collect::<Vec<_>>();
            super::filesystem_tools::append_truncation_notice(
                shown.join("\n"),
                total,
                limit,
                "files",
            )
        }
        "count" => {
            let mut counts: Vec<(&str, usize)> = Vec::new();
            for m in matches {
                match counts.last_mut() {
                    Some((path, n)) if *path == m.path => *n += 1,
                    _ => counts.push((m.path.as_str(), 1)),
                }
            }
            let total = counts.len();
            let lines = counts
                .iter()
                .take(limit)
                .map(|(path, n)| format!("{path}:{n}"))
                .collect::<Vec<_>>()
                .join("\n");
            super::filesystem_tools::append_truncation_notice(lines, total, limit, "files")
        }
        _ => {
            let total = matches.len();
            let lines = matches
                .iter()
                .take(limit)
                .map(|m| format!("{}:{}:{}", m.path, m.line, m.text))
                .collect::<Vec<_>>()
                .join("\n");
            super::filesystem_tools::append_truncation_notice(lines, total, limit, "matches")
        }
    }
}
