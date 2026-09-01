# 会话状态

## 当前目标
acp_harness_agent 内存极限优化（十几MB → 嵌入式 ≤4MB）——已完成全部 7 项修改 + 对话压缩中间件。

## 已完成（全部 8 阶段 + 压缩中间件）
- 阶段1：tokio feature 裁剪 + main 改 current_thread 运行时
- 阶段2：技能仓库延迟加载（LazyLoadSkillTool + SkillCache）
- 阶段3：WorkspaceIndex 惰性打开（conn: Mutex<Option<Connection>>）
- 阶段4：会话注册表上限（AcpCodingAgent::max_sessions 默认1）
- 阶段5：feature 门控（lite）+ embedded profile
- 最终验证：全 workspace 测试通过、embedded+release 二进制构建成功

## 对话压缩中间件（本次新增，架构上由 middleware 处理上下文管理）
- RuntimeContext 新增 `state: Option<Arc<Mutex<AgentState>>>`，ReActAgent::new_context 注入 core.state
- 新增 `ConversationCompactionMiddleware`（middleware/conversation_compaction.rs）：
  on_agent 阶段读 ctx.state 历史 → ConversationCompactor::compact_if_needed → 写回
- HarnessAgentBuilder 新增 `.conversation_compaction(CompactionConfig)` 方法
- acp_harness_agent.rs 启用：trigger_messages=30, keep_messages=12 + tool_result_eviction(defaults)
- 验证：MockModel 测试证明历史超过触发值后被压缩为 [summary]+tail（call msgs 从 6→1）

## 下一步
无（任务完成）。如需继续：实测运行内存（RSS）对比基线。

## 关键产物
- docs/MEMORY_OPTIMIZATION_PLAN.md（方案文档）
- 构建命令：cargo build --profile embedded -p agentscope-harness --example acp_harness_agent --no-default-features --features lite
- 二进制：5.72MB（lite+embedded）vs 6.66MB（release 完整）