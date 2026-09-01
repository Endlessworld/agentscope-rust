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

/// Edit-file parameters; `operations` supports batching multiple edits.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct EditFileParams {
    /// Batch of edit operations.
    pub operations: Vec<EditOperation>,
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
            description: "Write content to one or more files, creating parent directories if needed. Accepts a batch of write operations.".into(),
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
            description: "Perform exact string replacement in one or more files. old_string must be unique unless replace_all is true. Accepts a batch of edit operations.".into(),
            read_only: false,
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "operations": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string", "description": "File to edit"},
                                "old_string": {"type": "string", "description": "Text to find"},
                                "new_string": {"type": "string", "description": "Replacement text"},
                                "replace_all": {"type": "boolean", "description": "Replace all occurrences (default: false)", "default": false}
                            },
                            "required": ["path", "old_string", "new_string"]
                        },
                        "description": "要编辑的文件操作列表，支持批量编辑多个文件"
                    }
                },
                "required": ["operations"]
            }),
        }
    }

    pub fn grep_files(fs: Arc<dyn Filesystem>) -> Self {
        Self {
            fs,
            name: "grep_files".into(),
            description: "Search file contents for a literal text pattern.".into(),
            read_only: true,
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Literal text pattern to search for"},
                    "path": {"type": "string", "description": "Directory or file to search"},
                    "glob": {"type": "string", "description": "Optional file glob filter (e.g. *.java)"}
                },
                "required": ["pattern"]
            }),
        }
    }

    pub fn glob_files(fs: Arc<dyn Filesystem>) -> Self {
        Self {
            fs,
            name: "glob_files".into(),
            description: "Find files matching a glob pattern.".into(),
            read_only: true,
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Glob pattern (e.g. **/*.java)"},
                    "path": {"type": "string", "description": "Base directory to search from", "default": "/"}
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
                for op in &params.operations {
                    match self.fs.write(&rt, &op.path, &op.content).await {
                        Ok(()) => outputs.push(format!("### {}\nWritten", op.path)),
                        Err(e) => outputs.push(format!("### {}\nERROR: {e}", op.path)),
                    }
                }
                if outputs.is_empty() {
                    "No operations to write".to_string()
                } else {
                    outputs.join("\n\n")
                }
            }
            "edit_file" => {
                let params: EditFileParams =
                    serde_json::from_value(serde_json::Value::Object(args.clone()))
                        .map_err(|e| {
                            agentscope_core::Error::Tool(format!("Invalid edit_file params: {e}"))
                        })?;
                let mut outputs = Vec::new();
                for op in &params.operations {
                    match self
                        .fs
                        .edit(&rt, &op.path, &op.old_string, &op.new_string, op.replace_all)
                        .await
                    {
                        Ok(n) => outputs.push(format!("### {}\n{} replacement(s)", op.path, n)),
                        Err(e) => outputs.push(format!("### {}\nERROR: {e}", op.path)),
                    }
                }
                if outputs.is_empty() {
                    "No operations to edit".to_string()
                } else {
                    outputs.join("\n\n")
                }
            }
            "grep_files" => {
                let pattern = str_arg(&args, "pattern")?;
                let path = opt_str_arg(&args, "path");
                let glob = opt_str_arg(&args, "glob");
                match self
                    .fs
                    .grep(&rt, &pattern, path.as_deref(), glob.as_deref())
                    .await
                {
                    Ok(matches) if matches.is_empty() => "No matches found".to_string(),
                    Ok(matches) => matches
                        .iter()
                        .map(|m| format!("{}:{}:{}", m.path, m.line, m.text))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    Err(e) => return Ok(ToolResultBlock::error(e.to_string())),
                }
            }
            "glob_files" => {
                let pattern = str_arg(&args, "pattern")?;
                let path = opt_str_arg(&args, "path").unwrap_or_else(|| "/".into());
                match self.fs.glob(&rt, &pattern, &path).await {
                    Ok(files) if files.is_empty() => "No matching files found".to_string(),
                    Ok(files) => files
                        .iter()
                        .map(|f| {
                            if f.is_directory {
                                format!("{}/", f.path)
                            } else {
                                format!("{} ({} bytes)", f.path, f.size)
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                    Err(e) => return Ok(ToolResultBlock::error(e.to_string())),
                }
            }
            "list_files" => {
                let path = str_arg(&args, "path")?;
                match self.fs.ls(&rt, &path).await {
                    Ok(entries) if entries.is_empty() => {
                        format!("Empty or not a directory: {path}")
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
