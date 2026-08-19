//! Local-disk filesystem backend, mirroring `LocalFilesystem`.
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::{Error, Result};

use super::{validate_path, FileData, FileInfo, Filesystem, GrepMatch};

/// Path-resolution policy for a `LocalFilesystem`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LocalFsMode {
    /// All paths anchored to `root_dir`; traversal and outside-root absolute paths blocked.
    Sandboxed,
    /// Absolute paths accepted only under configured roots; relative paths anchor to `root_dir`.
    Rooted,
    /// Absolute paths pass through; relative paths anchor to `root_dir`.
    #[default]
    Unrestricted,
}

/// Filesystem backed by the local disk.
#[derive(Debug, Clone)]
pub struct LocalFilesystem {
    root: PathBuf,
    mode: LocalFsMode,
    max_file_size: usize,
}
impl LocalFilesystem {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_mode(root, LocalFsMode::Unrestricted, 10)
    }

    pub fn sandboxed(root: impl Into<PathBuf>) -> Self {
        Self::with_mode(root, LocalFsMode::Sandboxed, 10)
    }

    pub fn with_mode(
        root: impl Into<PathBuf>,
        mode: LocalFsMode,
        max_file_size_mb: usize,
    ) -> Self {
        let root = root.into();
        let root = if root.is_absolute() {
            root
        } else {
            std::env::current_dir().unwrap_or_default().join(root)
        };
        let root = lexical_normalize(&root);
        Self {
            root,
            mode,
            max_file_size: max_file_size_mb * 1024 * 1024,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn resolve(&self, _ctx: &RuntimeContext, path: &str) -> Result<PathBuf> {
        validate_path(path)?;
        let p = Path::new(path);
        match self.mode {
            LocalFsMode::Unrestricted => {
                if p.is_absolute() {
                    Ok(p.to_path_buf())
                } else {
                    Ok(self.root.join(p))
                }
            }
            LocalFsMode::Sandboxed => {
                // Anchor every path under `root`: strip leading separators so an agent-visible
                // absolute path like `/a.txt` resolves to `<root>/a.txt` on all platforms.
                let rel = path.trim_start_matches('/').trim_start_matches('\\');
                let joined = self.root.join(rel);
                let canonical = lexical_normalize(&joined);
                if !canonical.starts_with(&self.root) {
                    return Err(Error::Tool(format!("Path outside root: {path}")));
                }
                Ok(canonical)
            }
            LocalFsMode::Rooted => {
                if p.is_absolute() {
                    let canonical = lexical_normalize(p);
                    if canonical.starts_with(&self.root) {
                        Ok(canonical)
                    } else {
                        Err(Error::Tool(format!("Path outside root: {path}")))
                    }
                } else {
                    Ok(self.root.join(p))
                }
            }
        }
    }

    /// Converts an on-disk path to the agent-visible slash path under root.
    fn display_path(&self, full: &Path) -> String {
        let rel = full.strip_prefix(&self.root).unwrap_or(full);
        let s = rel.to_string_lossy().replace('\\', "/");
        format!("/{}", s.trim_start_matches('/'))
    }
}

/// Lexically normalizes a path by removing `.` and resolving `..` components.
pub(crate) fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}
fn modified_to_string(t: Option<std::time::SystemTime>) -> String {
    t.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| {
            chrono::DateTime::<chrono::Utc>::from_timestamp(
                d.as_secs() as i64,
                d.subsec_nanos(),
            )
            .unwrap_or_default()
            .to_rfc3339()
        })
        .unwrap_or_default()
}

