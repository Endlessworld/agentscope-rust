# Facts

## PROTOCOL 模块修复（2026-09-18 完成）
- 目标 crate：`crates\agentscope-extensions-protocol\`（波次 A）；Java 源 `E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-protocol\`。
- **失败测试根因（已修）**：`a2a::message::tests::media_part_mime_falls_back_to_primary_type_for_unknown_extension`（`src/a2a/message.rs:765`）——**测试断言写错，非实现 bug**。URL `sound.unknownext` 未知扩展 → MIME 回退 `"audio"`；Java `FilePartParser#getPrimaryType` 对无 `/` 的串返回原串，`switch case TYPE_AUDIO("audio")` 仍还原为 `AudioBlock`，故原断言 `part_to_content_block(&part).is_none()` 与 Java 及测试名相悖。改为断言可还原为 `ContentBlock::Audio` 且 URL 源为原址。未删断言/功能/未 ignore。
- **验证**：`cargo build -p agentscope-extensions-protocol` 0 error；`cargo test` 79 passed / 0 failed。
- **已落地 src**：`lib.rs`、`error.rs`、`agent_protocol.rs`（68830B）、`a2a/{mod,model,message,card,client,server}.rs`。测试分布：agent_protocol 22 / message 16 / server 15 / client 9 / model 9 / card 6 / error 2。
- **最大缺口**：`agui`（Java 8325 行）与 `chat-completions-web`（1851 行）**完全未迁移**（Rust 无任何对应文件）。agui 含 `AguiEvent`(59744B)、`AguiAgentAdapter`(33423B)、`AguiMessageConverter`(23830B)。
- P1 部分完成：A2A runner/registry/transport 未 1:1；agent-protocol 仅内存仓库（`WorkspaceProtocolTaskRepository` 未迁移）；SSE 不完整。
- 已回填 `docs\extensions\PROTOCOL__GAP_TRACKER.md` §1/§2/§3/§4（含 §4.3 失败根因）。
