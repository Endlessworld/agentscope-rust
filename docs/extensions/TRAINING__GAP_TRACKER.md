# TRAINING 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **TRAINING** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-training\`
> **Java 子模块**：backend/reward/runner/strategy
> **Rust 目标**：`crates/agentscope-extensions-training/`（波次 C）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-training` |
| Java 子模块 | backend/reward/runner/strategy |
| Java 规模（2026-10-22 实测） | 主代码 20 文件 / 3275 总行（有效行 1463）/ 测试 19 文件 / 3457 总行（有效行 2008）；主 20 个顶层类（+8 个嵌套类）；17 个测试类带 `@Test`，`@Test` 共 166 |
| Rust crate | `crates/agentscope-extensions-training` |
| Rust 规模（2026-10-31 实测） | 5 个源文件；`runner.rs` 2272 行（其余未变：`backend.rs` 748、`lib.rs` 32、`reward.rs` 107、`strategy.rs` 720）；**82 个测试**（2026-10-31：`#[test]` 65 + `#[tokio::test]` 17；分模块 strategy 18 / backend 17 / reward 3 / runner 44） |
| 迁移波次 | C |
| 当前状态 | **已完成**（P1/P2 清零；2026-10-31 实测 82 passed / 0 failed）—— 代码面 P0/P1/P2 全清（注册表去进程全局化、`shadow_pool_capacity` 接线、`TrainingHook` 卸载、测试串行锁删除）；余 live 联调 P2-1 需环境（非代码缺口）。历史见 history/TRAINING__MIGRATION_HISTORY.md |

## 2. Java 清单与依赖映射
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `backend/dto/CommitRequest` | Commit API 请求体（task_id/run_id/time_threshold） | `backend::dto::CommitRequest`（serde, snake_case） | builder 语义对齐 |
| `backend/dto/FeedbackRequest` | Feedback API 请求体（msg_ids/reward/task_id/run_id） | `backend::dto::FeedbackRequest` | 同上 |
| `backend/dto/StatusResponse` | 通用状态响应（status/message/isSuccess） | `backend::dto::StatusResponse` | `is_success()` 复刻 `"success".equalsIgnoreCase` |
| `backend/TrinityClient` | Trinity Feedback/Commit HTTP 客户端（OkHttp） | `backend::TrinityClient`（`reqwest`，async） | 只有 `/feedback`、`/commit`；Chat 走 OpenAI 兼容模型 |
| `backend/TrinityModelAdapter` | 组合包装 OpenAIChatModel，拦截响应收集 msg_id → RunExecutionContext | `backend::TrinityModelAdapter`（实现 core `Model` trait，组合 `Arc<dyn Model>`） | **不重建模型**：实现 core `Model` trait 并委派给内层 model；`stream` 中把每个 `ChatResponse.id` 记入 `RunExecutionContext`。**已完成（2026-10-29 勘误）**：原记「Java 强制 `stream(false)`，Rust 建的 `OpenAiChatModel` 恒为流式且无 stream 开关（见 §4.2 P0-2）」，实际已闭环——`TrinityModelAdapter::stream` 置 `options.stream = Some(false)` 并对连续重复 id 去重（`backend.rs:305-325`），MODEL 侧 `openai/mod.rs:271/320` 已读 `options.stream` 并走非流式分支（§4.2 P0-2） |
| `reward/RewardCalculator` | 依据 shadow Agent 执行结果算 reward | `reward::RewardCalculator`（trait，`fn calculate(&self, agent: &ReActAgent) -> f64`） | Rust 用 `ReActAgent` 具体类型（等价 Java `Agent` 参数） |
| `runner/AgentCloner` | 反射提取配置 + Builder 重建 Agent（换 model，新建空 memory） | `runner::AgentCloner::clone_with_model`（基于 `ReActAgent`/`ReActAgentBuilder` 显式重建） | Rust 无反射：name/description/toolkit 由 `Agent` trait 回读，`sys_prompt`/`max_iters` **已通过 core getter 回读**（`ReActAgent::sys_prompt()`/`max_iters()`），`ShadowAgentSpec` 降级为可选覆盖；`modelExecutionConfig`/`toolExecutionConfig`/`toolExecutionContext` **设计决定（不迁移）**（core 无字段/无 setter，见 §4.2 P1-3） |
| `runner/RunExecutionContext` | 单次 Run 的执行上下文（msgIds/msgs/timing） | `runner::RunExecutionContext` | `Arc<Mutex<...>>` 内部可变，线程安全 |
| `runner/RunRegistry` | taskId → Run 计数（自增 runId） | `runner::RunRegistry`（实例句柄：`new()` / `global()`） | Java 为静态单例；Rust 默认**每 router 一个实例**（`TrainingRouter::new`），`RunRegistry::global()` 保留 JVM 级共享能力（`TrainingRouter::with_global_registries`）。测试不再共享进程状态（见 history/TRAINING__MIGRATION_HISTORY.md） |
| `runner/TaskExecutionRegistry` | taskId → Vec<RunExecutionContext> 注册与统计 | `runner::TaskExecutionRegistry`（实例句柄，同上） | 同上；查询按 runId 数值排序 |
| `runner/TaskIdGenerator` | 生成 `task-{uuid}` / `task-{ts}-{rand}` | `runner::task_id_generator`（uuid crate） | `uuid` 已在 workspace deps |
| `runner/TrainingConfig` | 训练配置（endpoint/模型/策略/reward/commit 间隔/池/repeatTime） | `runner::TrainingConfig` + Builder | 默认值对齐 |
| `runner/TrainingRouter` | 训练路由 Hook：PreCall 存输入、PostCall 选择+shadow 执行+feedback | `runner::TrainingRouter` + **`runner::TrainingHook`**（core `Middleware`，`ExtensionPoint::OnAgent`，经 `TrainingRunner::attach(&mut MiddlewareChain)` 显式装配）+ 显式驱动 API `decide(...)` / `run_shadow(...)` + `ShadowRunReport` | 见 §4.2：Java Hook 的 `PreCall/PostCall` 由 `on_agent` 洋葱拦截等价承载（`TrainingHook`，2026-10-28 收口），保留 `-shadow` 递归保护 / repeatTime / runId / feedback 全流程 |
| `runner/TrainingRunner` | 训练生命周期控制（start/stop/周期 commit） | `runner::TrainingRunner` | commit 调度用 tokio task + interval |
| `strategy/TrainingSelectionStrategy` | 选择策略接口（`shouldSelect`/`priority`/`name`） | `strategy::TrainingSelectionStrategy`（trait） | reactor `ContextView` → 显式 `Option<&TrainingAnnotation>` 参数（见 §4.2） |
| `strategy/SamplingRateStrategy` | 按概率采样 | `strategy::SamplingRateStrategy` | 内部 PRNG（不引 rand 依赖） |
| `strategy/ExplicitMarkingStrategy` | 依据显式标记选择（含 TTL） | `strategy::ExplicitMarkingStrategy` | |
| `strategy/SelectionDecision` | 选择结论（shouldTrain/reason/labels/metadata） | `strategy::SelectionDecision` | accept/reject 工厂 |
| `strategy/TrainingAnnotation` | 标记（enabled/taskId/labels/metadata/timestamp + isExpired） | `strategy::TrainingAnnotation` | builder 对齐 |
| `strategy/TrainingContext` | 通过 Reactor Context 传播训练标记 | `strategy::TrainingContext` | 携带 `TrainingAnnotation` 的轻量类型；Rust 无 Reactor Context，改由调用方显式传入（见 §4.2） |
| `util/JsonUtils`（引用） | JSON 编解码 | `serde_json` 直用 | |

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| okhttp3（OkHttpClient/Request/Response/MediaType） | Trinity Feedback/Commit HTTP | `reqwest`（workspace deps 已有，0.12，json+rustls） | 仅本 crate `[dependencies]` 使用 |
| reactor-core（Mono/Flux/Schedulers） | 响应式 + 有界弹性线程池跑 shadow | `tokio`（async fn / spawn / interval） | shadow 跑在 tokio 任务；池参数两个都已接线（`shadow_pool_size` → `Semaphore` 并发上限，`shadow_pool_capacity` → 准入计数上限，2026-10-31） |
| reactor.util.context（Context/ContextView） | 训练标记跨线程传播 | 显式 `TrainingAnnotation` 参数 | Rust 无等价上下文；见 §4.2 |
| jackson（@JsonProperty snake_case） | DTO 序列化 | `serde` + `#[serde(rename_all = "snake_case")]` | commit/feedback 体字段名对齐 |
| `io.agentscope.core.model.openai.OpenAIChatModel` | Trinity Chat（OpenAI 兼容） | `agentscope_extensions_model::OpenAiChatModel`（2026-09-21 前为 `agentscope_core::model::OpenAiChatModel`） | TrinityModelAdapter 组合它（`Arc<dyn Model>`） |
| slf4j | 日志 | `tracing` | |
| java.util.UUID | task id | `uuid` crate | |
| Java 反射（AgentCloner） | 提取 agent 私有字段 | 无反射 → 用 `ReActAgent`/builder 访问器 | 见 §4.2 |

