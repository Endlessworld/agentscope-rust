//! Shell execution tool backed by a `SandboxFilesystem`, mirroring `ShellExecuteTool`.
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;
use agentscope_core::message::{
    JsonMap, ToolExecutionDetails, ToolExecutionOutcome, ToolResultBlock,
};
use agentscope_core::tool::{is_dangerous_command, Tool, ToolExecutionContext, Toolkit};
use async_trait::async_trait;

use super::output_compact::{compact_grep_like, CompactConfig};
use crate::filesystem::SandboxFilesystem;

/// Executes a shell command in the sandbox and returns combined output + exit code.
pub struct ShellExecuteTool {
    sandbox: Arc<dyn SandboxFilesystem>,
    compact: CompactConfig,
}

impl ShellExecuteTool {
    pub const NAME: &'static str = "execute";

    /// Creates the tool with shell-output compaction enabled (see [`CompactConfig`]).
    pub fn new(sandbox: Arc<dyn SandboxFilesystem>) -> Self {
        Self {
            sandbox,
            compact: CompactConfig::from_env(),
        }
    }

    /// Creates the tool with an explicit compaction configuration.
    pub fn with_output_compaction(
        sandbox: Arc<dyn SandboxFilesystem>,
        compact: CompactConfig,
    ) -> Self {
        Self { sandbox, compact }
    }

