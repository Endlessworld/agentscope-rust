//! Persistent "always allow / always reject" permission rules.
//!
//! A user picking `ALLOW_ALWAYS` / `REJECT_ALWAYS` in an ACP permission prompt
//! records a [`PermissionRule`] here so the decision survives session restarts
//! and applies to future sessions opened in the same workspace.
//!
//! Rules are stored per working directory at `{cwd}/.agentscope/permissions.json`
//! (mirrors the `PermissionSettings` utility of the ai-agents project).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use agentscope_core::PermissionRule;

/// Workspace-relative location of the persisted user permission rules.
const DEFAULT_RELATIVE_PATH: &str = ".agentscope/permissions.json";

/// Loads and appends user-selected permission rules for one working directory.
#[derive(Debug, Clone)]
pub struct PermissionRuleStore {
    file: PathBuf,
}

impl PermissionRuleStore {
    /// Creates a store rooted at `dir` (`{dir}/.agentscope/permissions.json`).
    pub fn at_dir(dir: impl AsRef<Path>) -> Self {
        Self {
            file: dir.as_ref().join(DEFAULT_RELATIVE_PATH),
        }
    }

    /// The backing file of this store.
    pub fn path(&self) -> &Path {
        &self.file
    }

    /// Loads all persisted rules. A missing or unreadable file yields an empty
    /// list rather than an error.
    pub fn load(&self) -> Vec<PermissionRule> {
        fs::read_to_string(&self.file)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Adds `rule`, removing any rule identical in
    /// (tool, rule content, behavior, source) first. Writes atomically through
    /// a temp file + rename so concurrent readers never observe a torn file.
    pub fn add(&self, rule: PermissionRule) -> io::Result<()> {
        let mut rules = self.load();
        rules.retain(|r| !same_rule(r, &rule));
        rules.push(rule);

        let data = serde_json::to_string_pretty(&rules).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("serialize rules: {e}"))
        })?;

        if let Some(parent) = self.file.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = self.file.with_extension("json.tmp");
        fs::write(&tmp, data)?;
        fs::rename(&tmp, &self.file)?;
        Ok(())
    }
}

fn same_rule(a: &PermissionRule, b: &PermissionRule) -> bool {
    a.tool_name == b.tool_name
        && a.rule_content == b.rule_content
        && a.behavior == b.behavior
        && a.source == b.source
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::{PermissionBehavior, PermissionRule};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agentscope-permission-store-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn add_then_load_roundtrips() {
        let dir = temp_dir("roundtrip");
        let store = PermissionRuleStore::at_dir(&dir);
        assert!(store.load().is_empty());

        let rule = PermissionRule::new(
            "execute",
            Some(r#"{"command":"npm install"}"#.to_string()),
            PermissionBehavior::Allow,
            "user-permission",
        );
        store.add(rule.clone()).unwrap();

        let loaded = store.load();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].tool_name, "execute");
        assert_eq!(loaded[0].behavior, PermissionBehavior::Allow);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn adding_identical_rule_dedupes() {
        let dir = temp_dir("dedupe");
        let store = PermissionRuleStore::at_dir(&dir);

        let rule =
            PermissionRule::new("execute", None, PermissionBehavior::Deny, "user-permission");
        store.add(rule.clone()).unwrap();
        store.add(rule.clone()).unwrap();

        assert_eq!(store.load().len(), 1, "duplicate rules must not accumulate");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_loads_empty() {
        let dir = temp_dir("missing");
        let store = PermissionRuleStore::at_dir(dir.join("nonexistent"));
        assert!(store.load().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