## 3. 详细缺口清单
### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 骨架（模块划分 strategy/reward/backend/runner） | 同包结构 | 已完成 | `src/{strategy,reward,backend,runner}.rs` + `lib.rs` 导出 |
| 新增依赖（reqwest/uuid） | okhttp3/java.util.UUID | 已完成 | 仅写入本 crate `Cargo.toml`；`reqwest`/`uuid` 走 workspace deps |

### 3.2 strategy（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| TrainingAnnotation（builder/isExpired） | TrainingAnnotation | 已完成 | `strategy::TrainingAnnotation`；含 timestamp/ttl 过期判定 |
| TrainingContext（标记传播） | TrainingContext | 已完成 | Rust 无 Reactor Context：提供 `mark`/`mark_with_labels`/`mark_with_metadata`/`with_annotation` + `is_expired` 辅助；不做 Reactor `contextWrite` |
| SelectionDecision（accept/reject + labels/metadata） | SelectionDecision | 已完成 | `strategy::SelectionDecision` |
| TrainingSelectionStrategy trait（priority/name） | TrainingSelectionStrategy | 已完成 | `should_select(&self, agent_name, inputs, output, annotation)` |
| SamplingRateStrategy | SamplingRateStrategy | 已完成 | 内部 PRNG（xorshift，无 rand 依赖）；0/1 边界 + 非法值报错 |
| ExplicitMarkingStrategy（TTL） | ExplicitMarkingStrategy | 已完成 | 无标记→reject(no-explicit-marking)、过期→reject(explicit-marking-expired)、taskId 透传 |

### 3.3 reward（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| RewardCalculator trait | RewardCalculator | 已完成 | `fn calculate(&self, agent: &ReActAgent) -> f64`；提供闭包适配 `FnRewardCalculator` |

