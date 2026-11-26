use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use agentscope_core::error::{Error, Result};
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext, Toolkit};
use async_trait::async_trait;
use serde_json::json;

/// Filesystem sandbox: every registered tool resolves paths inside `root`.
///
/// Path traversal (`..`) is rejected by logical normalization. Symlinks are **not** followed
/// out of the root in this prototype (documented limitation).
#[derive(Debug, Clone)]
pub struct FsSandbox {
    root: PathBuf,
    read_only: bool,
    max_read_bytes: usize,
    max_entries: usize,
}

impl FsSandbox {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            read_only: false,
            max_read_bytes: 8 << 20,
            max_entries: 10_000,
        }
    }

    pub fn read_only(mut self, v: bool) -> Self {
        self.read_only = v;
        self
    }

    pub fn max_read_bytes(mut self, n: usize) -> Self {
        self.max_read_bytes = n;
        self
    }

    /// Registers `read_file` / `write_file` / `list_dir` / `grep` / `glob` on `toolkit`.
    pub fn register(&self, toolkit: &Toolkit) {
        let sandbox = Arc::new(self.clone());
        for op in [
            FsOp::ReadFile,
            FsOp::WriteFile,
            FsOp::ListDir,
            FsOp::Grep,
            FsOp::Glob,
        ] {
            toolkit.register_tool(Arc::new(FsTool {
                op,
                sandbox: Arc::clone(&sandbox),
            }));
        }
    }

    fn root_norm(&self) -> PathBuf {
        normalize(&self.root)
    }

    fn resolve(&self, path: &str) -> Result<PathBuf> {
        let root_norm = self.root_norm();
        let root_canon = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| root_norm.clone());

        let raw = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            root_norm.join(path)
        };
        let norm = normalize(&raw);
        // Logical check first (rejects `..` traversal even for paths that do not exist yet).
        if !norm.starts_with(&root_norm) {
            return Err(Error::Tool(format!("path escapes sandbox: {path}")));
        }

        // Resolve the real path so a symlink cannot point outside the sandbox.
        let real = if norm.exists() {
            norm.canonicalize().map_err(Error::Io)?
        } else {
            // Write target: canonicalize the nearest existing ancestor, then re-append the tail.
            let mut ancestor = norm.clone();
            let mut tail: Vec<std::ffi::OsString> = Vec::new();
            loop {
                match ancestor.file_name() {
                    Some(name) => {
                        tail.push(name.to_os_string());
                        ancestor.pop();
                        if ancestor.exists() {
                            break;
                        }
                    }
                    None => {
                        return Err(Error::Tool(format!("path escapes sandbox: {path}")));
                    }
                }
            }
            let mut real = ancestor.canonicalize().map_err(Error::Io)?;
            for name in tail.into_iter().rev() {
                real.push(name);
            }
            real
        };
        if !real.starts_with(&root_canon) {
            return Err(Error::Tool(format!("path escapes sandbox: {path}")));
        }
        Ok(real)
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[derive(Debug, Clone, Copy)]
enum FsOp {
    ReadFile,
    WriteFile,
    ListDir,
    Grep,
    Glob,
}

struct FsTool {
    op: FsOp,
    sandbox: Arc<FsSandbox>,
}

#[async_trait]
impl Tool for FsTool {
    fn name(&self) -> &str {
        match self.op {
            FsOp::ReadFile => "read_file",
            FsOp::WriteFile => "write_file",
            FsOp::ListDir => "list_dir",
            FsOp::Grep => "grep",
            FsOp::Glob => "glob",
        }
    }

    fn description(&self) -> &str {
        match self.op {
            FsOp::ReadFile => "Read a UTF-8 file inside the sandbox.",
            FsOp::WriteFile => "Write a UTF-8 file inside the sandbox.",
            FsOp::ListDir => "List directory entries inside the sandbox.",
            FsOp::Grep => "Search file contents for a substring inside the sandbox.",
            FsOp::Glob => "Match file names with a * / ? glob inside the sandbox.",
        }
    }

    fn is_read_only(&self) -> bool {
        !matches!(self.op, FsOp::WriteFile)
    }