#[async_trait]
impl Filesystem for LocalFilesystem {
    async fn ls(&self, ctx: &RuntimeContext, path: &str) -> Result<Vec<FileInfo>> {
        let dir = self.resolve(ctx, path)?;
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(_) => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let entry_path = entry.path();
            let md = match entry.metadata().await {
                Ok(m) => m,
                Err(_) => continue,
            };
            let p = self.display_path(&entry_path);
            let modified = modified_to_string(md.modified().ok());
            if md.is_dir() {
                out.push(FileInfo {
                    path: format!("{p}/"),
                    is_directory: true,
                    size: 0,
                    modified_at: modified,
                });
            } else {
                out.push(FileInfo {
                    path: p,
                    is_directory: false,
                    size: md.len(),
                    modified_at: modified,
                });
            }
        }
        out.sort_by_key(|f| f.path.clone());
        Ok(out)
    }

    async fn read(
        &self,
        ctx: &RuntimeContext,
        path: &str,
        offset: usize,
        limit: usize,
    ) -> Result<FileData> {
        let resolved = self.resolve(ctx, path)?;
        if !resolved.is_file() {
            return Err(Error::Tool(format!("File '{path}' not found")));
        }
        let bytes = tokio::fs::read(&resolved).await.map_err(Error::Io)?;
        if bytes.contains(&0) {
            use base64::Engine as _;
            let encoded =
                base64::engine::general_purpose::STANDARD.encode(&bytes);
            return Ok(FileData::binary(encoded));
        }
        let content = String::from_utf8_lossy(&bytes).into_owned();
        let lines: Vec<&str> = content.split('\n').collect();
        let start = offset;
        let end = if limit > 0 {
            (start + limit).min(lines.len())
        } else {
            lines.len()
        };
        if start >= lines.len() {
            return Err(Error::Tool(format!(
                "Line offset {offset} exceeds file length ({} lines)",
                lines.len()
            )));
        }
        Ok(FileData::text(lines[start..end].join("\n")))
    }

    async fn write(&self, ctx: &RuntimeContext, path: &str, content: &str) -> Result<()> {
        let resolved = self.resolve(ctx, path)?;
        if resolved.exists() {
            return Err(Error::Tool(format!(
                "Cannot write to {path} because it already exists. Read and edit, or write to a new path."
            )));
        }
        if let Some(parent) = resolved.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(Error::Io)?;
        }
        tokio::fs::write(&resolved, content).await.map_err(Error::Io)?;
        Ok(())
    }
    async fn edit(
        &self,
        ctx: &RuntimeContext,
        path: &str,
        old: &str,
        new: &str,
        replace_all: bool,
    ) -> Result<usize> {
        let resolved = self.resolve(ctx, path)?;
        if !resolved.is_file() {
            return Err(Error::Tool(format!("File '{path}' not found")));
        }
        let content = tokio::fs::read_to_string(&resolved).await.map_err(Error::Io)?;
        let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
        let old_norm = old.replace("\r\n", "\n").replace('\r', "\n");
        if old_norm.is_empty() {
            return Err(Error::Tool("old_string must not be empty".into()));
        }
        let count = normalized.matches(&old_norm).count();
        if count == 0 {
            return Err(Error::Tool(format!("old_string not found in {path}")));
        }
        if !replace_all && count > 1 {
            return Err(Error::Tool(format!(
                "old_string is not unique in {path} ({count} occurrences); use replace_all=true or refine the search"
            )));
        }
        let new_content = if replace_all {
            normalized.replace(&old_norm, new)
        } else {
            normalized.replacen(&old_norm, new, 1)
        };
        tokio::fs::write(&resolved, new_content).await.map_err(Error::Io)?;
        Ok(count)
    }

    async fn delete(&self, ctx: &RuntimeContext, path: &str) -> Result<()> {
        let resolved = self.resolve(ctx, path)?;
        if !resolved.exists() {
            return Ok(());
        }
        if resolved.is_dir() {
            tokio::fs::remove_dir_all(&resolved).await.map_err(Error::Io)?;
        } else {
            tokio::fs::remove_file(&resolved).await.map_err(Error::Io)?;
        }
        Ok(())
    }

    async fn move_path(&self, ctx: &RuntimeContext, from: &str, to: &str) -> Result<()> {
        let src = self.resolve(ctx, from)?;
        let dst = self.resolve(ctx, to)?;
        if !src.exists() {
            return Err(Error::Tool(format!("Source not found: {from}")));
        }
        if let Some(parent) = dst.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(Error::Io)?;
        }
        tokio::fs::rename(&src, &dst).await.map_err(Error::Io)?;
        Ok(())
    }

    async fn exists(&self, ctx: &RuntimeContext, path: &str) -> Result<bool> {
        Ok(self.resolve(ctx, path)?.exists())
    }
    async fn grep(
        &self,
        ctx: &RuntimeContext,
        pattern: &str,
        path: Option<&str>,
        glob: Option<&str>,
    ) -> Result<Vec<GrepMatch>> {
        let base = self.resolve(ctx, path.unwrap_or("."))?;
        if !base.exists() {
            return Ok(Vec::new());
        }
        let glob = glob.map(|g| g.to_string());
        let mut matches = Vec::new();
        self.grep_recursive(&base, pattern, glob.as_deref(), &mut matches)
            .await?;
        Ok(matches)
    }

    async fn glob(&self, ctx: &RuntimeContext, pattern: &str, path: &str) -> Result<Vec<FileInfo>> {
        let search = if path == "/" || path.is_empty() {
            self.root.clone()
        } else {
            self.resolve(ctx, path)?
        };
        if !search.is_dir() {
            return Ok(Vec::new());
        }
        let pattern = pattern.strip_prefix('/').unwrap_or(pattern).to_string();
        let mut out = Vec::new();
        self.glob_recursive(&search, &pattern, &mut out).await?;
        out.sort_by_key(|f| f.path.clone());
        Ok(out)
    }
}