### 3.4 backend（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| DTO（CommitRequest/FeedbackRequest/StatusResponse） | dto/* | 已完成 | snake_case serde；`is_success()` |
| TrinityClient（feedback/commit HTTP） | TrinityClient | 已完成 | `reqwest` async；endpoint 必填校验；非 2xx → `Error` |
| TrinityModelAdapter（组合 Model + 收集 msg_id） | TrinityModelAdapter | 已完成 | 实现 core `Model` trait，委派内层 `Arc<dyn Model>`；**非流式强制 + id 去重（2026-10-29 勘误后确认）**：`stream` 内 `options.stream = Some(false)`（对齐 Java `.stream(false)`），并对连续重复 `ChatResponse.id` 只记 1 次，记入 `RunExecutionContext` |

### 3.5 runner（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| RunExecutionContext | RunExecutionContext | 已完成 | taskId/runId/msgIds/msgs/startTime + duration |
| RunRegistry（runId 自增 + cleanup） | RunRegistry | 已完成（2026-10-31 改实例句柄） | 实例字段 `Mutex<HashMap<String, AtomicUsize>>`；`RunRegistry::new()` 隔离实例、`RunRegistry::global()` 进程级单例（Java 对齐）；`TrainingRouter::new` 默认用实例，`with_global_registries` 用全局 |
| TaskExecutionRegistry（注册/查询/统计） | TaskExecutionRegistry | 已完成（2026-10-31 改实例句柄） | 同上：`new()` / `global()` / `TrainingRouter::with_registries(...)` 注入 |
| TaskIdGenerator | TaskIdGenerator | 已完成 | `task-{uuid}` / `task-{ts}-{rand}` |
| TrainingConfig + Builder | TrainingConfig | 已完成 | 默认值对齐；build 校验 endpoint+rewardCalculator |
| AgentCloner（换 model 重建 shadow） | AgentCloner | 已完成（2026-10-28） | 基于 `ReActAgentBuilder` 重建（name+`-shadow`/description/toolkit/model/`todo_tool(false)`）；**`sys_prompt`/`max_iters` 默认从原 agent 回读**（`original.sys_prompt()`/`original.max_iters()`），`ShadowAgentSpec` 仅在显式提供时覆盖（§4.2 P1-1 已关闭）；`modelExecutionConfig`/`toolExecutionConfig`/`toolExecutionContext` = **设计决定（不迁移）**（core 无关字段与 setter，见 §4.2 P1-3） |
| TrainingRouter（选择 + shadow + feedback） | TrainingRouter | 已完成（语义调整） | 提供显式 `decide`/`run_shadow` API，以及 **`TrainingHook`（core `Middleware`, `OnAgent`）** 自动驱动；保留 `-shadow` 递归保护、repeatTime/runId/feedback 全流程；`shadow_pool_size` 接线 = 信号量并发上限（`available_shadow_slots()`），`shadow_pool_capacity` **2026-10-31 接线** = 有界准入队列上限（`pending_shadow_runs()`，超限丢弃并告警；§4.2 P2-2）；`run_registry()`/`task_registry()` 暴露实例注册表 |
| TrainingRunner（start/stop/周期 commit） | TrainingRunner | 已完成 | tokio 任务 + interval；start/stop 幂等；**调度器只看 `commit_interval_seconds`**（`enable_auto_commit` 仅记录，对齐 Java，§4.2 P1-4）；`has_commit_scheduler()` 可观测；`attach(&mut MiddlewareChain)` 提供显式 hook 装配（§4.2 P1-2）；**`stop()` 会 disable 已装 hook**（≈`removeSystemHook`，§4.2 P2-5），`attached_hook_count()`/`disable_attached_hooks()` 可观测/可手动调用 |

### 3.6 测试（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| strategy 单测 | *Test | 已完成 | 18 项（strategy.rs）：annotation（enabled/过期/labels+metadata）、context（mark/mark_with_labels/mark_with_metadata/with_annotation）、decision 工厂+Display、sampling（边界/非法值 panic/priority/name）、explicit（无标记/未启用/过期/优先级/名称/metadata+taskId 透传）、`is_expired` 辅助 |
| backend DTO + client 单测 | *Test | 已完成 | 17 项（backend.rs，含 7 个 `#[tokio::test]`）：DTO snake_case 往返（commit/feedback）、缺省字段不序列化、StatusResponse 反序列化+大小写不敏感成功、endpoint/timeout 校验、loopback mock HTTP（feedback/commit 请求体+非 2xx）、adapter 收集 msg_id / 无 context 警告路径 / 由 base_url 自建模型 |
| reward 单测 | RewardCalculatorTest | 已完成 | 3 项：闭包代理、Zero 常量、闭包读取 agent 字段 |
| runner 单测 | *Test | 已完成 | 44 项（runner.rs，含 10 个 `#[tokio::test]`；2026-10-28 新增 8 项：cloner 继承/覆盖、pool 接线、commit 调度两个、hook 装配与有/无绑定两种路径；**2026-10-31 新增 8 项**：注册表实例隔离/global 单例×2、router 默认隔离注册表、队列容量准入×2、hook 禁用直通、`stop()` 禁用已装 hook）：RunExecutionContext（收集/空 id panic/eq/display/set_messages）、RunRegistry（自增/清理/空 id panic/cleanup_old_tasks）、TaskExecutionRegistry（注册/排序/空查询/统计/ids/cleanup）、TaskIdGenerator、TrainingConfig（默认/校验/可选 setter/repeatTime/sampleRate）、TrainingRunner（builder 校验/start-stop 幂等/混用忽略）、router（recursion/sampling/explicit）、AgentCloner+ShadowAgentSpec、run_shadow 未选中短路 |
| **合计** | | | **82 passed / 0 failed**（2026-10-31 源码计数：`#[test]` 65 + `#[tokio::test]` 17 = 82；分模块 strategy 18 / backend 17 / reward 3 / runner 44；无 `#[ignore]`，无 doc-test。2026-10-28 基线 74，本轮 +8。2026-10-31 主智能体实测确认） |

