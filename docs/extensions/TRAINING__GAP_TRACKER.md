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
| 当前状态 | **已完成**（P1/P2 清零；2026-10-31 实测 82 passed / 0 failed）—— 代码面 P0/P1/P2 全清（注册表去进程全局化、`shadow_pool_capacity` 接线、`TrainingHook` 卸载、测试串行锁删除）；余 live 联调 P2-1 需环境（非代码缺口）。历史见 history/TRAINING__MIGRATION_HISTORY.md（**注**：2026-10-31 全量对齐复核新增 1×P0 + 2×P1，见 §3.5，本行「全清」仅指 2026-10-31 之前的清单） |
| 2026-10-31 全量对齐复核 | 按 BRIEF 逐文件复核 Java 20 个类 → 新增 §3.5（`TRAIN-01`..`TRAIN-25`）与 §4.3（`TRAIN-D01`..`D06` / `TRAIN-X01`..`X11`）；结论：**新增 1 项 P0（hook 路径丢失训练标记）、2 项 P1（hook 内联阻塞生产调用、bind 遗漏静默直通）+ 6 项 P2 + 2 项待裁定**。§1–§4.2 历史内容未改 |

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

## 3.5 2026-10-31 全量对齐复核清单（唯一编号）

> 复核基线：Java `main` @ `1524b3191`（本机 `D:\IdeaProjects\agentscope-java`），模块主代码 **20 个类 / 约 2.9k 行**（backend 5 / reward 1 / runner 8 / strategy 6，含 `TrainingSelectionStrategy`）；Rust 工作树 HEAD 见仓库，`crates/agentscope-extensions-training` **5 个源文件 / 约 3.6k 行 / 82 个测试**（既有实测，本轮未跑 cargo）。存量 §3.1–§3.5 历史清单保持原样，未重新编号。
> 编号规则：`TRAIN-NN`（缺口/待裁定，状态 `待办` / `进行中` / `已完成` / `设计决定（不迁移）` / `待裁定`）；差异另有 `TRAIN-D<NN>` / `TRAIN-X<NN>` 系列（见 §4.3）。编号本模块内唯一、单调递增，不复用历史编号。
> 阅读提示：上文 `### 3.5 runner（P0）` 是 2026-09 的**三级**历史小节，与本节的**二级** `## 3.5` 不是同一章节，历史内容原样保留。

### 3.5.1 缺口与待裁定