impl LocalFilesystem {
    async fn grep_recursive(
        &self,
        dir: &Path,
        pattern: &str,
        glob: Option<&str>,
        out: &mut Vec<GrepMatch>,
    ) -> Result<()> {
        let mut entries = tokio::fs::read_dir(dir).await.map_err(Error::Io)?;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let p = entry.path();
            let md = match entry.metadata().await {
                Ok(m) => m,
                Err(_) => continue,
            };
            if md.is_dir() {
                Box::pin(self.grep_recursive(&p, pattern, glob, out)).await?;
                continue;
            }
            if md.len() > self.max_file_size as u64 {
                continue;
            }
            if let Some(glob) = glob {
                let rel = p
                    .strip_prefix(&self.root)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .replace('\\', "/");
                if !glob_match(glob, &rel) {
                    continue;
                }
            }
            if let Ok(content) = tokio::fs::read_to_string(&p).await {
                let display = self.display_path(&p);
                for (i, line) in content.lines().enumerate() {
                    if line.contains(pattern) {
                        out.push(GrepMatch {
                            path: display.clone(),
                            line: i + 1,
                            text: line.to_string(),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    async fn glob_recursive(
        &self,
        dir: &Path,
        pattern: &str,
        out: &mut Vec<FileInfo>,
    ) -> Result<()> {
        let mut entries = tokio::fs::read_dir(dir).await.map_err(Error::Io)?;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let p = entry.path();
            let md = match entry.metadata().await {
                Ok(m) => m,
                Err(_) => continue,
            };
            let rel = p
                .strip_prefix(&self.root)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            let display = format!("/{}", rel.trim_start_matches('/'));
            if glob_match(pattern, &rel) {
                if md.is_dir() {
                    out.push(FileInfo {
                        path: format!("{display}/"),
                        is_directory: true,
                        size: 0,
                        modified_at: String::new(),
                    });
                } else {
                    out.push(FileInfo {
                        path: display,
                        is_directory: false,
                        size: md.len(),
                        modified_at: String::new(),
                    });
                }
            }
            if md.is_dir() {
                Box::pin(self.glob_recursive(&p, pattern, out)).await?;
            }
        }
        Ok(())
    }
}

/// Simple glob matcher supporting `*`, `?`, `**` and `**/` (path-aware).
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.replace('\\', "/").chars().collect();
    let t: Vec<char> = text.replace('\\', "/").chars().collect();
    glob_match_inner(&p, 0, &t, 0)
}

fn glob_match_inner(p: &[char], pi: usize, t: &[char], ti: usize) -> bool {
    if pi == p.len() {
        return ti == t.len();
    }
    match p[pi] {
        '*' => {
            if pi + 1 < p.len() && p[pi + 1] == '*' {
                let mut j = pi;
                while j < p.len() && p[j] == '*' {
                    j += 1;
                }
                if j == p.len() {
                    return true;
                }
                for k in ti..=t.len() {
                    if glob_match_inner(p, j, t, k) {
                        return true;
                    }
                }
                false
            } else {
                for k in ti..=t.len() {
                    if glob_match_inner(p, pi + 1, t, k) {
                        return true;
                    }
                    if k < t.len() && t[k] == '/' {
                        break;
                    }
                }
                false
            }
        }
        '?' => {
            if ti == t.len() {
                false
            } else {
                glob_match_inner(p, pi + 1, t, ti + 1)
            }
        }
        c => {
            if ti == t.len() || c != t[ti] {
                false
            } else {
                glob_match_inner(p, pi + 1, t, ti + 1)
            }
        }
    }
}
