//! Materialises non-workspace skill resources (Layer 1 / Layer 2 / marketplace)
//! to `<wsRoot>/.skills-cache/<source-ns>/<skill-name>/` so that shell-mode
//! agents can execute the staged scripts via absolute paths, mirroring
//! `MarketplaceStager`.
//!
//! The stager keeps no cross-call state: each invocation rebuilds the white-list
//! of directories that should remain under `.skills-cache`, materialises any
//! files whose SHA-256 has changed, and deletes orphan directories not present
//! in the white-list. Workspace-native skills (source repository is
//! `Workspace`) are NOT staged — they already live under `<wsRoot>/skills/`.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::Engine;
use sha2::{Digest, Sha256};

use super::model::{AgentSkill, SkillResources};

/// Cache directory (relative to the workspace root).
pub const CACHE_DIR: &str = ".skills-cache";
/// Namespace used for repositories with an empty source.
pub const GLOBAL_NAMESPACE: &str = "_global";

/// The repository that produced a skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillRepo {
    /// `WorkspaceSkillRepository` — skills already under `skills/`.
    Workspace,
    /// Any other repository, identified by its source string.
    External { source: String },
}

impl SkillRepo {
    pub fn source(&self) -> String {
        match self {
            SkillRepo::Workspace => String::new(),
            SkillRepo::External { source } => source.clone(),
        }
    }
}

/// Outcome of staging one skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageResult {
    /// No staging applied — skill has no shell-reachable representation.
    None,
    /// Skill comes from `WorkspaceSkillRepository` (already in `workspace/skills/`).
    WorkspaceNative,
    /// Skill staged under `.skills-cache/<sourceNs>/<skillName>/`.
    Cached { source_namespace: String, skill_name: String },
}

/// Pairs a skill with the repository that produced it.
#[derive(Debug, Clone)]
pub struct RepoBound {
    pub skill: AgentSkill,
    pub repo: SkillRepo,
}

impl RepoBound {
    pub fn new(skill: AgentSkill, repo: SkillRepo) -> Self {
        Self { skill, repo }
    }
}

/// Resolves per-repository source namespaces. When two repositories report the
/// same source, the second and subsequent ones receive an `_<idx>` suffix.
/// Workspace repositories consume no namespace slot.
pub fn resolve_source_namespaces(repos: &[SkillRepo]) -> Vec<String> {
    // Count occurrences of each effective source (excluding workspace).
    let mut count: BTreeMap<String, usize> = BTreeMap::new();
    for repo in repos {
        let src = effective_source(repo);
        if !src.is_empty() {
            *count.entry(src).or_insert(0) += 1;
        }
    }
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut out = Vec::with_capacity(repos.len());
    for repo in repos {
        let src = effective_source(repo);
        if src.is_empty() {
            out.push(String::new());
            continue;
        }
        let total = count.get(&src).copied().unwrap_or(1);
        if total == 1 {
            out.push(src);
        } else {
            let idx = seen.entry(src.clone()).or_insert(0);
            *idx += 1;
            out.push(format!("{src}_{idx}"));
        }
    }
    out
}

fn effective_source(repo: &SkillRepo) -> String {
    match repo {
        SkillRepo::Workspace => String::new(),
        SkillRepo::External { source } => {
            if source.trim().is_empty() {
                GLOBAL_NAMESPACE.to_string()
            } else {
                source.clone()
            }
        }
    }
}

