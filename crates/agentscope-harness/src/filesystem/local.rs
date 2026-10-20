//! Local-disk filesystem backend, mirroring `LocalFilesystem`.
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::{Error, Result};
use globset::{GlobBuilder, GlobMatcher};
use regex::{Regex, RegexBuilder};

use super::{validate_path, FileData, FileInfo, Filesystem, GrepMatch};

/// Number of leading bytes inspected when deciding whether a file is binary.
///
/// Only the head is checked so a large text file that happens to contain a NUL
/// byte far into its content is still treated as text.
const BINARY_SNIFF_LEN: usize = 8 * 1024;

/// Returns true when `bytes` look binary, i.e. a NUL byte occurs in the first
/// [`BINARY_SNIFF_LEN`] bytes.
///
/// Single source of truth for the binary heuristic: [`LocalFilesystem::read`]
/// (base64 result) and [`read_text`] (grep skip) must not drift apart.
fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_SNIFF_LEN).any(|b| *b == 0)
}

/// Generated/build directories skipped during recursive grep/glob walks.
/// The search base itself is always walked even if its name matches.
const GENERATED_DIRS: &[&str] = &[
    "target",
    ".git",
    "node_modules",
    ".idea",
    ".agentscope",
    "__pycache__",
    "dist",
    "build",
    ".next",
    ".venv",
    "venv",
];

/// Grep pattern matcher: regex with a literal-substring fallback, so arbitrary
/// user text (e.g. memory_search queries) never fails to compile.
pub(crate) enum PatternMatcher {
    Regex(Regex),
    Literal { text: String, ignore_case: bool },
}
impl PatternMatcher {
    pub(crate) fn new(pattern: &str) -> Self {
        // A leading (?i) is honoured by the literal fallback too: leaving it inside
        // the pattern would turn it into matchable text and never match.
        let (ignore_case, body) = match pattern.strip_prefix("(?i)") {
            Some(rest) => (true, rest),
            None => (false, pattern),
        };
        match RegexBuilder::new(body).case_insensitive(ignore_case).build() {
            Ok(r) => PatternMatcher::Regex(r),
            Err(_) => PatternMatcher::Literal {
                text: body.to_string(),
                ignore_case,
            },
        }
    }
    pub(crate) fn is_match(&self, line: &str) -> bool {
        match self {
            PatternMatcher::Regex(r) => r.is_match(line),
            PatternMatcher::Literal { text, ignore_case } => {
                if *ignore_case {
                    line.to_lowercase().contains(&text.to_lowercase())
                } else {
                    line.contains(text.as_str())
                }
            }
        }
    }
}

/// Builds a glob matcher from an agent-supplied pattern; a leading `/` is
/// treated as relative to the search base. `literal_separator` makes `*`/`?`
/// not cross `/` (Linux glob semantics; `**` still recurses).
/// Returns `None` for invalid patterns.
pub(crate) fn build_glob_matcher(pattern: &str) -> Option<GlobMatcher> {
    let pattern = pattern.strip_prefix('/').unwrap_or(pattern);
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .ok()
        .map(|g| g.compile_matcher())
}

/// Glob filter for `grep`, following the `rg -g` rule: a pattern containing `/`
/// is matched against the path relative to the search base, any other pattern
/// against the file name (so `*.rs` stays recursive while `src/*.rs` /
/// `**/*.java` select by path).
pub(crate) struct GrepGlob {
    matcher: GlobMatcher,
    path_based: bool,
}

impl GrepGlob {
    pub(crate) fn build(pattern: &str) -> Option<Self> {
        let matcher = build_glob_matcher(pattern)?;
        Some(Self {
            matcher,
            path_based: pattern.contains('/'),
        })
    }

    pub(crate) fn matches(&self, rel_path: &str, file_name: &str) -> bool {
        if self.path_based {
            self.matcher.is_match(rel_path)
        } else {
            self.matcher.is_match(file_name)
        }
    }
}

/// Builds the walker used by recursive `grep`/`glob`.
///
/// `.gitignore` / `.ignore` / global git excludes are honoured (`require_git(false)`
/// keeps that behaviour outside a git repository, e.g. in a bare workspace); hidden
/// files are still walked, and build directories are pruned by name.
fn build_walker(base: &Path) -> ignore::Walk {
    ignore::WalkBuilder::new(base)
        .hidden(false)
        .require_git(false)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(|e| {
            e.depth() == 0
                || !e
                    .file_type()
                    .is_some_and(|t| t.is_dir() && is_generated_name(e.file_name()))
        })
        .build()
}

/// Collects `(path, is_dir)` below `base` via [`build_walker`].
///
/// The `ignore` walker is synchronous, so it runs on the blocking pool to keep the
/// async runtime free.
async fn walk_entries(base: &Path) -> Vec<(PathBuf, bool)> {
    let base = base.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut entries = Vec::new();
        for entry in build_walker(&base) {
            let Ok(entry) = entry else { continue };
            if entry.depth() == 0 {
                continue;
            }
            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            entries.push((entry.into_path(), is_dir));
        }
        entries
    })
    .await
    .unwrap_or_default()
}

