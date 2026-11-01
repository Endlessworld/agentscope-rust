# HARNESS 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **HARNESS** 模块迁移的**单一事实来源**（自 2026-10-31 起）。开工前必须读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md`；完成后更新对应条目的 `执行状态`。切勿臆断代码现状——以本文件 + 源码 + `git status` + 实际 `cargo` 验证为准。
>
> **取代关系**：本文件**取代**根目录 `GAP_TRACKER.md`（GAP-001..GAP-305，2026-10-21 收尾，全部 `DONE`/`MATCHES_JAVA`）与 `docs/MIGRATION_GAP_TRACKER.md` §3「agentscope-harness 详细缺口清单」中 harness 相关段落的口径。被取代文件**保持原样不删不改**，其条目编号（`GAP-1xx/2xx/3xx`）在本文件内**不复用**。
>
> **Java 源码**：`D:\IdeaProjects\agentscope-java\agentscope-harness\src\main\java`（`main` @ `1524b3191`）
> **Rust 目标**：`crates/agentscope-harness/`（配套：`crates/agentscope-extensions-acp/`）

## 0. 如何阅读本文件

- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）` / `待裁定`。
- **优先级**：P0=核心链路 / 生产阻断；P1=重要能力缺失或语义偏离；P2=增强 / 长尾 / 测试密度。
- **证据门槛**：Java 侧写 `相对路径#符号`；Rust 侧写 `文件:行`，或用 2–3 个同义词 `rg` 零命中交叉验证。**「没看到调用」不等于「未实现」**。
- 编号：缺口 `HARNESS-NN`（本文件内唯一、单调递增）；差异 `HARNESS-DNN`（不迁移）/ `HARNESS-XNN`（已知差异）。`§3.5` 的编号是**本轮唯一编号体系**。
- 本文件只写**现行状态**（§0–§5）；执行过程/回执/反向验证明细写 `docs/extensions/history/HARNESS__MIGRATION_HISTORY.md`，本文件只在 §6 留链接。

## 1. 模块概览

| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-harness`（单一包 `io.agentscope.harness.agent.**`） |
| Java 规模（2026-10-31 实测） | **295** 个 `.java` / **≈53.3k** 行（主代码）；`src/test` **192** 个 `.java` |
| Rust crate | `crates/agentscope-harness`（**153** 个 `.rs` / **≈49.6k** 行）+ 36 个 `tests/*.rs`；`src` 内测试属性 **524** 处 |
| 迁移波次 | core-adjacent（非 extensions 波次）；ACP 独立为 `crates/agentscope-extensions-acp` |
| 当前状态 | **主体已迁移、生产可用**；复核发现的 2 处「文件已落盘但未接线」（HARNESS-04/05）与 HARNESS-01/09 装配缺口已于 2026-10-31 完成接线并补真实 `build()` 回归；原 1 处 **P0 待裁定**（HARNESS-15）已去重退役，执行归 acp crate 的 `ACP-01`，本 crate P0 归零 |

> 2026-10-31 全量对齐复核：按 Java 包逐包复核完毕（§2），新增缺口/待裁定 17 条（§3.5），差异 8 + 9 条（§4.3）。全量测试数字沿用 `EXTENSIONS_GAP_TRACKER.md` §4.1 实测（workspace 156 targets / 5395 passed / 0 failed / 5 ignored；`agentscope-harness --lib` 519），**本轮未跑 cargo**。

> **2026-11-01 Round 4**：关闭 6 项（HARNESS-07/08/10/11/13/14，逐行见 §3.5.1；新增不迁移条目 D09/D10 与差异 X10）。`cargo build -p agentscope-harness -j 2` EXITCODE=0（无新增 warning），`cargo test -p agentscope-harness --lib -j 2 --no-fail-fast` = **573 passed / 0 failed**（含本批 15 个新用例）+ `--test harness_mcp_bridge_test` 3 passed（含本批 1 个新用例）；反向验证：回退 `PinnedSandboxFilesystem::release()` 的 latch → `pinned_keeps_the_acquired_filesystem_and_rejects_after_release` FAILED → 恢复后绿。同轮修了 core 新增 `ActionObservation.execution_details` 导致的 harness 测试编译错（`agent/verification/mod.rs:239`，非本批功能）。改动仅限 `crates/agentscope-harness/**` + 本 tracker。

## 2. Java 包与 Rust 映射（逐包）

结论口径：`已迁移` = 该包全部 Java 类在 Rust 有对应且已接线；`部分迁移` = 主要类在、长尾/装配缺；`未迁移` = 包内无对应实现；`设计决定` = 有意不迁（见 §4.3.1）。

| Java 包 | 文件/行 | Rust 对应 | 结论 | 装配是否真接线（源码核验） |
|---|---|---|---|---|
| `agent`（根） | 4 / 4348 | `agent/harness_agent.rs`、`distributed_store.rs`、`isolation_scope.rs`、`mod.rs`、`shutdown.rs` | 已迁移（builder 面仅剩 HARNESS-02/03/12） | `HarnessAgent::builder().build()` 为唯一装配入口；会话工具/子代理/中间件均在 `build()` 内注册，`harness_agent.rs:1500-2260` |
| `agent.context` | 17 / 1526 | `agent/context/{builder,context_middleware,conventions,item,mod,model_calls,policy,render,source,usage}.rs`（10 / 4022+） | 已迁移（Rust 更宽） | `ContextMiddleware` 最后注册、位于 `call_gate` 内；`harness_agent.rs:1559-1580`、`2125-2132` |
| `agent.session` | 6 / 1018 | `agent/session/{session,storage,store,sqlite,answer,mod}.rs`（6 / 3741） | 已迁移 | `session_log_store` 在 `build()` 解析一次，ReAct 与会话工具共用；`harness_agent.rs:1500-1520`、`2094-2125` |
| `agent.observation` | 2 / 126 | 无独立模块；`core::observation::ActionObservation` + `verification::SettledAction` | 部分迁移（存储回放不迁，D01） | 观察者由 core `SessionRecorder` 承担；`verification/mod.rs:22-33` 明确记录不回放 |
| `agent.verification` | 5 / 572 | `agent/verification/mod.rs`（1 / 489） | 部分迁移（trait+verify 在，两个具体 verifier 与 load 缺 → HARNESS-06） | `verify()` 需调用方传入 recorder；未被 `build()` 自动挂载（Java 亦为调用方驱动） |
| `agent.subagent` | 8 / 1762 | `agent/subagent/{loader,manager,materialize,entry,spec_generator,mod}.rs`（6 / 2054+） | 已迁移 | `DefaultAgentManager` + 工厂在 `build()` 装配，`agent_spawn/agent_send/agent_list` 注册于 toolkit；`harness_agent.rs:1943-2064` |
| `agent.subagent.protocol` | 6 / 680 | `agent/subagent/protocol/{codec,event,mod}.rs`（3 / 614） | 已迁移 | 由 `RemoteSubagentTransport`/`protocol_client` 消费 |
| `agent.subagent.task` | 13 / 2300 | `agent/subagent/task/{background,delivery,protocol_client,record,repository,spec,status,workspace,mod}.rs`（9 / 3563） | 已迁移 | `task_repo` 在 `build()` 兜底为 `WorkspaceTaskRepository`；`harness_agent.rs:1941-1948` |
| `agent.sandbox` | 22 / 2417 | `agent/sandbox/{manager,lease,guard,state,key,context,helpers,session_state_store,snapshot,layout,workspace_spec,docker,mod}.rs`（13 / 4499） | 部分迁移（生命周期中间件未接线 → HARNESS-05；平台实现迁至 extensions-sandbox） | `Sandbox`/`SandboxClient` trait 在 `sandbox/mod.rs:76,118`；4 平台实现在 `crates/agentscope-extensions-sandbox` |
| `agent.sandbox.snapshot` | 9 / 638 | `agent/sandbox/snapshot.rs`（Local/Remote/Noop + 三种 Spec + `RemoteSnapshotClient`） | 已迁移 | `SandboxManager` 经 Spec 取快照；`snapshot.rs:16-320` |
| `agent.sandbox.layout` | 8 / 473 | `agent/sandbox/layout.rs`（7 类型 + `LayoutSpec` + `WorkspaceEntry` 枚举） | 已迁移（Jackson 模块不迁，D02） | 由 `SandboxFilesystemSpec::layout_spec`（extensions-sandbox）生成 |
| `agent.sandbox.impl.docker` | 5 / 1462 | `agent/sandbox/docker.rs` + `agent/sandbox/state.rs`（`DockerSandbox`/`DockerSandboxClient`/`DockerSandboxState`） | 部分迁移（`DockerSandboxClientOptions`/`DockerFilesystemSpec` 未单独建模） | `tests/harness_docker_sandbox_test.rs` 走真实 docker 客户端接口（默认离线） |
| `agent.gateway` | 17 / 2286 | `agent/gateway/{harness_gateway,gateway_bootstrap,msg_context,session_turn_gate,subagent,subagent_bridge,task_bridge,wakeup_dispatcher,mod}.rs`（10 / 2005） | 已迁移（`TurnLease` 类型另有归属，X06） | `HarnessGateway` 持有 `ChannelManager`/`SubagentRegistry`；`harness_gateway.rs:109-224,363` |
| `agent.gateway.channel` (+`chatui`) | 17 / 1772 | `agent/gateway/channel/{channel,binding,config,router,dm_scope,inbound,peer,chatui,mod}.rs` + `platform_channel.rs`（10） | 已迁移 | 平台适配器在 `crates/agentscope-extensions-channel`（5 平台 + 微信） |
| `agent.memory` (+`compaction`,`session`) | 11 / 2930 | `agent/memory/{compaction,consolidator,eviction,flush,session,mod}.rs`（6 / 2393） | 已迁移 | `MemoryFlushMiddleware`/`MemoryMaintenanceMiddleware` 在 `harness_agent.rs:1621-1645` 挂载 |
| `agent.middleware` | 20 / 5846 | `agent/middleware/*.rs`（19 / 6174，另有 `tool_call_sanitizer`/`graceful_shutdown`/`state_checkpoint`） | 部分迁移（`AsyncToolMiddleware`/`SandboxLifecycleMiddleware` 未接线 → HARNESS-04/05；context 约定文案 2026-11-01 以资源 + 显式注册落地） | 已接线：agent_trace:1591、workspace_context:1594、at_path:1607、memory_flush:1621、memory_maintenance:1634、compaction:1658、eviction:1671、inbox:1692、teams:1704、skill_usage:1818、skill_curator:1839、plan_mode、graceful_shutdown:1536、state_checkpoint(仅 Legacy):2111、context:2125 |
| `agent.skill` + `curator` + `runtime` | 33 / 5056 | `agent/skill/{curator,filters,load_tool,manage,model,promoter,prompt,repository,runtime,security,shell_path,stager,mod}.rs`（13 / 4664） | 部分迁移（curator 长尾 4 类：store 后端与组合过滤器已于 2026-11-01 补齐，其余见 §4.3.1 D10） | `HarnessSkillMiddleware`/`SkillUsageMiddleware`/`SkillCuratorMiddleware` 在 `harness_agent.rs:1818,1839,1854-1870`；`skill_manage`/`propose_skill` 在 `:1815` |
| `agent.team` | 11 / 1109 | `agent/team/mod.rs`（1 / 597） | 已迁移（`TeamConflictException` → `TeamError`） | `TeamTool` + `TeamsMiddleware` 仅在 `teams_client`/`teams_context` 齐备时注册；`harness_agent.rs:1692-1706` |
| `agent.tool` | 19 / 6263 | `tool/*.rs`（18 / 5197）+ `agent/skill/manage.rs`（`SkillManageTool`/`ProposeSkillTool`） | 已迁移 | 文件系统/Shell/Web/Memory/Session/Task/Plan 工具在 `build()` 按开关注册；`harness_agent.rs:890-900,1815` |
| `agent.tools`（MCP 配置面） | 9 / 1033 | `agent/mcp.rs`（`McpServerRegistrar`/`Registrar`/listener/`McpMetaPropagationOptions`）+ `agent/mcp_client_manager.rs` | 部分迁移（`ToolsConfig`/`ToolFilter`/`ToolsConfigLoader`/`HarnessPlatformTools` 缺 → HARNESS-01；前缀已于 2026-11-01 补齐） | `bridge_mcp_tools_to_toolkit` 由宿主/ACP 显式调用（`acp/session.rs:1107`），builder 无 MCP 入口 |
| `agent.bus` | 6 / 824 | `agent/bus/{async_tool,message_bus,workspace_message_bus,mod}.rs` | 已迁移 | `message_bus` 缺省兜底 `WorkspaceMessageBus`；`harness_agent.rs:1678-1691` |
| `agent.coordination` | 3 / 189 | `agent/coordination/mod.rs`（`PeriodicGate`/`LocalPeriodicGate`/`StoreBackedPeriodicGate`） | 已迁移 | 由 curator/maintenance 周期门消费 |
| `agent.workspace` (+`plan`) | 7 / 1959 | `agent/workspace/{constants,index,manager,namespace,path_normalizer,plan,mod}.rs`（7 / 1480） | 已迁移（`PathPolicy` 以 `LocalFilesystem::additional_roots` 表达，X 见 §4.3.2） | `WorkspacePathNormalizer` 在 `build()` 构造并下发；`harness_agent.rs:2095` |
| `agent.filesystem`（含 `local`/`remote`/`store`/`sandbox`/`spec`/`model`/`util`） | 33 / 6510 | `filesystem/{local,remote,remote/store,baked,composite,overlay,sandbox,result,util,mod}.rs`（10 / 2673+） | 部分迁移（`spec/*` 三件与组合子缺 → HARNESS-03；`Pinned`/`Routed` 已于 2026-11-01 补齐） | `Filesystem` trait 为唯一抽象；`filesystem/mod.rs:122` |
| `agent.artifact` | 3 / 122 | `agent/artifact.rs` | 已迁移 | `ArtifactDeliveryTool` 按 `artifact_delivery_target` 注册；`harness_agent.rs:1362` |

### 2.1 依赖映射（Java → Rust）

| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| Project Reactor（`Mono`/`Flux`） | 异步流 | `tokio` + `futures::Stream` | 无 backpressure 语义差异需求 |
| Jackson（含多态 `@JsonTypeInfo`） | DTO / 沙箱布局 | `serde` + `serde_json` + 标签枚举 | 见 D02 |
| SLF4J | 日志 | `tracing` | — |
| JDK `HttpClient` | Web / Docker / 远端 | `reqwest` | 见 `harness_agent.rs:1352 web_http_client` |
| `AutoCloseable` | 租约/会话 | `Drop` / `OwnedMutexGuard` | 见 X06 |
| `CompletableFuture` / `ExecutorService` | 后台任务 | `tokio::spawn` + `JoinHandle` | 见 X05 |
| `java.nio.file.Path` | 路径 | `std::path::{Path,PathBuf}` | `Windows` 分隔符差异见 `workspace/path_normalizer.rs` |
| JUnit 5 | 测试 | 内置 `#[test]` / `#[tokio::test]` + `tests/*.rs` | 36 个集成测试文件 |

## 3. 详细缺口清单

本文件为**首版**，无历史 §3.1–§3.4 存量清单（历史条目见被取代的 `GAP_TRACKER.md`）。本轮清单即 §3.5。

## 3.5 2026-10-31 全量对齐复核清单（唯一编号）

> **2026-10-31 合并更正（dev `5883c98`）**：HARNESS-15 的 Windows 永久挂起**已在测试侧修复**（不再 sleep 竞态），降为 P1 设计风险；HARNESS-16 的 `acp_stdio_thought_test` 已改为脚本化 SSE，不再依赖 live reasoning（残留只是 `agent_exe()` 断言预构建 bin，见 ACP-05）。

> **2026-10-31 装配层批次**：HARNESS-01 / 04 / 05 / 09 已完成接线，各自补了走真实 `builder().build()` 的回归用例（不再只直接构造中间件）；同轮补齐 `on_agent_built` + `HarnessAgentHandle`、`deliver_with_receipt` + `DeliveryReceipt`。改动均在 `crates/agentscope-harness/**`，未动 core 与 extension crate。

> 复核基线：Java `main` @ `1524b3191`；Rust 工作树 HEAD `d7f5218`。本表为本轮逐包核对结论，**存量 §3.1–§3.4 历史清单保持原样未重新编号**（本文件为首版，故无历史段）。
> 编号规则：`<MOD>-NN`（缺口，状态列取值 `待办` / `进行中` / `已完成` / `设计决定（不迁移）` / `待裁定`）；§4.3 差异另有 `HARNESS-D<NN>` / `HARNESS-X<NN>` 系列。编号本模块内唯一、单调递增，不复用历史编号。
> `HARNESS-15` 已去重退役（2026-10-31）：与 `docs/ACP_AGENT__GAP_TRACKER.md` 的 `ACP-01` 为同一问题，执行归 ACP-01；编号保留、不复用（清单条目 544 → 543）。
> **2026-10-31 裁定（汇总方已确认）**：本文件的 `待裁定` 项已全部裁定，状态列更新为 `待办` / `设计决定（不迁移）` / `已完成`；裁定结论、依据与执行批次见 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` §3.3。
> 正文其它位置出现的旧「待裁定 N 项」为复核时点快照，一律以 §3.5/§3.6 表格的状态列为准。

### 3.5.1 缺口与待裁定

| 编号 | 优先级 | 缺口项（可执行描述） | Java 参考（文件#符号） | Rust 现状（文件:行 / rg 零命中） | 执行状态 | 下一动作 |
|---|---|---|---|---|---|---|
| HARNESS-01 | P1 | 缺 `workspace/tools.json` 的工具面治理：`allow`/`deny` 过滤 + 平台工具豁免 + `mcpServers` 声明式注册 | `tools/ToolsConfig.java#allow,deny,mcpServers`；`tools/ToolFilter.java#apply`；`tools/ToolsConfigLoader.java#load`；`tools/HarnessPlatformTools.java#NAMES` | `rg 'ToolsConfig'` 仅命中 `extensions-controlplane/src/adapter/workspace_factory.rs:60,485` 的注释；`rg 'ToolFilter|ToolsConfigLoader|HarnessPlatformTools'` 全仓**零命中**（另核 `allow_tools`/`deny_tools` 亦零命中）；`workspace/constants.rs:9` 定义 `TOOLS_JSON` 但**无读取者** | 已完成（2026-10-31） | 新增 `agent/tools.rs`：`ToolsConfig`/`McpServerEntry`/`ToolsConfigLoader`（`workspace/tools.json`，`${VAR}` 替换，缺失或非法降级为无配置）/`ToolFilter`/`HarnessPlatformTools::NAMES`。接线点在 `harness_agent.rs` 的「Tool governance」段（`harness_agent.rs:2130` 起）：**所有 catalogued 工具注册之后、meta tool/子代理/plan/skill 等平台工具之前**，先按 `mcpServers` 调 `Registrar::register`，再 `ToolFilter::apply`（`allow` 不动平台工具、`deny` 覆盖 `allow`）。测试：`tests/harness_agent_built_test.rs::builder_hands_the_finished_agent_to_the_host_callback`（allow 窄化 + `agent_spawn` 存活 + deny 生效，走真实 `builder().build()`）、`::tools_json_in_the_workspace_is_loaded_and_applied`（无显式配置时从 workspace 读取）、`agent/tools.rs` 内 4 个单测（过滤语义/装载/环境变量替换） |
| HARNESS-02 | P1 | 缺模型故障转移：主模型失败后切 `fallbackModel`，并回调 `FailoverListener`；builder 未暴露 `maxRetries` | `HarnessAgent.java#Builder.fallbackModel,failoverListener,maxRetries`（委托 core `ReActAgent.Builder`）；core `agent/config/FailoverListener.java` | `rg 'fallback_model|failover'` 于 `crates/agentscope-core/src` + `crates/agentscope-harness/src` **零命中**（另核 `secondary model`/`max_retries` 亦零命中）。core 仅有重试（`react_agent.rs:2539 ExecutionConfig::model_defaults`，max_attempts=7） | 待办 | core 侧新增 failover（需 core 单智能体独占改 `react_agent.rs`）+ harness builder 三个旋钮；验收：主模型返回 5xx 连续失败后切备用模型并只发一次 failover 事件 |
| HARNESS-03 | P1 | 缺文件系统 Spec 三件与 builder 的 Spec/路由入口（`filesystem(Local/Remote/SandboxFilesystemSpec)`、`filesystemRoute`） | `filesystem/spec/{Local,Remote,Sandbox}FilesystemSpec.java`；`HarnessAgent.java#Builder.filesystem,filesystemRoute,abstractFilesystem` | harness 无 Spec 类型：`crates/agentscope-extensions-sandbox/src/fs_spec.rs:1-20` 明确记录「harness has **no** `SandboxFilesystemSpec`」并自建 stand-in；builder 只有 `filesystem(Arc<dyn Filesystem>)`（`harness_agent.rs:826`）；`rg 'filesystem_route|abstract_filesystem'` **零命中** | 待办 | harness 引入 `FilesystemSpec`（`build(&self, ...) -> Arc<dyn Filesystem>`）+ `CompositeFilesystem` 路由入口；验收：spec 构造与 Java 字段一一对应，扩展 crate 的 stand-in 可改回 harness 类型 |
| HARNESS-04 | P1 | **`AsyncToolMiddleware` 未接线**：`async_tool_timeout` 是死旋钮（存了、传进子代理蓝图，但 `build()` 从未构造中间件） | `HarnessAgent.java:2988`（`new AsyncToolMiddleware(messageBus, asyncToolTimeout, asyncToolRegistry)`） | `harness_agent.rs:1328` 存旋钮、`:2246` 传入 blueprint，`build()` 内**无** `AsyncToolMiddleware::new`；全仓仅 `tests/harness_middleware_test.rs:156,244` 直接构造 | 已完成（2026-10-31） | 接线点 `harness_agent.rs:1883-1891`：`async_tool_timeout` 为 `Some` 时，用与 `InboxMiddleware` 同一份 `message_bus` + `async_tool_registry` 构造 `AsyncToolMiddleware` 并挂在 inbox 之前（二者默认值同为 `WorkspaceMessageBus`/`WorkspaceAsyncToolRegistry`，路径 `.agentscope/bus`）。测试：`tests/harness_async_tool_wiring_test.rs::async_tool_timeout_installs_the_offloading_middleware`（走真实 `build()`：第二次模型调用收到 `running in background` 占位、后台工具仍执行一次）、`:without_async_tool_timeout_the_tool_result_is_inline`（反例，证明是旋钮驱动而非默认行为） |
| HARNESS-05 | P1 | **`SandboxLifecycleMiddleware` 未接线**：builder 无 sandbox 入口，中间件只被测试构造 | `HarnessAgent.java:2805`（`new SandboxLifecycleMiddleware(sandboxManager, capturedSandboxFs)`）；`:214,247` 持有字段 | 仅 `tests/harness_sandbox_lifecycle_test.rs:119` 构造；`harness_agent.rs` **零引用**（`rg 'SandboxLifecycle'` 只命中 `middleware/mod.rs:39` 与测试） | 已完成（2026-10-31） | builder 已有 `sandbox_manager(Arc<SandboxManager>)`（`harness_agent.rs:1468`）与 `sandbox_context(..)`（`:1475`）；`build()` 在 `harness_agent.rs:1716-1728` 据其安装 `SandboxLifecycleMiddleware`（无 manager 则不装）。同轮把 `SandboxManager::release` 改为按值消费 `SandboxAcquireResult` + `take_sandbox()` + `close()`（fail-closed：`close` 失败仍补 `shutdown`），lease 仍只释放一次。测试：`tests/harness_sandbox_wiring_test.rs::builder_installs_the_sandbox_lifecycle_middleware`（真实 `build()` + `stream_events`：create==1、stop==1、shutdown==1，第二次调用仍能 acquire 证明 lease 已释放）、`manager.rs` 内 `release_closes_a_self_managed_sandbox` / `release_leaves_a_user_managed_sandbox_alive` |
| HARNESS-06 | P1 | 缺两个内置确定性校验器与报告回读：`ShellExitCodeVerifier`、`JsonResultSchemaVerifier`、`VerificationService#load` | `verification/ShellExitCodeVerifier.java#verify`；`verification/JsonResultSchemaVerifier.java#verify`；`verification/VerificationService.java#load` | harness 仅有 trait `DeterministicVerifier`（`verification/mod.rs:61`）与 `verify()`；`rg 'ShellExitCodeVerifier|JsonResultSchemaVerifier'` 全 workspace **零命中**（另核 `shell_exit`/`json_result_schema` 零命中）；`load` 未迁（`verification/mod.rs:25-45` 自述） | 待办 | 在 `verification/` 下补两实现（出错即 `VerificationOutcome::Error`，reason ≤2000 字符）+ `load`（读 `SessionProjection::verifications`）；验收：Java 错误文案逐字对齐 + 反向验证 |
| HARNESS-07 | P1 | `AgentGenerateTool` 退化为模板渲染：缺 LLM 驱动的 `SubagentSpecGenerator` | `subagent/SubagentSpecGenerator.java`（模型生成 frontmatter markdown）；`tool/AgentGenerateTool.java:37,50` 包装 | 新增 `agent/subagent/spec_generator.rs`：`SubagentSpecGenerator::{generate_markdown,generate_and_validate}` + `GeneratedSpec`；`tool/agent_generate.rs:23` 增 `model: Option<Arc<dyn Model>>` + `with_model`（无模型时仍走模板） | 已完成（2026-11-01） | 生成用 `context::auxiliary_text`（单次 Summary 调用）并 `loader::parse` 往返校验，不可解析即 `Error::Agent("LLM produced a malformed subagent spec …")`（对齐 Java `IllegalStateException`）；`mod.rs`/`agent/mod.rs` re-export。`AgentGenerateTool` 有模型时用生成结果，生成或解析失败记 warn 并降级为原模板。测试：`spec_generator.rs::generated_markdown_round_trips_through_the_loader`、`::existing_ids_reach_the_prompt`、`::a_malformed_answer_is_an_error`、`agent_generate.rs::model_generation_writes_a_parseable_spec`、`::a_malformed_generation_falls_back_to_the_template` |
| HARNESS-08 | P2 | MCP 工具名无 `server__` 前缀选项：两 server 同名工具会覆盖 | `tools/McpServerRegistrar.java:146`（`isPrefixToolNames()` → `reg.mcpToolNamePrefix(name + "__")`） | `McpServerConfig` 增 `prefix_tool_names: bool`（默认 `false`）+ builder `prefix_tool_names(bool)`（`mcp_client_manager.rs:146-215`）；`agent/tools.rs::McpServerEntry.prefix_tool_names`（JSON `prefixToolNames`）经 `to_server_config` 传入；`agent/mcp.rs::try_bridge_mcp_tools_to_toolkit` 按客户端配置注册 `{server}__{tool}` | 已完成（2026-11-01） | 只对**注册名**加前缀：远端调用、错误文案与 `McpMetaPropagationOptions` 的 per-tool 覆盖仍用未加前缀的原始名（与 Java 覆盖键语义一致，`McpToolNamePrefix` 仅影响 toolkit 可见名）。测试：`tests/harness_mcp_bridge_test.rs::prefix_tool_names_registers_server_prefixed_tool`（真实 echo MCP server：`echo-server__echo_message` 可调、裸名不存在） |
| HARNESS-09 | P2 | MCP 服务器装配不在 builder：无 `mcpServerRegistrationListener` / `mcpServers` 入口 | `HarnessAgent.java#Builder.mcpServerRegistrationListener`；`tools/McpServerConfig.java` | `McpServerRegistrar` 为独立 API（`agent/mcp.rs:79`）；`rg 'mcp' harness_agent.rs` **零命中**（builder 无 MCP 方法） | 已完成（2026-10-31） | builder 增 `mcp_registrar(Arc<dyn Registrar>)`（`harness_agent.rs:1491`），`build()` 在工具治理段把 `tools.json` 的 `mcpServers` 逐个交给它（与 HARNESS-01 同一条装配链）；`mcpServerRegistrationListener` 由 `McpServerRegistrar::with_listener` 在注册器侧携带。`McpServerConfig` 同轮补齐 `transport: Option<McpTransport>`（显式 pin 权威、`None` 保留嗅探）与 harness 侧 `McpToolSource` + `McpClientSource` 适配器。测试：`mcp_client_manager.rs` 内 `transport_pin_overrides_sniffing` 等 / `tests/harness_mcp_tool_source_test.rs` |
| HARNESS-10 | P2 | 文件系统组合子缺：`RoutedSandboxFilesystem`、`PinnedSandboxFilesystem`、`ProjectAwareOverlay`、`Abstract/BaseSandboxFilesystem` | `filesystem/{RoutedSandboxFilesystem,PinnedSandboxFilesystem,ProjectAwareOverlay,AbstractSandboxFilesystem,BaseSandboxFilesystem}.java` | `filesystem/sandbox.rs` 新增 `PinnedSandboxFilesystem` / `RoutedSandboxFilesystem`，`filesystem/mod.rs` re-export；`Abstract/BaseSandboxFilesystem` 仍不迁（D05），`ProjectAwareOverlay` 见 D09 | 已完成（2026-11-01，Pinned + Routed） | `Pinned`：持有已 acquire 的 `Arc<dyn SandboxFilesystem>`（`inner()` 可 `Arc::ptr_eq` 验证不重新 acquire），`release()` 发布 latch（幂等）后所有文件/执行操作返回 `Err("Sandbox has been released")`。`Routed`：文件操作经 `CompositeFilesystem`（最长前缀胜，primary 为默认后端），`execute` 恒走 primary，因此挂载额外 store 不会丢失 `shell_execute`；`kind()` 上报 `Sandbox`。测试：`sandbox.rs::pinned_keeps_the_acquired_filesystem_and_rejects_after_release`、`::routed_sends_prefixed_paths_to_the_route_and_keeps_execute_on_primary` |
| HARNESS-11 | P2 | skill curator 长尾缺 4 类：`BaseStoreSkillUsageBackend`、`CompositeFilter`、`AbstractAgentCreatedFilter`、`LocalApprovalGate` | `skill/curator/{BaseStoreSkillUsageBackend,CompositeFilter,AbstractAgentCreatedFilter,LocalApprovalGate}.java` | `skill/filters.rs` 新增 `BaseStoreSkillUsageBackend`（`USAGE_STORE_NAMESPACE = ["skills","usage"]`）；`skill/promoter.rs` 新增 `CompositeFilter`；两者已 re-export；`AbstractAgentCreatedFilter` / `LocalApprovalGate` 见 D10 | 已完成（2026-11-01，store 后端 + 组合过滤器） | `BaseStoreSkillUsageBackend`：`load_all` = `search(ns, 1000, 0)`（畸形记录跳过），`get` 空名返回 `None`，`mutate` = `get`→mutator→`put_if_version`（最多 5 次 CAS；返回同一记录则不写，返回 `None` 则删），`replace_all` 只 upsert 已有键。`CompositeFilter`：按序 AND，空集恒真。测试：`filters.rs::base_store_backend_roundtrip_and_cas_versions`（版本 1→2）、`::base_store_backend_ignores_malformed_records`、`::base_store_backend_no_op_mutation_does_not_write`、`promoter.rs::composite_filter_requires_every_filter_to_admit` |
| HARNESS-12 | P2 | 缺 `stopOnReject`：工具被权限拒绝后是否终止本轮 | `HarnessAgent.java:1941`（委托 core `ReActAgent.Builder.stopOnReject`，core `ReActAgent.java:6362`） | `rg 'stop_on_reject'` 于 core+harness **零命中**（另核 `stop_on_rejection` 零命中） | 待办 | core 增字段 + harness 旋钮；验收：拒绝后按开关分别「继续下一步」与「以 Rejected 结束」 |
| HARNESS-13 | P2 | builder 未暴露执行配置（`modelExecutionConfig`/`toolExecutionConfig`/`maxRetries`），只能经 `generate_options_defaults` 迂回 | `HarnessAgent.java#Builder.modelExecutionConfig,toolExecutionConfig` | builder 新增 `model_execution_config(ExecutionConfig)` / `tool_execution_config(ExecutionConfig)`（`harness_agent.rs:1443-1489`）；前者经私有 `effective_generate_options()` 叠到 generation defaults，后者写入 `ToolkitConfig.execution_config` | 已完成（2026-11-01） | 工具策略在 `build()` 创建 toolkit 后、任何工具注册与子代理蓝图 `toolkit.copy()` 之前写入，因此子代理继承；模型策略在调用方未设 generation defaults 时只带 `execution_config`（其余字段仍为默认，语义与不设相同）。`maxRetries` 归 HARNESS-02（需 core failover），不在本条目。测试：`harness_agent.rs::tool_execution_config_reaches_the_built_toolkit`、`::model_execution_config_overlays_the_generation_defaults` |
| HARNESS-14 | P2 | 缺框架内置 context 约定文案（Java 从 classpath 资源加载一次性注入） | `middleware/ContextConventionsMiddleware.java`（`/io/agentscope/harness/context-conventions.md`，`HarnessRuntimeMiddleware` 标记） | `assets/context-conventions.md`（Java 资源逐字迁移）+ `agent/context/conventions.rs`：`CONTEXT_CONVENTIONS`（`include_str!`）、`CONTEXT_CONVENTIONS_INSTRUCTION_ID`，经 `context/mod.rs` re-export | 已完成（2026-11-01，资源 + 显式注册） | **不默认注入**：Rust context 编译器是 opt-in（无 source/instruction 时完全不挂 `ContextMiddleware`），无条件注册会把每个 agent 拉进预算/压缩路径；Java 默认 on 的差异记 X10。调用方：`builder.instruction(CONTEXT_CONVENTIONS_INSTRUCTION_ID, CONTEXT_CONVENTIONS)`（System placement、required、priority 100）。测试：`conventions.rs::the_resource_carries_the_java_conventions`、`::registering_the_conventions_yields_a_required_system_instruction` |
| HARNESS-16 | P1 | `acp_stdio_thought_test` 依赖 live 模型 reasoning，偶发失败 | Java 无对应 | `crates/agentscope-extensions-acp/tests/acp_stdio_thought_test.rs`；`EXTENSIONS_GAP_TRACKER.md` §4.2-2 | 已完成 | 归属 acp crate：改为本地 StubModel 注入 reasoning，去掉 live 依赖 【裁定 2026-10-31：已修：acp_stdio_thought_test 改为内置脚本化 SSE，不再依赖 live reasoning】 |
| HARNESS-17 | P2 | `training::runner` 进程级 `OnceLock` 静态注册表 + `clear_all()` 测试间竞争 | Java 无对应 | `crates/agentscope-extensions-training/src/...runner`；`EXTENSIONS_GAP_TRACKER.md` §4.2-3 | 待办 | 归属 training crate：注册表改为实例级或加串行锁 【裁定 2026-10-31：改实例级注册表，删全局 clear_all 依赖】 |

### 3.5.2 本轮复核确认已对齐（抽样，不求穷举）

| 编号 | 能力 | Java 参考 | Rust 证据（文件:行 + 测试名） | 执行状态 |
|---|---|---|---|---|
| HARNESS-18 | 双轨 session（EventLog 为唯一权威、Legacy 仅 opt-out） | `HarnessAgent.java#Builder.sessionHistoryMode,legacySessionHistory` | `harness_agent.rs:795,1389-1392,1500-1520,2192-2210`；`tests/harness_session_journal_wiring_test.rs`、`harness_session_test.rs` | 已完成 |
| HARNESS-19 | context 编译器（预算/裁剪/manifest 观测/溢出转 compaction） | `context/{HarnessContextBuilder,ContextRenderer,ContextManifest,ContextPolicy}.java` | `agent/context/builder.rs:171-500`（`BudgetExceeded` :130）；`tests/harness_context_wiring_test.rs` | 已完成 |
| HARNESS-20 | 子代理编排（声明/动态扫描/工厂/任务仓库） | `subagent/{DefaultAgentManager,SubagentFactory,AgentSpecLoader}.java`、`SubagentDeclaration.java` | `harness_agent.rs:1943-2064`；`tests/harness_dynamic_subagents_test.rs`、`harness_agent_spawn_test.rs`、`harness_subagent_wiring_test.rs` | 已完成 |
| HARNESS-21 | 沙箱全家族（Manager/Lease/Guard/IsolationKey/快照三态/布局） | `sandbox/{SandboxManager,SandboxLease,SandboxExecutionGuard,SandboxIsolationKey}.java` | `sandbox/{manager,lease,guard,key,snapshot,layout}.rs`（`manager.rs`/`lease.rs`/`guard.rs`/`key.rs`）；`tests/harness_docker_sandbox_test.rs`（生命周期中间件的**接线**另见 HARNESS-05） | 已完成（fail-closed 取锁 2026-10-31） | `SandboxGuard::try_enter` 现签名 `async fn try_enter(&self, key: SandboxIsolationKey) -> agentscope_core::error::Result<SandboxLease>`（`guard.rs:36-44`）：取锁失败**必须**返回 `Err`，不得返回 `SandboxLease::noop()` 继续执行；`SandboxManager::acquire` 处 `lease = self.execution_guard.try_enter(key.clone()).await?`（`manager.rs:213`）失败即中止、不创建沙箱；进程内默认 guard 与 COS/OSS 委托型 guard 恒 `Ok(lease)`。回归：`manager.rs::guard_acquisition_failure_aborts_the_acquire`（注入恒失败 guard：错误上浮 + `create` 调用 0 次） |

| HARNESS-22 | 网关/渠道路由（含 chatui 与平台适配层） | `gateway/{HarnessGateway,ChannelManager,ChannelRouter,SessionTurnGate}.java` | `gateway/harness_gateway.rs:109-224`、`gateway/session_turn_gate.rs:67`；`tests/harness_channel_test.rs` | 已完成 |
| HARNESS-23 | 记忆管线（flush/maintenance/compaction/eviction/consolidation） | `memory/{MemoryFlushManager,MemoryConsolidator,compaction/ConversationCompactor}.java` | `harness_agent.rs:1621-1675`；`tests/harness_memory_test.rs`、`compaction_metrics_test.rs` | 已完成 |
| HARNESS-24 | skill 运行时 + curator + 自学习工具 | `skill/runtime/{SkillRuntime,SkillCatalog,SkillLoadTool}.java`、`skill/curator/SkillCurator.java`、`tool/{SkillManageTool,ProposeSkillTool}.java` | `agent/skill/{runtime,curator,manage}.rs`；`tests/harness_skill_workspace_test.rs` | 已完成 |
| HARNESS-25 | MCP 桥接 + meta 传播（fail-open 与逐工具覆盖） | `tools/McpServerRegistrar.java`、`McpServerRegistrationListener.java` | `agent/mcp.rs:254-283,194-219`；`tests/harness_mcp_bridge_test.rs`、`harness_mcp_meta_test.rs` | 已完成 |
| HARNESS-26 | team 客户端与团队工具/中间件 | `team/{LocalTeamClient,TeamClient,TeamContext}.java`、`tool/TeamTool.java` | `agent/team/mod.rs`、`tool/team.rs`；`tests/harness_team_test.rs`、`harness_team_tool_test.rs`、`harness_teams_middleware_test.rs` | 已完成 |
| HARNESS-27 | 权限/计划模式（plan 白名单、`plan_mode` 状态机） | `middleware/PlanModeMiddleware.java`、`workspace/plan/PlanModeManager.java`、`tool/PlanModeTools.java` | `middleware/plan_mode.rs`、`workspace/plan.rs`、`tool/plan_mode.rs`；`tests/harness_plan_wait_tool_test.rs` | 已完成 |
| HARNESS-28 | 中断恢复（pending tool recovery + 逐迭代 checkpoint） | `ReActAgent.java`（pending recovery）、`middleware/StateCheckpoint*.java` | `middleware/state_checkpoint.rs`、`harness_agent.rs:1301`；`tests/resume_dangling_tool_call_test.rs`、`state_checkpoint_per_iteration_test.rs`、`state_checkpoint_conflict_policy_test.rs` | 已完成 |
| HARNESS-29 | builder 构建回调：外部 crate 在 `build()` 收尾时拿到代理句柄（Java 用 `bindAgent`，无等价回调） | Java 无对应 | `harness_agent.rs:152` `AgentBuiltCallback`、`:160` `HarnessAgentHandle`、`:1500` `on_agent_built`，回调在 build 尾部调用一次；`tests/harness_agent_built_test.rs::builder_hands_the_finished_agent_to_the_host_callback` | 已完成 |
| HARNESS-30 | 渠道回执：`Channel`/`ChannelSink` 默认 `deliver_with_receipt` + `DeliveryReceipt`（默认 `Unsupported`，自定义 sink 可返回 provider message id / 原始 JSON） | Java `gateway/Channel.java`（发送返回回执） | `agent/gateway/channel/channel.rs`、`agent/gateway/harness_gateway.rs`、`agent/gateway/{channel/mod,mod}.rs`、`agent/mod.rs`、`lib.rs` re-export；`tests/harness_channel_receipt_test.rs` | 已完成 |

## 4. 项目状态与下一步

### 4.1 当前状态

- **已核实**：Java 295 个类按 24 个包/子包完成映射归类（§2）；11 条抽样能力确认已接线且有走真实装配入口的测试（§3.5.2）。
- **测试基线**（2026-11-01 本轮实测）：
  - `cargo build -p agentscope-harness -j 2` → **EXITCODE=0**，改动文件无新增 warning（既有 warning：`mcp_client_manager.rs` 的 `SdkResult` 未使用、`sandbox/manager.rs` 的三个未使用导入、`gateway/harness_gateway.rs::ExposedSession` 字段未读）。
  - `cargo test -p agentscope-harness --lib -j 2 --no-fail-fast` → **573 passed / 0 failed / 0 ignored**（无 `^failures:` 段）。
  - `cargo test -p agentscope-harness --test harness_mcp_bridge_test -j 2` → **3 passed / 0 failed**（真实 node echo MCP server）。
  - 全量集成 target 未跑（并行构建锁竞争下 21 分钟才链接一次 lib test，见 §4.2-6）。
- **本 crate 主要薄弱点**：builder 装配面仅剩 filesystem Spec（HARNESS-03）与 failover（HARNESS-02）/stopOnReject（HARNESS-12，两者需 core）；执行配置（HARNESS-13）、MCP 前缀（HARNESS-08）、FS 组合子（HARNESS-10）、skill store 后端（HARNESS-11）、spec 生成（HARNESS-07）、context 约定（HARNESS-14）已于 2026-11-01 补齐。

### 4.2 下一步与阻塞

1. **已完成（2026-10-31）**：HARNESS-04（`AsyncToolMiddleware`）、HARNESS-05（`SandboxLifecycleMiddleware`）两处「已落盘未接线」已接入 `build()`，另有 HARNESS-01/09 的装配链；每项均有走真实 `builder().build()` 的回归用例（`tests/harness_async_tool_wiring_test.rs`、`tests/harness_sandbox_wiring_test.rs`、`tests/harness_agent_built_test.rs`）。**2026-11-01 Round 4** 另关闭 HARNESS-07/08/10/11/13/14（无 core 依赖，逐行见 §3.5.1）。
2. ~~产品可用性~~：HARNESS-01（tools.json 治理）与 HARNESS-09（MCP 声明式注册）已串成同一条链（先 `mcpServers` 注册、后 `allow`/`deny` 过滤）。
3. **可靠性**：HARNESS-02（failover）与 HARNESS-12（stopOnReject）需改 `agentscope-core`，**必须由单一智能体独占串行**，不得与其他 core 改动并行。
4. **阻塞**：HARNESS-03 依赖是否把 `FilesystemSpec` 上提到 harness（当前扩展 crate 已有 stand-in，属架构裁定，记 §4.2 待汇总方裁定）。
5. **跨 crate 项（2026-10-31 裁定后）**：原 HARNESS-15 已去重退役（归 acp `ACP-01`）、HARNESS-16 关闭为 `已完成`、HARNESS-17 转 `待办`（training 注册表改实例级）；三者均不在本 crate 边界内，仅登记不实施。
6. **2026-11-01 Round 4 观测**：本轮 20 路并行共享 `target` 下，单次 lib test 需排队 ~40 分钟、链接占 ~21 分钟；建议后续把 harness 的验证放到独立时段，或由主智能体集中跑一次。另一个真实副作用：core 新增 `ActionObservation.execution_details` 直接打穿了 harness 的测试构造（已就地补 `execution_details: None`）——**core 加字段必跑 `cargo test -p agentscope-harness --lib --no-run`** 以及时暴露。

### 4.3 与 Java 的已知差异（有意为之，非缺口）

#### 4.3.1 设计决定（不迁移）

| 编号 | 项 | Java 参考 | 不迁移理由（源码级依据） | 影响面 |
|---|---|---|---|---|
| HARNESS-D01 | `StateStoreActionObserver` / `StoredActionObservation` 的独立观测存储与回放 | `observation/StateStoreActionObserver.java`、`observation/StoredActionObservation.java` | Rust 以 `core::observation::ActionObservation`（只带身份，不带证据）+ `verification::SettledAction` 由调用方传证；`verification/mod.rs:22-33` 明确「引入 store 会形成与 session journal 平行的持久化路径」，故不迁 | verification 需调用方自备证据与持久化（见 HARNESS-06） |
| HARNESS-D02 | `HarnessSandboxJacksonModule`（Jackson 多态注册模块） | `sandbox/json/HarnessSandboxJacksonModule.java` | Rust 用 serde 标签枚举表达同一多态：`sandbox/layout.rs:216 WorkspaceEntry`、`sandbox/layout.rs:297 LayoutSpec`；`rg 'HarnessSandboxJacksonModule'` 零命中 | 无（JVM 序列化专属） |
| HARNESS-D03 | `HarnessRuntimeMiddleware` 标记接口（框架自有中间件不被 `fromAgent` 复制） | `middleware/HarnessRuntimeMiddleware.java` | Rust 无 Java `Builder.fromAgent(ReActAgent)` 的等价构造（`rg 'from_agent|fromAgent' harness_agent.rs` 零命中；`subagent/protocol/codec.rs:25 from_agent_event` 是无关的协议转换）；`rg 'HarnessRuntimeMiddleware'` 零命中 | 无 |
| HARNESS-D04 | `externalSubagentTool(Object)` / `registerExternalSchemas(List<ToolSchema>)` 的 JVM 反射式注入 | `HarnessAgent.java#Builder.externalSubagentTool,registerExternalSchemas`；`toolkit.registerSchemas` | Rust 侧等价能力为 `Toolkit::register_tool(Arc<dyn Tool>)` 与 `core::tool::SchemaOnlyTool`（`core/src/tool/schema_only_tool.rs`）；`rg 'external_subagent|register_external_schemas'` 零命中 | 宿主需显式注册外部工具（ACP 已这么做，`acp/session.rs:1107`） |
| HARNESS-D05 | 抽象基类 `AbstractBaseSandbox` / `AbstractSandboxFilesystem` / `BaseSandboxFilesystem` | `sandbox/AbstractBaseSandbox.java`、`filesystem/sandbox/{AbstractSandboxFilesystem,BaseSandboxFilesystem}.java` | Rust 以 trait + 组合表达：`sandbox/mod.rs:76 Sandbox`、`:118 SandboxClient`、`filesystem/mod.rs:122 Filesystem`、`filesystem/sandbox.rs:17 SandboxFilesystem`；`rg 'AbstractBaseSandbox|BaseSandboxFilesystem'` 零命中 | 无（模板方法模式由组合替代） |
| HARNESS-D06 | `LocalFilesystemWithShell`（把 shell 能力塞进 FS 接口） | `filesystem/local/LocalFilesystemWithShell.java` | Rust 把 shell 做成独立工具 `tool/shell_execute_tool.rs`（`ShellExecuteTool`），FS 与 exec 解耦；`rg 'LocalFilesystemWithShell'` 零命中 | shell 与 FS 权限需分别配置（`harness_agent.rs:1065 disable_shell_tool`） |
| HARNESS-D07 | `SandboxAware`（DI 容器标记接口） | `sandbox/SandboxAware.java` | Rust 依赖显式 `Arc` 注入，无需容器标记；`rg 'SandboxAware'` 零命中 | 无 |
| HARNESS-D08 | `HarnessAgentBuilderSupport`（构建期静态辅助类） | `HarnessAgent.java`、`HarnessAgentBuilderSupport.java` | Rust 单文件 builder（`harness_agent.rs:806-1400`）已覆盖其职责，无拆分必要 | 无 |
| HARNESS-D09 | `ProjectAwareOverlay`（按 project 挂载 overlay 的 FS 装饰器） | `filesystem/ProjectAwareOverlay.java` | Rust 已用 `OverlayFilesystem`（写穿透 overlay）与 `CompositeFilesystem`（前缀路由）组合表达同一语义，且 harness 无 Java `ProjectRegistry` / `WorkspaceManager.getProject()` 等价物（`rg 'project_aware|ProjectRegistry'` 零命中）。2026-11-01 新增 `RoutedSandboxFilesystem` 后可直接拼出「primary 沙箱 + 前缀挂载」，无需另建装饰器 | 多 project 路由由宿主用 `CompositeFilesystem` 组合 |
| HARNESS-D10 | `AbstractAgentCreatedFilter`（只约束 agent 自建 skill 的过滤基类）与 `LocalApprovalGate`（stdin HITL 提升门） | `skill/curator/{AbstractAgentCreatedFilter,LocalApprovalGate}.java` | ①Rust 无继承：「非 agent 自建一律直通」已内联在 `CanaryFilter::is_visible`（仅 `created_by == "agent"` 参与百分比门控），再建基类会形成两条等价路径；组合需求由新增 `CompositeFilter` 满足。②`SkillPromotionGate::review` 是**同步非阻塞**（`promoter.rs:63`），而 `LocalApprovalGate` 靠阻塞 stdin + 30min 超时；Rust 的 HITL 语义由 `NotifyAndWaitGate`（写 `.review_request.json` 后返 `NeedsReview`，批准由外部 promote 调用触发）承担，阻塞式 prompter 不入库 | 单机交互式批准由宿主 CLI 自实现 `SkillPromotionGate` |

#### 4.3.2 设计决定与已知差异

| 编号 | 项 | Rust 实现 | 与 Java 的差异 | 为何保留（收益/依据） |
|---|---|---|---|---|
| HARNESS-X01 | 双轨 session：`session_history_mode` 默认 `EventLog`，`Legacy` 仅显式 opt-out | `harness_agent.rs:795`（默认值）、`:1389-1392`（切 Legacy 的 doc）、`:2192-2210`（`match &session_log_store` 的两个 builder 分支） | Java 以 `legacySessionHistory(boolean)` 反向开关；Rust 用枚举且默认不兼容旧布局 | 单一历史权威：EventLog 下 `StateCheckpointMiddleware` **不挂载**（`harness_agent.rs:2111`），避免两个写入者在崩溃后互相矛盾（Java 同样规则） |
| HARNESS-X02 | HITL 走 `ConfirmHandler` 内联 await | `harness_agent.rs:421,1263`；core `react_agent.rs:3367`（调用点） | Java 用 `GenerateReason::PERMISSION_ASKING` + `Msg.METADATA_CONFIRM_RESULTS` 恢复（core `ReActAgent.java:2217,2294`，32 轮上限）；Rust 无该往返，直接 await handler | 无 32 轮上限、无需重建对话；代价：控制面不可达时按 deny 处理（Java 抛异常） |
| HARNESS-X03 | ACP 为 Rust 原生实现，独立 crate | `crates/agentscope-extensions-acp/`（17 个源文件 + 6 个集成测试） | Java `agentscope-harness` 只有 `io.agentscope.harness.agent.**` 单包（295 类，无 `acp`/`mcp`/`cli`/`event` 子包），故 ACP 无 Java 基线 | 以 ACP 生态（`agent-client-protocol`）为准，`acp/session.rs` 承载会话/权限/模型切换 |
| HARNESS-X04 | `SessionCompactor`/`SessionHistorySource`/`AgentRuntimeSource`/`AgentTaskStarter` 归宿主，不在 harness 实现 | `crates/agentscope-extensions-controlplane/src/adapter/{support,host_impls,task_starter}.rs`（trait + `HarnessSessionCompactor`/`StoreSessionHistorySource` 实现） | Java 同名接口也在 controlplane（`extensions-controlplane/.../adapter/*.java`），harness 同样不实现；但 Rust 的 harness 额外提供 `SessionLog`/`SessionProjection` 作为实现底座（`session/storage.rs`、core `session/views.rs`） | 依赖方向正确（extensions → harness），harness 不反向依赖控制面 |
| HARNESS-X05 | 后台任务用 `tokio::spawn`，`JoinHandle` drop 即 detach | `agent/subagent/task/background.rs:29,71`（`attach_handle` 可选持有）、`agent/bus/async_tool.rs` | Java 用 `ExecutorService` + `Future`，`cancel(true)` 可中断 | 工具执行**永不取消**（与 Java 语义一致：offload 后后台跑完）；drop=detach 避免误杀；停机由 `GracefulShutdownController` 统一 |
| HARNESS-X06 | 会话轮次互斥用 `tokio::sync::Mutex` 的 `OwnedMutexGuard` 表达租约 | `gateway/session_turn_gate.rs:67 try_acquire`；控制面另有 `TurnLease`（`extensions-controlplane/src/store/session_turn_gate.rs`） | Java `SessionTurnGate#acquire` 返回 `TurnLease extends AutoCloseable`（`gateway/TurnLease.java:25`），靠 close 释放 | RAII 保证释放（含 panic 路径），无泄漏租约 |
| HARNESS-X07 | MCP 工具默认用原始名（不加 `server__`） | `agent/mcp.rs:269-283` | Java 需 `isPrefixToolNames()` 显式开启才加前缀（`McpServerRegistrar.java:146`），Rust 目前**无该开关**（HARNESS-08 为缺口，默认行为本身一致） | 默认对齐 Java；缺口仅限可选增强 |
| HARNESS-X08 | context 预算超限是**可恢复**信号，触发 compaction | `agent/context/builder.rs:112-170`（`BudgetExceeded::into_error`）、`:446-500`（`validation` 标记） | Java 抛 `ContextBudgetExceededException`（`context/ContextBudgetExceededException.java`）由调用方捕获 | 单次请求即可自愈（截断/压缩后重试），减少一次失败往返 |
| HARNESS-X09 | 工作区路径白名单以 `LocalFilesystem::additional_roots` 表达，而非独立 `PathPolicy` 类 | `filesystem/local.rs:189,201-254`（`LocalFsMode::Rooted` + `additional_roots`） | Java `workspace/PathPolicy.java` 是独立不可变类，随 `LocalFilesystemSpec` 注入 | 收敛到 FS 自身，减少跨层传参；语义等价（空策略=拒绝绝对路径可由 `Rooted` + 空 roots 表达） |
| HARNESS-X10 | context 约定文案以资源 + **显式** instruction 注册，不默认注入 | `agent/context/conventions.rs`（`CONTEXT_CONVENTIONS` = `include_str!("../../../assets/context-conventions.md")`）；调用方 `builder.instruction(CONTEXT_CONVENTIONS_INSTRUCTION_ID, CONTEXT_CONVENTIONS)` | Java `ContextConventionsMiddleware` 默认挂载并每调用重置 `WorkspaceContextMaterials` 后注入（`order() = MAX_VALUE-1000`），Rust 默认不注册 | Rust 的 context 编译器本身是 opt-in：注册任何 instruction 都会启用 `ContextMiddleware`（预算估计 + 溢出转 compaction），默认注入会把所有 agent 无声拉进该路径，改变既有部署的请求形状与 token 成本；显式注册保留同一文案与同一 System/required 语义 |

## 5. 执行约定（所有会话必须遵守）

- 开工前：读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-harness` 基线。
- 改动中：保持 `cargo build/test -p agentscope-harness` 通过；**不得**用「删断言 / `#[ignore]` / 删功能」凑绿。
- **边界**：本模块可改 `crates/agentscope-harness/**`；`crates/agentscope-core/**` 的改动（HARNESS-02/12/13）必须由 core 独占串行并在此登记；`crates/agentscope-extensions-*` 只登记不代改。
- **装配验证**：新增中间件/工具必须有**走真实 `HarnessAgentBuilder::build()`** 的测试（直接构造中间件不算）；`trait` 默认实现会掩盖漏实现（历史 `on_bridge_attached` 事故）。
- 完工后：更新 §3.5 对应行状态 + §4.1/§4.2，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` §1 对应行登记；执行记录写 `docs/extensions/history/HARNESS__MIGRATION_HISTORY.md`。
- 判绿口径：只看 `^failures:` 段与 `EXITCODE`；`rg` 零命中须 2–3 个同义词交叉验证后才可记为「未实现」。

## 6. 执行记录

- 归档：`docs/extensions/history/HARNESS__MIGRATION_HISTORY.md`（本轮未新建，按 BRIEF 要求历史进程不入本文件）。