/// Stage all eligible inputs and return an ordered map from `skill.name` to its
/// resolved `StageResult`. When `workspace_root` is `None` (no host workspace
/// available), workspace-native skills report `WorkspaceNative` and all others
/// report `None`.
pub fn stage(
    workspace_root: Option<&Path>,
    visible: &[RepoBound],
    source_ns: &[String],
) -> std::io::Result<BTreeMap<String, StageResult>> {
    let mut roots = BTreeMap::new();
    let Some(workspace_root) = workspace_root else {
        for bound in visible {
            let name = bound.skill.name().to_string();
            let result = match bound.repo {
                SkillRepo::Workspace => StageResult::WorkspaceNative,
                _ => StageResult::None,
            };
            roots.insert(name, result);
        }
        return Ok(roots);
    };

    let cache_root = workspace_root.join(CACHE_DIR);
    let mut retained = Vec::new();

    for (i, bound) in visible.iter().enumerate() {
        let name = bound.skill.name().to_string();
        if name.trim().is_empty() {
            continue;
        }
        match bound.repo {
            SkillRepo::Workspace => {
                roots.insert(name, StageResult::WorkspaceNative);
            }
            _ => {
                let ns = source_ns
                    .get(i)
                    .filter(|s| !s.is_empty())
                    .cloned()
                    .unwrap_or_else(|| bound.repo.source());
                let ns = if ns.is_empty() { GLOBAL_NAMESPACE.to_string() } else { ns };
                let staged_dir = cache_root.join(&ns).join(&name);
                materialize_if_changed(&staged_dir, bound.skill.resources())?;
                retained.push(staged_dir.clone());
                roots.insert(
                    name,
                    StageResult::Cached {
                        source_namespace: ns,
                        skill_name: bound.skill.name().to_string(),
                    },
                );
            }
        }
    }

    garbage_collect_orphans(&cache_root, &retained);
    Ok(roots)
}

fn materialize_if_changed(staged_dir: &Path, resources: &SkillResources) -> std::io::Result<()> {
    std::fs::create_dir_all(staged_dir)?;
    if resources.is_empty() {
        return Ok(());
    }
    let mut expected = Vec::new();
    for (rel, content) in resources {
        if rel.trim().is_empty() || rel.starts_with('/') || rel.contains("..") {
            continue;
        }
        let target = staged_dir.join(rel);
        if !target.starts_with(staged_dir) {
            continue;
        }
        let bytes = decode(content);
        write_if_changed(&target, &bytes)?;
        expected.push(target);
    }
    remove_unexpected(staged_dir, &expected);
    Ok(())
}

fn write_if_changed(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if target.exists() {
        let existing = std::fs::read(target)?;
        if sha256_hex(&existing) == sha256_hex(bytes) {
            return Ok(());
        }
    }
    std::fs::write(target, bytes)?;
    maybe_mark_executable(target, bytes);
    Ok(())
}

