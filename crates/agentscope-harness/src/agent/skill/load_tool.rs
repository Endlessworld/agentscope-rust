//! `load_skill_through_path` tool, mirroring `SkillLoadTool`. Resolves a skill id
//! to its on-disk directory and reads a file inside it (defaults to `SKILL.md`).
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;

use agentscope_core::error::Result;
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext};
use async_trait::async_trait;
use serde_json::json;

use super::model::SkillCatalogSnapshot;

/// Tool that loads a skill's documentation or resource by `skillId`.
pub struct LoadSkillTool {
    index: HashMap<String, PathBuf>,
}

impl LoadSkillTool {
    /// Canonical tool name.
    pub const TOOL_NAME: &'static str = "load_skill_through_path";

    pub fn new(index: HashMap<String, PathBuf>) -> Self {
        Self { index }
    }

    /// Convenience builder from a list of `(skill_id, dir)` pairs.
    pub fn from_entries(entries: Vec<(String, PathBuf)>) -> Self {
        Self {
            index: entries.into_iter().collect(),
        }
    }

    /// Builds a tool restricted to the given catalog snapshot (skills without a
    /// files root cannot be loaded).
    pub fn from_snapshot(snapshot: &SkillCatalogSnapshot) -> Self {
        Self {
            index: snapshot
                .files_roots
                .iter()
                .map(|(id, root)| (id.clone(), PathBuf::from(root)))
                .collect(),
        }
    }
}

#[async_trait]
impl Tool for LoadSkillTool {
    fn name(&self) -> &str {
        Self::TOOL_NAME
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
        json!({
            "type": "object",
            "properties": {
                "skillId": {"type": "string", "description": "The skill id, e.g. <name>_<source> as shown in <available_skills>"},
                "path": {"type": "string", "description": "Relative path inside the skill directory (default: SKILL.md)"}
            },
            "required": ["skillId"]
        })
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let skill_id = args
            .get("skillId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                agentscope_core::Error::Tool("Missing string argument: skillId".into())
            })?;
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
            return Ok(ToolResultBlock::error(
                "path must be a relative path within the skill; '..' and absolute paths are not allowed.",
            ));
        }
        let target = dir.join(&rel_normalized);
        if !target.starts_with(dir) {
            return Ok(ToolResultBlock::error("path escapes the skill directory."));
        }
        if target.is_dir() {
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
        let byte_len = bytes.len();
        match String::from_utf8(bytes) {
            Ok(text) => Ok(ToolResultBlock::text(text)),
            Err(_) => Ok(ToolResultBlock::text(format!(
                "[binary file {rel_normalized} in skill '{skill_id}', {} bytes — use the shell/filesystem tools to process it]",
                byte_len
            ))),
        }
    }
}

/// 惰性 `load_skill_through_path` 工具：持有 [`HarnessSkillMiddleware`] 的 Arc 引用，
/// 首次被调用时才构建技能索引（`load_index` 内部惰性 + 缓存），
/// 从而把启动时的全量磁盘扫描推迟到第一次实际加载技能时（嵌入式内存优化）。
pub struct LazyLoadSkillTool {
    middleware: std::sync::Arc<crate::agent::middleware::HarnessSkillMiddleware>,
}

impl LazyLoadSkillTool {
    pub fn new(
        middleware: std::sync::Arc<crate::agent::middleware::HarnessSkillMiddleware>,
    ) -> Self {
        Self { middleware }
    }
}

#[async_trait]
impl Tool for LazyLoadSkillTool {
    fn name(&self) -> &str {
        LoadSkillTool::TOOL_NAME
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
        json!({
            "type": "object",
            "properties": {
                "skillId": {"type": "string", "description": "The skill id, e.g. <name>_<source> as shown in <available_skills>"},
                "path": {"type": "string", "description": "Relative path inside the skill directory (default: SKILL.md)"}
            },
            "required": ["skillId"]
        })
    }

    async fn call(&self, args: JsonMap, ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let index = self.middleware.load_index();
        LoadSkillTool::new(index).call(args, ctx).await
    }
}
