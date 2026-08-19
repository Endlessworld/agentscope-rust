//! Resolves the absolute `filesRoot` path emitted in `<available_skills>` and
//! `load_skill_through_path` responses given a skill's `StageResult` and the
//! current shell mode, mirroring `ShellPathPolicy`.
//!
//! Three modes: no shell (returns `None` for every skill), sandbox (paths under
//! the sandbox-internal `/workspace/` prefix), and local-with-shell (paths
//! absolute on the host).
use std::path::PathBuf;

use super::stager::StageResult;

/// Default sandbox workspace prefix.
pub const SANDBOX_WORKSPACE_PREFIX: &str = "/workspace";

/// Shell availability mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    NoShell,
    Sandbox,
    LocalWithShell,
}

/// Resolves shell-reachable files roots for staged skills.
#[derive(Debug, Clone)]
pub struct ShellPathPolicy {
    mode: Mode,
    workspace_root: Option<PathBuf>,
    sandbox_prefix: String,
}

impl ShellPathPolicy {
    pub fn no_shell() -> Self {
        Self { mode: Mode::NoShell, workspace_root: None, sandbox_prefix: SANDBOX_WORKSPACE_PREFIX.to_string() }
    }

    pub fn sandbox() -> Self {
        Self::sandbox_with_prefix(SANDBOX_WORKSPACE_PREFIX)
    }

    pub fn sandbox_with_prefix(workspace_prefix: &str) -> Self {
        let prefix = if workspace_prefix.trim().is_empty() {
            SANDBOX_WORKSPACE_PREFIX.to_string()
        } else {
            workspace_prefix.to_string()
        };
        Self { mode: Mode::Sandbox, workspace_root: None, sandbox_prefix: prefix }
    }

    pub fn local_with_shell(workspace_root: PathBuf) -> Self {
        Self { mode: Mode::LocalWithShell, workspace_root: Some(workspace_root), sandbox_prefix: SANDBOX_WORKSPACE_PREFIX.to_string() }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Returns the absolute `filesRoot` for the given skill, or `None` when
    /// shell is unavailable / the skill has no shell-reachable representation.
    pub fn resolve(&self, skill_name: &str, stage: Option<&StageResult>) -> Option<String> {
        if self.mode == Mode::NoShell {
            return None;
        }
        let stage = stage?;
        match stage {
            StageResult::None => None,
            StageResult::WorkspaceNative => self.join_skills(skill_name),
            StageResult::Cached { source_namespace, skill_name: cached_name } => {
                self.join_cache(source_namespace, cached_name)
            }
        }
    }

    fn join_skills(&self, skill_name: &str) -> Option<String> {
        match self.mode {
            Mode::Sandbox => Some(escape_spaces(&format!("{}/skills/{skill_name}", self.sandbox_prefix))),
            Mode::LocalWithShell => {
                let root = self.workspace_root.as_ref()?;
                Some(escape_spaces(&root.join("skills").join(skill_name).to_string_lossy().replace('\\', "/")))
            }
            Mode::NoShell => None,
        }
    }

    fn join_cache(&self, source_ns: &str, skill_name: &str) -> Option<String> {
        match self.mode {
            Mode::Sandbox => Some(escape_spaces(&format!(
                "{}/{}/{source_ns}/{skill_name}",
                self.sandbox_prefix,
                super::stager::CACHE_DIR,
            ))),
            Mode::LocalWithShell => {
                let root = self.workspace_root.as_ref()?;
                Some(escape_spaces(&root
                    .join(super::stager::CACHE_DIR)
                    .join(source_ns)
                    .join(skill_name)
                    .to_string_lossy()
                    .replace('\\', "/")))
            }
            Mode::NoShell => None,
        }
    }
}

/// Escapes spaces with backslash (e.g. `"a b" -> "a\ b"`) so the value can be
/// embedded unquoted in POSIX shell commands without word-splitting.
pub fn escape_spaces(value: &str) -> String {
    value.replace(' ', "\\ ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached(ns: &str, name: &str) -> StageResult {
        StageResult::Cached { source_namespace: ns.to_string(), skill_name: name.to_string() }
    }

    #[test]
    fn no_shell_resolves_none() {
        let p = ShellPathPolicy::no_shell();
        assert_eq!(p.resolve("data", Some(&cached("x", "data"))), None);
        assert_eq!(p.resolve("data", Some(&StageResult::WorkspaceNative)), None);
    }

    #[test]
    fn sandbox_workspace_native() {
        let p = ShellPathPolicy::sandbox();
        assert_eq!(
            p.resolve("data", Some(&StageResult::WorkspaceNative)),
            Some("/workspace/skills/data".to_string())
        );
    }

    #[test]
    fn sandbox_cached_joins_under_cache() {
        let p = ShellPathPolicy::sandbox();
        let out = p.resolve("data", Some(&cached("market", "data"))).unwrap();
        assert!(out.starts_with("/workspace/.skills-cache/market/data"));
    }

    #[test]
    fn local_with_shell_uses_abs_path() {
        let root = PathBuf::from("/home/user/ws");
        let p = ShellPathPolicy::local_with_shell(root);
        let out = p.resolve("data", Some(&StageResult::WorkspaceNative)).unwrap();
        assert_eq!(out, "/home/user/ws/skills/data");
    }

    #[test]
    fn spaces_are_escaped() {
        assert_eq!(escape_spaces("a b"), "a\\ b");
        assert_eq!(escape_spaces("abc"), "abc");
    }
}
