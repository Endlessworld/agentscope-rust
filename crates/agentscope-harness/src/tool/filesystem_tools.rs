//! Agent-callable filesystem tools backed by a `Filesystem`, mirroring `FilesystemTool`.
//! Registers `read_file`, `write_file`, `edit_file`, `grep_files`, `glob_files`, `list_files`.
use std::sync::Arc;

use async_trait::async_trait;
use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::JsonMap;
use agentscope_core::tool::{Tool, ToolExecutionContext, Toolkit};
use agentscope_core::error::Result;
use agentscope_core::message::ToolResultBlock;

use crate::filesystem::Filesystem;

/// Metadata key carrying ACP file diffs (`[{path, old_text, new_text}]`) produced by
/// a write/edit tool, so the ACP event mapper can emit `ToolCallContent::Diff` for
/// clients that render inline diffs.
pub const METADATA_ACP_DIFFS: &str = "acp_diffs";

/// Builds one ACP diff entry as JSON (`path`, optional `old_text`, `new_text`).
fn diff_entry(path: &str, old_text: Option<String>, new_text: String) -> serde_json::Value {
    let mut entry = serde_json::Map::new();
    entry.insert("path".to_string(), serde_json::Value::String(path.to_string()));
    if let Some(old) = old_text {
        entry.insert("old_text".to_string(), serde_json::Value::String(old));
    }
    entry.insert("new_text".to_string(), serde_json::Value::String(new_text));
    serde_json::Value::Object(entry)
}

/// Attaches ACP diff entries to a tool result's metadata (no-op when empty).
fn with_diffs(mut result: ToolResultBlock, diffs: Vec<serde_json::Value>) -> ToolResultBlock {
    if !diffs.is_empty() {
        result.metadata.insert(
            METADATA_ACP_DIFFS.to_string(),
            serde_json::Value::Array(diffs),
        );
    }
    result
}

/// Read-file parameters, mirroring `ReadFileTool.FilesReader`.
/// `file_paths` supports batching multiple absolute paths.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadFileParams {
    /// Absolute path(s) to read; batch is supported.
    #[serde(rename = "filePath")]
    pub file_paths: Vec<String>,
    /// 0-indexed starting line offset (default: file start).
    #[serde(default)]
    pub offset: usize,
    /// Max lines to read per file (default: 100).
    #[serde(default = "default_read_limit")]
    pub limit: usize,
}

fn default_read_limit() -> usize {
    100
}

/// A single write operation.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct WriteOperation {
    /// Target file path.
    pub path: String,
    /// File content to write.
    pub content: String,
}

/// Write-file parameters; `operations` supports batching multiple writes.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct WriteFileParams {
    /// Batch of write operations.
    pub operations: Vec<WriteOperation>,
}

/// A single edit operation.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct EditOperation {
    /// File to edit.
    pub path: String,
    /// Text to find.
    pub old_string: String,
    /// Replacement text.
    pub new_string: String,
    /// Replace all occurrences (default: false).
    #[serde(default)]
    pub replace_all: bool,
}

/// Edit-file parameters. Two shapes are accepted and normalised by
/// [`EditFileParams::into_operations`]:
///
/// - batched (preferred): `{"operations": [{path, old_string, new_string, replace_all?}, ...]}`
/// - flat single operation: `{path, old_string, new_string, replace_all?}`
///
/// The flat form is kept because the tool description advertises top-level
/// `path`/`old_string`/`new_string`; rejecting it only produced an unactionable
/// `"path" is a required property` validation error.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct EditFileParams {
    /// Batch of edit operations (preferred form).
    #[serde(default)]
    pub operations: Vec<EditOperation>,
    /// Flat single-operation form: target file path.
    #[serde(default)]
    pub path: Option<String>,
    /// Flat single-operation form: text to find.
    #[serde(default)]
    pub old_string: Option<String>,
    /// Flat single-operation form: replacement text.
    #[serde(default)]
    pub new_string: Option<String>,
    /// Flat single-operation form: replace all occurrences.
    #[serde(default)]
    pub replace_all: bool,
}

impl EditFileParams {
    /// Normalises both accepted shapes into a list of operations.
    ///
    /// An empty result means neither shape supplied a usable operation; the
    /// caller reports that explicitly instead of silently doing nothing.
    pub fn into_operations(self) -> Vec<EditOperation> {
        if !self.operations.is_empty() {
            return self.operations;
        }
        match (self.path, self.old_string, self.new_string) {
            (Some(path), Some(old_string), Some(new_string)) => vec![EditOperation {
                path,
                old_string,
                new_string,
                replace_all: self.replace_all,
            }],
            _ => Vec::new(),
        }
    }
}

