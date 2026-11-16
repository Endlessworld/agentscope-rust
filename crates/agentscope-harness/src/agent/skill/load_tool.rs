//! `load_skill_through_path` tool, mirroring `SkillLoadTool`. Resolves a skill id
//! to its on-disk directory and reads a file inside it (defaults to `SKILL.md`).
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use agentscope_core::error::Result;
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext};
use async_trait::async_trait;
use serde_json::json;

use super::model::SkillCatalogSnapshot;

/// Upper bound on tracked load receipts. Process-wide; a long-lived host serves many sessions
/// from one process, so the set needs a ceiling (FIFO eviction).
const LOAD_RECEIPT_CAPACITY: usize = 4096;

/// `agent|session|skill|file` receipts of text already returned in this process.
static LOAD_RECEIPTS: LazyLock<Mutex<LoadReceipts>> =
    LazyLock::new(|| Mutex::new(LoadReceipts::default()));

#[derive(Default)]
struct LoadReceipts {
    seen: HashSet<String>,
    order: VecDeque<String>,
}

impl LoadReceipts {
    /// Records `key`; `true` when it had not been recorded yet.
    fn insert(&mut self, key: String) -> bool {
        if !self.seen.insert(key.clone()) {
            return false;
        }
        self.order.push_back(key);
        while self.order.len() > LOAD_RECEIPT_CAPACITY {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        true
    }
}

/// Records one successful text read and reports whether it is new for this agent + session.
///
/// A repeated `load_skill_through_path` for the same file used to re-emit the whole body
/// (2000+ tokens); the second read now answers with a short notice (#1569). The key carries the
/// resolved file, so a different resource of the same skill still reads in full.
fn note_loaded(ctx: &ToolExecutionContext, skill_id: &str, target: &Path) -> bool {
    let field = |key: &str| {
        ctx.runtime
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let key = format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}",
        field("agentId"),
        field("sessionId"),
        skill_id,
        target.display()
    );
    LOAD_RECEIPTS
        .lock()
        .map(|mut guard| guard.insert(key))
        .unwrap_or(true)
}

/// Tool that loads a skill's documentation or resource by `skillId`.
pub struct LoadSkillTool {
    index: HashMap<String, PathBuf>,
    /// `skillId -> skill name`, so a name-based grant (one entry covering every copy) can be
    /// applied to a load request that only carries an id.
    names: HashMap<String, String>,
}

impl LoadSkillTool {
    /// Canonical tool name.
    pub const TOOL_NAME: &'static str = "load_skill_through_path";

    pub fn new(index: HashMap<String, PathBuf>) -> Self {
        Self {
            index,
            names: HashMap::new(),
        }
    }

    /// Attaches the `skillId -> name` map used by the capability gate.
    pub fn with_names(mut self, names: HashMap<String, String>) -> Self {
        self.names = names;
        self
    }

    /// Convenience builder from a list of `(skill_id, dir)` pairs.
    pub fn from_entries(entries: Vec<(String, PathBuf)>) -> Self {
        Self::new(entries.into_iter().collect())
    }

    /// Builds a tool restricted to the given catalog snapshot (skills without a
    /// files root cannot be loaded).
    pub fn from_snapshot(snapshot: &SkillCatalogSnapshot) -> Self {
        Self::new(
            snapshot
                .files_roots
                .iter()
                .map(|(id, root)| (id.clone(), PathBuf::from(root)))
                .collect(),
        )
    }

    /// Whether the call's capability surface grants this skill.
    ///
    /// A missing surface means the call never entered an agent funnel: direct execution is
    /// trusted assembler code, the same stance as the executor gate.
    fn granted(&self, skill_id: &str, ctx: &ToolExecutionContext) -> bool {
        let Some(surface) = ctx.capability_surface() else {
            return true;
        };
        let name = self.names.get(skill_id).map(String::as_str).unwrap_or("");
        surface.grants_skill(name, Some(skill_id))
    }

    /// The refusal for a skill that exists but is not granted to this caller.
    ///
    /// Distinct from [`Self::unknown_skill`] on purpose: a bare "not found" made a missing grant
    /// look like a typo, so the model retried ids instead of reporting a capability gap (#2169).
    fn ungranted_skill(&self, skill_id: &str) -> String {
        format!("Skill '{skill_id}' is not granted to this caller.")
    }

