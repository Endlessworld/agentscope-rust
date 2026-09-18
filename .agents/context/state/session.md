# 当前会话状态

## 本轮已完成（P2 批量落地 — docs/MIGRATION_GAP_TRACKER.md）
- M2 `agent/coordination/`：PeriodicGate / LocalPeriodicGate（OnceLock 进程级 map）/ StoreBackedPeriodicGate（BaseStore CAS）。
- S1/S2/S8/S3 filesystem：
  - `filesystem/remote/mod.rs`（原 remote.rs 移入）RemoteFilesystem 全实现（ls/read/write CAS/edit 重试/grep/glob/exists/delete/move）。
  - `filesystem/remote/store.rs`：BaseStore/StoreItem/NamespaceFactory/StaticNamespace/DefaultNamespace/InMemoryStore。
  - `filesystem/util.rs`：FilesystemUtils（get_file_type/perform_string_replacement/shell_quote）。
  - `filesystem/baked.rs`：BakedContextFilesystem。
- S9/S10 skill：`skill/security.rs`（6 类 18 规则扫描器 + 安装策略）、`skill/filters.rs`（SkillUsageBackend/FilesystemSkillUsageBackend/CanaryFilter/EnvironmentFilter/NotificationSink/NotifyAndWaitGate）；SkillUsageRecord 增 environments，SkillUsageStore 增 get。
- B2/B3/B4 sandbox：`sandbox/snapshot.rs`（Local/Remote/Noop + 工厂 trait）、`sandbox/layout.rs`（7 entry + LayoutSpec）、`sandbox/helpers.rs`（MountSupport/ArchiveExtractor/ProjectionApplier）。`SandboxSnapshotSpec` 由结构体升级为 trait，context/manager/docker/tests 随动。
- G2/G4/G6 gateway：`gateway/subagent_bridge.rs`、TurnBusyException + try_acquire、StoreBackedSubagentRegistry。
- R1/R2/R3 subagent：`subagent/protocol/{event,codec}.rs`、`subagent/task/protocol_client.rs`（含 SSE）。
- T2/T7/T10/T11/T12 经核对已在 b5e1635 完成（tracker 滞后）。

## 验证结果
- `cargo test -p agentscope-harness`：lib 266 + 集成全绿（0 failed）。
- `cargo clippy -p agentscope-harness --lib`：本轮新增文件 0 条告警。
- 全量 test 曾因磁盘出现 link.exe exit 1180/1318 → 清 `target/debug/incremental` + `-j 2` 后通过。

## 下一步
- P2 剩余：D2 HarnessAgentBuilderSupport、D5 SubagentDeclaration、D6 WorkspaceTaskRepository。
- T1 AgentSpawnTool 深化（streaming events / remote targets）。

## 追加（Docker snapshot 接线）
- `sandbox/docker.rs`：`DockerSandbox` 新增 `snapshot: Option<Arc<dyn SandboxSnapshot>>` + `with_snapshot()`；
  `create` 时按 session_id `spec.build()` 绑定；`stop()` 在 persistence enabled 时
  `persist_workspace()`→`snapshot.persist`（noop 跳过归档）；`start()` 改为 A 保留 / B·C snapshot
  可恢复则 hydrate+重放 spec / D 全新初始化。新增 2 单测。lib 268 全绿，docker/snapshot 文件 clippy 0 告警。

## 追加（D3 SkillRuntime 完整性）
- `skill/model.rs`：新增 `SkillCatalogSnapshot`（ids + filesRoots，serde 投影）。
- `skill/runtime.rs`：`install(catalog, &mut RuntimeContext)` 写 `metadata[agentscope_skill_catalog]`；
  `catalog_snapshot_for(ctx)` 读；`prepare_toolkit(toolkit, snapshot)` 幂等注册限定 catalog 的
  `LoadSkillTool`；`load_tool()`；保留 legacy catalog。`LoadSkillTool::TOOL_NAME` + `from_snapshot`。
- 新增 5 单测。lib 273 全绿，runtime/load_tool/model clippy 0 告警。
- 缺口：Rust 中间件 `on_*` 拿 `&RuntimeContext`（不可变），per-call 绑定需宿主持有可变 context；
  已在 runtime.rs 文档注明。