## 4. 项目状态与下一步
### 4.1 当前状态
- **状态：已完成（2026-10-31 收口）**：截至 2026-10-28 除 P0-1（实测 Java 亦无非缺口）、P1-3（设计决定）、P2-1（live 待条件）、P2-2/P2-3 外已全部关闭；**2026-10-31 又关闭 P2-2（`shadow_pool_capacity` 接线）、P2-3（注册表去进程全局化）、P2-5（`TrainingHook` 卸载）与 见 history/TRAINING__MIGRATION_HISTORY.md 测试串行锁**，源码测试数 82（2026-10-31 实测）。21 个 Java 类全部有 Rust 对应（详见 §2），划分为 4 个模块：
  - `strategy.rs`：`TrainingAnnotation`(+Builder)/`TrainingContext`/`SelectionDecision`/`TrainingSelectionStrategy`(trait)/`SamplingRateStrategy`/`ExplicitMarkingStrategy`/`is_expired`
  - `reward.rs`：`RewardCalculator`(trait) + `FnRewardCalculator` + `ZeroRewardCalculator`
  - `backend.rs`：`dto::{CommitRequest,FeedbackRequest,StatusResponse}` + `TrinityClient`(+Builder) + `TrinityModelAdapter`(+Builder)
  - `runner.rs`：`task_id_generator`/`RunExecutionContext`/`RunRegistry`（`new`/`global`）/`TaskExecutionRegistry`（`new`/`global`）(+`RegistryStats`)/`TrainingConfig`(+Builder)/`AgentCloner`(+`ShadowAgentSpec`)/`TrainingRouter`(+`ShadowRunReport`)/`TrainingHook`/`TrainingRunner`(+Builder)
- **验证命令与结果**（2026-09-18 收尾重验；2026-10-22 静态复核确认测试数自洽）：
  - `cargo build -p agentscope-extensions-training` → 通过（Finished，exit 0，无 error）
  - `cargo test -p agentscope-extensions-training` → **64 passed; 0 failed**（2026-09-18 基线；经 见 history/TRAINING__MIGRATION_HISTORY.md 增至 66，2026-10-28 增至 **74**，见下行）
  - 首轮基线 37 passed；本轮补齐 27 个离线单测（无网络、无 Trinity 依赖）。
  - **2026-10-28 复验**：`cargo build -p agentscope-extensions-training` → exit 0（本 crate 无新警告）；`cargo test -p agentscope-extensions-training` → **74 passed / 0 failed**（lib 74，doc-test 0）；连跑 3 次一致。新增 8 项测试见 history/TRAINING__MIGRATION_HISTORY.md。
  - **2026-10-31 收口（已实测）**：源码测试函数 82（`#[test]` 65 + `#[tokio::test]` 17），本 crate 改动仅 `src/runner.rs`；`registry_test_lock` 已删除，测试之间不再共享任何进程全局可变状态。主智能体实测 `cargo test -p agentscope-extensions-training` = **82 passed / 0 failed**（`-j 2`）。
  - 静态复核（2026-10-22，未编译）：源码内 `#[test]` 57 + `#[tokio::test]` 7 = **64**，与声明一致；无 `#[ignore]`、无 doc-test 块，故「64 test 通过」与代码一致（未重跑 cargo）。
- **复用既有 trait（未重建平行类型）**：
  - `core::Model` trait：`TrinityModelAdapter` 实现它并委派内层 `Arc<dyn Model>`（组合模式，对齐 Java）。
  - `core::ReActAgent` / `ReActAgentBuilder`：`AgentCloner` 用 builder 重建 shadow；name/description/toolkit 经 `Agent` trait 回读。
  - `Msg`/`ChatResponse`/`GenerateOptions`/`ToolSchema`/`ExecutionConfig` 全部复用 core。
  - 与 harness 无新增耦合（未触及 `DistributedStore` 等；`Cargo.toml` 中 4 个零引用依赖已于 2026-10-28 删除，见 §4.2 P2-4）。
  - 已确认：core 无独立 `Hook` 类型；`middleware::Middleware` 的 `on_pre_call` 对齐 Java `PreCallEvent`，而 **`on_agent` 的洋葱包装（可观察 `AGENT_RESULT`）能承载 post-call** → 2026-10-28 已由本 crate 的 `TrainingHook` 落地（见 §4.2 P1-2 / 见 history/TRAINING__MIGRATION_HISTORY.md）。
