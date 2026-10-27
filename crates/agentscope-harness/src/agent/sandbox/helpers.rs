//! Sandbox workspace helpers, mirroring
//! `WorkspaceMountSupport`, `WorkspaceArchiveExtractor` and `WorkspaceProjectionApplier`.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use agentscope_core::error::{Error, Result};
use sha2::Digest;

use super::layout::{BindMountEntry, LayoutSpec, WorkspaceEntry};

/// Helpers for [`BindMountEntry`] handling.
pub struct WorkspaceMountSupport;

impl WorkspaceMountSupport {
    /// Relative workspace paths (POSIX-style, no leading slash) for every bind mount in
    /// the spec tree, in stable iteration order.
    pub fn bind_mount_relative_paths(spec: &LayoutSpec) -> Vec<String> {
        let mut out = Vec::new();
        walk(&spec.entries, "", &mut out);
        out
    }

    pub fn has_bind_mounts(spec: &LayoutSpec) -> bool {
        !Self::bind_mount_relative_paths(spec).is_empty()
    }

    /// Absolute mount path inside the sandbox for a top-level entry key.
    pub fn container_mount_path(workspace_root: &str, entry_key: &str) -> String {
        let root = workspace_root.strip_suffix('/').unwrap_or(workspace_root);
        let key = entry_key
            .strip_prefix('/')
            .unwrap_or(entry_key)
            .replace('\\', "/");
        format!("{root}/{key}")
    }

    /// `tar` CLI `--exclude` arguments (`./relative`) for each bind mount path.
    pub fn tar_exclude_args_for_bind_mounts(spec: &LayoutSpec) -> Vec<String> {
        Self::bind_mount_relative_paths(spec)
            .into_iter()
            .filter(|rel| !rel.is_empty())
            .map(|rel| format!("--exclude=./{}", rel.replace('\\', "/")))
            .collect()
    }

    /// Top-level [`BindMountEntry`] instances keyed by entry key.
    pub fn top_level_bind_mounts(spec: &LayoutSpec) -> BTreeMap<String, BindMountEntry> {
        spec.entries
            .iter()
            .filter_map(|(k, v)| match v {
                WorkspaceEntry::BindMount(b) => Some((k.clone(), b.clone())),
                _ => None,
            })
            .collect()
    }

    /// Normalized absolute host path.
    pub fn normalized_host_path(host_path: &str) -> String {
        if host_path.trim().is_empty() {
            return String::new();
        }
        let path = Path::new(host_path);
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        };
        normalize_lexical(&absolute)
            .to_string_lossy()
            .replace('\\', "/")
    }
}

fn walk(entries: &BTreeMap<String, WorkspaceEntry>, prefix: &str, out: &mut Vec<String>) {
    for (key, value) in entries {
        let rel = if prefix.is_empty() {
            key.replace('\\', "/")
        } else {
            format!("{prefix}/{}", key.replace('\\', "/"))
        };
        match value {
            WorkspaceEntry::BindMount(_) => out.push(rel),
            WorkspaceEntry::Dir(dir) if !dir.children.is_empty() => {
                walk(&dir.children, &rel, out);
            }
            _ => {}
        }
    }
}

/// Securely extracts tar archives into a target directory, defending against the
/// "Zip Slip" class of path traversal attacks.
pub struct WorkspaceArchiveExtractor;

impl WorkspaceArchiveExtractor {
    /// Extracts a tar archive (bytes) into `dest_root`, which must already exist.
    ///
    /// Rejects entries with absolute paths, `..` segments, or null bytes; verifies the
    /// resolved destination stays within the canonical root.
    pub fn extract_tar_bytes(dest_root: &Path, archive: &[u8]) -> Result<()> {
        let canonical_root = normalize_lexical(dest_root);
        let mut offset = 0usize;

        while offset + 512 <= archive.len() {
            let header = &archive[offset..offset + 512];
            // Two consecutive zero blocks mark the end of the archive.
            if header.iter().all(|b| *b == 0) {
                break;
            }
            let name = read_cstr(&header[0..100]);
            let prefix = read_cstr(&header[345..500]);
            let full_name = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let size = parse_octal(&header[124..136]).unwrap_or(0);
            let typeflag = header[156] as char;
            offset += 512;

            let is_dir = typeflag == '5' || full_name.ends_with('/');
            let data_len = if is_dir { 0 } else { size };
            if !full_name.is_empty() {
                let dest = secure_join(&canonical_root, &full_name)?;
                if is_dir {
                    std::fs::create_dir_all(&dest).map_err(Error::Io)?;
                } else {
                    let end = (offset + data_len).min(archive.len());
                    let data = &archive[offset..end];
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent).map_err(Error::Io)?;
                    }
                    std::fs::write(&dest, data).map_err(Error::Io)?;
                }
            }
            offset += data_len.div_ceil(512) * 512;
        }
        Ok(())
    }
}