fn remove_unexpected(staged_dir: &Path, expected: &[PathBuf]) {
    if let Ok(entries) = std::fs::read_dir(staged_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && !expected.iter().any(|e| e == &path) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

fn garbage_collect_orphans(cache_root: &Path, retained: &[PathBuf]) {
    if !cache_root.is_dir() {
        return;
    }
    let Ok(ns_dirs) = std::fs::read_dir(cache_root) else { return };
    for ns in ns_dirs.flatten() {
        let ns_path = ns.path();
        if !ns_path.is_dir() {
            continue;
        }
        let Ok(skill_dirs) = std::fs::read_dir(&ns_path) else { continue };
        for skill in skill_dirs.flatten() {
            let skill_path = skill.path();
            if skill_path.is_dir() && !retained.iter().any(|r| r == &skill_path) {
                let _ = std::fs::remove_dir_all(&skill_path);
            }
        }
        // Clean up empty namespace dir.
        if std::fs::read_dir(&ns_path).map(|mut d| d.next().is_none()).unwrap_or(false) {
            let _ = std::fs::remove_dir(&ns_path);
        }
    }
}

fn decode(content: &str) -> Vec<u8> {
    if let Some(rest) = content.strip_prefix("base64:") {
        base64::engine::general_purpose::STANDARD.decode(rest).unwrap_or_default()
    } else {
        content.as_bytes().to_vec()
    }
}

/// SHA-256 hex digest of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Known interpreter / script suffixes. Conservative — we only mark "obvious" scripts.
const SCRIPT_SUFFIXES: [&str; 9] = [".sh", ".bash", ".zsh", ".ksh", ".py", ".rb", ".pl", ".js", ".mjs"];

/// Script-detection heuristic: shebang at byte 0/1 OR a known-script suffix.
pub fn should_be_executable(target: &Path, bytes: &[u8]) -> bool {
    if bytes.len() >= 2 && bytes[0] == b'#' && bytes[1] == b'!' {
        return true;
    }
    let file_name = target.file_name().map(|f| f.to_string_lossy().to_lowercase());
    let Some(file_name) = file_name else { return false };
    let Some(dot) = file_name.rfind('.') else { return false };
    let ext: &str = &file_name[dot..];
    SCRIPT_SUFFIXES.contains(&ext)
}

fn maybe_mark_executable(target: &Path, bytes: &[u8]) {
    if !should_be_executable(target, bytes) {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(target) {
            let mode = meta.permissions().mode();
            // Mirror `chmod +x`: add owner/group/other exec where read is set.
            let mut new_mode = mode;
            if mode & 0o444 != 0 {
                new_mode |= (mode & 0o444) >> 2;
            }
            let _ = std::fs::set_permissions(target, std::fs::Permissions::from_mode(new_mode));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = target;
        let _ = bytes;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext_skill(name: &str) -> RepoBound {
        RepoBound::new(
            AgentSkill::builder().name(name).source("market").description("d").build(),
            SkillRepo::External { source: "market".to_string() },
        )
    }

    #[test]
    fn should_be_executable_detects_shebang_and_suffix() {
        assert!(should_be_executable(Path::new("run.sh"), b"#!/bin/sh\n"));
        assert!(should_be_executable(Path::new("tool.py"), b"print(1)"));
        assert!(!should_be_executable(Path::new("notes.md"), b"# title"));
        assert!(!should_be_executable(Path::new("data.json"), b"{}"));
    }

    #[test]
    fn sha256_matches_known_vector() {
        // SHA-256 of "abc".
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn decode_handles_base64_prefix() {
        assert_eq!(decode("base64:aGVsbG8="), b"hello");
        assert_eq!(decode("plain"), b"plain");
    }

    #[test]
    fn resolve_namespaces_disambiguates_collisions() {
        let repos = vec![
            SkillRepo::External { source: "market".to_string() },
            SkillRepo::External { source: "market".to_string() },
            SkillRepo::External { source: "other".to_string() },
            SkillRepo::Workspace,
        ];
        let ns = resolve_source_namespaces(&repos);
        assert_eq!(ns[0], "market_1");
        assert_eq!(ns[1], "market_2");
        assert_eq!(ns[2], "other");
        assert_eq!(ns[3], "");
    }

    #[test]
    fn stage_without_root_reports_native_or_none() {
        let visible = vec![
            RepoBound::new(
                AgentSkill::builder().name("ws").build(),
                SkillRepo::Workspace,
            ),
            ext_skill("ext"),
        ];
        let roots = stage(None, &visible, &[]).unwrap();
        assert_eq!(roots.get("ws"), Some(&StageResult::WorkspaceNative));
        assert_eq!(roots.get("ext"), Some(&StageResult::None));
    }

    #[test]
    fn stage_materializes_to_cache_dir() {
        let base = std::env::temp_dir().join(format!("asr_stager_{}", std::process::id()));
        let cache = base.join(CACHE_DIR).join("market").join("tool");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let skill = AgentSkill::builder()
            .name("tool")
            .source("market")
            .description("d")
            .add_resource("SKILL.md", "# tool")
            .add_resource("scripts/run.py", "print(1)")
            .build();
        let visible = vec![RepoBound::new(skill, SkillRepo::External { source: "market".to_string() })];
        let ns = resolve_source_namespaces(&[SkillRepo::External { source: "market".to_string() }]);

        let roots = stage(Some(&base), &visible, &ns).unwrap();
        assert!(matches!(roots.get("tool"), Some(StageResult::Cached { .. })));
        assert!(cache.join("SKILL.md").exists());
        assert!(cache.join("scripts/run.py").exists());
        assert!(cache.join("scripts").join("run.py").is_file());

        let _ = std::fs::remove_dir_all(&base);
    }
}