- **本轮新增代码**：`TrainingContext::mark_with_metadata(...)`（补齐 Java `mark(Map)` 重载），以及 4 个业务文件测试模块扩写；无破坏性改动、未删除既有功能/测试。
- **依赖**：仅本 crate `Cargo.toml` 新增 `reqwest`、`uuid`（均走已有 workspace deps，对应 0.12 / 1，**未改根 Cargo.toml**）。未引 `rand`（用内部 xorshift PRNG）。
- **已知差异/设计决定**：
  1. Java `TrainingSelectionStrategy.shouldSelect(agent, inputs, output, ContextView)` 的 Reactor `ContextView` → Rust 改为显式 `Option<&TrainingAnnotation>`（无等价隐式调用域上下文）。
  2. Java `TrainingRouter` 是 `Hook`（`PreCallEvent`/`PostCallEvent`）；Rust 提供两种驱动方式：显式 API `decide(...)` + `run_shadow(...)`，以及 `TrainingHook`（`Middleware::on_agent` 包装）自动触发（2026-10-28 新增，见 §4.2 P1-2）；`-shadow` 递归保护、选择分支、repeatTime/runId/feedback 全流程保持一致。
  3. Java `AgentCloner` 用反射提取 `sysPrompt`/`maxIters` 等；Rust 无反射 → name/description/toolkit 从 `Agent` trait 读取，`sys_prompt`/`max_iters` **默认用 core getter 回读**，`ShadowAgentSpec` 仅作为显式覆盖（2026-10-28 修正，见 §4.2 P1-1）。
  4. Java `RewardCalculator.calculate(Agent)` → Rust `calculate(&ReActAgent)`（具体类型）。
  5. Java `TrinityModelAdapter` 强制 `stream(false)`（Trinity 不支持流式）；**已完成（2026-10-29 勘误）**：原记「Rust 侧 `OpenAiChatModel` 恒为流式且无关闭开关（见 §4.2 P0-2）」，实际 `TrinityModelAdapter::stream` 已强制 `options.stream = Some(false)` + 重复 id 去重，MODEL 侧已有非流式分支（§4.2 P0-2）。
  6. **（2026-10-31）注册表作用域**：Java `RunRegistry`/`TaskExecutionRegistry` 是 `static` 字段（全 JVM 共享）；Rust 改为实例句柄，`TrainingRouter::new` 默认一 router 一套，`with_global_registries` 复现 Java 语义。取舍理由见 §4.2 P2-3（可测试性 + 避免隐式进程状态）。
  7. **（2026-10-31）hook 卸载**：Java `removeSystemHook` 真移除；Rust 用 `TrainingHook::disable()`（引用仍留在不可变 chain 内，但 `on_agent` 直通），`TrainingRunner::stop()` 自动 disable 所有 `attach()` 过的 hook（§4.2 P2-5）。

### 4.2 剩余缺口（分级）与阻塞 / 跨模块依赖
> 以下分级为 **2026-10-22 静态复核**结果（未编译、未运行 cargo test）。P0-1 / P0-2 为本轮新发现，详见 history/TRAINING__MIGRATION_HISTORY.md 的静态复核记录。

**状态总览（2026-10-28 执行后）**

| 编号 | 主题 | 2026-10-28 状态 |
|---|---|---|
| P0-1 | 训练数据格式/轨迹采样（Java 亦无） | 非缺口（不变） |
| P0-2 | `TrinityModelAdapter` 流式 vs 非流式 | **已完成（2026-10-29 勘误）**：原记「部分修复（见 history/TRAINING__MIGRATION_HISTORY.md 不变，仍待 MODEL 侧非流式分支）」，实际 `backend.rs:305-308` 已强制 `options.stream = Some(false)` 并对重复 id 去重，MODEL `openai/mod.rs:271/320` 已读 `options.stream` 并落地非流式分支（一次调用 = 一个 `ChatResponse` = 一个 msg_id） |
| P1-1 | `AgentCloner` 丢 `sys_prompt`/`max_iters` | **已完成**（改回读，+2 测试） |
| P1-2 | 无法全局挂载 hook | **已收口**：不改 core，新增 `TrainingHook` + `TrainingRunner::attach`（+3 测试） |
| P1-3 | 3 个 ExecutionConfig 字段 | **设计决定（不迁移）**（core 无字段/setter） |
| P1-4 | `enable_auto_commit` 语义 | **已完成**（对齐 Java：只看 interval，+2 测试） |
| P2-1 | live 验证 | 待条件（不变） |
| P2-2 | `shadow_pool_size`/`capacity` | **已完成（2026-10-31）**：size → 信号量并发上限；capacity → 有界准入（`pending_shadow_runs()`，超限丢弃 + 告警） |
| P2-3 | 全局单例隔离 | **已完成（2026-10-31）**：注册表改实例句柄（`new()`/`global()`），`TrainingRouter::new` 默认隔离，`with_global_registries` 保留 Java 语义 |
| P2-4 | 4 个零引用依赖 | **已完成**（已删除） |
| P2-5 | `TrainingHook` 无卸载路径 | **已完成（2026-10-31）**：`disable()/enable()/is_enabled()` + `TrainingRunner::stop()` 自动 disable（+2 测试） |

**P0-1：Java → Rust 缺「训练数据生成 / 格式转换」与「轨迹采样」两域（实测确认 Java 侧亦无）**
- 复核以全仓 `*.java` 检索 `sharegpt` / `ShareGPT` / `conversations` / `toTrainingData` / `trainingSample` / `sft` / `rft`：**命中 0**（50 处 `conversations` 命中全部来自 node_modules `package-lock.json`，与 Java 源码无关）。
- 结论：Java `agentscope-extensions-training` **本身不含** ShareGPT / OpenAI-messages 训练数据格式转换器，也不含轨迹采样器。数据格式化（Experience → SFT/RFT）由**外部 Trinity-RFT**（Python，见 `docs/v1/*/docs/task/online-training.md`）承担，AgentScope 侧只负责回传 `msg_ids` + `reward`。
- 故 Rust **同样不需要**实现该两域：Java 无 → Rust 无，属**非缺口**。若后续产品要求 Rust 侧直接产出 ShareGPT/OpenAI-messages，则属**新增需求**（Java 也无对应实现可对齐），须单独立项。