| 编号 | 优先级 | 缺口项（可执行描述） | Java 参考（文件#符号） | Rust 现状（文件:行 / rg 零命中） | 执行状态 | 下一动作 |
|---|---|---|---|---|---|---|
| TRAIN-01 | P0 | 自动驱动路径（`TrainingHook`）恒把 `annotation` 传 `None` → `ExplicitMarkingStrategy` 在该路径 100% `reject("no-explicit-marking")`，使用显式标记模式时训练数据**静默为 0**（无告警） | `strategy/TrainingContext.java#mark`/`#getCurrent`（`REACTOR_KEY="agentscope.training.marker"`，`:65`/`:81`/`:156`）+ `runner/TrainingRouter.java#handlePostCall`（`Mono.deferContextual` 把 ctx 传给 `shouldSelect(..., ctx)`） | `runner.rs:1166` `router.run_shadow(&agent_name, &agent, inputs, output, None, spec)`（第 5 参硬编码 `None`）；`strategy.rs:436`/`:440` 均返回 `reject("no-explicit-marking")`；`rg -n 'annotation' runner.rs` 仅 856/864/878/882（`decide`/`run_shadow` 形参）与 1668/1672（测试） | 待办 | 按 TRAIN-11 裁定的方案，把本次调用的标记透传到 `run_shadow` 第 5 参；core 已有 seam：`RuntimeContext.extensions`（`core/src/agent/runtime_context.rs:33-52` `insert/insert_arc/get`，`:251-275` `set_extension/with_extension`）+ 每调用注入入口 `ReActAgent::call_with_format_and_extensions`（`core/src/agent/react_agent.rs:605`）。验收：新增「hook + `ExplicitMarkingStrategy`」用例——有标记时 `completed_calls()==1` 且产生 shadow 轮次，无标记时为 0 |
| TRAIN-02 | P1 | hook 内联 `await` 全部 shadow 轮次：生产调用延迟 = `repeat_time` × shadow 全链路（克隆 + 训练模型调用 + feedback），与 Java「异步、不阻塞生产」相反 | `runner/TrainingRouter.java#handlePostCall`（`runShadowAgent(...).subscribeOn(asyncScheduler).subscribe(...)` 立即返回；`newBoundedElastic(shadowPoolSize, shadowPoolCapacity, ...)`） | `runner.rs:1132-1171`：`on_agent` 把 `router.run_shadow(...).await` 放进 `futures::stream::once(...)`（`:1152-1170`）再 `inner.chain(tail)`（`:1171`）；`run_shadow` 内 `repeat_time` 逐轮 `await`（`runner.rs:894-933`） | 待办 | hook 内改 `tokio::spawn`；`ShadowAdmission`（`:727-734`，借用 `&TrainingRouter`）改为 owned/`Arc` 计数以跨 spawn；`completed_calls` 在后台任务结束时自增。验收：假 transport 加延迟的用例断言生产 `agent.call()` 在 shadow 结束前返回，且期间 `pending_shadow_runs() > 0` |
| TRAIN-03 | P1 | `attach()` 后忘记 `bind_agent()` 时**静默直通**（无日志/无计数差异），Java 无此绑定步骤 → 部署遗漏即零训练数据且无提示 | `runner/TrainingRunner.java#start`（`AgentBase.addSystemHook(router)`，无需绑定 agent） | `runner.rs:1219` `attach` 仅 `TrainingHook::new` + `install`；`:1157-1159` `agent` 为 `None` 时 `return`；`:2195-2210` 测试 `training_hook_without_bound_agent_is_a_passthrough` 把静默直通**固化为期望行为** | 待办 | 加 `TrainingHook::has_agent()` 与 `TrainingRunner::unbound_hook_count()`；`start()` 对「已 attach 但未 bind」的 hook 打 `tracing::warn!`；验收：新增用例断言 `unbound_hook_count()==1`（或 `start()` 返回 `Err`，二者择一） |
| TRAIN-04 | P2 | shadow 与生产 agent 共享**同一** `Toolkit`（`Clone` 是 Arc 浅拷贝），叠加 `.todo_tool(false)` 造成工具面不一致：shadow 仍能用生产的 `todo_write`（绑定生产 `TaskContextState`），却没有生产的 `TaskReminderMiddleware` | `runner/AgentCloner.java#cloneReActAgent`（原样 `toolkit(toolkit)`；`ReActAgent.java:5839 taskListEnabled=false`，默认不注册 `TodoTools`，故 Java 两侧一致） | `runner.rs:690-693` `.toolkit(original.toolkit().clone()).todo_tool(false)`；`core/src/tool/toolkit.rs:17-22` `tools: Arc<Mutex<HashMap<..>>>`（Clone 共享内部表）；`core/src/agent/react_agent.rs:1465-1467`（`enable_todo_tool` 同时注册 `TodoWriteTool` 与 `TaskReminderMiddleware`） | 待办 | 给 shadow 一份**独立** `Toolkit`（新 `Toolkit` + 逐名 `get_tool` 重新注册），再按 `original.toolkit().get_tool("todo_write").is_some()` 决定 `todo_tool(true)` 重绑到 shadow 自己的 `TaskContextState`。**禁止**在共享 toolkit 上直接 `todo_tool(true)`（`register_tool` 按名覆盖，`toolkit.rs:119-122`，会写穿生产绑定）。验收：断言 shadow `tool_names()` 与生产一致、且 shadow 调 `todo_write` 不改生产 state |
| TRAIN-05 | P2 | `run_shadow` 只在最后一轮清理 run 计数；中途因容量准入 `break` 时 `cleanup(task_id)` 不执行 → `RunRegistry` 计数器残留（长跑进程泄漏） | `runner/TrainingRouter.java#handlePostCall`（每轮 `doFinally` 内 `if (isLastRun) RunRegistry.cleanup(taskId)`） | `runner.rs:898-905`（准入失败即 `break`）与 `:928-930`（`if i == repeat - 1` 才 cleanup）互斥 | 待办 | 把 `cleanup(&task_id)` 移到循环之后无条件执行（或 `Drop` 守卫）。验收：capacity=1、repeat=3，占住一次准入后调用 `run_shadow`，断言 `run_registry().get_current_run_count(&task)==0` |
| TRAIN-06 | P2 | `TrinityModelAdapter` 只转发 3 个能力位，`supports_tool_choice_specific` / `max_output_tokens` / `descriptor` 落到 trait 默认（`false` / `0` / `None`）→ 以 adapter 为 model 的 shadow 在 A5 结构化输出/工具强选路径上退化 | `backend/TrinityModelAdapter.java#doStream`（Java 仅转发 `getModelName`，其余走 `ChatModelBase` 默认；Rust 已多转 3 项） | `backend.rs:333-345`（`supports_native_structured_output*`、`context_window_size`）；缺口三项见 `core/src/model/model.rs:22-55` | 待办 | 补 3 个转发 getter（`max_output_tokens` 对 shadow 的上下文预算尤其有用）。验收：假 delegate 各 getter 返回特异值，断言 adapter 一致 |
| TRAIN-07 | P2 | 构建期校验用 `assert!`/panic，与同 builder 的 `build() -> Result` 风格不一致（库内 panic 会 abort 宿主线程） | `runner/TrainingConfig.java#Builder.sampleRate`/`#repeatTime`、`runner/RunExecutionContext.java#RunExecutionContext`、`runner/RunRegistry.java#allocateRunId`（均 `IllegalArgumentException`） | `strategy.rs:428-431`（`SamplingRateStrategy::of`）、`runner.rs:604-608`（`repeat_time`）、`runner.rs:78-79`（`RunExecutionContext::create`）、`runner.rs:226`（`allocate_run_id`） | 待办 | 至少把 `TrainingConfigBuilder::sample_rate` 改为 `Result`（其余在文档标注 panic 契约）。验收：`sample_rate(1.5)` 返回 `Err` 而非 abort |
| TRAIN-08 | P2 | `TrainingHook` 未覆盖 `Middleware::order()`（默认 `1`，即最外层**最早**执行）；Java `TrainingRouter.priority()==500` 语义是「低优先级、在业务逻辑之后」 | `runner/TrainingRouter.java#priority`（`return 500`，注释 `Low priority, runs after business logic`） | `runner.rs:1127-1131` 只实现 `active_points`；默认 `order()` 见 `core/src/middleware/middleware.rs:15-20` | 待办 | `impl Middleware for TrainingHook { fn order(&self) -> i32 { -500 } }`（本项目 `order` 越大越外层）。验收：与另一 middleware 同链时断言 hook 位于内层 |
| TRAIN-09 | P2 | 测试密度缺口：①`router_shadow_pool_size_is_wired` 用 0% 采样，permit 申请/释放路径从未真正执行；②hook 路径无 `ExplicitMarkingStrategy` 用例（TRAIN-01 因此未被任何测试捕获） | Java 侧 19 个测试类 / 166 个 `@Test`（§1 基线） | `runner.rs:2100-2121`（`sample_rate=0.0` → `run_shadow` 在 `:882-885` 即返回，`:911` 的 `acquire_owned()` 不可达）；`runner.rs:2169-2193`/`:2241-2281` hook 用例均为 0% 采样 | 待办 | 用「loopback mock + 100% 采样」覆盖 permit 获取/释放与 feedback 上报；并补 TRAIN-01 的 hook + 显式标记用例（≥2 条，通过后基线从 82 上移） |
| TRAIN-10 | 待裁定 | 是否把「hook 装配 + agent 绑定」上提为 **harness** 级能力（`HarnessAgent` 预留 attach 点），避免每个宿主手写 `MiddlewareChain` + `bind_agent` 且易漏 bind（TRAIN-03） | `runner/TrainingRunner.java#start`（JVM 级全局注册，宿主零配置） | `runner.rs:1017-1045`（模块文档给出四步手工装配示例）、`runner.rs:1219` | 待裁定 | 跨模块：需 harness 提供 attach/bind 便利 API 或 builder 钩子（本 crate 不可单独完成）。裁定后：若上提则本 crate 只保留 `TrainingHook`，否则按 TRAIN-03 加告警 |
| TRAIN-11 | 待裁定 | TRAIN-01 的透传方案二选一：(a) 走 core `RuntimeContext.extensions`（per-call 语义正确，但宿主须改用 `call_with_format_and_extensions` 注入标记）；(b) 本 crate 提供 `TrainingHook::annotation_provider(Arc<dyn Fn() -> Option<TrainingAnnotation>>)`（零宿主改动，但只有「全局/线程」语义，无 per-call 精确性） | `strategy/TrainingContext.java#mark*`（Reactor `contextWrite`，天然 per-subscription） | 现状两者都无：`runner.rs:1166` 硬编码 `None`；`TrainingHook` 字段表 `runner.rs:1053-1064` 无 annotation/闭包槽 | 待裁定 | 建议 (a) 为主、(b) 为兼容兜底（并存不冲突）。裁定后落到 TRAIN-01 的下一动作 |

