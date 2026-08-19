//! Normalizes file paths to workspace-relative form by stripping the active
//! mode's workspace prefix, mirroring `WorkspacePathNormalizer`.
//!
//! Only the prefix matching the current filesystem mode is registered, so there
//! is no risk of a sandbox prefix (`/workspace/`) accidentally matching a real
//! host directory in local mode, or vice versa. Paths that don't match any
//! registered prefix pass through unchanged.

/// Normalizes paths by stripping registered workspace prefixes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePathNormalizer {
    prefixes: Vec<String>,
}

impl WorkspacePathNormalizer {
    /// Creates a normalizer that strips the given single prefix.
    pub fn of(workspace_prefix: &str) -> Self {
        Self::of_many(std::slice::from_ref(&workspace_prefix))
    }

    /// Creates a normalizer that tries multiple prefixes in order. Use only when
    /// the active mode has more than one valid prefix (e.g. local-with-shell where
    /// both project dir and workspace dir are valid roots).
    pub fn of_many(workspace_prefixes: &[&str]) -> Self {
        let mut prefixes = Vec::new();
        for p in workspace_prefixes {
            let trimmed = trim_trailing_slash(p);
            if !trimmed.is_empty() {
                prefixes.push(trimmed);
            }
        }
        Self { prefixes }
    }

    /// Normalize a path to workspace-relative form by stripping the prefix,
    /// or return the original path when no registered prefix matched.
    pub fn normalize(&self, path: &str) -> String {
        if path.trim().is_empty() {
            return path.to_string();
        }
        for prefix in &self.prefixes {
            if let Some(stripped) = try_strip(path, prefix) {
                return stripped;
            }
        }
        path.to_string()
    }
}

fn try_strip(path: &str, prefix: &str) -> Option<String> {
    let normalized_path = path.replace('\\', "/");
    let normalized_prefix = prefix.replace('\\', "/");
    if normalized_path.starts_with(&format!("{normalized_prefix}/")) {
        let rest = normalized_path[normalized_prefix.len() + 1..].to_string();
        return Some(if rest.is_empty() { ".".to_string() } else { rest });
    }
    if normalized_path == normalized_prefix {
        return Some(".".to_string());
    }
    None
}

fn trim_trailing_slash(s: &str) -> String {
    let mut t = s.replace('\\', "/");
    while t.len() > 1 && t.ends_with('/') {
        t.pop();
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_sandbox_prefix() {
        let n = WorkspacePathNormalizer::of("/workspace");
        assert_eq!(n.normalize("/workspace/skills/data/run.py"), "skills/data/run.py");
    }

    #[test]
    fn prefix_root_maps_to_dot() {
        let n = WorkspacePathNormalizer::of("/workspace");
        assert_eq!(n.normalize("/workspace"), ".");
        assert_eq!(n.normalize("/workspace/"), ".");
    }

    #[test]
    fn unmatched_path_passes_through() {
        let n = WorkspacePathNormalizer::of("/workspace");
        assert_eq!(n.normalize("/etc/hosts"), "/etc/hosts");
        assert_eq!(n.normalize("relative/path"), "relative/path");
    }

    #[test]
    fn windows_separators_normalized() {
        let n = WorkspacePathNormalizer::of("C:\\ws");
        assert_eq!(n.normalize("C:\\ws\\a\\b.txt"), "a/b.txt");
    }

    #[test]
    fn multiple_prefixes_are_tried_in_order() {
        let n = WorkspacePathNormalizer::of_many(&["/ws", "/workspace"]);
        assert_eq!(n.normalize("/ws/x"), "x");
        assert_eq!(n.normalize("/workspace/y"), "y");
    }
}
