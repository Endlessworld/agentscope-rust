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

/// Bound mapped identities while leaving room for a disambiguating digest.
const MAX_CACHE_SEGMENT: usize = 64;

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
    ///
    /// Also returned when a repository published no resources that could be staged (a lazy
    /// repository, or a map whose entries were all rejected). Nothing was staged and nothing is
    /// retained for the skill on this pass, so a directory written by an earlier call becomes
    /// orphan-GC eligible: `None` does not mean "the files cached earlier are still there".
    None,
    /// Skill comes from `WorkspaceSkillRepository` (already in `workspace/skills/`).
    WorkspaceNative,
    /// Skill staged under `.skills-cache/<sourceNs>/<skillName>/`.
    Cached {
        source_namespace: String,
        skill_name: String,
    },
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
            out.push(source_namespace_segment(&src));
        } else {
            let idx = seen.entry(src.clone()).or_insert(0);
            *idx += 1;
            out.push(source_namespace_segment(&format!("{src}_{idx}")));
        }
    }
    out
}

/// Maps a repository source identifier to one safe directory segment.
///
/// Source identifiers are metadata rather than user-controlled skill names, and shipped
/// repositories use values such as `owner/repository`; rejecting those values would silently
/// disable staging. A digest suffix disambiguates identifiers whenever sanitisation changes them.
fn source_namespace_segment(namespace: &str) -> String {
    if namespace.trim().is_empty() {
        return GLOBAL_NAMESPACE.to_string();
    }
    let safe: String = namespace
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    // A suffix alone would leave names such as NUL.txt-<digest> reserved on Windows.
    let safe = if is_windows_device_name(&safe) {
        format!("_{safe}")
    } else {
        safe
    };
    let lossless = safe == namespace
        && safe.len() <= MAX_CACHE_SEGMENT
        // Spaces are already replaced above; this also excludes "." and "..".
        && !safe.ends_with('.');
    if lossless {
        return safe;
    }
    let digest = sha256_hex(namespace.as_bytes());
    let digest = &digest[..12];
    let keep = safe.len().min(MAX_CACHE_SEGMENT - digest.len() - 1);
    format!("{}-{}", &safe[..keep], digest)
}

/// Resolves one directory component without allowing aliases of its parent or siblings.
fn resolve_staging_directory(parent: &Path, name: &str) -> std::io::Result<PathBuf> {
    let resolved = parent.join(name);
    // Check both separator styles and drive syntax even on Unix, since the cache may be
    // projected onto another filesystem. Windows also aliases trailing dots and spaces
    // and reserves device names, including those with an extension.
    if resolved.parent() != Some(parent)
        || name.contains('/')
        || name.contains('\\')
        || name.contains(':')
        || name.ends_with('.')
        || name.ends_with(' ')
        || is_windows_device_name(name)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Staging path component must be a single directory name",
        ));
    }
    Ok(resolved)
}

fn is_windows_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    let mut chars = stem.chars();
    let prefix: String = chars.by_ref().take(3).collect();
    matches!(prefix.as_str(), "COM" | "LPT")
        // Win32 also recognises superscript 1, 2 and 3 in COM/LPT device names.
        && matches!(
            chars.next(),
            Some('1'..='9' | '\u{00b9}' | '\u{00b2}' | '\u{00b3}')
        )
        && chars.next().is_none()
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
                // Source identifiers may contain repository-specific values (`owner/repo`); they
                // are mapped to one safe directory segment before any cache path is built.
                let ns = source_namespace_segment(&ns);
                let staged_dir = match resolve_staging_directory(&cache_root, &ns)
                    .and_then(|namespace_dir| resolve_staging_directory(&namespace_dir, &name))
                {
                    Ok(dir) => dir,
                    Err(e) => {
                        // An unsafe skill name publishes no staging root; other skills still stage.
                        tracing::warn!("Failed to stage skill '{name}' (source-ns={ns}): {e}");
                        roots.insert(name, StageResult::None);
                        continue;
                    }
                };
                if !materialize_if_changed(&staged_dir, bound.skill.resources())? {
                    // Nothing was materialised — exactly what a lazy repository produces, since it
                    // publishes no in-memory resources. Reporting `Cached` here would advertise a
                    // `files-root` for a directory that stays permanently empty, so report `None`
                    // and let shell-mode rendering omit the skill. The directory is also left
                    // unretained, making a directory written by an earlier call GC-eligible.
                    roots.insert(name, StageResult::None);
                    continue;
                }
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

