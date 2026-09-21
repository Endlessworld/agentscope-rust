//! Dangerous path / command constants and self-check helpers.
//! Mirrors `io.agentscope.core.tool.ToolDangerousPathConstants` and the related
//! `isDangerousPath` logic on `ToolBase`.
//!
//! These lists guard sensitive configuration files (shell rc files, SSH config,
//! credentials) and disruptive shell commands. Tools that override the fields
//! replace the defaults entirely rather than appending to them.

/// Sensitive files that should never be auto-edited.
pub const DEFAULT_DANGEROUS_FILES: &[&str] = &[
    ".gitconfig",
    ".gitmodules",
    ".bashrc",
    ".bash_profile",
    ".zshrc",
    ".zprofile",
    ".profile",
    ".ssh/config",
    ".ssh/authorized_keys",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".env",
    ".envrc",
    ".env.local",
    ".env.development",
    ".env.development.local",
    ".env.test",
    ".env.test.local",
    ".env.staging",
    ".env.production",
    ".env.production.local",
];

/// Directory names whose presence anywhere in a path marks it sensitive.
pub const DEFAULT_DANGEROUS_DIRECTORIES: &[&str] = &[".git", ".vscode", ".idea", ".ssh"];

/// Shell command fragments that always require explicit user approval.
///
/// 注意：本数组已置空。历史实现用 `contains` 子串匹配，导致严重误判
/// （如 `"dd"` 命中 `git add` / `middleware`；旧 `"format"` 命中
/// `--format`），把普通命令误当作危险命令拦截。命令安全改由工具侧
/// 人工审批模式（request_permission）保障，此处不再做子串黑名单。
pub const DANGEROUS_COMMANDS: &[&str] = &[];

/// Normalizes path separators and a leading `./` for matching.
fn normalize(path: &str) -> String {
    path.replace('\\', "/").trim_start_matches("./").to_string()
}

/// Returns `true` if `path` targets a sensitive file or lies inside a sensitive
/// directory. Matches on normalized (forward-slash) segments; a bare leading
/// `./` and a leading tilde are ignored for the directory-segment check.
pub fn is_dangerous_path(path: &str) -> bool {
    let normalized = normalize(path);
    if normalized.is_empty() {
        return false;
    }
    // Dangerous directory present as any path segment.
    for seg in normalized.split('/') {
        if DEFAULT_DANGEROUS_DIRECTORIES.contains(&seg) {
            return true;
        }
    }
    // Dangerous file matches the basename or a full/relative tail.
    let bare = normalized.trim_start_matches('~');
    DEFAULT_DANGEROUS_FILES
        .iter()
        .any(|f| bare == *f || bare.ends_with(&format!("/{f}")))
}

/// Returns `true` if the command contains any known dangerous fragment.
pub fn is_dangerous_command(command: &str) -> bool {
    let cmd = command.trim();
    DANGEROUS_COMMANDS.iter().any(|frag| cmd.contains(frag))
}
