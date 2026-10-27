//! Basic filesystem tools (read / write / list), mirroring the core file tools.
use async_trait::async_trait;

use crate::error::{Error, Result};
use crate::message::{ContentBlock, JsonMap, TextBlock, ToolResultBlock};
use crate::tool::ToolExecutionContext;

use super::tool::Tool;

/// A filesystem tool that reads, writes and lists files under a base directory.
#[derive(Debug, Clone)]
pub struct FilesystemTool {
    name: String,
    description: String,
    read_only: bool,
    base_dir: std::path::PathBuf,
}

impl FilesystemTool {
    pub fn new(base_dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            name: "filesystem".into(),
            description: "Read, write and list files under the working directory.".into(),
            read_only: false,
            base_dir: base_dir.into(),
        }
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn read_only(mut self, v: bool) -> Self {
        self.read_only = v;
        self
    }

    fn resolve(&self, path: &str) -> std::path::PathBuf {
        let p = std::path::Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.base_dir.join(p)
        }
    }
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
        serde_json::json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["read", "write", "list"],
                    "description": "Operation to perform"
                },
                "path": { "type": "string", "description": "File or directory path" },
                "content": { "type": "string", "description": "Content to write (write only)" }
            },
            "required": ["operation", "path"]
        })
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let op = args
            .get("operation")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Tool("operation is required".into()))?;
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Tool("path is required".into()))?;
        let resolved = self.resolve(path);

        let output = match op {
            "read" => {
                let content = tokio::fs::read_to_string(&resolved)
                    .await
                    .map_err(|e| Error::Io(e))?;
                content
            }
            "write" => {
                if self.read_only {
                    return Err(Error::Tool("filesystem tool is read-only".into()));
                }
                let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(parent) = resolved.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| Error::Io(e))?;
                }
                tokio::fs::write(&resolved, content)
                    .await
                    .map_err(|e| Error::Io(e))?;
                format!("Wrote {} bytes to {}", content.len(), resolved.display())
            }
            "list" => {
                let mut entries = tokio::fs::read_dir(&resolved)
                    .await
                    .map_err(|e| Error::Io(e))?;
                let mut names = Vec::new();
                while let Some(entry) = entries.next_entry().await.map_err(|e| Error::Io(e))? {
                    names.push(entry.file_name().to_string_lossy().to_string());
                }
                names.sort();
                names.join("\n")
            }
            other => return Err(Error::Tool(format!("unknown operation: {}", other))),
        };

        Ok(ToolResultBlock::new(
            "",
            &self.name,
            vec![ContentBlock::Text(TextBlock::new(output))],
        ))
    }
}