### 3.5.2 本轮复核确认已对齐（抽样，不求穷举）

| 编号 | 能力 | Java 参考 | Rust 证据（文件:行 + 测试名） | 执行状态 |
|---|---|---|---|---|
| TRAIN-12 | `SelectionDecision`（accept/reject/accept(labels)/accept(labels+metadata)、不可变语义、`toString`） | `strategy/SelectionDecision.java#accept`/`#reject` | `strategy.rs:212-297`（`accept`/`accept_with_labels`/`accept_full`/`reject`）、`strategy.rs:483-493` `Display` 输出同形；测试 `selection_decision_factories_and_display` | 已完成 |
| TRAIN-13 | `SamplingRateStrategy`（`[0,1]` 校验、`priority=200`、`name="SamplingRate(x)"`、`getSampleRate`） | `strategy/SamplingRateStrategy.java#shouldSelect`/`#priority`/`#name` | `strategy.rs:419-491`（内部 xorshift `next_unit`，无 `rand` 依赖）；测试 `sampling_bounds_accept_or_reject_deterministically` / `sampling_priority_and_name` / `sampling_rejects_out_of_range` | 已完成 |
| TRAIN-14 | `ExplicitMarkingStrategy`（默认 TTL 60s、过期→`explicit-marking-expired`、`taskId` 注入 metadata、`priority=10`、`name="ExplicitMarking"`） | `strategy/ExplicitMarkingStrategy.java#shouldSelect`/`#create` | `strategy.rs:398-460`（`:405` `create()` 60_000ms、`:444` 过期、`:449` `taskId`、`:455` priority 10、`:459` name）；测试 `explicit_rejects_without_annotation` / `explicit_accepts_enabled_annotation` / `explicit_passes_through_task_id` / `explicit_rejects_expired` / `explicit_priority_and_name` / `explicit_merges_annotation_metadata` | 已完成 |
| TRAIN-15 | `TrainingAnnotation`（builder、`isExpired` 用 `>` 边界、`enabled/withLabels/withLabelsAndMetadata` 工厂、equals） | `strategy/TrainingAnnotation.java#Builder`/`#isExpired` | `strategy.rs:22-149`（`:95` `is_expired` 用 `now - ts > ttl`，与 Java 同边界）；测试 `annotation_enabled_and_expiry` | 已完成 |
| TRAIN-16 | `RewardCalculator` 接口 + lambda 适配 | `reward/RewardCalculator.java#calculate` | `reward.rs:8-10`（trait）、`:22-45` `FnRewardCalculator`、`:48-54` `ZeroRewardCalculator`；测试 3 项（含读 agent 字段） | 已完成 |
| TRAIN-17 | DTO：`task_id`/`run_id`/`time_threshold`/`msg_ids`/`reward` snake_case、`StatusResponse.isSuccess()` 大小写不敏感 | `backend/dto/*.java`（`@JsonProperty`、`"success".equalsIgnoreCase`） | `backend.rs:23-166`（`dto` 模块，`:158` `eq_ignore_ascii_case("success")`）；测试 `commit_request_snake_case_roundtrip`/`feedback_request_snake_case_roundtrip`/`status_response_success` | 已完成 |
| TRAIN-18 | `TrinityClient.feedback`/`commit`：POST `{endpoint}/feedback`、`{endpoint}/commit`、endpoint 必填校验、默认 300s 超时、非 2xx 报错、成功体解析 `StatusResponse` | `backend/TrinityClient.java#feedback`/`#commit`/`#Builder.build` | `backend.rs:189-227`（`:202` `post_status` 统一处理超时/非 2xx/JSON 解码）、`:254-262` endpoint 非空校验 + 300s 默认；测试 `client_requires_endpoint`/`client_feedback_posts_snake_case_body`/`client_commit_posts_snake_case_body`（`spawn_mock_server`，backend 17 项） | 已完成 |
| TRAIN-19 | `TrinityModelAdapter`：组合内层 model、强制非流式（Java `.stream(false)`）、收集 `ChatResponse.id` 到 `RunExecutionContext`、无 ctx 时告警 | `backend/TrinityModelAdapter.java#doStream`/`#Builder.build` | `backend.rs:295-330`（`:308` `options.stream = Some(false)`，`:313-321` 连续重复 id 去重后 `add_msg_id`）、`:364-377` builder 缺省（`dummy`/`training-model`）、`:406-440` 由 `base_url` 自建 `OpenAiChatModel`；测试 7 项（`adapter_collects_msg_ids`/`adapter_forces_stream_off`/`adapter_records_duplicate_chunk_ids_once`/`adapter_requires_delegate_or_base_url`/`adapter_builds_model_from_base_url`/`adapter_without_execution_context_is_ok` 等） | 已完成 |
| TRAIN-20 | `RunExecutionContext`（taskId/runId 空校验、msgIds/msgs 并发安全收集、`duration`、equals 按 (task,run)、`toString` 同形） | `runner/RunExecutionContext.java#create`/`#addMsgId`/`#setMessages` | `runner.rs:64-186`（`Mutex<Vec<_>>` 替代 `CopyOnWriteArrayList`，`:78-79` 空 id 断言、`:161-167` equals 仅 (task,run)）；测试 5 项（`run_execution_context_collects`/`run_execution_context_set_messages`/`run_execution_context_equality_and_display`/空 task/run id 两条） | 已完成 |
| TRAIN-21 | `RunRegistry`（自增 runId 从 "0"）与 `TaskExecutionRegistry`（注册/按 runId 数值排序查询/统计/cleanup/`RegistryStats` 含 avg） | `runner/RunRegistry.java#allocateRunId`、`runner/TaskExecutionRegistry.java#register`/`#getRunsByTask`/`#getStats` | `runner.rs:203-270`、`:277-410`（`:301-309` 排序、`:368` `get_stats`、`:397-401` avg）；测试 `run_registry_allocates_incrementing_ids`/`task_execution_registry_flow`/`task_execution_registry_lists_task_ids`/`registry_stats_zero_and_display`/`run_registry_global_is_a_shared_singleton` 等（共 11 项） | 已完成 |
| TRAIN-22 | `TrainingConfig` 默认值（apiKey `dummy`、model `training-model`、commit 300s、http 300s、autoCommit true、pool 10/1000、repeatTime 1）、`build` 校验 endpoint+reward、缺省策略 10% 采样、`getSampleRate` 非采样返 -1 | `runner/TrainingConfig.java#Builder`/`#build`/`#getSampleRate` | `runner.rs:424-643`（默认值 `:532-548`、校验 `:611-621`、默认策略 `:622-624`、`get_sample_rate` `:507-514` 走 `as_any` downcast 返 -1.0）；测试 5 项（`config_builder_validates_and_defaults`/`config_optional_setters_are_applied`/`config_endpoint_and_reward_are_required`/`config_repeat_time_rejects_zero`/`config_sample_rate_negative_for_non_sampling`） | 已完成 |
| TRAIN-23 | `AgentCloner`：`-shadow` 后缀名、复制 description/toolkit、`sysPrompt`/`maxIters` 默认继承、model 替换、全新空 state | `runner/AgentCloner.java#cloneReActAgent` | `runner.rs:675-699`（`:684-685` 回读 getter、`:687-693` builder 重建）；测试 `agent_cloner_inherits_prompt_and_max_iters_by_default`/`agent_cloner_spec_overrides_inherited_values`/`agent_cloner_rebuilds_shadow_with_spec` | 已完成 |
| TRAIN-24 | `TrainingRunner` 生命周期：start/stop 幂等、stop 时最后一次 commit、周期 commit **只看 interval**（`enableAutoCommit` 与 Java 同为记录值）、builder 混用 config 冲突 | `runner/TrainingRunner.java#start`/`#stop`/`#startCommitScheduler`/`#Builder` | `runner.rs:1256-1326`（`:1301-1304` 仅判 `interval <= 0`）、`:1340-1349` 混用报错；测试 `training_runner_stop_is_idempotent`/`commit_scheduler_skipped_for_non_positive_interval`/`commit_scheduler_ignores_enable_auto_commit_flag`/`training_runner_keeps_config_when_setters_mixed` | 已完成 |
| TRAIN-25 | 「状态轮询 / 批量上传 / 传输层重试」三项：**Java 侧同样不存在**（非缺口） | `rg -i 'retry\|backoff\|poll\(\|batch'` 于 Java 模块主代码：4 处字面量命中，均为注释措辞或 `StatusResponse#getStatus` 取值方法（`RunRegistry.java:31`、`TrainingRouter.java:54`、`RunExecutionContext.java:219`、`StatusResponse.java:32`），无重试/退避/轮询实现 | Rust 侧 `rg -i 'retry\|backoff\|poll\|batch' crates/agentscope-extensions-training/src` → **0 命中**；feedback 每次 run 提交一条（`msg_ids` 列表即批次载体，`runner.rs:990-999`） | 设计决定（不迁移） |

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