**P0-2：`TrinityModelAdapter` 的 msg_id 采集与 Java 不 1:1（流式 vs 非流式）**
- Java `TrinityModelAdapter.Builder.build()` 显式 `.stream(false)`，注释「Trinity doesn't support streaming, force disable」→ 每次模型调用产出 **1 个 `ChatResponse`（1 个 msg_id）**。
- Rust `TrinityModelAdapter` 构造的 `OpenAiChatModel`：`build_request()` 内 `stream: Some(true)` **硬编码**，且 `options.stream` 在该 provider 内**从未被读取**（无 `with_stream`/stream 开关，`OpenAiChatModel::new` 也不接受 stream 参数）→ 恒为流式。
- 后果二连：
  1. **协议层**：Rust 向 Trinity 的 OpenAI 兼容端点发 `stream: true`，与 Java 显式规避的行为相反；Trinity 若不支持流式，shadow 调用直接失败 → 无 msg_id → 不发 feedback → **训练闭环断链**。
  2. **数据层**：`OpenAiChatModel` 的 `StreamIncrementer` 对每个 SSE chunk 产出 1 个 `ChatResponse`，且所有 chunk 复用**同一个 completion id**（`self.id` 仅在首个非空 chunk 赋值，后续 chunk 沿用）。`TrinityModelAdapter::stream()` 对**每个** response 都 `add_msg_id(resp.id)` → 同一 id 被重复写入 N 次（N = chunk 数），而 Java 恒为 1 次。**msg_ids 列表语义被污染**（重复项），直接影响 feedback 上报的轨迹条数。
- 裁定建议：需在 `agentscope-extensions-model` 的 `OpenAiChatModel` 暴露 stream 开关（或在 `TrinityModelAdapter` 内做 id 去重 + 强制非流式）。**当前不阻塞编译，但阻塞真实 Trinity 联调**，属本轮最高优先级缺口。
- **已闭环（2026-10-29 勘误）**：上述两条路径都已落地——①`TrinityModelAdapter::stream` 内 `options.stream = Some(false)`（`backend.rs:305-308`）并对连续重复 id 去重；②MODEL `openai/mod.rs:271` `let streaming = options.stream.unwrap_or(true)` + `:320` 非流式分支（整次 completion → 单个 `ChatResponse`）。真实 Trinity 联调仍未做（P2-1）。

**P1-1：`AgentCloner` 丢弃 `sys_prompt` / `max_iters`（core 已有 getter，本 crate 未回读）**
- 上一版 tracker 称「core 无 getter，故由 `ShadowAgentSpec` 显式传入」——**该前提不成立**：`agentscope-core/src/agent/react_agent.rs:283` 已有 `pub fn sys_prompt(&self) -> &str`，`:288` 已有 `pub fn max_iters(&self) -> i32`。
- 现状：`AgentCloner::clone_with_model` 只在 `ShadowAgentSpec.sys_prompt/max_iters` 为 `Some` 时才设置；`ShadowAgentSpec::default()` 两项皆 `None` → **shadow agent 的 system prompt 为空串、max_iters 回落到默认 10**。Java 则通过反射 100% 复制这两个字段。
- 影响：shadow agent 与生产 agent 行为不一致 → 采集到的训练轨迹不是「同一请求在训练模型上的真实表现」，reward 失真。`run_shadow` 要求调用方**记得**传 spec，缺省路径即静默降级。
- 建议：直接在 `clone_with_model` 内用 `original.sys_prompt()` / `original.max_iters()` 回读（无需 core 改动），`ShadowAgentSpec` 降级为可选覆盖。
- **已实施（2026-10-28）**：按此建议实现（`runner.rs` `AgentCloner::clone_with_model` 先取 `spec.*`，为 `None` 时回读原 agent），新增 `agent_cloner_inherits_prompt_and_max_iters_by_default` / `agent_cloner_spec_overrides_inherited_values` 两条用例。

**P1-2：`TrainingRunner.start()` 无法全局挂载 hook（需 core/harness 支持）**
- Java `start()` 通过 `AgentBase.addSystemHook(router)` 全局注册；Rust 无等价机制。当前 `TrainingRunner` 仅管理自身生命周期 + commit 调度，**不会自动挂到 agent**；调用方需自行把 `TrainingRouter` 接入 agent 调用路径。
- 复核补充：Rust core `middleware::Middleware` 现有 `on_pre_call`（语义对齐 Java `PreCallEvent`），但**无 post-call 钩子**，故 Java 的 `PostCallEvent`（选择 + shadow + feedback）确实无法用 core middleware 直接承载——原「core 无 PreCallEvent」表述不准确（见 history/TRAINING__MIGRATION_HISTORY.md 勘误）。
- 需跨模块裁定是否在 core 引入 post-call 扩展点，再由汇总方统一处理。
- **已收口（2026-10-28，未改 core）**：`Middleware::on_agent` 本身就是包住整次调用的洋葱拦截，包住 `AGENT_RESULT` 之后即可当 post-call 用，所以无需新扩展点。本 crate 新增 `TrainingHook`（`Middleware`，只参与 `ExtensionPoint::OnAgent`，`Weak<ReActAgent>` 绑定避免自持引用）与 `TrainingRunner::attach(&mut MiddlewareChain)` 作为**显式装配 API**；差异与用法见 history/TRAINING__MIGRATION_HISTORY.md。