    /// The not-found answer for an id no repository published. Only granted skills are listed.
    fn unknown_skill(&self, skill_id: &str, ctx: &ToolExecutionContext) -> String {
        let mut available: Vec<&String> = self
            .index
            .keys()
            .filter(|id| self.granted(id, ctx))
            .collect();
        available.sort();
        let listed = available
            .into_iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        format!("Unknown skillId '{skill_id}'. Available skills: {listed}")
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

    /// Framework meta tool: a restricted surface must not revoke the load tool itself; the
    /// per-skill grant is enforced inside [`Tool::call`]. Mirrors Java `SkillLoadTool#isSystem`.
    fn is_system(&self) -> bool {
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

        // An id no repository published is a not-found answer whatever the grants say: the
        // grant check below only tells an authorization gap from a typo for skills that exist
        // (#2169).
        let Some(dir) = self.index.get(&skill_id) else {
            return Ok(ToolResultBlock::error(self.unknown_skill(&skill_id, &ctx)));
        };

        // Capability gate (defense in depth on top of the grant-filtered catalog): a skill the
        // caller is not granted is refused as an authorization gap, not as a missing id. The
        // available list is the granted view only.
        if !self.granted(&skill_id, &ctx) {
            tracing::info!(
                "skill load denied by capability grant: skillId='{}'",
                skill_id
            );
            return Ok(ToolResultBlock::error(self.ungranted_skill(&skill_id)));
        }
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
            Ok(text) => {
                if !note_loaded(&ctx, &skill_id, &target) {
                    return Ok(ToolResultBlock::text(format!(
                        "Already loaded {rel_normalized} of skill '{skill_id}' in this session; \
                         content omitted (identical to the earlier read)."
                    )));
                }
                Ok(ToolResultBlock::text(text))
            }
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

    fn is_system(&self) -> bool {
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
        let names = self.middleware.load_name_index();
        LoadSkillTool::new(index)
            .with_names(names)
            .call(args, ctx)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::agent::{CapabilitySurface, CAPABILITY_SURFACE_RUNTIME_KEY};

    fn skill_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("asr_load_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "# body\n").unwrap();
        dir
    }

    fn tool(dir: &PathBuf) -> LoadSkillTool {
        LoadSkillTool::new(
            [("pdf_user".to_string(), dir.clone())]
                .into_iter()
                .collect(),
        )
        .with_names(
            [("pdf_user".to_string(), "pdf".to_string())]
                .into_iter()
                .collect(),
        )
    }

    fn ctx_granting(names: Option<&[&str]>) -> ToolExecutionContext {
        let mut ctx = ToolExecutionContext::default();
        if let Some(names) = names {
            let surface = CapabilitySurface::new(
                Some(names.iter().map(|n| n.to_string()).collect()),
                None,
                None,
            );
            if let Ok(value) = serde_json::to_value(&surface) {
                ctx.runtime
                    .insert(CAPABILITY_SURFACE_RUNTIME_KEY.to_string(), value);
            }
        }
        ctx
    }

    fn args(skill_id: &str) -> JsonMap {
        let mut map = JsonMap::new();
        map.insert("skillId".to_string(), json!(skill_id));
        map
    }

    fn text(block: &ToolResultBlock) -> String {
        block
            .output
            .iter()
            .filter_map(|b| match b {
                agentscope_core::message::ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[tokio::test]
    async fn an_ungranted_skill_is_refused_as_unauthorized() {
        let dir = skill_dir("grant");
        let tool = tool(&dir);

        // A granted name covers the copy from that source.
        let ok = tool
            .call(args("pdf_user"), ctx_granting(Some(&["pdf"])))
            .await
            .unwrap();
        assert!(text(&ok).contains("# body"), "{}", text(&ok));

        // Another name grants nothing here: the refusal names the missing grant (#2169) and
        // does not list a skill the caller may not use.
        let denied = tool
            .call(args("pdf_user"), ctx_granting(Some(&["docx"])))
            .await
            .unwrap();
        assert_eq!(
            text(&denied),
            "Error: Skill 'pdf_user' is not granted to this caller."
        );

        // An unknown id stays a not-found answer.
        let unknown = tool
            .call(args("nope"), ctx_granting(Some(&["docx"])))
            .await
            .unwrap();
        assert!(text(&unknown).starts_with("Error: Unknown skillId 'nope'. Available skills: "));

        // A call that never entered an agent funnel has no surface: trusted assembler code.
        // The load receipt is keyed by session, so a fresh one keeps this call from being
        // deduplicated against the successful load above.
        let mut direct_ctx = ctx_granting(None);
        direct_ctx.runtime.insert(
            "sessionId".to_string(),
            serde_json::Value::String("direct".to_string()),
        );
        let direct = tool.call(args("pdf_user"), direct_ctx).await.unwrap();
        assert!(text(&direct).contains("# body"), "{}", text(&direct));
    }

    #[tokio::test]
    async fn a_repeated_load_returns_a_short_notice_instead_of_the_body() {
        let dir = skill_dir("dedupe");
        let tool = tool(&dir);
        let mut ctx = ToolExecutionContext::default();
        ctx.runtime.insert("agentId".to_string(), json!("a1"));
        ctx.runtime.insert("sessionId".to_string(), json!("s1"));

        let first = tool.call(args("pdf_user"), ctx.clone()).await.unwrap();
        assert!(text(&first).contains("# body"), "{}", text(&first));

        let second = tool.call(args("pdf_user"), ctx.clone()).await.unwrap();
        let second_text = text(&second);
        assert!(second_text.contains("Already loaded"), "{second_text}");
        assert!(!second_text.contains("# body"), "{second_text}");

        // Another session shares nothing with the first.
        let mut other = ctx;
        other.runtime.insert("sessionId".to_string(), json!("s2"));
        let third = tool.call(args("pdf_user"), other).await.unwrap();
        assert!(text(&third).contains("# body"), "{}", text(&third));
    }
}