fn validate_entry_name(name: &str) -> Result<()> {
    if name.starts_with('/') || name.starts_with('\\') {
        return Err(Error::Tool(format!(
            "Tar entry has absolute path (rejected): {name}"
        )));
    }
    if name.contains('\0') {
        return Err(Error::Tool(format!(
            "Tar entry name contains null byte (rejected): {name}"
        )));
    }
    for segment in name.split(['/', '\\']) {
        if segment == ".." {
            return Err(Error::Tool(format!(
                "Tar entry contains path traversal (rejected): {name}"
            )));
        }
    }
    Ok(())
}

fn secure_join(root: &Path, rel: &str) -> Result<PathBuf> {
    validate_entry_name(rel)?;
    let dest = normalize_lexical(&root.join(rel));
    if !dest.starts_with(root) {
        return Err(Error::Tool(format!(
            "Tar entry escapes destination root: {rel}"
        )));
    }
    Ok(dest)
}

fn read_cstr(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

fn parse_octal(bytes: &[u8]) -> Option<usize> {
    let s = read_cstr(bytes);
    if s.is_empty() {
        return Some(0);
    }
    usize::from_str_radix(s.trim(), 8).ok()
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Projection result used at sandbox start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionPayload {
    /// Deterministic hash over projected file paths and bytes.
    pub hash: String,
    /// Tar archive containing projected files.
    pub tar_bytes: Vec<u8>,
    /// Number of projected files.
    pub file_count: usize,
}

/// Builds a deterministic archive payload for workspace projection entries.
pub struct WorkspaceProjectionApplier;

impl WorkspaceProjectionApplier {
    /// Builds projection payload from all `WorkspaceProjection` items in the spec.
    pub fn build(spec: &LayoutSpec) -> Result<Option<ProjectionPayload>> {
        let mut projections = Vec::new();
        collect_projection_entries(&spec.entries, &mut projections);
        if projections.is_empty() {
            return Ok(None);
        }

        let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for entry in &projections {
            let source_root = entry.source_root.trim();
            if source_root.is_empty() {
                continue;
            }
            let source_root = normalize_lexical(&PathBuf::from(source_root));
            for root in &entry.include_roots {
                if root.trim().is_empty() {
                    continue;
                }
                let resolved = normalize_lexical(&source_root.join(root));
                if !resolved.starts_with(&source_root) {
                    continue;
                }
                if resolved.is_file() {
                    files.insert(
                        normalize_path(root),
                        std::fs::read(&resolved).map_err(Error::Io)?,
                    );
                } else if resolved.is_dir() {
                    collect_dir_files(&source_root, &resolved, &mut files)?;
                }
            }
        }

        if files.is_empty() {
            return Ok(None);
        }

        let mut hasher = sha2::Sha256::new();
        for (rel, content) in &files {
            hasher.update(rel.as_bytes());
            hasher.update([0u8]);
            hasher.update(content);
        }
        let hash = hex_lower(&hasher.finalize());

        let mut tar_bytes = Vec::new();
        for (rel, content) in &files {
            append_ustar_file(&mut tar_bytes, rel, content);
        }
        // Two zero blocks terminate the archive.
        tar_bytes.extend_from_slice(&[0u8; 1024]);

        let file_count = files.len();
        Ok(Some(ProjectionPayload {
            hash,
            tar_bytes,
            file_count,
        }))
    }
}

fn collect_projection_entries(
    entries: &BTreeMap<String, WorkspaceEntry>,
    out: &mut Vec<super::layout::WorkspaceProjectionEntry>,
) {
    for entry in entries.values() {
        match entry {
            WorkspaceEntry::WorkspaceProjection(p) => out.push(p.clone()),
            WorkspaceEntry::Dir(dir) if !dir.children.is_empty() => {
                collect_projection_entries(&dir.children, out);
            }
            _ => {}
        }
    }
}

fn collect_dir_files(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) -> Result<()> {
    let entries = std::fs::read_dir(dir).map_err(Error::Io)?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_dir_files(root, &path, out)?;
        } else if path.is_file() {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            out.insert(
                normalize_path(&rel.to_string_lossy()),
                std::fs::read(&path).map_err(Error::Io)?,
            );
        }
    }
    Ok(())
}