**P1-3：`AgentCloner` 缺三个 ExecutionConfig 类字段的迁移（需 core 支持）**
- Java `cloneReActAgent` 反射提取 `modelExecutionConfig`/`toolExecutionConfig`/`toolExecutionContext` 并回注 builder（Java 侧有 `getModelExecutionConfig()`/`getToolExecutionConfig()`/`getToolExecutionContext()` 三个 getter，ReActAgent.java:5625/5630/5635）；Rust `ReActAgent` 的 `AgentCore` **根本没有**这三个字段（core 全仓检索 `model_execution_config` / `tool_execution_config` 无命中），`ReActAgentBuilder` 亦无对应 setter。
- 裁定建议（择一，由汇总方决定；**本 crate 未改 core**）：(a) core `ReActAgent` 增补这三个字段 + getter/setter，本 crate 再扩 `ShadowAgentSpec`；(b) 归档为「设计决定（不迁移）」。当前不阻塞。
- **2026-10-28 裁定：(b) 设计决定（不迁移）**。依据（本次只读核查）：core `AgentCore` 字段表（`react_agent.rs:77` 起，含 `sys_prompt`/`max_iters`/`middleware_chain` 等）无 `model_execution_config`/`tool_execution_config`/`tool_execution_context`；`ReActAgentBuilder` 字段表（`react_agent.rs:1220` 起）同样无对应字段与 setter，全仓 `rg 'model_execution_config|tool_execution_config|tool_execution_context'` 在 core 只命中自由函数 `tool_execution_context()`（构造每次工具调用的 `ToolExecutionContext`，与 Java 的 agent 级 getter 不同物）。Rust 的执行调控按调用（`GenerateOptions::execution_config`，`model/execution_config.rs`）与按工具（`ToolConfig::execution_config`，`tool/config.rs`）下发；把它们上提为 agent 级字段会与现有配置模型产生两套真相。

**P1-4：`TrainingConfig.enable_auto_commit` 语义与 Java 不一致**
- Java `TrainingRunner.startCommitScheduler()` **只**判 `commitIntervalSeconds <= 0`，`enableAutoCommit` 字段虽存在且可设，但 `start()` 路径**从未读取** → Java 该字段实为死配置（默认 `true`，设 `false` 无效）。
- Rust `start_commit_scheduler()` 判 `interval <= 0 || !enable_auto_commit` → Rust **会**因 `enable_auto_commit(false)` 关闭调度器。
- 结论：Rust 行为更符合字段命名直觉，但与 Java 不等价。需裁定统一口径（建议 Rust 保留并把 Java 侧差异记为 Java bug，或两者对齐为「仅看 interval」）。
- **2026-10-28 裁定并实施：对齐 Java（只看 interval）**。`start_commit_scheduler` 现在只判 `interval <= 0`；`enable_auto_commit` 保留字段/getter/builder（公开 API 不变）但在字段、getter、builder setter 三处文档注明「与 Java 一致：不参与调度器启停，仅作为配置记录」。新增 `commit_scheduler_ignores_enable_auto_commit_flag`（false + interval>0 仍起调度器）与 `commit_scheduler_skipped_for_non_positive_interval` 两条用例，并加 `TrainingRunner::has_commit_scheduler()` 可观测点。

**P2-1：live 验证待条件**
- `TrinityClient`/`TrinityModelAdapter` 真实 HTTP 往返需 Trinity 服务端点；本轮单测均离线（loopback mock + 假 Model），未做真连。P0-2 的流式问题在离线单测中不可见（假 Model 只产出 1 个带 id 的 response）。

**P2-2：`shadow_pool_size` / `shadow_pool_capacity` 存值未生效（2026-10-31 已全部接线；标题保留为历史记录）**
- Java 用 `Schedulers.newBoundedElastic(shadowPoolSize, shadowPoolCapacity, "training-shadow")` 调度 shadow 调用（异步、限流、队列有界）。
- Rust `TrainingConfig` 保留这两个字段并有 getter/builder，但**全 crate 无任何消费点**（检索仅命中定义与测试断言）→ shadow 并发度实际由调用方自行 `tokio::spawn` 决定，配置形同虚设。tracker §2.1 原表述「shadow 跑在 tokio 任务，池大小由 Tokio 运行时承载」掩盖了「配置值未被使用」的事实。
- **2026-10-28 部分接线**：`shadow_pool_size` → `TrainingRouter` 内 `Arc<tokio::sync::Semaphore>`（许可数 `shadow_pool_size.max(1)`，`run_shadow` 每轮 `acquire_owned()`），新增 `available_shadow_slots()` 观测点。`shadow_pool_capacity` **明确为预留字段**（tokio 信号量没有有界等待队列，无法 1:1 映射 Java `newBoundedElastic` 的 queue bound），既有 getter/builder 保留，文档已标注「not consumed at runtime」。
- 测试：`router_shadow_pool_size_is_wired`（size=3 → 3 slots；size=0 → 兜底 1 slot，不阻塞）。
- **2026-10-31 完成（capacity 接线）**：不再依赖「tokio 无有界队列」的妥协——`TrainingRouter` 增加 `shadow_in_flight: AtomicUsize` 与私有 `try_admit_shadow_run() -> Option<ShadowAdmission<'_>>`：准入计数 `fetch_add` 大于等于 `shadow_pool_capacity.max(1)` 即退回并**丢弃**该轮 shadow（`tracing::warn!` + `break`），`ShadowAdmission` 是 RAII 守卫在轮次结束 `fetch_sub` 释放。语义对齐 Java `newBoundedElastic(size, capacity, ...)` 的「队列满即拒绝」，差异仅为拒绝后是跳过而非抛 `RejectedExecutionException`（训练数据采集属尽力而为，不应打断生产调用）。观测点 `pending_shadow_runs()`。
- 新增测试：`router_shadow_queue_capacity_bounds_admission`（capacity=1 → 第二个准入为 `None`，释放后可再准入；capacity=0 退化为 1）、`run_shadow_checks_queue_capacity_before_running`（占满准入后 `run_shadow` 返回空 reports 且不发网络请求 → 证明 `run_shadow` 真的走了准入）。