/// A single filesystem tool; constructed via the factory methods below.
#[derive(Clone)]
pub struct FilesystemTool {
    fs: Arc<dyn Filesystem>,
    name: String,
    description: String,
    read_only: bool,
    schema: serde_json::Value,
}
impl FilesystemTool {
    pub fn read_file(fs: Arc<dyn Filesystem>) -> Self {
        Self {
            fs,
            name: "read_file".into(),
            description: "Read file content with line numbers. Accepts a batch of absolute paths; supports pagination via offset and limit.".into(),
            read_only: true,
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "filePath": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "要读取的文件或目录的绝对路径列表，支持批量传入多个路径"
                    },
                    "offset": {"type": "integer", "description": "起始行偏移行（从0开始计数），默认从文件开头读取", "default": 0},
                    "limit": {"type": "integer", "description": "最大读取行数，默认100行，防止一次性读取过大文件", "default": 100}
                },
                "required": ["filePath"]
            }),
        }
    }

    pub fn write_file(fs: Arc<dyn Filesystem>) -> Self {
        Self {
            fs,
            name: "write_file".into(),
            description: "Write content to one or more files, creating parent directories if needed. Accepts a batch of write operations. This tool cannot overwrite existing files".into(),
            read_only: false,
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "operations": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string", "description": "Target file path"},
                                "content": {"type": "string", "description": "File content to write"}
                            },
                            "required": ["path", "content"]
                        },
                        "description": "要写入的文件操作列表，支持批量写入多个文件"
                    }
                },
                "required": ["operations"]
            }),
        }
    }

    pub fn edit_file(fs: Arc<dyn Filesystem>) -> Self {
        Self {
            fs,
            name: "edit_file".into(),
            description: r#"在单个或多个文件中执行精确字符串替换，支持批量编辑。
                             调用格式（二选一，推荐批量）：
                             1) 批量：{"operations":[{"path":"filePath1","old_string":"aaa","new_string":"bbb","replace_all":false}]}
                             2) 单个：{"path":"filePath1","old_string":"aaa","new_string":"bbb"}
                             参数：
                             - path（必填）：目标文件路径。
                             - old_string（必填）：要替换的原字符串（不得为空）。
                             - new_string（必填）：替换后的新字符串。
                             - replace_all（可选，默认 false）：为 true 时替换所有匹配项；否则 old_string 在文件中必须唯一，否则不执行任何替换。
                             使用要求：
                             - 所有替换均按精确字符串匹配（含换行与缩进）执行。
                             - 单次调用建议不超过 10 个操作；new_string 很长时请拆成多次调用，避免单次参数过大。
                             - 先想清楚需要修改哪些文件和内容，再一次性完成所有编辑。
                             返回：成功时每个文件给出 ### <path> 与 n replacement(s)，末尾以 Applied X/Y operation(s) 汇总；失败时会标出是第几个操作。
             "#.into(),
            read_only: false,
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "operations": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string", "description": "File to edit is required"},
                                "old_string": {"type": "string", "description": "Text to find"},
                                "new_string": {"type": "string", "description": "Replacement text"},
                                "replace_all": {"type": "boolean", "description": "Replace all occurrences (default: false)", "default": false}
                            },
                            "required": ["path", "old_string", "new_string"]
                        },
                        "description": "要编辑的文件操作列表，支持批量编辑多个文件"
                    },
                    "path": {"type": "string", "description": "单个编辑形式的目标文件路径（与 operations 二选一）"},
                    "old_string": {"type": "string", "description": "单个编辑形式的原字符串"},
                    "new_string": {"type": "string", "description": "单个编辑形式的替换文本"},
                    "replace_all": {"type": "boolean", "description": "单个编辑形式：是否替换所有匹配项（默认 false）", "default": false}
                }
            }),
        }
    }

    pub fn grep_files(fs: Arc<dyn Filesystem>) -> Self {
        Self {
            fs,
            name: "grep_files".into(),
            description: "Search file contents with a regular expression pattern (supports inline flags like (?i); invalid regex falls back to literal substring matching). Returns at most 100 matches by default to keep tool output bounded.".into(),
            read_only: true,
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "正则表达式（支持 (?i) 等内联 flag；非法正则会回退为字面量匹配）"},
                    "path": {"type": "string", "description": "Directory or file to search; defaults to the filesystem root"},
                    "glob": {"type": "string", "description": "Optional file filter glob. A pattern without / matches the file name at any depth (e.g. *.rs); a pattern containing / matches the path relative to path (e.g. src/**/*.java)"},
                    "ignore_case": {"type": "boolean", "description": "Case-insensitive pattern matching (default false)", "default": false},
                    "output_mode": {"type": "string", "enum": ["content", "files_with_matches", "count"], "description": "content = matching lines (default); files_with_matches = paths only; count = matches per file", "default": "content"},
                    "type": {"type": "string", "description": "Optional file type name (rust, python, js, ts, go, java, c, cpp, markdown, json, toml, yaml, html, css, shell). Mutually exclusive with glob"},
                    "paths": {"type": "array", "items": {"type": "string"}, "description": "Optional list of files/dirs to search instead of path"},
                    "limit": {"type": "integer", "description": "Maximum matches to return (default/recommended: 100; hard maximum: 1000)", "default": 100}
                },
                "required": ["pattern"]
            }),
        }
    }

    pub fn glob_files(fs: Arc<dyn Filesystem>) -> Self {
        Self {
            fs,
            name: "glob_files".into(),
            description: "Find files matching a glob pattern relative to the base path (supports *, ?, **, [abc], {a,b}). Returns at most 200 files by default to keep tool output bounded.".into(),
            read_only: true,
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Glob pattern, matched relative to the base path (e.g. *.java, **/*.java, src/**/*.rs)"},
                    "path": {"type": "string", "description": "Base directory to search from", "default": "/"},
                    "limit": {"type": "integer", "description": "Maximum files to return (default/recommended: 200; hard maximum: 1000)", "default": 200}
                },
                "required": ["pattern"]
            }),
        }
    }

    pub fn list_files(fs: Arc<dyn Filesystem>) -> Self {
        Self {
            fs,
            name: "list_files".into(),
            description: "List files and directories at the given path.".into(),
            read_only: true,
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Directory path to list"}
                },
                "required": ["path"]
            }),
        }
    }

    /// Registers all filesystem tools into a toolkit.
    pub fn register(toolkit: &Toolkit, fs: Arc<dyn Filesystem>) {
        for tool in [
            Self::read_file(fs.clone()),
            Self::write_file(fs.clone()),
            Self::edit_file(fs.clone()),
            Self::grep_files(fs.clone()),
            Self::glob_files(fs.clone()),
            Self::list_files(fs),
        ] {
            toolkit.register_tool(Arc::new(tool));
        }
    }
}
fn str_arg(args: &JsonMap, key: &str) -> Result<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| agentscope_core::Error::Tool(format!("Missing string argument: {key}")))
}

