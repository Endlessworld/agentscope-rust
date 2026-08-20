# 当前会话状态

## 当前目标
- 推进 P1 待办项：Formatter 转换层 + 状态持久化 e2e

## 进行中任务
- 无（本轮已完成）

## 已完成任务
- **修复 rust-mcp-sdk 依赖** ✅
  - path 依赖指向不存在的 D:/IdeaProjects/rust-mcp-sdk → 改为远程 crates.io v1.0.1
- **修复 debug_url.rs 示例编译错误** ✅
  - stream 返回类型错误（async block 不是 Stream）→ 用 futures::stream::once 包装
- **Formatter 转换层（P1）** ✅
  - formatter/base_formatter.rs：extract_text_content / extract_text_content_from_block / has_media_content / format_role_label / convert_tool_result_to_string / save_base64_data_to_temp_file / should_bypass_history
  - 8 单测通过
  - openai.rs 的 to_request_messages 已重构复用该层
- **状态持久化重启恢复 e2e（P1）** ✅
  - 新增集成测试 json_file_store_survives_restart：新 store 实例从磁盘恢复 AgentState + list + session 列表

## 下一步建议
1. **P1 重要待办**：
   - SandboxLifecycleMiddleware（依赖 SandboxManager + SandboxBackedFilesystem，需解决 &mut self 借用 + set_sandbox 注入）
   - Tool 残余中高层（ToolkitAware rebindToolkit、McpClientManager 等）
   - Formatter 的 Formatter trait 泛型化（如需多 provider）
2. **P2 增强可选**：
   - Shutdown 精细化（GracefulShutdownMiddleware）
   - Observability 增强（OtelTracingMiddleware）
   - RAG 模块（全新模块）
   - Agent 协作（ObservableAgent observe、SubagentEventBus）

## 测试基线
- **cargo test**: core 115 通过（lib 62 + 集成 53）, harness 215 通过（lib 128 + 集成 87）, 共 **330** 通过
- **clippy 基线**: core 16 条既有警告; harness 12 条既有

## 已验证功能
- Formatter 转换层（Msg→provider 请求转换公共层）
- JsonFileAgentStateStore 重启恢复 e2e
- rust-mcp-sdk 远程依赖可正常编译