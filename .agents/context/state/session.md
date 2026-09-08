# 当前会话状态

## 本轮已完成（2026-09-07，Java → Rust P0/P1 同步，未提交）
- P0-1 (#2413)：`LocalFilesystem::ls` 区分 not-exists/not-a-directory 报错；
  `FilesystemTool.list_files` 空消息改 "Empty directory"。
- P0-2 (#2832)：`grep_files`/`glob_files` 加 limit 参数（默认 100/200、硬上限 1000）
  + 截断提示；limit<1 报错。
- P1-3 (#2600/#2499)：core `ReActAgent` 加 clear_context/clear_context_for/interrupt/
  interrupt_session；`HarnessAgent` 加对应 wrapper。
- P1-4 (#2877)：MCP 注册结果上报——`McpServerRegistrationListener` trait +
  `McpServerRegistrationResult`（Success/Failed/Skipped）+ `McpServerRegistrar::with_listener`。
- P1-5 (#3009)：compaction token 估算加 thinking block 按内容计数（含 ToolResult 内嵌套）。
- 新增测试：harness_filesystem_test（ls 报错 + limit 截断）、harness_memory_test
  （thinking token）、react_agent（clear_context）。
- 验证：cargo build ✅；core 127 测试 + harness 全套通过（acp_stdio_thought_test 为已知 flaky）。

## 遗留 / 待办
- command_with_working_directory 的 Windows `cd /d` cmd 语法 bug 已修复（改 pwsh `Set-Location -LiteralPath '...' ;`，见 shell_execute_tool.rs；repro_shell_timeout.rs 示例同步更新；真实 pwsh 验证通过 + 2 单测）
- dev 分支领先 origin/dev，待推送
- core 侧 detail 项（#2755 空响应重试等）仍待专项核对（见 docs/SYNC_JAVA_SEP2026.md 第四节）

## 下一步
- 按需推送 dev
- 专项核对 core detail 同步项