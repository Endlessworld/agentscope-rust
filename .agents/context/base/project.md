# 项目画像：AgentScope Rust 迁移

## 项目定位
将 AgentScope Java 框架迁移为 Rust 实现，保持功能对齐和架构一致性。

## 技术栈
- **语言**: Rust 2021 Edition
- **异步运行时**: Tokio
- **核心依赖**: async-openai v0.41.3, rusqlite v0.31, reqwest, serde_json
- **构建工具**: Cargo (workspace 结构)

## 关键路径
- **crates/agentscope-core**: 核心模块（model, tool, agent, event, middleware 等）
- **crates/agentscope-harness**: 辅助模块（gateway, memory, sandbox, skill 等）
- **MIGRATION_PLAN.md**: 迁移方案
- **MIGRATION_GAP_TRACKER.md**: 进度跟踪

## 版本状态
- **当前版本**: 0.1.0
- **迁移进度**: 约 70% 对齐
- **关键里程碑**: async-openai 集成已完成（2024年12月）