### 4.3 与 Java 的已知差异（有意为之，非缺口）

> 本节由 2026-10-31 全量对齐复核重建：§4.2 中散落的「已知差异/设计决定」条目已归并到下面两张子表（§4.2 的缺口描述与其历史裁定**未改动**），编号与 §3.5 的 `TRAIN-NN` 不共用。

#### 4.3.1 设计决定（不迁移）

| 编号 | 项 | Java 参考 | 不迁移理由（源码级依据） | 影响面 |
|---|---|---|---|---|
| TRAIN-D01 | `AgentCloner` 复制 `modelExecutionConfig` / `toolExecutionConfig` / `toolExecutionContext` 三个 agent 级字段 | `runner/AgentCloner.java#cloneReActAgent`（`extractField(original, "modelExecutionConfig"/"toolExecutionConfig"/"toolExecutionContext")`）+ `ReActAgent.java#getModelExecutionConfig`/`#getToolExecutionConfig`/`#getToolExecutionContext` 三个 getter | Rust core **无这三个字段与 setter**：`AgentCore` 字段表（`core/src/agent/react_agent.rs:77` 起）与 `ReActAgentBuilder` 字段表（`:1250` 起）均无；`rg 'model_execution_config\|tool_execution_config\|tool_execution_context' crates/agentscope-core/src` 仅命中自由函数 `tool_execution_context()`（构造**每次工具调用**的上下文，与 Java 的 agent 级 getter 不同物）。Rust 把执行调控下放到按调用（`GenerateOptions::execution_config`）与按工具（`ToolConfig::execution_config`），上提为 agent 级会产生两套真相。 | shadow agent 每次调用用默认执行配置；若宿主需要与生产一致，需在 `ShadowAgentSpec` 之外按调用设置 `GenerateOptions`（当前未暴露）。**不阻塞** |
| TRAIN-D02 | 训练数据格式转换（ShareGPT / OpenAI-messages / `toTrainingData`）与轨迹采样器 | 无（Java 侧也不存在） | 全仓 `rg -i 'sharegpt\|totrainingdata\|trainingsample' --glob '*.java'` → **0 命中**；另核 `conversations`（34 个 Java 文件命中，均为记忆/压缩类注释与示例文案，无训练数据格式转换逻辑）。Java 只回传 `msg_ids` + `reward`，数据格式化由外部 Trinity-RFT（Python）承担。 | Rust 同样不需要；若产品要求 Rust 侧直接产出 ShareGPT，属**新增需求**（Java 无基线可对齐），须单独立项 |
| TRAIN-D03 | JVM 级全局 Hook 注册（`AgentBase.addSystemHook(router)` / `removeSystemHook`） | `runner/TrainingRunner.java#start`/`#stop`、`runner/TrainingRouter.java`（`implements Hook`，`onEvent` 收 `PreCallEvent`/`PostCallEvent`） | Rust core 无进程级 hook 注册表，`MiddlewareChain` 在 `build()` 后不可变（`core/src/middleware/middleware.rs:180-226` 只有 `new`/`add`/`add_arc`/`len`/`is_empty`/`participants`，**无移除/重置 API**）。改为**显式装配**：`TrainingRunner::attach(&mut MiddlewareChain)` + `TrainingHook::bind_agent`（`runner.rs:1219`/`:1112`），卸载用 `disable()` 直通（`runner.rs:1088`）。 | 宿主需为每个 agent 装配一次；漏装配/漏 bind 的后果见 §3.5 TRAIN-03 |
| TRAIN-D04 | `AgentCloner` 用反射读私有字段（`Field.setAccessible(true)`） | `runner/AgentCloner.java#extractField` | Rust 无反射；改用 `Agent` trait + `ReActAgent` 公开 getter 回读（`original.sys_prompt()` / `original.max_iters()` / `name()` / `description()` / `toolkit()`，`runner.rs:684-693`）。语义等价且编译期安全。 | 若未来 core 新增不可回读字段，需 core 补 getter（不属本 crate） |
| TRAIN-D05 | 训练标记经 Reactor `Context` 隐式传播（`TrainingContext.mark*` 返回 `Function<Context,Context>`、`getCurrent(ContextView)`、key `agentscope.training.marker`） | `strategy/TrainingContext.java:65/81/102/130/146/156` | Rust 无 Reactor 隐式调用域上下文；改为显式 `Option<&TrainingAnnotation>` 形参（`strategy.rs:305-314`）与 `TrainingContext` 轻量载体（`strategy.rs:158-207`，含 `mark`/`mark_with_labels`/`mark_with_metadata`/`with_annotation`）。 | 显式 API（`decide`/`run_shadow`）已可用；**自动路径（hook）尚未接线**，见 TRAIN-01/TRAIN-11 |
| TRAIN-D06 | `RunRegistry.cleanupOldTasks(Duration)` 的按时间自动清理 | `runner/RunRegistry.java#cleanupOldTasks`（方法体仅 TODO 注释，**Java 亦为空实现**） | 保持与 Java 相同：Rust `runner.rs:263-264` `pub fn cleanup_old_tasks(&self, _ttl: Duration) {}`，注释「Reserved for future time-based cleanup」。 | 无（两侧都需手工 `cleanup(task_id)`） |