    pub fn register(toolkit: &Toolkit, sandbox: Arc<dyn SandboxFilesystem>) {
        toolkit.register_tool(Arc::new(Self::new(sandbox)));
    }
}
fn opt_str_arg(args: &JsonMap, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Renders a finished command into the tool result the model sees.
///
/// Kept separate from [`Tool::call`] so the output shaping (exit-code header,
/// compaction, truncation notice, metadata counters) is testable without a sandbox.
pub(crate) fn format_execute_output(
    exit_code: i32,
    output: &str,
    truncated: bool,
    timed_out: bool,
    compact: &CompactConfig,
) -> ToolResultBlock {
    let mut text = format!("Exit code: {exit_code}\n");
    if timed_out {
        text.push_str("Timed out: true\n");
    }
    let mut metadata = JsonMap::new();
    if !output.trim().is_empty() {
        text.push('\n');
        let report = compact_grep_like(output, compact);
        if report.compacted {
            tracing::debug!(
                saved_chars = report.saved_chars,
                hoisted_files = report.hoisted_files,
                hoisted_lines = report.hoisted_lines,
                "shell output compacted"
            );
        }
        report.record_metadata(&mut metadata);
        text.push_str(&report.text);
    }
    if truncated && !timed_out {
        // A timeout already implies the output is incomplete; the timeout line says why.
        text.push_str("\n(output was truncated)");
    }
    // Producer-supplied execution facts (mirrors Java `ShellExecuteTool`): a successful
    // process is not a verified task, and an exit code is the only process-level evidence.
    let outcome = if exit_code == 0 {
        ToolExecutionOutcome::Succeeded
    } else {
        ToolExecutionOutcome::Failed
    };
    let mut block = ToolResultBlock::text(text).with_execution_details(
        ToolExecutionDetails::new("shell", outcome)
            .with_exit_code(exit_code)
            .with_output_truncated(truncated),
    );
    // Merge, not replace: the compaction counters and the execution details share the
    // attribute map.
    block.metadata.extend(metadata);
    block
}

/// Default command timeout in seconds, mirroring Java `ShellExecuteTool`.
const DEFAULT_TIMEOUT_SECONDS: u32 = 30;

/// Longest single command this tool accepts.
///
/// A command past this budget used to be accepted (`timeout: 1200`) but was silently cut at
/// core's 300s tool-execution ceiling, so the model believed it had hours of headroom that
/// never existed. The host now offloads slow tool calls to the background instead
/// (`AsyncToolMiddleware`), which needs a bounded ceiling on the command itself.
const MAX_TIMEOUT_SECONDS: u32 = 1800;

/// The one rejection message, so the model learns both bounds.
fn invalid_timeout_message() -> String {
    format!("timeout must be a positive number of seconds no greater than {MAX_TIMEOUT_SECONDS}.")
}

/// Reads the `timeout` argument, rejecting unsupported values before execution.
///
/// Mirrors Java: a non-positive timeout used to be silently replaced by the default, which made
/// an explicit `timeout: 0` look like a 30 second run. It is now an error the model can fix.
fn timeout_arg(args: &JsonMap) -> std::result::Result<u32, String> {
    match args.get("timeout") {
        None | Some(serde_json::Value::Null) => Ok(DEFAULT_TIMEOUT_SECONDS),
        Some(value) => match value.as_u64() {
            Some(seconds) if (1..=MAX_TIMEOUT_SECONDS as u64).contains(&seconds) => {
                Ok(seconds as u32)
            }
            _ => Err(invalid_timeout_message()),
        },
    }
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
        format!(
            "cd '{}' && {}",
            working_directory.replace('\'', "'\\\\''"),
            command
        )
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
        // 同理：pwsh 不为原生 exe 展开通配符（Linux bash 会先展开），而 rg 的路径位置参数本身
        // 也不做 glob，于是 `rg pat src/*.rs` 会把 `*.rs` 当字面路径 → `os error 123`。
        if cfg!(windows) {
            "Execute a shell command and return combined output + exit code. Use for git, npm, build, test, and other terminal operations. If a dedicated tool exists (e.g., read_file, write_file), you MUST use it instead of shell commands. Note: on Windows the command runs in PowerShell (pwsh), not cmd or bash. PowerShell treats `$` as a variable/interpolation prefix, so an unquoted or double-quoted `$` is expanded and silently eaten (e.g. `echo price is $5` prints `price is `; `awk {print $1}` loses `$1`). Always wrap any literal `$` in SINGLE quotes: `echo 'price is $5'`, `awk '{print $1}'`, `git log --format='%h %s'`. Single quotes keep text verbatim in pwsh; the same applies to other PowerShell metacharacters such as backtick (escape char), `@(...)` (array splat) and `#` (comment). PowerShell also does NOT expand wildcards for native executables, and rg never globs its path arguments, so `rg pat src/*.rs` passes *.rs literally and fails with 'os error 123'. Put file filters in the tool's own flag instead: `rg <pattern> <dir> -g '*.rs'`, and a -g glob containing / must be cwd-relative (`rg -g 'crates/x/src/*.rs' .`). For plain content/file search prefer the grep_files and glob_files tools over shell rg."
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
                "timeout": {"type": "integer", "description": "Positive timeout in seconds (default: 30, max: 1800). Values outside that range are rejected before execution."}
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
            .ok_or_else(|| {
                agentscope_core::Error::Tool("Missing string argument: command".into())
            })?;
        if is_dangerous_command(&command) {
            return Ok(ToolResultBlock::error(
                "Command contains a dangerous fragment (e.g. 'rm -rf', 'dd', 'mkfs', 'chmod 777', 'kill -9'). Refuse to execute without explicit approval.",
            ));
        }
        let wd = opt_str_arg(&args, "working_directory");
        let timeout = match timeout_arg(&args) {
            Ok(seconds) => seconds,
            Err(message) => return Ok(ToolResultBlock::error(message)),
        };

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
        let result = self.sandbox.execute(&rt, &effective_command, timeout).await;
        tracing::info!(
            command = %effective_command,
            elapsed_ms = started.elapsed().as_millis() as u64,
            ok = result.is_ok(),
            "shell execute done"
        );
        let result = result?;
        Ok(format_execute_output(
            result.exit_code,
            &result.output,
            result.truncated,
            result.timed_out(),
            &self.compact,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{command_with_working_directory, format_execute_output, timeout_arg};
    use crate::tool::CompactConfig;

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

    fn grep_like_output() -> String {
        let mut out = String::new();
        for line in [25usize, 65, 122, 298, 310, 402, 455] {
            out.push_str(&format!(
                "crates/agentscope-harness/src/agent/session/storage.rs:{line}:    fn temp_dir(tag: &str) -> std::path::PathBuf {{\n"
            ));
        }
        out.trim_end().to_string()
    }

    #[test]
    fn execute_output_is_compacted_and_counted() {
        let block = format_execute_output(
            0,
            &grep_like_output(),
            false,
            false,
            &CompactConfig::default(),
        );
        let text = block
            .output
            .iter()
            .filter_map(|b| match b {
                agentscope_core::message::ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect::<String>();
        assert!(text.starts_with("Exit code: 0\n\n[paths grouped"));
        assert!(text.contains("\ncrates/agentscope-harness/src/agent/session/storage.rs\n25:"));
        assert_eq!(
            block.metadata.get("agentscope.output_compact.hoistedFiles"),
            Some(&serde_json::Value::from(1u64))
        );
        assert_eq!(
            block.metadata.get("agentscope.output_compact.hoistedLines"),
            Some(&serde_json::Value::from(7u64))
        );
    }

    #[test]
    fn execute_output_compaction_can_be_switched_off() {
        let raw = grep_like_output();
        let block = format_execute_output(0, &raw, false, false, &CompactConfig::disabled());
        let text = block
            .output
            .iter()
            .filter_map(|b| match b {
                agentscope_core::message::ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(text, format!("Exit code: 0\n\n{raw}"));
        // No compaction counters; the producer execution details are the only attribute.
        assert_eq!(block.metadata.len(), 1);
        assert!(block
            .metadata
            .contains_key(agentscope_core::message::ToolResultBlock::METADATA_EXECUTION_DETAILS));
    }

    #[test]
    fn powershell_select_string_output_is_compacted() {
        // The obvious Windows search command produces a `> ` / `  ` gutter in front of
        // every `path:line:text`; the path still repeats, so it must be hoisted.
        let path = r"crates\agentscope-harness\src\tool\grep_options.rs";
        let raw = [
            format!("  {path}:108:"),
            format!("> {path}:109:pub(crate) fn format_matches("),
            format!("  {path}:110:    let text = match mode {{"),
            format!("  {path}:111:        \"count\" => {{"),
        ]
        .join("\n");
        let block = format_execute_output(0, &raw, false, false, &CompactConfig::default());
        let text = block
            .output
            .iter()
            .filter_map(|b| match b {
                agentscope_core::message::ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect::<String>();
        assert!(
            text.starts_with(&format!(
                "Exit code: 0\n\n[paths grouped; N:<text> = line N of the path above]\n{path}\n  108:\n> 109:pub(crate) fn format_matches("
            )),
            "{text}"
        );
        assert_eq!(
            block.metadata.get("agentscope.output_compact.hoistedLines"),
            Some(&serde_json::Value::from(4u64))
        );
    }

    #[test]
    fn plain_command_output_is_untouched() {
        let block =
            format_execute_output(0, "hello\nworld", false, false, &CompactConfig::default());
        let text = block
            .output
            .iter()
            .filter_map(|b| match b {
                agentscope_core::message::ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(text, "Exit code: 0\n\nhello\nworld");
        // No compaction counters; the producer execution details are the only attribute.
        assert_eq!(block.metadata.len(), 1);
        assert!(block
            .metadata
            .contains_key(agentscope_core::message::ToolResultBlock::METADATA_EXECUTION_DETAILS));
    }

    #[test]
    fn a_successful_run_carries_shell_execution_details() {
        use agentscope_core::message::{ToolExecutionDetails, ToolExecutionOutcome};

        let block = format_execute_output(0, "hello", false, false, &CompactConfig::disabled());
        let details = block.execution_details().expect("shell execution details");
        assert_eq!(details.kind, "shell");
        assert_eq!(details.outcome, ToolExecutionOutcome::Succeeded);
        assert_eq!(details.exit_code, Some(0));
        assert!(!details.output_truncated);

        // The attribute round-trips, so the observation side reads the same facts.
        let raw = block
            .metadata
            .get(agentscope_core::message::ToolResultBlock::METADATA_EXECUTION_DETAILS)
            .expect("the wire attribute");
        let decoded: ToolExecutionDetails = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(decoded, details);
    }

    #[test]
    fn a_nonzero_exit_code_is_a_failed_execution() {
        use agentscope_core::message::ToolExecutionOutcome;

        let block = format_execute_output(2, "boom", true, false, &CompactConfig::disabled());
        let details = block.execution_details().expect("shell execution details");
        assert_eq!(details.kind, "shell");
        assert_eq!(details.outcome, ToolExecutionOutcome::Failed);
        assert_eq!(details.exit_code, Some(2));
        assert!(details.output_truncated);
    }

    #[test]
    fn truncation_notice_lands_after_the_compacted_body() {
        let block = format_execute_output(
            1,
            &grep_like_output(),
            true,
            false,
            &CompactConfig::default(),
        );
        let text = block
            .output
            .iter()
            .filter_map(|b| match b {
                agentscope_core::message::ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect::<String>();
        assert!(text.starts_with("Exit code: 1\n\n[paths grouped"));
        assert!(text.ends_with("\n(output was truncated)"));
    }

    #[test]
    fn a_confirmed_timeout_is_reported_to_the_model() {
        use agentscope_core::message::ToolExecutionOutcome;

        let block = format_execute_output(
            124,
            "Command timed out after 1 seconds.",
            true,
            true,
            &CompactConfig::disabled(),
        );
        let text = block
            .output
            .iter()
            .filter_map(|b| match b {
                agentscope_core::message::ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect::<String>();
        assert!(
            text.starts_with("Exit code: 124\nTimed out: true\n\n"),
            "{text}"
        );
        assert!(
            text.contains("Command timed out after 1 seconds."),
            "{text}"
        );
        // A timeout already explains the missing output; a second notice only adds noise.
        assert!(!text.contains("output was truncated"), "{text}");

        let details = block.execution_details().expect("shell execution details");
        assert_eq!(details.outcome, ToolExecutionOutcome::Failed);
        assert_eq!(details.exit_code, Some(124));
    }

    #[test]
    fn timeout_arguments_must_be_positive_and_bounded() {
        use agentscope_core::message::JsonMap;

        let args = |value: Option<serde_json::Value>| {
            let mut map = JsonMap::new();
            if let Some(value) = value {
                map.insert("timeout".to_string(), value);
            }
            map
        };

        assert_eq!(timeout_arg(&args(None)).unwrap(), 30);
        assert_eq!(timeout_arg(&args(Some(serde_json::json!(90)))).unwrap(), 90);
        // The longest accepted budget is the host's background-offload ceiling.
        assert_eq!(
            timeout_arg(&args(Some(serde_json::json!(1800)))).unwrap(),
            1800
        );
        for rejected in [
            serde_json::json!(0),
            serde_json::json!(-1),
            // Above the ceiling the old code accepted the value and core cut the run short.
            serde_json::json!(1801),
            serde_json::json!(12000),
        ] {
            assert_eq!(
                timeout_arg(&args(Some(rejected))).unwrap_err(),
                "timeout must be a positive number of seconds no greater than 1800."
            );
        }
    }
}
