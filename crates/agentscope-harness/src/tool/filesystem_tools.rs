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
            description: "Read file content with line numbers. Supports pagination via offset and limit.".into(),
            read_only: true,
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path to read"},
                    "offset": {"type": "integer", "description": "Start line (0-indexed). Default: 0", "default": 0},
                    "limit": {"type": "integer", "description": "Max lines to return. Default: 0 (all lines)", "default": 0}
                },
                "required": ["path"]
            }),
        }
    }

    pub fn write_file(fs: Arc<dyn Filesystem>) -> Self {
        Self {
            fs,
            name: "write_file".into(),
            description: "Write content to a new file, creating parent directories if needed.".into(),
            read_only: false,
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Target file path"},
                    "content": {"type": "string", "description": "File content to write"}
                },
                "required": ["path", "content"]
            }),
        }
    }

    pub fn edit_file(fs: Arc<dyn Filesystem>) -> Self {
        Self {
            fs,
            name: "edit_file".into(),
            description: "Perform exact string replacement in a file. The old_string must be unique unless replace_all is true.".into(),
            read_only: false,
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File to edit"},
                    "old_string": {"type": "string", "description": "Text to find"},
                    "new_string": {"type": "string", "description": "Replacement text"},
                    "replace_all": {"type": "boolean", "description": "Replace all occurrences (default: false)", "default": false}
                },
                "required": ["path", "old_string", "new_string"]
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

fn int_arg(args: &JsonMap, key: &str) -> Option<usize> {
    args.get(key).and_then(|v| v.as_u64()).map(|u| u as usize)
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
                let path = str_arg(&args, "path")?;
                let offset = int_arg(&args, "offset").unwrap_or(0);
                let limit = int_arg(&args, "limit").unwrap_or(0);
                match self.fs.read(&rt, &path, offset, limit).await {
                    Ok(d) => d.content,
                    Err(e) => return Ok(ToolResultBlock::error(e.to_string())),
                }
            }
            "write_file" => {
                let path = str_arg(&args, "path")?;
                let content = str_arg(&args, "content")?;
                match self.fs.write(&rt, &path, &content).await {
                    Ok(()) => format!("Written to {path}"),
                    Err(e) => return Ok(ToolResultBlock::error(e.to_string())),
                }
            }
            "edit_file" => {
                let path = str_arg(&args, "path")?;
                let old = str_arg(&args, "old_string")?;
                let new = str_arg(&args, "new_string")?;
                let replace_all = args
                    .get("replace_all")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                match self.fs.edit(&rt, &path, &old, &new, replace_all).await {
                    Ok(n) => format!("Edited {path} ({n} replacement(s))"),
                    Err(e) => return Ok(ToolResultBlock::error(e.to_string())),
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