#### 4.3.2 设计决定与已知差异

| 编号 | 项 | Rust 实现 | 与 Java 的差异 | 为何保留（收益/依据） |
|---|---|---|---|---|
| TRAIN-X01 | run/task 注册表作用域 | `RunRegistry::new()`/`global()`、`TaskExecutionRegistry::new()`/`global()`，`TrainingRouter::new` 默认一 router 一套，`with_global_registries` 复现 Java（`runner.rs:203-270`/`:277-410`/`:748-807`） | Java 两个注册表是 `static` 字段，全 JVM 共享 | 消除进程级可变状态 → 测试可并行、多 runner 不互相污染；需要 Java 语义时显式选用 `global()`/`with_global_registries`。**测试已并行安全**：`global()` 用例改用唯一 task id（`runner.rs:1494`/`:1754`）而非 `clear_all()` |
| TRAIN-X02 | hook 卸载语义 | `TrainingHook::disable()` → `on_agent` 直接 `next(input)` 直通；`TrainingRunner::stop()` 自动 disable（`runner.rs:1088`/`:1232-1242`/`:1279-1281`） | Java `removeSystemHook(router)` 真把 hook 从列表移除 | `MiddlewareChain` build 后不可变（core 无移除 API）；直通开销仅一次原子读。测试 `training_hook_disable_makes_it_a_passthrough`、`runner_stop_disables_attached_hooks` |
| TRAIN-X03 | 池/队列为 0 的容错 | `shadow_pool_size.max(1)`（`runner.rs:797`）、`shadow_pool_capacity().max(1)`（`runner.rs:837`） | Reactor `newBoundedElastic(size, capacity, ..)` 要求两者 > 0，0 会 `IllegalArgumentException` | 配置笔误不应让 router 死锁或 panic（训练采集属尽力而为）；已被 `router_shadow_pool_size_is_wired`（size=0 → 1 slot）、`router_shadow_queue_capacity_bounds_admission`（capacity=0 → 1）锁定 |
| TRAIN-X04 | 队列满的处理 | 丢弃该轮 + `tracing::warn!` + `break`（`runner.rs:836-844`/`:898-905`），不报错给生产调用 | Java 提交被 bounded elastic 拒绝时抛 `RejectedExecutionException`，由 `subscribe(..., error -> logger.error(...))` 接住——**同样不影响生产调用**，差异仅在「丢弃 vs 错误回调」 | 语义等价、少一层异常路径；代价是丢弃计数不可观测（建议见 TRAIN-05/09 的观测点补强） |
| TRAIN-X05 | 错误映射 | `Error::Http("Trinity request failed: …")`（传输层）/ `Error::http_status(code, "Trinity API failed: …")`（非 2xx）/ `Error::Http("… decode failed")`（`backend.rs:202-227`）；`commit()` 返回 `Err` 并记录（`runner.rs:1282-1296`） | Java 统一抛 `RuntimeException("Feedback API failed: <code>")`；`commit()` 的 `doOnError(...).then()` 会**吞掉错误**，Mono 仍成功 | 结构化错误便于宿主分类重试；`commit()` 失败可见（Java 会静默成功），`stop()` 内以 `let _ =` 忽略以保持停机语义 |
| TRAIN-X06 | 参数校验失败形态 | `assert!`/panic（`strategy.rs:428-431`、`runner.rs:78-79`/`:226`/`:604-608`） | Java 抛 `IllegalArgumentException`（unchecked 异常，同样会让调用栈中断） | 语义上同为「调用方错误、不可恢复」；panic 不返回 `Result` 省去大量样板。**风险**：库内 panic 在 `panic=abort` 的嵌入式 profile 下会终止进程，故 TRAIN-07 建议至少对 `sample_rate` 改 `Result` |
| TRAIN-X07 | 按 runId 排序的健壮性 | `sort_by_key(|c| c.get_run_id().parse::<i64>().unwrap_or(0))`（`runner.rs:308`） | Java `Comparator.comparing(ctx -> Integer.parseInt(ctx.getRunId()))`，非数字 runId 抛 `NumberFormatException` | runId 由本 crate 生成（恒为数字），异常输入不应让查询 panic；回退 0 只为排序稳定 |
| TRAIN-X08 | 生产调用的输入消息采集 | `TrainingHook::on_agent` 在闭包内直接捕获 `input.messages`（`runner.rs:1141`）并在事件流末尾使用 | Java 用 `Map<agentId, List<Msg>> callInputs`（`ConcurrentHashMap`，PreCall 写入 / PostCall `remove`） | 无跨调用共享 map → 无并发覆盖、无并发调用同一 agent 时的串扰、无需清理；Java 的 map 若 PostCall 未触发会泄漏条目 |
| TRAIN-X09 | 模型能力位转发 | adapter 额外转发 `supports_native_structured_output`/`_with_tools`/`context_window_size`（`backend.rs:333-345`） | Java `TrinityModelAdapter` 只覆写 `doStream`+`getModelName`，能力位全走 `ChatModelBase` 默认 | 让 shadow 的 A5 结构化输出判定与真实 delegate 一致；仍缺 3 项（TRAIN-06） |
| TRAIN-X10 | HTTP 超时模型 | 单个「整个请求」超时：`RequestBuilder::timeout(self.timeout)`（`backend.rs:205`），默认 300s（`backend.rs:236-246`） | Java OkHttp 分别设 connect/read/write 三个 300s（`TrinityClient.java#TrinityClient`），无总时限 | reqwest 无 per-phase 超时；总时限更严格（避免慢速流式响应无限拖长）。**代价**：极慢但持续有数据的大响应会比 Java 更早失败（feedback 体量极小，风险可忽略） |
| TRAIN-X11 | DTO 空字段序列化 | `#[serde(skip_serializing_if = "Option::is_none")]`（`backend.rs:23-30`/`:71-82`）→ `commit()` 发送 `{}` | Java Jackson 无 `@JsonInclude(NON_NULL)`（`rg 'NON_NULL\|Include\.' agentscope-core/.../util` 零命中）→ 发送 `{"task_id":null,"run_id":null,"time_threshold":null}` | 省略 null 更省流且被 FastAPI/pydantic 默认接受；**风险**：若 Trinity 侧校验「字段必须存在」会 422，届时改用默认序列化即可（已有测试 `commit_request_omits_unset_fields`/`feedback_request_omits_unset_reward` 锁定当前行为，改动需同步改测） |

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-training` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-training` / `cargo test -p agentscope-extensions-training` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-training/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. 执行历史（已归档）

- 执行记录 / 静态复核 / 勘误 / 反向验证明细：[`history/TRAINING__MIGRATION_HISTORY.md`](./history/TRAINING__MIGRATION_HISTORY.md)
- 本文件只保留现行状态（§0–§5）；历史条目一律写进上述归档文件，不再回填本文。