## 追加（D5/D6/D2 缺口核对 + tracker 更新）
- 核对 `worker_output/worker_result_{e7714892,ae6624a8,d90adbd7}.txt` 三份缺口报告（详见 docs/MIGRATION_GAP_TRACKER.md §缺口核对记录）：
  - **D5**（SubagentDeclaration）：P0/P1 全部已落地 → 🟠 改 ✅（残余 3 项 P2：new() steps 默认、SubagentSpecGenerator、load_from_file）。
  - **D6**（WorkspaceTaskRepository）：确认字段 / CAS 守卫 / mark_delivered 幂等 / 传输抽象 + set_completion_callback 已落地；
    仍缺 trait 级 shutdown+回调、SUPPRESS_COMPLETION_CALLBACK、put_task 远程分支接线（poll_remote_until_done）、
    update_awaiting_confirm、heartbeat/sweep_orphaned_tasks → 维持 🟠。
  - **D2**（HarnessAgent builder）：d90adbd7 **整篇作废**（Java/Rust 两侧基线均 grep 0 命中，详见 learnings）。
    已按真实源码重新推导：Java HarnessAgent.java 2989 行 / 98 setter vs Rust 51 字段 / 53 pub fn；
    P0 = filesystem(Local/Remote/Sandbox spec) + toolsConfig/disableToolsConfig + teamsMode，P1/P2 见 tracker §D2。
- tracker 改动：D2/D5/D6 三行状态 + 新增「缺口核对记录」章节 + P2 汇总 31/34 → 32/34。
- 未跑 cargo（仅改文档）。

## 追加（README 生成）
- 新增根 `README.md`（项目门面：核心能力 / 仓库结构 / 模块表 / 环境要求 / 构建测试 / 示例 / ACP 用法 / 文档索引 / 已知限制 / 许可）。
- 内容来源：`Cargo.toml`（workspace + feature/profile）、两个 crate 的 `lib.rs` 导出面、`examples/*.rs` 与 `examples/README.md`、`docs/*.md`、`.agents/context/base/project.md`。
- 关键提醒已写入 README：`agent-client-protocol` 为**绝对路径依赖**（`E:/…/rust-sdk/src/agent-client-protocol`）、仓库无 `LICENSE` 文件、Docker e2e 未验证、无 `@Tool` 过程宏。
- 未跑 cargo（仅新增文档）。

## 追加（Codex 智能体实现对比）
- 新增 `docs/CODEX_AGENT_COMPARISON.md`（~31KB / 347 行 / 16 章）：对比 `E:\local-github\codex\codex-rs`（≈110 crate）逐维度展开：运行时内核、工具系统、权限审批、沙箱执行、上下文压缩、记忆、技能、钩子/插件/中间件、子智能体与多智能体、协议与前端、持久化、模型层。
- 主要结论：内核同构（采样→工具→回灌）；最大代差 = execpolicy 规则引擎 / guardian LLM 复核 / 4 平台 OS 级沙箱 / unified_exec 常驻进程 / tool_search 延迟加载 / thread fork-revert-search + rollout 追加日志。
- 本项目领先项：技能治理（security/filters/curator/promoter/audit）、gateway+channel+bus+coordination、team/transcript、Local/Overlay/Composite/Sandbox/Remote/Baked 文件系统抽象、lite/embedded 可裁剪特性。
- 建议优先级：P0 = execpolicy 类命令策略引擎、子智能体拓扑 + 深度限制；P1 = 工具延迟加载、常驻 shell、上下文片段可枚举化。
- 未跑 cargo（仅新增文档）；未使用 worker（按要求）。

## 追加（AgentSpawnTool 接线到 HarnessAgent / acp_harness_agent）
- 对齐 Java `HarnessAgentBuilderSupport` + `SubagentsMiddleware.getTools()`：
  - 新增 `agent/subagent/materialize.rs`：`SUBAGENT_CONTEXT_SECTION`、`build_subagent_sys_prompt`、
    `allowlisted_inherited_toolkit`、`derive_child_session_id`（name@sid#uid）、
    `resolve_declared_workspace`（SHARED→main / ISOLATED→agents/<name>/workspace）、
    `SubagentBlueprint` + `LeafFactory`（每次 spawn 递归 build 一个 leaf HarnessAgent）。
  - `subagent/mod.rs`：`CustomSubagentFactory`（Java `SubagentFactoryEntry`）+ materialize re-exports。
  - `HarnessAgent` 实现 `Agent` / `CallableAgent`（委托内嵌 `ReActAgent`）→ `Arc<HarnessAgent>` 可作子 agent。
  - builder 新增字段 `toolkit` / `subagent_declarations` / `subagent_factories`；新增方法
    `toolkit()` / `subagent()` / `subagents()` / `subagent_factory[_with_description]()`。
  - `build()`：subagent 段从「工具注册之前」搬到 meta tool 之后（children 必须继承完整父 toolkit）；
    无条件注册 `general-purpose`；entries = declarations + custom factories + `subagent_entries`
    +（static 模式）`workspace/subagents/*.md` 扫描；`DefaultAgentManager` → `AgentSpawnTool`
    （+task repo）→ `AgentSendTool` / `AgentListTool`；middleware 接
    `with_agent_manager` + `with_factory_builder` + `with_extra_tools`。