fn opt_str_arg(args: &JsonMap, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}


/// Resolves a search-tool `limit` argument: missing → `default_limit`, present →
/// clamped to `max_limit`. Returns an error when the caller passes a value < 1.
fn search_limit(args: &JsonMap, key: &str, default_limit: usize, max_limit: usize) -> Result<usize> {
    match args.get(key).and_then(|v| v.as_u64()) {
        None => Ok(default_limit),
        Some(0) => Err(agentscope_core::Error::Tool(format!(
            "{key} must be greater than 0"
        ))),
        Some(u) => Ok((u as usize).min(max_limit)),
    }
}

/// Appends a truncation notice when `total` exceeds `limit`, mirroring the Java
/// `FilesystemTool#appendTruncationNotice`.
pub(super) fn append_truncation_notice(output: String, total: usize, limit: usize, result_label: &str) -> String {
    if total <= limit {
        return output;
    }
    let mut out = output;
    out.push_str(&format!(
        "\n\n(Truncated: showing {} of {} {result_label}. Narrow the path/pattern or increase limit to see more.)",
        limit, total
    ));
    out
}
#[async_trait]
impl Tool for FilesystemTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.schema.clone()
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let rt = RuntimeContext::default();
        let output = match self.name.as_str() {
            "read_file" => {
                let params: ReadFileParams =
                    serde_json::from_value(serde_json::Value::Object(args.clone()))
                        .map_err(|e| {
                            agentscope_core::Error::Tool(format!("Invalid read_file params: {e}"))
                        })?;
                let offset = params.offset;
                let limit = params.limit;
                let mut outputs = Vec::new();
                for path in &params.file_paths {
                    match self.fs.read(&rt, path, offset, limit).await {
                        Ok(d) => outputs.push(format!("### {path}\n{}", d.content)),
                        Err(e) => outputs.push(format!("### {path}\nERROR: {e}")),
                    }
                }
                if outputs.is_empty() {
                    "No files to read".to_string()
                } else {
                    outputs.join("\n\n")
                }
            }
            "write_file" => {
                let params: WriteFileParams =
                    serde_json::from_value(serde_json::Value::Object(args.clone()))
                        .map_err(|e| {
                            agentscope_core::Error::Tool(format!("Invalid write_file params: {e}"))
                        })?;
                let mut outputs = Vec::new();
                let mut diffs = Vec::new();
                for op in &params.operations {
                    // Capture the pre-write content (None for a new file) so the ACP
                    // client can render a proper diff. A failed read is non-fatal.
                    let before = self
                        .fs
                        .read(&rt, &op.path, 0, 0)
                        .await
                        .ok()
                        .map(|d| d.content);
                    match self.fs.write(&rt, &op.path, &op.content).await {
                        Ok(()) => {
                            outputs.push(format!(" {} Written", op.path));
                            diffs.push(diff_entry(&op.path, before, op.content.clone()));
                        }
                        Err(e) => outputs.push(format!(" {}\nERROR: {e}", op.path)),
                    }
                }
                let text = if outputs.is_empty() {
                    "No operations to write".to_string()
                } else {
                    outputs.join("\n\n")
                };
                return Ok(with_diffs(ToolResultBlock::text(text), diffs));
            }
            "edit_file" => {
                let params: EditFileParams =
                    serde_json::from_value(serde_json::Value::Object(args.clone()))
                        .map_err(|e| {
                            agentscope_core::Error::Tool(format!("Invalid edit_file params: {e}"))
                        })?;
                let operations = params.into_operations();
                if operations.is_empty() {
                    return Ok(ToolResultBlock::text(
                        "No operations provided: pass `operations: [{path, old_string, new_string}]` \
                         or the flat `{path, old_string, new_string}` form.",
                    ));
                }
                let total = operations.len();
                let mut outputs = Vec::new();
                let mut diffs = Vec::new();
                let mut applied = 0usize;
                for (i, op) in operations.iter().enumerate() {
                    let before = self
                        .fs
                        .read(&rt, &op.path, 0, 0)
                        .await
                        .ok()
                        .map(|d| d.content);
                    match self
                        .fs
                        .edit(&rt, &op.path, &op.old_string, &op.new_string, op.replace_all)
                        .await
                    {
                        Ok(n) => {
                            applied += 1;
                            outputs.push(format!("{} {n} replacement(s)", op.path));
                            if let Some(old) = before {
                                let new = self
                                    .fs
                                    .read(&rt, &op.path, 0, 0)
                                    .await
                                    .ok()
                                    .map(|d| d.content)
                                    .unwrap_or_default();
                                diffs.push(diff_entry(&op.path, Some(old), new));
                            }
                        }
                        Err(e) => outputs
                            .push(format!("{}\nERROR (operation #{}): {e}", op.path, i + 1)),
                    }
                }
                let text = format!(
                    "{}\n\nApplied {applied}/{total} operation(s)",
                    outputs.join("\n\n")
                );
                return Ok(with_diffs(ToolResultBlock::text(text), diffs));
            }
            "grep_files" => {
                let pattern = super::grep_options::pattern(&args)?;
                let glob = super::grep_options::glob(&args)?;
                let limit = search_limit(&args, "limit", 100, 1000)?;
                let mode = opt_str_arg(&args, "output_mode").unwrap_or_else(|| "content".into());
                let mut matches = Vec::new();
                for target in super::grep_options::targets(&args) {
                    let found = match self
                        .fs
                        .grep(&rt, &pattern, Some(&target), glob.as_deref())
                        .await
                    {
                        Ok(found) => found,
                        Err(e) => return Ok(ToolResultBlock::error(e.to_string())),
                    };
                    matches.extend(found);
                }
                super::grep_options::format_matches(&matches, limit, &mode)
            }
            "glob_files" => {
                let pattern = str_arg(&args, "pattern")?;
                let path = opt_str_arg(&args, "path").unwrap_or_else(|| "/".into());
                let limit = search_limit(&args, "limit", 200, 1000)?;
                match self.fs.glob(&rt, &pattern, &path).await {
                    Ok(files) if files.is_empty() => "No matching files found".to_string(),
                    Ok(files) => {
                        let total = files.len();
                        let output = files
                            .iter()
                            .take(limit)
                            .map(|f| {
                                if f.is_directory {
                                    format!("{}/", f.path)
                                } else {
                                    format!("{} ({} bytes)", f.path, f.size)
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        append_truncation_notice(output, total, limit, "files")
                    }
                    Err(e) => return Ok(ToolResultBlock::error(e.to_string())),
                }
            }
            "list_files" => {
                let path = str_arg(&args, "path")?;
                match self.fs.ls(&rt, &path).await {
                    Ok(entries) if entries.is_empty() => {
                        format!("Empty directory: {path}")
                    }
                    Ok(entries) => entries
                        .iter()
                        .map(|f| {
                            let prefix = if f.is_directory { "[DIR]  " } else { "[FILE] " };
                            let suffix = if f.is_directory {
                                String::new()
                            } else {
                                format!(" ({} bytes)", f.size)
                            };
                            format!("{prefix}{}{suffix}", f.path)
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                    Err(e) => return Ok(ToolResultBlock::error(e.to_string())),
                }
            }
            other => return Err(agentscope_core::Error::Tool(format!("unknown operation: {other}"))),
        };
        Ok(ToolResultBlock::text(output))
    }
}