fn normalize_path(path: &str) -> String {
    path.replace('\\', "/").trim_start_matches('/').to_string()
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Appends a minimal ustar header + content for a regular file.
fn append_ustar_file(out: &mut Vec<u8>, name: &str, content: &[u8]) {
    let mut header = [0u8; 512];
    let (prefix, base) = split_ustar_name(name);
    write_field(&mut header[0..100], base);
    write_octal(&mut header[100..108], 0o644);
    write_octal(&mut header[108..116], 0);
    write_octal(&mut header[116..124], 0);
    write_octal(&mut header[124..136], content.len() as u64);
    write_octal(&mut header[136..148], 0);
    header[148..156].fill(b' ');
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    write_field(&mut header[345..500], prefix);

    let checksum: u32 = header.iter().map(|b| *b as u32).sum();
    let chk = format!("{checksum:06o}\0 ");
    header[148..156].copy_from_slice(chk.as_bytes());

    out.extend_from_slice(&header);
    out.extend_from_slice(content);
    let pad = (512 - (content.len() % 512)) % 512;
    out.extend(std::iter::repeat(0u8).take(pad));
}

fn split_ustar_name(name: &str) -> (&str, &str) {
    if name.len() <= 100 {
        return ("", name);
    }
    match name.rmatch_indices('/').find(|(i, _)| *i <= 155) {
        Some((i, _)) => (&name[..i], &name[i + 1..]),
        None => ("", name),
    }
}

fn write_field(dst: &mut [u8], value: &str) {
    let bytes = value.as_bytes();
    let n = bytes.len().min(dst.len());
    dst[..n].copy_from_slice(&bytes[..n]);
}

fn write_octal(dst: &mut [u8], value: u64) {
    let s = format!("{:0width$o}", value, width = dst.len() - 1);
    let bytes = s.as_bytes();
    let n = bytes.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&bytes[..n]);
    dst[n] = 0;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::sandbox::layout::{
        BindMountEntry, DirEntry, FileEntry, WorkspaceProjectionEntry,
    };

    fn layout_with_bind_mounts() -> LayoutSpec {
        let mut top = DirEntry::new();
        top.children.insert(
            "cache".to_string(),
            BindMountEntry::new("/host/cache").into(),
        );
        LayoutSpec::new()
            .entry("data", BindMountEntry::new("/host/data"))
            .entry("nest", top)
    }

    #[test]
    fn bind_mount_paths_walk_nested_tree() {
        let spec = layout_with_bind_mounts();
        let mut paths = WorkspaceMountSupport::bind_mount_relative_paths(&spec);
        paths.sort();
        assert_eq!(paths, vec!["data".to_string(), "nest/cache".to_string()]);
        assert!(WorkspaceMountSupport::has_bind_mounts(&spec));
        assert_eq!(
            WorkspaceMountSupport::container_mount_path("/workspace/", "/data"),
            "/workspace/data"
        );
        let mut excludes = WorkspaceMountSupport::tar_exclude_args_for_bind_mounts(&spec);
        excludes.sort();
        assert_eq!(excludes, vec!["--exclude=./data", "--exclude=./nest/cache"]);
        assert_eq!(WorkspaceMountSupport::top_level_bind_mounts(&spec).len(), 1);
    }

    #[test]
    fn no_bind_mounts_reports_empty() {
        let spec = LayoutSpec::new().entry("README.md", FileEntry::new("hi"));
        assert!(!WorkspaceMountSupport::has_bind_mounts(&spec));
        assert!(WorkspaceMountSupport::bind_mount_relative_paths(&spec).is_empty());
    }

    #[test]
    fn extractor_rejects_path_traversal() {
        let dir = std::env::temp_dir().join(format!("asr_extract_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Build an archive with an escaping entry.
        let mut archive = Vec::new();
        append_ustar_file(&mut archive, "../escape.txt", b"evil");
        archive.extend_from_slice(&[0u8; 1024]);

        let err = WorkspaceArchiveExtractor::extract_tar_bytes(&dir, &archive).unwrap_err();
        assert!(err.to_string().contains("traversal") || err.to_string().contains("escapes"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extractor_extracts_regular_file() {
        let dir = std::env::temp_dir().join(format!("asr_extract_ok_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut archive = Vec::new();
        append_ustar_file(&mut archive, "sub/hello.txt", b"hello world");
        archive.extend_from_slice(&[0u8; 1024]);

        WorkspaceArchiveExtractor::extract_tar_bytes(&dir, &archive).unwrap();
        let content = std::fs::read_to_string(dir.join("sub/hello.txt")).unwrap();
        assert_eq!(content, "hello world");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn projection_builds_hash_and_tar() {
        let src = std::env::temp_dir().join(format!("asr_proj_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&src);
        std::fs::create_dir_all(src.join("docs")).unwrap();
        std::fs::write(src.join("docs/a.md"), "alpha").unwrap();
        std::fs::write(src.join("docs/b.md"), "beta").unwrap();

        let entry = WorkspaceProjectionEntry {
            ephemeral: false,
            source_root: src.to_string_lossy().to_string(),
            include_roots: vec!["docs".to_string()],
        };
        let spec = LayoutSpec::new().entry("project", entry);
        let payload = WorkspaceProjectionApplier::build(&spec).unwrap().unwrap();
        assert_eq!(payload.file_count, 2);
        assert_eq!(payload.hash.len(), 64);
        assert!(!payload.tar_bytes.is_empty());

        // Rebuilding yields an identical hash (deterministic).
        let payload2 = WorkspaceProjectionApplier::build(&spec).unwrap().unwrap();
        assert_eq!(payload.hash, payload2.hash);
        let _ = std::fs::remove_dir_all(&src);
    }

    #[test]
    fn projection_absent_returns_none() {
        let spec = LayoutSpec::new().entry("README.md", FileEntry::new("hi"));
        assert!(WorkspaceProjectionApplier::build(&spec).unwrap().is_none());
    }
}