/// Materialises `resources` under `staged_dir`, returning whether anything was staged.
///
/// Returns `false` — and leaves no directory behind — when the map is empty or every entry is
/// rejected by the path-safety filter. The directory is created lazily by [`write_if_changed`] on
/// the first surviving entry, so a directory whose files are all rejected is not materialised for
/// nothing.
fn materialize_if_changed(staged_dir: &Path, resources: &SkillResources) -> std::io::Result<bool> {
    if resources.is_empty() {
        return Ok(false);
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
    Ok(!expected.is_empty())
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
    let Ok(ns_dirs) = std::fs::read_dir(cache_root) else {
        return;
    };
    for ns in ns_dirs.flatten() {
        let ns_path = ns.path();
        if !ns_path.is_dir() {
            continue;
        }
        let Ok(skill_dirs) = std::fs::read_dir(&ns_path) else {
            continue;
        };
        for skill in skill_dirs.flatten() {
            let skill_path = skill.path();
            if skill_path.is_dir() && !retained.iter().any(|r| r == &skill_path) {
                let _ = std::fs::remove_dir_all(&skill_path);
            }
        }
        // Clean up empty namespace dir.
        if std::fs::read_dir(&ns_path)
            .map(|mut d| d.next().is_none())
            .unwrap_or(false)
        {
            let _ = std::fs::remove_dir(&ns_path);
        }
    }
}

fn decode(content: &str) -> Vec<u8> {
    if let Some(rest) = content.strip_prefix("base64:") {
        base64::engine::general_purpose::STANDARD
            .decode(rest)
            .unwrap_or_default()
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
const SCRIPT_SUFFIXES: [&str; 9] = [
    ".sh", ".bash", ".zsh", ".ksh", ".py", ".rb", ".pl", ".js", ".mjs",
];

/// Script-detection heuristic: shebang at byte 0/1 OR a known-script suffix.
pub fn should_be_executable(target: &Path, bytes: &[u8]) -> bool {
    if bytes.len() >= 2 && bytes[0] == b'#' && bytes[1] == b'!' {
        return true;
    }
    let file_name = target
        .file_name()
        .map(|f| f.to_string_lossy().to_lowercase());
    let Some(file_name) = file_name else {
        return false;
    };
    let Some(dot) = file_name.rfind('.') else {
        return false;
    };
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
            AgentSkill::builder()
                .name(name)
                .source("market")
                .description("d")
                .build(),
            SkillRepo::External {
                source: "market".to_string(),
            },
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
            SkillRepo::External {
                source: "market".to_string(),
            },
            SkillRepo::External {
                source: "market".to_string(),
            },
            SkillRepo::External {
                source: "other".to_string(),
            },
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
        let visible = vec![RepoBound::new(
            skill,
            SkillRepo::External {
                source: "market".to_string(),
            },
        )];
        let ns = resolve_source_namespaces(&[SkillRepo::External {
            source: "market".to_string(),
        }]);

        let roots = stage(Some(&base), &visible, &ns).unwrap();
        assert!(matches!(
            roots.get("tool"),
            Some(StageResult::Cached { .. })
        ));
        assert!(cache.join("SKILL.md").exists());
        assert!(cache.join("scripts/run.py").exists());
        assert!(cache.join("scripts").join("run.py").is_file());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn stage_empty_resources_reports_none_and_creates_no_dir() {
        // A lazy repository publishes no in-memory resources; staging must report `None`
        // and leave no staged directory behind (a `<files-root>` for a permanently empty
        // directory makes the skill's files unreachable inside a sandbox).
        let base = std::env::temp_dir().join(format!("asr_stager_empty_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let skill = AgentSkill::builder()
            .name("lazy")
            .source("market")
            .description("d")
            .build();
        let visible = vec![RepoBound::new(
            skill,
            SkillRepo::External {
                source: "market".to_string(),
            },
        )];
        let ns = resolve_source_namespaces(&[SkillRepo::External {
            source: "market".to_string(),
        }]);

        let roots = stage(Some(&base), &visible, &ns).unwrap();
        assert_eq!(roots.get("lazy"), Some(&StageResult::None));
        let staged_dir = base.join(CACHE_DIR).join("market").join("lazy");
        assert!(
            !staged_dir.exists(),
            "nothing was staged, so no staged directory may be left behind"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn stage_all_rejected_resources_reports_none_and_creates_no_dir() {
        // A non-empty map whose entries are all rejected by the path-safety filter must
        // behave like the empty-map case rather than creating a directory no file lands in.
        let base = std::env::temp_dir().join(format!("asr_stager_reject_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let skill = AgentSkill::builder()
            .name("unsafe")
            .source("market")
            .description("d")
            .add_resource("/etc/passwd", "root\n")
            .add_resource("../escape.sh", "#!/bin/sh\n")
            .build();
        let visible = vec![RepoBound::new(
            skill,
            SkillRepo::External {
                source: "market".to_string(),
            },
        )];
        let ns = resolve_source_namespaces(&[SkillRepo::External {
            source: "market".to_string(),
        }]);

        let roots = stage(Some(&base), &visible, &ns).unwrap();
        assert_eq!(roots.get("unsafe"), Some(&StageResult::None));
        let staged_dir = base.join(CACHE_DIR).join("market").join("unsafe");
        assert!(
            !staged_dir.exists(),
            "no file survived the filter, so no directory may remain"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn an_absolute_skill_name_cannot_write_outside_the_cache() {
        let base = std::env::temp_dir().join(format!("asr_stager_esc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let escaped = base.join("escaped");

        let name = escaped.to_string_lossy().into_owned();
        let skill = AgentSkill::builder()
            .name(&name)
            .source("market")
            .description("d")
            .add_resource("SKILL.md", "# escaped")
            .build();
        let visible = vec![RepoBound::new(
            skill,
            SkillRepo::External {
                source: "market".to_string(),
            },
        )];
        let ns = resolve_source_namespaces(&[SkillRepo::External {
            source: "market".to_string(),
        }]);

        let roots = stage(Some(&base), &visible, &ns).unwrap();
        assert_eq!(roots.get(&name), Some(&StageResult::None));
        assert!(!escaped.exists(), "no write may land outside the cache");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn traversal_and_device_names_are_rejected_as_skill_names() {
        assert!(resolve_staging_directory(Path::new("cache"), "../sibling").is_err());
        assert!(resolve_staging_directory(Path::new("cache"), "nested/dir").is_err());
        assert!(resolve_staging_directory(Path::new("cache"), "nested\\dir").is_err());
        assert!(resolve_staging_directory(Path::new("cache"), "C:\\x").is_err());
        assert!(resolve_staging_directory(Path::new("cache"), "trailing.").is_err());
        assert!(resolve_staging_directory(Path::new("cache"), "trailing ").is_err());
        assert!(resolve_staging_directory(Path::new("cache"), "NUL").is_err());
        assert!(resolve_staging_directory(Path::new("cache"), "con.md").is_err());
        // Supported names keep uppercase letters, internal spaces, underscores and dots.
        for ok in ["My Skill_1.2", "tool"] {
            assert_eq!(
                resolve_staging_directory(Path::new("cache"), ok).unwrap(),
                Path::new("cache").join(ok)
            );
        }
    }

    #[test]
    fn a_source_namespace_is_mapped_to_one_safe_segment() {
        let repos = vec![
            SkillRepo::External {
                source: "owner/repository".to_string(),
            },
            SkillRepo::External {
                source: "../escape".to_string(),
            },
        ];
        let ns = resolve_source_namespaces(&repos);
        for segment in &ns {
            assert!(!segment.contains('/'), "{segment}");
            assert!(!segment.contains('\\'), "{segment}");
            assert_ne!(segment, "..");
            assert!(resolve_staging_directory(Path::new("cache"), segment).is_ok());
        }
    }
}