/// Relative POSIX path of `path` under `base`; falls back to the file name when
/// `path` is the search base itself (single-file grep).
fn relative_path(path: &Path, base: &Path) -> String {
    let rel = path
        .strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    let rel = rel.trim_start_matches('/');
    if rel.is_empty() {
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string()
    } else {
        rel.to_string()
    }
}

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
        if !dir.exists() {
            return Err(Error::Tool(format!("Path does not exist: {path}")));
        }
        if !dir.is_dir() {
            return Err(Error::Tool(format!("Not a directory: {path}")));
        }
        let mut entries = tokio::fs::read_dir(&dir).await.map_err(Error::Io)?;
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
        if looks_binary(&bytes) {
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
        // Normalise the replacement too: the file was matched and rewritten in
        // LF form, so writing a CRLF replacement verbatim would leave the file
        // with mixed line endings.
        let new_norm = new.replace("\r\n", "\n").replace('\r', "\n");
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
            normalized.replace(&old_norm, &new_norm)
        } else {
            normalized.replacen(&old_norm, &new_norm, 1)
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
        let matcher = PatternMatcher::new(pattern);
        let glob = glob.filter(|g| !g.trim().is_empty()).and_then(GrepGlob::build);
        let mut matches = Vec::new();
        if base.is_file() {
            // Linux grep semantics: a single-file path searches that file.
            self.grep_file(&base, &base, &matcher, glob.as_ref(), &mut matches)
                .await;
        } else {
            self.grep_recursive(&base, &matcher, glob.as_ref(), &mut matches)
                .await?;
        }
        Ok(matches)
    }

    async fn glob(&self, ctx: &RuntimeContext, pattern: &str, path: &str) -> Result<Vec<FileInfo>> {
        let search = if path == "/" || path.is_empty() {
            self.root.clone()
        } else {
            self.resolve(ctx, path)?
        };
        if !search.exists() {
            return Ok(Vec::new());
        }
        let matcher = match build_glob_matcher(pattern) {
            Some(m) => m,
            None => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        if search.is_file() {
            if let Some(name) = search.file_name().and_then(|n| n.to_str()) {
                if matcher.is_match(name) {
                    let display = self.display_path(&search);
                    out.push(FileInfo {
                        path: display,
                        is_directory: false,
                        size: search.metadata().map(|m| m.len()).unwrap_or(0),
                        modified_at: String::new(),
                    });
                }
            }
        } else {
            self.glob_recursive(&search, &matcher, &mut out).await?;
        }
        out.sort_by_key(|f| f.path.clone());
        Ok(out)
    }
}

impl LocalFilesystem {
    /// Greps a single file, applying the optional glob filter to its path.
    async fn grep_file(
        &self,
        path: &Path,
        base: &Path,
        matcher: &PatternMatcher,
        glob: Option<&GrepGlob>,
        out: &mut Vec<GrepMatch>,
    ) {
        let md = match path.metadata() {
            Ok(m) => m,
            Err(_) => return,
        };
        if !md.is_file() || md.len() > self.max_file_size as u64 {
            return;
        }
        if let Some(glob) = glob {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !glob.matches(&relative_path(path, base), name) {
                return;
            }
        }
        let Some(content) = read_text(path).await else {
            return;
        };
        {
            let display = self.display_path(path);
            for (i, line) in content.lines().enumerate() {
                if matcher.is_match(line) {
                    out.push(GrepMatch {
                        path: display.clone(),
                        line: i + 1,
                        text: line.to_string(),
                    });
                }
            }
        }
    }

    async fn grep_recursive(
        &self,
        base: &Path,
        matcher: &PatternMatcher,
        glob: Option<&GrepGlob>,
        out: &mut Vec<GrepMatch>,
    ) -> Result<()> {
        for (path, is_dir) in walk_entries(base).await {
            if is_dir {
                continue;
            }
            self.grep_file(&path, base, matcher, glob, out).await;
        }
        Ok(())
    }

    async fn glob_recursive(
        &self,
        base: &Path,
        matcher: &GlobMatcher,
        out: &mut Vec<FileInfo>,
    ) -> Result<()> {
        for (p, is_dir) in walk_entries(base).await {
            // The pattern is matched against the path relative to the search base,
            // mirroring `cd <base> && ls <pattern>` / `find <base> -name <pattern>`.
            let rel = relative_path(&p, base);
            if !matcher.is_match(&rel) {
                continue;
            }
            let display = self.display_path(&p);
            if is_dir {
                out.push(FileInfo {
                    path: format!("{display}/"),
                    is_directory: true,
                    size: 0,
                    modified_at: String::new(),
                });
            } else {
                let size = p.metadata().map(|m| m.len()).unwrap_or(0);
                out.push(FileInfo {
                    path: display,
                    is_directory: false,
                    size,
                    modified_at: String::new(),
                });
            }
        }
        Ok(())
    }
}

/// Reads a text file for grep: binary files (see [`looks_binary`]) are skipped,
/// non-UTF-8 content is decoded lossily instead of being dropped.
async fn read_text(path: &Path) -> Option<String> {
    let bytes = tokio::fs::read(path).await.ok()?;
    if looks_binary(&bytes) {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Returns true when `name` is a directory basename in [`GENERATED_DIRS`].
fn is_generated_name(name: &OsStr) -> bool {
    name.to_str().is_some_and(|n| GENERATED_DIRS.contains(&n))
}
