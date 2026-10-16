//! Shell execution tool backed by a `SandboxFilesystem`, mirroring `ShellExecuteTool`.
use std::sync::Arc;

use async_trait::async_trait;
use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::tool::{is_dangerous_command, Tool, ToolExecutionContext, Toolkit};

use crate::filesystem::SandboxFilesystem;

/// Executes a shell command in the sandbox and returns combined output + exit code.
pub struct ShellExecuteTool {
    sandbox: Arc<dyn SandboxFilesystem>,
}

impl ShellExecuteTool {
    pub const NAME: &'static str = "execute";

    pub fn new(sandbox: Arc<dyn SandboxFilesystem>) -> Self {
        Self { sandbox }
    }

    pub fn register(toolkit: &Toolkit, sandbox: Arc<dyn SandboxFilesystem>) {
        toolkit.register_tool(Arc::new(Self::new(sandbox)));
    }
}
fn opt_str_arg(args: &JsonMap, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}

fn int_arg(args: &JsonMap, key: &str) -> Option<u32> {
    args.get(key).and_then(|v| v.as_u64()).map(|u| u as u32)
}

/// Prefixes `command` with a `cd` into `working_directory`.
pub fn command_with_working_directory(working_directory: &str, command: &str) -> String {
    if cfg!(windows) {
        // 执行器是 pwsh（见 filesystem/sandbox.rs build_windows_command），不能用 cmd 的 `cd /d` 语法。
        // pwsh 中 `cd /d "path"` 会被解析为 `Set-Location /d <path>` 而报 "A positional parameter cannot be found"。
        // 改用 pwsh 原生 `Set-Location -LiteralPath`，并用 `;` 分隔（pwsh 7 虽支持 `&&`，但 `;` 更稳妥）。
        format!(
            "Set-Location -LiteralPath '{}' ; {}",
            working_directory.replace('\'', "''"),
            command
        )
    } else {
        format!("cd '{}' && {}", working_directory.replace('\'', "'\\\\''"), command)
    }
}

#[async_trait]
impl Tool for ShellExecuteTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        // Windows 下命令由 PowerShell (pwsh) 执行（见 filesystem/sandbox.rs build_windows_command），
        // 而 pwsh 把 `$` 当变量/插值前缀：未定义的 `$5`/`$1` 会展开为空串而“消失”。
        // 把该语义提示写进 description，避免模型按 cmd/bash 直觉写命令导致 `$` 被吞。
        if cfg!(windows) {
            "Execute a shell command and return combined output + exit code. Use for git, npm, build, test, and other terminal operations. If a dedicated tool exists (e.g., read_file, write_file), you MUST use it instead of shell commands. Note: on Windows the command runs in PowerShell (pwsh), not cmd or bash. PowerShell treats `$` as a variable/interpolation prefix, so an unquoted or double-quoted `$` is expanded and silently eaten (e.g. `echo price is $5` prints `price is `; `awk {print $1}` loses `$1`). Always wrap any literal `$` in SINGLE quotes: `echo 'price is $5'`, `awk '{print $1}'`, `git log --format='%h %s'`. Single quotes keep text verbatim in pwsh; the same applies to other PowerShell metacharacters such as backtick (escape char), `@(...)` (array splat) and `#` (comment)."
        } else {
            "Execute a shell command and return combined output + exit code. Use for git, npm, build, test, and other terminal operations. If a dedicated tool exists (e.g., read_file, write_file), you MUST use it instead of shell commands."
        }
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to execute"},
                "working_directory": {"type": "string", "description": "Working directory (relative to workspace root, optional)"},
                "timeout": {"type": "integer", "description": "Timeout in seconds (default: 30)"}
            },
            "required": ["command"]
        })
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let rt = RuntimeContext::default();
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| agentscope_core::Error::Tool("Missing string argument: command".into()))?;
        if is_dangerous_command(&command) {
            return Ok(ToolResultBlock::error(
                "Command contains a dangerous fragment (e.g. 'rm -rf', 'dd', 'mkfs', 'chmod 777', 'kill -9'). Refuse to execute without explicit approval.",
            ));
        }
        let wd = opt_str_arg(&args, "working_directory");
        let timeout = int_arg(&args, "timeout").unwrap_or(30);

        let effective_command = if let Some(wd) = wd {
            let wd = wd.trim();
            if wd.is_empty() {
                command
            } else if wd.starts_with('/') || wd.starts_with('~') || wd.contains("..") {
                return Ok(ToolResultBlock::error(
                    "working_directory must be a relative path within the workspace (absolute paths, '~', and '..' are not allowed).",
                ));
            } else {
                command_with_working_directory(wd, &command)
            }
        } else {
            command
        };

        let started = std::time::Instant::now();
        tracing::info!(
            command = %effective_command,
            timeout_s = timeout,
            "shell execute start"
        );
        let result = self
            .sandbox
            .execute(&rt, &effective_command, timeout)
            .await;
        tracing::info!(
            command = %effective_command,
            elapsed_ms = started.elapsed().as_millis() as u64,
            ok = result.is_ok(),
            "shell execute done"
        );
        let result = result?;
        let mut sb = format!("Exit code: {}\n", result.exit_code);
        if !result.output.trim().is_empty() {
            sb.push('\n');
            sb.push_str(&result.output);
        }
        if result.truncated {
            sb.push_str("\n(output was truncated)");
        }
        Ok(ToolResultBlock::text(sb))
    }
}

#[cfg(test)]
mod tests {
    use super::command_with_working_directory;

    #[test]
    fn windows_uses_pwsh_syntax() {
        let s = command_with_working_directory("src/foo bar", "git status");
        if cfg!(windows) {
            assert!(s.starts_with("Set-Location -LiteralPath '"));
            assert!(s.contains("src/foo bar"));
            assert!(s.ends_with(" ; git status"));
            assert!(!s.contains("cd /d"));
        } else {
            assert!(s.starts_with("cd '"));
            assert!(s.ends_with(" && git status"));
        }
    }

    #[test]
    fn windows_escapes_single_quote() {
        let s = command_with_working_directory("a'b", "echo hi");
        if cfg!(windows) {
            assert!(s.contains("'a''b'"));
        } else {
            assert!(s.contains("a'\\''b"));
        }
    }
}