- `examples/acp_harness_agent.rs`：仅加模块文档说明（代码零改动即自动获得 agent_spawn）。
- 测试：新增 `tests/harness_subagent_wiring_test.rs`（4 例：工具注册 / general-purpose spawn /
  声明式 leaf + label 通道 send + list / `workspace/subagents/*.md` 扫描→物化→spawn）；
  修 `harness_agent_spawn_test.rs` 2 处断言（响应新增 agent_key header）。
- 验证：`cargo test -p agentscope-harness`（lib 291 + 集成全绿）、`cargo test -p agentscope-core` 全绿、
  `cargo build -p agentscope-harness --examples` 通过。
- 已知差异（本轮未做）：`decl.model` 覆盖（Java modelResolver）、skills 白名单、父 middleware/plan-mode 转发、
  `timeout_seconds`/`MAX_SPAWN_DEPTH`、`SubagentEventBus` 流式、gateway/暴露。

## 追加（subagent 声明字段补全：decl.model 覆盖 + decl.skills 白名单）
- `materialize.rs` 新增（对齐 Java `resolveModel` / `buildDeclaredFactory` 的 skillFilter 段）：
  - `resolve_declared_model(override, parent, resolver, name)`：空/失败/无 resolver → 回退父模型（带 warn）。
  - `declared_skill_filter(parent, decl)`：`decl.skills` 为空→继承父 filter；否则 `SkillFilter::only(decl.skills ∩ parent)`。
  - `SubagentBlueprint` 新增 `model_resolver` / `skill_filter`；`LeafFactory` 新增
    `model_override` / `skill_filter`（general-purpose 继承父 filter，declared 走白名单）。
- builder 新增 `model_resolver(fn(&str) -> Option<Arc<dyn Model>>)`（对齐 Java `Builder.modelResolver`）。
- `examples/acp_harness_agent.rs`：接上 `model_resolver`（用 `ModelConfig::resolve_model` 查 provider 后
  新建模型，session 头复用当前 ACP sessionId）。
- 测试：`harness_subagent_wiring_test.rs` 新增 `declared_model_override_uses_the_resolver`
  （有 resolver → 子模型；无 resolver → 回退父模型）；`ScriptedModel` 改为携带固定应答文本。
- 验证：`cargo test -p agentscope-harness` 全绿（lib 291 + 集成 5/5 新例）；
  `cargo build -p agentscope-harness --examples` 通过。
- 仍缺：父 middleware/plan-mode 转发、`timeout_seconds`/`MAX_SPAWN_DEPTH`、`SubagentEventBus` 流式、
  gateway/`expose_to_user`、`decl.temperature/top_p` 叠加。

## 追加（ACP 继续对话 400 修复：悬挂 tool 调用）
- 根因 2 处（详见 history/learnings.md 2026-09-18 条目）：
  `react_agent::run_reasoning` 忽略中间件改写后的 `ReasoningInput`（模型拿到的是中间件执行前的冻结消息）
  → `ToolCallPairSanitizerMiddleware` 等所有 `on_reasoning` 中间件形同虚设；
  `extract_pending_tool_calls` 把已应答的 tool_call 当挂起 → max_iters 收尾重复写 TOOL 结果。
- 改动：`crates/agentscope-core/src/agent/react_agent.rs`（`run_reasoning` 改用链传入的
  ReasoningInput 构造 `ModelCallInput`；`extract_pending_tool_calls` 跳过已被后续 TOOL 应答的调用）。
- 测试：core 新增 2 例（`reasoning_middleware_rewrites_reach_the_model`、
  `max_iters_does_not_duplicate_answered_tool_results`）；harness 新增
  `tests/resume_dangling_tool_call_test.rs`（复现“续聊 + 重复 tool 应答”，旧代码实测失败 left:2/right:1）。
- 验证：`cargo test --workspace` 全绿（0 failed）；重建
  `target/embedded/examples/acp_harness_agent.exe`
  （`cargo build --profile embedded --no-default-features --features lite --example acp_harness_agent`）。
- 注：已存在的坏 state（如 session-1789696812989）**无需手工修**，sanitizer 生效后自动修剪。

