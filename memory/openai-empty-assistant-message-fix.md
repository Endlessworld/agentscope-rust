# agentscope-rust 构建环境坑（2026-09-20 发现）
- `crates/agentscope-harness/Cargo.toml:36` 的 `agent-client-protocol` 依赖指向 **`D:/IdeaProjects/agentclientprotocol/rust-sdk/src/agent-client-protocol`**，但该路径实际不存在（仓库在 `E:\local-github\agentclientprotocol`）。
- 后果：任何 `cargo` 命令（含 `-p agentscope-core`）都因 workspace manifest 加载失败报 `os error 3`，**与目标 crate 无关**。
- 绕过（不改仓库文件、不改 CARGO_TARGET_DIR）：建目录联结
  `New-Item -ItemType Directory -Path 'D:\IdeaProjects' -Force; cmd /c mklink /J "D:\IdeaProjects\agentclientprotocol" "E:\local-github\agentclientprotocol"`
  之后 cargo 正常。`cargo --config 'paths=[...]'` 覆盖无效（属不同机制）。

# ReActAgent 400 根因与修复（2026-09-20）
- 症状：`HTTP error 400 ... [invalid_request_error] Invalid assistant message: content or tool_calls must be set`。
- 根因：`crates/agentscope-core/src/model/openai.rs` `to_request_messages` 的 `MsgRole::Assistant` 分支，`extract_text_content(&msg)` 为空时把 `content` 置 `None`。而 `extract_text_content` 跳过 `ThinkingBlock`（`formatter/base_formatter.rs:16`），故「仅 thinking 块」或「tool_calls 被 middleware 剪枝（harness `agent/middleware/tool_call_sanitizer.rs`）」的 assistant 消息被序列化成 `{"role":"assistant"}`（`Option::None` + `skip_serializing_if`），严格 OpenAI 兼容端点（vLLM 等）直接 400。
- 修复：始终发送 `content`，空文本回退 `""`。对齐 Java `OpenAIMessageConverter.convertAssistantMessage`（Java 有同款注释，属已知问题，Qwen3 thinking 模式触发）。
- 回归测试：`openai.rs` tests 新增 `assistant_without_text_or_tool_calls_still_has_content`、`assistant_with_only_tool_calls_still_has_content`。
- 验证：`cargo test -p agentscope-core` = **145 passed / 0 failed**。
- 同类路径无此缺陷：`model/anthropic.rs:250+`、`model/responses.rs:233+` 的 assistant 分支在 content 为空时**整条丢弃消息**，不会发空消息。
