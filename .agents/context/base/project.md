# 项目画像：AgentScope Rust 迁移

## 项目定位
将 AgentScope Java 框架迁移为 Rust 实现，保持功能对齐和架构一致性。

## 技术栈
- **语言**: Rust 2021 Edition
- **异步运行时**: Tokio
- **核心依赖**: async-openai v0.41.3, rust-mcp-sdk v1.0.1, rusqlite v0.31, reqwest, serde_json
- **构建工具**: Cargo (workspace 结构)

## 关键路径
- **crates/agentscope-core**: 核心模块（model, tool, agent, event, middleware 等）
- **crates/agentscope-harness**: 辅助模块（gateway, memory, sandbox, skill 等）
- **MIGRATION_PLAN.md**: 迁移方案
- **MIGRATION_GAP_TRACKER.md**: 进度跟踪

## 版本状态
- **当前版本**: 0.1.0
- **迁移进度**: 约 80% 对齐
- **关键里程碑**: async-openai 集成（2024-12）、Formatter 转换层 + 状态持久化 e2e（2025-01）、**ACP CodingAgent（2026-08，harness acp/ 模块）**、**ACP权限确认机制（2026-09，harness acp/ 模块）**

## 关键路径（新增）
- **crates/agentscope-harness/src/acp/**: ACP 协议 CodingAgent（AcpCodingAgent/AcpAgentSession/config/event），底层包装 `HarnessAgent`（嵌入 ReActAgent）
- **crates/agentscope-harness/src/acp/permission_store.rs**: 权限规则持久化（PermissionRuleStore）
- **crates/agentscope-harness/src/agent/harness_agent.rs**: `HarnessAgent` 装配 filesystem+toolkit+middleware，嵌入 `ReActAgent` 提供 `stream_events()` 驱动
- **docs/ACP_AGENT_PLAN.md**: ACP CodingAgent 方案
- **docs/ACP_AGENT__GAP_TRACKER.md**: ACP CodingAgent 进度跟踪