    fn parameters_schema(&self) -> serde_json::Value {
        match self.op {
            FsOp::ReadFile | FsOp::ListDir => json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
            FsOp::WriteFile => json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
            FsOp::Grep => json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string" }
                },
                "required": ["pattern"]
            }),
            FsOp::Glob => json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string" }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        match self.op {
            FsOp::ReadFile => {
                let path = arg_str(&args, "path")?;
                let p = self.sandbox.resolve(&path)?;
                let meta = tokio::fs::metadata(&p).await.map_err(Error::Io)?;
                if meta.len() as usize > self.sandbox.max_read_bytes {
                    return Err(Error::Tool(format!(
                        "file too large: {} bytes (max {})",
                        meta.len(),
                        self.sandbox.max_read_bytes
                    )));
                }
                let text = tokio::fs::read_to_string(&p).await.map_err(Error::Io)?;
                Ok(ToolResultBlock::text(text))
            }
            FsOp::WriteFile => {
                if self.sandbox.read_only {
                    return Err(Error::Tool("filesystem tool is read-only".into()));
                }
                let path = arg_str(&args, "path")?;
                let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
                if content.len() > self.sandbox.max_read_bytes {
                    return Err(Error::Tool(format!(
                        "content too large: {} bytes (max {})",
                        content.len(),
                        self.sandbox.max_read_bytes
                    )));
                }
                let p = self.sandbox.resolve(&path)?;
                if let Some(parent) = p.parent() {
                    tokio::fs::create_dir_all(parent).await.map_err(Error::Io)?;
                }
                tokio::fs::write(&p, content).await.map_err(Error::Io)?;
                Ok(ToolResultBlock::text(format!(
                    "wrote {} bytes",
                    content.len()
                )))
            }
            FsOp::ListDir => {
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
                let p = self.sandbox.resolve(path)?;
                let mut rd = tokio::fs::read_dir(&p).await.map_err(Error::Io)?;
                let mut names = Vec::new();
                while let Some(entry) = rd.next_entry().await.map_err(Error::Io)? {
                    names.push(entry.file_name().to_string_lossy().to_string());
                }
                names.sort();
                Ok(ToolResultBlock::text(names.join("\n")))
            }
            FsOp::Grep => {
                let pattern = arg_str(&args, "pattern")?;
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
                let dir = self.sandbox.resolve(path)?;
                let sandbox = Arc::clone(&self.sandbox);
                let out = tokio::task::spawn_blocking(move || run_grep(&sandbox, &dir, &pattern))
                    .await
                    .map_err(|e| Error::Tool(format!("grep task failed: {e}")))??;
                Ok(ToolResultBlock::text(out))
            }
            FsOp::Glob => {
                let pattern = arg_str(&args, "pattern")?;
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
                let dir = self.sandbox.resolve(path)?;
                let sandbox = Arc::clone(&self.sandbox);
                let out = tokio::task::spawn_blocking(move || run_glob(&sandbox, &dir, &pattern))
                    .await
                    .map_err(|e| Error::Tool(format!("glob task failed: {e}")))??;
                Ok(ToolResultBlock::text(out))
            }
        }
    }
}

fn arg_str(args: &JsonMap, key: &str) -> Result<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| Error::Tool(format!("missing string argument: {key}")))
}

fn run_grep(sandbox: &FsSandbox, dir: &Path, pattern: &str) -> Result<String> {
    let mut out = String::new();
    let mut files = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if !path.is_file() {
                continue;
            }
            files += 1;
            if files > sandbox.max_entries {
                return Ok(out);
            }
            let meta = entry.metadata()?;
            if meta.len() as usize > sandbox.max_read_bytes {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&path) {
                for (i, line) in content.lines().enumerate() {
                    if line.contains(pattern) {
                        out.push_str(&format!("{}:{}: {}\n", path.display(), i + 1, line));
                    }
                }
            }
        }
    }
    Ok(out)
}

fn run_glob(sandbox: &FsSandbox, dir: &Path, pattern: &str) -> Result<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                stack.push(path.clone());
            }
            if glob_match(pattern, &name) {
                out.push(path.display().to_string());
                if out.len() >= sandbox.max_entries {
                    break;
                }
            }
        }
    }
    out.sort();
    Ok(out.join("\n"))
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    fn rec(p: &[char], t: &[char]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        match p[0] {
            '*' => rec(&p[1..], t) || (!t.is_empty() && rec(p, &t[1..])),
            '?' => !t.is_empty() && rec(&p[1..], &t[1..]),
            c => !t.is_empty() && t[0] == c && rec(&p[1..], &t[1..]),
        }
    }
    rec(&p, &t)
}