**P2-3：全局单例隔离（2026-10-31 已完成）**
- 原状：`RunRegistry`/`TaskExecutionRegistry` 为进程内全局单例（`static OnceLock<Mutex<HashMap>>`，对齐 Java 静态字段）；测试靠 `registry_test_lock()` 串行化才不互相 `clear_all()`。
- **改造**：两者改为**实例句柄**——字段私有 `Mutex<HashMap<...>>`，`new()` 返回独立实例，`global()` 返回 `Arc<Self>` 进程级单例（`OnceLock<Arc<...>>`，保留 Java 全静态语义）。
  - `TrainingRouter::new(config, client)` → 每 router 一套独立注册表；
  - `TrainingRouter::with_global_registries(config, client)` → 共用 `global()`（Java 对齐）；
  - `TrainingRouter::with_registries(config, client, run_registry, task_registry)` → 依赖注入；
  - `TrainingRouter::run_registry()` / `task_registry()` 取回句柄。
- 影响：`run_shadow` 改用 `self.run_registry.allocate_run_id/cleanup`、`run_single_shadow` 改用 `self.task_registry.register`；**无进程全局可变状态参与测试**，`registry_test_lock` 及其 `clear_all()` 已删除（见 history/TRAINING__MIGRATION_HISTORY.md 与 见 history/TRAINING__MIGRATION_HISTORY.md）。
- 差异（有意）：Java 的 run 计数在所有 router 间共享；Rust 默认按 router 隔离（更符合「一个 runner 一次训练」的直觉），需要 Java 语义时显式选 `with_global_registries`。

**P2-5：`TrainingHook` 无卸载路径（2026-10-31 已完成）**
- 原状：`MiddlewareChain` 在 `build()` 后不可变，hook 装上后无法移除，`TrainingRunner::stop()` 只停生命周期与 commit 调度 → 对应 Java `AgentBase.removeSystemHook` 的缺失。
- **改造**：`TrainingHook` 增加 `enabled: AtomicBool` + `set_enabled/enable/disable/is_enabled`；`on_agent` 在禁用时**直接 `next(input)` 直通**（不克隆 reply、不跑 shadow）。`TrainingRunner` 以 `Mutex<Vec<Weak<TrainingHook>>>` 记录 `attach()` 装过的 hook（弱引用，不延长 agent/middleware 生命周期），`stop()` 调用 `disable_attached_hooks()`；另暴露 `attached_hook_count()` 与可手动调用的 `disable_attached_hooks()`。
- 测试：`training_hook_disable_makes_it_a_passthrough`（disable → 调一次 agent，`completed_calls` 仍为 0；enable 后变 1）、`runner_stop_disables_attached_hooks`（attach → start → 调 agent（1）→ stop → 再调 agent，仍为 1，且 agent 本身照常返回）。

**P2-4：无依赖清理**
- `Cargo.toml` 声明 `agentscope-harness`、`chrono`、`async-trait`、`thiserror` 四个依赖，但 `src/**/*.rs` 引用数均为 **0** → 可移除（减少编译面）。`agentscope-harness` 尤其值得清理，避免与「未触及 harness」的表述产生歧义。
- **2026-10-28 已清理**：先逐项 `Select-String` 复核（`agentscope_harness`/`chrono`/`thiserror`/`async_trait` 命中均为 0）再删除。剩余 `[dependencies]`：`agentscope-core`、`agentscope-extensions-model`、`serde`、`serde_json`、`tokio`、`futures`、`tracing`、`reqwest`、`uuid`；build/test 均通过。

**真实剩余（2026-10-31 收口复核；P0-2 / P1-1~P1-4 / P2-2 / P2-3 / P2-4 / P2-5 均已关闭）**
- **live Trinity 联调未做**（P2-1，唯一未关闭项，且**非代码缺口**）：`TrinityClient`/`TrinityModelAdapter` 仍只经 loopback mock + 假 Model 离线验证；本环境无 Trinity 服务端点，且无 `#[ignore]`/env gated 用例（按硬约束不得用忽略凑绿）。
- 可选后续（非缺口，未立项）：真正的「有界阻塞队列」（当前超容量即丢弃而非排队等待）；若要 Java `RejectedExecutionException` 等价的硬失败，需宿主层把丢弃改为错误。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-training` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-training` / `cargo test -p agentscope-extensions-training` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-training/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. 执行历史（已归档）

- 执行记录 / 静态复核 / 勘误 / 反向验证明细：[`history/TRAINING__MIGRATION_HISTORY.md`](./history/TRAINING__MIGRATION_HISTORY.md)
- 本文件只保留现行状态（§0–§5）；历史条目一律写进上述归档文件，不再回填本文。
