//! Workspace-spec bridging helpers shared by the remote backends.
//!
//! A [`crate::WorkspaceSpec`]-equivalent (harness [`WorkspaceSpec`]) must be projected
//! onto each remote filesystem using shell commands, because the remote APIs only expose
//! exec + file transfer. These pure helpers build the shell command sequences and
//! resolve remote paths; they are unit-testable without any network access.

use std::collections::BTreeMap;

use agentscope_harness::agent::sandbox::{ExecResult, WorkspaceSpec};

/// Resolves a workspace-relative path into an absolute remote path.
///
/// Mirrors `WorkspaceSpec.resolve` but normalizes redundant slashes and rejects
/// empty/absolute relative inputs by returning the root unchanged only for `"."`.
pub fn resolve_remote_path(root: &str, relative: &str) -> String {
    let rel = relative.trim();
    if rel.is_empty() || rel == "." {
        return root.trim_end_matches('/').to_string();
    }
    if rel.starts_with('/') {
        return rel.to_string();
    }
    format!("{}/{}", root.trim_end_matches('/'), rel.trim_start_matches('/'))
}

/// Quotes a path for safe interpolation into a POSIX shell command.
pub fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@%_+=:,./-".contains(c))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

/// Builds the shell command that materializes a workspace spec on the remote host.
///
/// The generated script:
/// 1. `mkdir -p` the workspace root (and any declared directories),
/// 2. writes each file via a heredoc into `cat > path`,
/// 3. `cd`s nowhere — the applier returns the commands so the caller can chain `cd`.
///
/// The result is deterministic for a given spec so it can be asserted in tests.
pub fn build_workspace_setup_script(spec: &WorkspaceSpec) -> String {
    let root = spec.root.to_string_lossy().replace('\\', "/");
    let mut lines: Vec<String> = Vec::new();
    lines.push("set -e".to_string());
    lines.push(format!("mkdir -p {}", shell_quote(&root)));

    for dir in &spec.directories {
        let path = resolve_remote_path(&root, dir);
        lines.push(format!("mkdir -p {}", shell_quote(&path)));
    }

    if let Some(cwd) = &spec.working_directory {
        let path = resolve_remote_path(&root, cwd);
        lines.push(format!("mkdir -p {}", shell_quote(&path)));
    }

    // BTreeMap iteration order is stable -> deterministic script.
    let files: BTreeMap<&String, &String> = spec.files.iter().collect();
    for (rel, content) in files {
        let path = resolve_remote_path(&root, rel);
        if let Some(parent) = parent_dir(&path) {
            lines.push(format!("mkdir -p {}", shell_quote(&parent)));
        }
        let delimiter = format!("AGENTSCOPE_EOF_{}", short_hash(rel));
        lines.push(format!(
            "cat > {} <<'{}'\n{}\n{}",
            shell_quote(&path),
            delimiter,
            content.trim_end_matches('\n'),
            delimiter
        ));
    }

    lines.join("\n")
}

/// Extracts the parent directory of an absolute POSIX path.
pub fn parent_dir(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => None,
        Some(idx) => Some(trimmed[..idx].to_string()),
        None => None,
    }
}

/// A tiny stable hash used to derive unique heredoc delimiters (FNV-1a).
pub fn short_hash(value: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in value.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Wraps a remote exec result into command output diagnostics on failure.
///
/// Returns `Ok(())` when the command exited 0; otherwise produces an
/// [`crate::SandboxExtensionError`] carrying the combined output.
pub fn ensure_success(
    context: &str,
    result: &ExecResult,
) -> std::result::Result<(), crate::SandboxExtensionError> {
    if result.is_success() {
        Ok(())
    } else {
        Err(crate::SandboxExtensionError::Runtime(
            crate::error::SandboxErrorCode::CommandExecutionError,
            format!(
                "{context} failed with exit code {}: {}",
                result.exit_code,
                result.combined_output()
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_relative_paths() {
        assert_eq!(resolve_remote_path("/workspace", "src/main.rs"), "/workspace/src/main.rs");
        assert_eq!(resolve_remote_path("/workspace/", "./file.txt"), "/workspace/./file.txt");
        assert_eq!(resolve_remote_path("/workspace", "."), "/workspace");
        assert_eq!(resolve_remote_path("/workspace", "/abs/path"), "/abs/path");
    }

    #[test]
    fn shell_quote_escapes_spaces_and_quotes() {
        assert_eq!(shell_quote("simple"), "simple");
        assert_eq!(shell_quote("with space"), "'with space'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn parent_dir_extraction() {
        assert_eq!(parent_dir("/workspace/a/b.txt"), Some("/workspace/a".to_string()));
        assert_eq!(parent_dir("/file.txt"), None);
        assert_eq!(parent_dir("file.txt"), None);
    }

    #[test]
    fn setup_script_is_deterministic_and_complete() {
        let spec = WorkspaceSpec::new("/workspace")
            .with_directory("src")
            .with_file("README.md", "# Title")
            .with_file("src/main.rs", "fn main() {}")
            .with_working_directory("src");
        let script = build_workspace_setup_script(&spec);
        assert!(script.starts_with("set -e"));
        assert!(script.contains("mkdir -p /workspace"));
        assert!(script.contains("mkdir -p /workspace/src"));
        assert!(script.contains("cat > /workspace/README.md"));
        assert!(script.contains("cat > /workspace/src/main.rs"));
        // Deterministic across calls.
        assert_eq!(script, build_workspace_setup_script(&spec));
    }

    #[test]
    fn short_hash_is_stable() {
        assert_eq!(short_hash("abc"), short_hash("abc"));
        assert_ne!(short_hash("abc"), short_hash("abd"));
    }
}
