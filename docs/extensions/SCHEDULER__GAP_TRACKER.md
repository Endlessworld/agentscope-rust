# SCHEDULER 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **SCHEDULER** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`D:\IdeaProjects\agentscope-java\agentscope-extensions\agentscope-extensions-scheduler\`（原先记录的 `E:\local-github\...` 已不存在，2026-10-26 更正为 D 盘）
> **Java 子模块**：scheduler-common, scheduler-quartz, scheduler-xxl-job
> **Rust 目标**：`crates/agentscope-extensions-scheduler/`（波次 C）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-scheduler` |
| Java 子模块 | scheduler-common, scheduler-quartz, scheduler-xxl-job |
| Java 规模（实测 2026-10-22） | 主代码 3234 行（14 个顶层类型）/ 测试 3178 行（13 个测试类 + 1 个 Stub 辅助类，共 180 个 `@Test`）；分模块 主/测：common 1868/1094、quartz 975/1445、xxl-job 391/639 |
| Rust crate | `crates/agentscope-extensions-scheduler` |
| 迁移波次 | C |
| 当前状态 | **已完成**（P1/P2 清零；2026-10-31 实测 100 passed / 0 failed（lib），另 3 个集成）—— §3/§4.2 无未关闭条目，剩余项全部为「设计决定（不迁移）」并附源码级依据。见 history/SCHEDULER__MIGRATION_HISTORY.md |

> **2026-10-31 全量对齐复核**：§1 状态维持「已完成」口径（历史条目未回改）；本轮新增 §3.5（缺口/待裁定 11 条 + 已对齐抽样 13 条）与 §4.3（不迁移 D01–D06、有意差异 X01–X08）。结论：核心调度链路可用，但仍有 **1 条 P0**（`schedule(AgentConfig)` 入口不可执行，SCHED-01）与 5 条 P1（cancel 联动、misfire、FIXED_RATE 首触发、集群互斥、xxl-job 执行器协议），其中 P1 内 2 条「待裁定」（SCHED-05、SCHED-06），另有 1 条 P2 待裁定（SCHED-09）；编号 `SCHED-01`–`SCHED-24`。

## 2. Java 清单与依赖映射

### 2.0 子模块结构
Java 聚合模块 = 1 个 Rust crate；3 个 Java 子模块映射为 crate 内 3 个模块（`common` / `quartz` / `xxljob`，对齐 `EXTENSIONS_MIGRATION_PLAN.md` §1「子模块以 `src/<sub>.rs` 体现」）。
**`common` 抽象为本 crate 内共享层**（`src/common/`），quartz / xxljob 两个后端均实现其 trait；不新建独立 crate、不放到 core/harness。

| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `scheduler.AgentScheduler`（interface） | 调度器核心契约：schedule/cancel/getScheduledAgent/getAllScheduleAgentTasks/shutdown/getSchedulerType | `common::scheduler::AgentScheduler`（async_trait） | 直接映射；Rust 侧 `schedule(AgentConfig, ScheduleConfig)` 与 `schedule_with_input(...)` **两个方法并列**（Java 是 `schedule` 重载），另有后端专有 `schedule_runtime(Arc<RuntimeAgentConfig>, ...)`。**勘误：不存在 `Arc<dyn AgentConfig>`**——`AgentConfig` 是具体 struct，按值传入 |
| `scheduler.ScheduleAgentTask<T>`（interface） | 任务句柄：getId/getName/run/cancel | `common::task::ScheduleAgentTask`（async_trait） | `Mono<T>` → `async fn run(Vec<Msg>) -> Result<Msg>`；泛型 T 收敛为 `Msg`；额外 `as_any()` 下转型钩子 + `run_empty()` 默认方法 |
| `scheduler.BaseScheduleAgentTask` | 默认实现：UUID id、cancelled/executionCount 原子量、每次执行新建 agent、createAgent 校验 | `common::task::BaseScheduleAgentTask` | 每次执行经注入的 `AgentFactory` 运行（**不直接 new `ReActAgent`**，与 Java 内建 `ReActAgent.builder()` 不同）；`AtomicBool`/`AtomicU64` 对齐。**校验缺口**：只查 name/sysPrompt，不查 model |
| `config.AgentConfig` | 可序列化元数据（name/sysPrompt/modelConfig），builder + 校验 | `common::config::AgentConfig` | serde 往返；name/modelConfig 必填（校验对齐）。Rust 用 `model_name: String` 取代 `ModelConfig` 对象，且**字段名无 camelCase 重命名**（Java JSON 为 `sysPrompt`/`modelConfig`，Rust 为 `sys_prompt`/`model_name`）→ 跨语言持久化不互通 |
| `config.RuntimeAgentConfig` | 追加运行时对象（model/toolkit/hooks） | `common::config::RuntimeAgentConfig` | Rust 侧用 `Arc<dyn Model>` + `Toolkit` + `Vec<Arc<dyn Middleware>>`（Rust 无 hooks，走 middleware）；**继承改为内嵌**（Rust 无类继承）。**可变性缺口已补（2026-10-31）**：Java `getHooks()` 可变 → Rust 新增复制构造 `with_middleware(Arc<dyn Middleware>)` / `with_model(Arc<dyn Model>)`（`src/common/config/spec.rs`），xxl-job 用它追加日志中间件（见 history/SCHEDULER__MIGRATION_HISTORY.md） |
| `config.ModelConfig`（interface） | getModelName + createModel | `common::config::ModelConfig`（trait） | `create_model() -> Arc<dyn Model>`（Java `Model` → core `Model` trait） |
| `config.DashScopeModelConfig` | DashScope 模型配置实现 | `common::config::StaticModelConfig`（占位实现，`src/common/config/spec.rs`） | Rust 无 DashScopeChatModel（属 MODEL 模块，未迁移）；实际提供的是 `StaticModelConfig`（包裹已构造的 `Arc<dyn Model>`），**不存在名为 `ModelSpec` 的类型**，见 §4.2 |
| `config.ScheduleConfig` | 调度参数（mode/cron/fixedRate/fixedDelay/initialDelay/zoneId），builder + 按 mode 校验 | `common::config::ScheduleConfig` | serde 往返；ms 语义对齐（Java Long ms → Rust `Option<u64>`） |
| `config.ScheduleMode` | 枚举 NONE/CRON/FIXED_RATE/FIXED_DELAY | `common::config::ScheduleMode` | 变体名天然一致（`NONE`/`CRON`/...），**未使用任何 `#[serde(rename)]`**（grep 命中 0），靠变体同名而非显式 rename 对齐 |
| `quartz.AgentQuartzJob` | Quartz Job：查 scheduler → 取 task → run（FIXED_DELAY 完成后重排） | `quartz::job::spawn_job`（**模块私有，无公开 `AgentJob` 类型**） | 定时循环体内执行；FIXED_DELAY 在 run 完成后 sleep 再排下次。**勘误：不存在 `quartz::AgentJob`**，`job` 为 `mod job;`（私有），仅导出 `spawn_job` |
| `quartz.QuartzAgentSchedulerRegistry`（package-private） | schedulerId → scheduler 静态注册表 | `quartz::SchedulerRegistry` | 进程内 `RwLock<HashMap>` 单例；Rust 无 JVM 静态，用 `OnceLock`。**2026-10-31 已对齐**：登记值改存 `Weak<QuartzAgentScheduler>` 句柄（`RegistryEntry{id, scheduler_type, handle}`），新增反向查找 `get_handle(id) -> Option<Arc<QuartzAgentScheduler>>`（= Java `registry.get(id)`）；`scheduler_type` 现为真实类型 `"quartz"`（此前误写为 id）；失去强引用的条目在查找时被剪除（Java 无此场景，Rust 避免僵尸条目）。注册入口：`QuartzAgentScheduler::new_shared` / `register_self` / `QuartzAgentSchedulerBuilder::build_shared`（Java 在构造函数注册，Rust 需 `Arc` 才能取弱引用） |
| `quartz.QuartzScheduleAgentTask`（package-private） | 包装 JobKey + Quartz Scheduler，pause/resume | `quartz::QuartzScheduleAgentTask` | 包装 `JoinHandle` + `AtomicBool` 控制位 + 2 个 `Notify`；pause/resume/**无 reschedule**（Java 的 `rescheduleNextFixedDelay` 在 Rust 中由 job 循环 sleep 替代） |
| `xxljob.XxlJobAgentScheduler` | XXL-Job 实现：注册 JobHandler（仅支持 NONE）、由控制台触发、cancel 抛 Unsupported | `xxljob::XxlJobAgentScheduler` | Rust 无 xxl-job SDK；提供 **JobHandler 注册表 + 本地触发 `trigger(name, params)`**，对齐「只注册 handler、调度由外部控制台决定」语义。**cancel 语义不等价**（Java 抛异常，Rust 返回 false，另有 `try_cancel` 报原文错误）；**日志 Hook 已于 2026-10-31 补齐**：`XxlJobLogMiddleware`（`src/xxljob/log.rs`）挂 core `on_reasoning_result`，由 `schedule_runtime` 经 `with_middleware` 注入（见 history/SCHEDULER__MIGRATION_HISTORY.md）；无 executor `destroy()`（设计决定，见 history/SCHEDULER__MIGRATION_HISTORY.md） |

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| reactor-core (Mono/Flux) | 异步结果 | `tokio` async/await（`async fn -> Result<Msg>`） | 已在本 crate Cargo.toml 声明 |
| quartz-scheduler | 调度引擎 | 自研 `quartz::cron::CronExpression`（`src/quartz/cron.rs`，293 行）+ `tokio::time`；**不引 Quartz C 绑定** | 勘误：**未使用** `tokio-cron-scheduler`（`Cargo.toml` 无此依赖，代码中 `tokio_cron` 引用数 = 0）；cron 为手写解析器，支持面见 history/SCHEDULER__MIGRATION_HISTORY.md |
| xxl-job-core | 分布式调度 | 无 SDK；自建 `JobHandlerRegistry` + 本地 `trigger` | JobHandler 名 = agent name |
| jackson-databind | JSON 序列化（inputMsg 存 JobDataMap） | `serde` / `serde_json` | 已声明 |
| slf4j | 日志 | `tracing` | 已声明 |
| java.util.UUID | 任务 id | `uuid`（workspace v1） | 勘误：`uuid = { workspace = true }` **已在** `Cargo.toml:19` 声明，无需再补 |
| java.time / TimeZone | zoneId | `chrono`（已声明） | **已修复（2026-10-26，见 history/SCHEDULER__MIGRATION_HISTORY.md）**：新增 `common/config/zone.rs`（`resolve_zone` → `chrono::FixedOffset`）+ `ScheduleConfig::zone_offset()`，`quartz::job::CronPlan::matches_at` 按配置时区求值。**限制**：未引入 `chrono-tz`，只支持 UTC 别名 / 固定偏移 / 免 DST 的 IANA 白名单；`Europe/London` 等 DST 区报明确错误 |
| agentscope-core (Model/Toolkit/Hook/ReActAgent/Msg) | 核心类型 | `agentscope-core`（已声明） | `Model`/`Toolkit`/`Msg`/`ReActAgent` 直接复用 |
| agentscope-extensions-model-dashscope | DashScopeChatModel | **无对应**（MODEL 模块未迁移该 provider） | 依赖注入 `Arc<dyn Model>`，见 §4.2；`RuntimeAgentConfig::model` 为 `Option`，缺失时 `BaseScheduleAgentTask::run_with` **先校验 name/sysPrompt、再校验 model**（2026-10-31 已对齐 Java `createAgent`，错误文案逐字 `Model cannot be null.`，§3.1）。注意：`schedule(AgentConfig)` 这一**可序列化入口**无法构造 model（本 crate 无 provider 注册表），→ 执行期报该错；需要可执行请用 `schedule_runtime` + `StaticModelConfig` |

### 2.2 Java 类清单逐项核对（3 个子模块，14 个主代码类型）

**scheduler-common（9 个类型）**
| Java 类 | Rust 是否存在 | 行为覆盖 |
|---|---|---|
| `AgentScheduler` | ✅ trait | 6 个方法齐全；Java 的 `schedule(a,b,Msg)` 默认抛 Unsupported → Rust `schedule_with_input` 默认同语义 |
| `ScheduleAgentTask<T>` | ✅ trait | run/cancel/getId/getName 齐全；Java 无 `as_any`/`run_empty` |
| `BaseScheduleAgentTask` | ✅ struct | 见 §2.0 表格；**校验缺口已关闭（2026-10-31）**：`run_with` 在 name/sysPrompt 之后校验 model，错误文案 `Model cannot be null.` 与 Java 逐字一致 |
| `config.AgentConfig` | ✅ struct | 字段少 `ModelConfig` 对象（降级为 `model_name` 字符串），serde 字段名不兼容 |
| `config.RuntimeAgentConfig` | ✅ struct | toolkit/middleware/model 齐全；**Java 的 `hooks` 为可变 `CopyOnWriteArrayList`（xxl-job 依赖此可变性追加日志 Hook）**，Rust 为不可变 `Vec` → 2026-10-31 以复制构造 `with_middleware` / `with_model` 代替（`XxlJobLogMiddleware` 即走此路径） |
| `config.ModelConfig` | ✅ trait | `model_name` + `create_model` 对齐 |
| `config.DashScopeModelConfig` | ⚠️ 替代为 `StaticModelConfig` | 无 DashScope 语义（无 apiKey/modelName/stream 等字段）；Java 侧 15 个 `@Test`，Rust 侧 0 个对应测试 |
| `config.ScheduleConfig` | ✅ struct | 6 字段齐全；Java 19 个 `@Test`，Rust 5 个 |
| `config.ScheduleMode` | ✅ enum | 4 变体对齐；Java 6 个 `@Test`，Rust 0 个独立测试（仅散落在 schedule 测试中） |

**scheduler-quartz（4 个类型）**
| Java 类 | Rust 是否存在 | 行为覆盖 |
|---|---|---|
| `QuartzAgentScheduler`（657 行 / 26955B） | ✅ struct | 2026-10-31：补 **Builder**（`QuartzAgentSchedulerBuilder`，`scheduler_id` 可指定；`autoStart` 无对应、自定义 Quartz `Scheduler` 不可注入 → 见 history/SCHEDULER__MIGRATION_HISTORY.md）、**单参便捷法** `schedule_default(AgentConfig)`、**真实 interrupt**（2026-10-28）。仍无 `loadTaskFromQuartz` 集群回读（见 history/SCHEDULER__MIGRATION_HISTORY.md）、无 `rescheduleNextFixedDelay` 公开能力（FIXED_DELAY 循环已等价） |
| `AgentQuartzJob` | ⚠️ 仅 `job::spawn_job`（私有） | `InterruptableJob` 语义、`inputMsg` 重放均已对齐（见 history/SCHEDULER__MIGRATION_HISTORY.md）；反序列化失败时 `setUnscheduleFiringTrigger(true)` 无对应（Rust 初始消息为 `Msg` 结构体，不经 JSON 反序列化，见 history/SCHEDULER__MIGRATION_HISTORY.md） |
| `QuartzAgentSchedulerRegistry` | ✅ struct | 2026-10-31：改为 `Weak` 句柄 + 反向查找 `get_handle`（见 history/SCHEDULER__MIGRATION_HISTORY.md） |
| `QuartzScheduleAgentTask` | ✅ struct | 缺 `getJobKey()` 等价物（Rust 无 JobKey 概念，`next_fire_time`/`schedule_config` 已覆盖可观测需求，见 history/SCHEDULER__MIGRATION_HISTORY.md） |

**scheduler-xxl-job（1 个类型）**
| Java 类 | Rust 是否存在 | 行为覆盖 |
|---|---|---|
| `XxlJobAgentScheduler`（363 行 / 15762B） | ✅ struct | 见 §2.0 表格与 见 history/SCHEDULER__MIGRATION_HISTORY.md；**协议层已于 2026-10-28 补齐**：`XxlJobContext`（job id / job param / 分片 / 日志 id）+ `XxlJobHelper`（`log`/`handleSuccess`/`handleFail`）+ `try_cancel` 报错。**`PostReasoningEvent` 日志 Hook 已于 2026-10-31 补齐**：`XxlJobLogMiddleware` 挂 core `on_reasoning_result`，`schedule_runtime` 经 `with_middleware` 注入（见 history/SCHEDULER__MIGRATION_HISTORY.md）。仍无 executor `destroy()`（Rust 无 executor，设计决定） |

## 3. 详细缺口清单
### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 依赖（uuid, tokio-cron-scheduler/chrono, async-trait 等） | pom.xml ×3 | 已完成 | `Cargo.toml` 现含 core/serde/serde_json/thiserror/async-trait/tokio/tracing/chrono/uuid；**2026-10-31 清理**：删除 0 引用的 `agentscope-harness`（`rg` 全 crate 0 命中），`futures` 降为 `[dev-dependencies]`（仅测试桩模型构造 `BoxStream` 需要，生产代码 0 引用）。`tokio_cron` 引用仍为 0（未引入） |
| `common` 共享层骨架（本 crate 内） | scheduler-common | 已完成 | 实为 7 个文件而非 4：`src/common/{mod,config/mod,config/schedule,config/spec,scheduler/mod,scheduler/scheduler,scheduler/task}.rs`（`config/spec.rs` 承载 AgentConfig+RuntimeAgentConfig+ModelConfig+StaticModelConfig） |
| ScheduleMode / ScheduleConfig | 同名 | 已完成 | serde + builder + 按 mode 校验；5 个 Rust 单测（非「6 个 Java 测试对应」，Java ScheduleConfigTest 实为 19 个 `@Test`） |
| AgentConfig / RuntimeAgentConfig / ModelConfig | 同名 | 已完成 | RuntimeAgentConfig 内嵌 AgentConfig；注意 **AgentConfig 无 `description`/`agentFactory` 字段**（Java 已无 description，doc 注释残留），`model_name: String` 取代 `ModelConfig` 对象 |
| AgentScheduler / ScheduleAgentTask trait | 同名 | 已完成 | async_trait；泛型 T→Msg；额外提供 `run_empty()` 便捷方法与 `as_any()` 下转型钩子 |
| BaseScheduleAgentTask | 同名 | 已完成 | 每次执行新建 agent（经 `AgentFactory`）+ 计数 + cancel；**2026-10-31 关闭 model 校验缺口**：`run_with` 在 name/sysPrompt 之后校验 `agent_config.model()`，缺失返回 `Error::Agent("Model cannot be null.")`（Java `createAgent` 文案逐字，`spec.rs` 的 `AgentConfigError::MissingModel` 同步补句点）。测试：`common::scheduler::task::tests::missing_model_fails_with_the_java_message`、`failed_execution_still_counts` |

### 3.2 quartz 后端（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| QuartzAgentScheduler schedule/cancel/getAll/shutdown/getSchedulerType | QuartzAgentScheduler(26955B) | 已完成 | tokio 定时循环 + 手写 cron 解析；2026-10-31 补 `QuartzAgentSchedulerBuilder`（`scheduler_id`）与 `schedule_default(AgentConfig)`。**集群回读为设计决定（不迁移）**：Java `loadTaskFromQuartz` 从 Quartz JDBC store 重建任务，Rust 无共享 store（见 history/SCHEDULER__MIGRATION_HISTORY.md）。**shutdown 语义已于 2026-10-31 对齐**：`shutdown(true)` = 先等 in-flight 跑完再退出循环（`join_with_grace`，30s 上限后 abort 防死等），不再是 `abort()` 丢弃（见 history/SCHEDULER__MIGRATION_HISTORY.md） |
| CRON / FIXED_RATE / FIXED_DELAY 三模式 | 同名 | 已完成 | 三模式均已实现。cron 4 项 P1 语义于 2026-10-26 对齐（见 history/SCHEDULER__MIGRATION_HISTORY.md）；**Quartz 扩展关键字与年字段于 2026-10-28 落地（见 history/SCHEDULER__MIGRATION_HISTORY.md）**：`L`/`L-n`/`LW`/`nW`（dom）、`L`/`nL`/`n#k`（dow）、7 字段年（1970-2199）、`a/1` 展开、`?` 仅限日字段。`next_after` **已接入生产循环**（`CronPlan::next_fire_time` → 精确休眠，替换 1s 轮询），并补查询 API `QuartzAgentScheduler::next_fire_time` / `QuartzScheduleAgentTask::next_fire_time`（对齐 `Trigger#getNextFireTime`）。余额：无（DST 时区仍不支持，见 §4.2） |
| pause / resume / interrupt / getStatus | 同名 | 已完成 | **2026-10-28 补齐**：`interrupt()` 经 `tokio::sync::watch` 中止 in-flight 执行（`execute_with` 内 `select!` 竞争，回落 `Error::Interrupted`），FIXED_DELAY 被中断时跳过重排（对齐 `AgentQuartzJob:49/107`）；返回「是否真有 in-flight」对齐 `Scheduler#interrupt`。`TaskStatus` 扩为 Quartz 6 值 + Rust 专用 `Cancelled`（`None/Normal/Paused/Complete/Error/Blocked/Cancelled`，新增 `as_str`/`is_terminal`），并新增 Java 同形的 `trigger_state(name) -> TaskStatus`（未知任务 = `None`）。`get_status` 保留 `Option<TaskStatus>` |
| SchedulerRegistry | QuartzAgentSchedulerRegistry | 已完成 | 2026-10-31：`OnceLock<RwLock<HashMap<String, RegistryEntry>>>`，条目存 `Weak<QuartzAgentScheduler>` + 真实类型 `"quartz"`；`get_handle(id)` 反查活体调度器（Java `registry.get(id)`），弱引用失效的条目在查找时剪除。注册入口 `new_shared` / `register_self` / `build_shared`；`shutdown()` 注销。测试：`quartz::registry::tests::{register_lookup_unregister, entry_dies_with_the_scheduler_and_is_pruned}`、`quartz::tests::builder_sets_a_deterministic_scheduler_id`、`shutdown_clears_registry_and_tasks` |
| QuartzScheduleAgentTask | 同名 | 已完成 | P0（`with_factory` 未接线）已于 2026-10-22 修复（见 history/SCHEDULER__MIGRATION_HISTORY.md）；**2026-10-28 补 `schedule_config()` / `agent_config()`**（Java `getScheduleConfig`/`getAgentConfig`，见 history/SCHEDULER__MIGRATION_HISTORY.md 遗留），并持有 `CronPlan` 供 `next_fire_time` 查询。`cancel` 现在 `shutdown()` + `join().await`（`join` 不再是死代码），`cancel` 返回后 job 循环与 in-flight 执行均已终止 |
| AgentJob | AgentQuartzJob | 已完成 | 循环体 + `inputMsg` 已接（P0-2，见 history/SCHEDULER__MIGRATION_HISTORY.md）；2026-10-28 起 CRON 分支改为「睡到 `next_fire_time`」而非按秒轮询，无未来触发时置 `Complete`；interrupt 语义按 Java 短路 |

### 3.3 xxl-job 后端（P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| XxlJobAgentScheduler（JobHandler 注册 + 本地 trigger） | XxlJobAgentScheduler(15762B) | 已完成 | 仅 NONE 模式；**2026-10-28 补齐协议层（见 history/SCHEDULER__MIGRATION_HISTORY.md）**：本 crate 新增 `xxljob::context` — `XxlJobContext`（job_id / job_param / shard_index+total / log_id / 日志与 handle 结果）+ `XxlJobHelper`（`log` / `handle_success` / `handle_fail` / `get_job_param` / `get_job_id` / `get_shard_index` / `get_shard_total` / `get_log_id`），用 `tokio::task_local!` 替代 Java 的 ThreadLocal；`trigger_with_context(name, Arc<XxlJobContext>)` 在上下文作用域内执行：`getJobParam()` → `Msg::user`，成功后 `handleSuccess`、失败 `handleFail`。**2026-10-31 补齐日志 Hook**：`XxlJobLogMiddleware`（`src/xxljob/log.rs`）挂 core `on_reasoning_result`，由 `schedule_runtime` 用 `RuntimeAgentConfig::with_middleware` 注入；`trigger_with_context` 不再把最终回复写入日志（Java 只经 Hook 记录每个推理步）。测试：端到端装配 `dispatch_logs_each_reasoning_step_through_the_attached_middleware`、中间件本身 `xxljob::log::tests::{reasoning_message_reaches_the_dispatch_log, logging_outside_a_dispatch_is_a_noop}`、注入点 `schedule_appends_the_reasoning_log_middleware`。`cancel` 见下行 |
| JobHandler 注册表 | XxlJobExecutor.registryJobHandler | 已完成 | 进程内 `RwLock<HashMap<String, JobHandler>>`；**无 executor 生命周期**（设计决定：Rust 无 executor，`shutdown()` 清 map）；注册失败无异常路径（纯内存 insert 不会失败，属设计差异）。**cancel 语义（2026-10-28）**：新增 `XxlJobAgentScheduler::try_cancel()` 返回 Java 原文错误 `"Cancel operation is not supported by XxlJobAgentScheduler. Please use XXL-Job admin console to pause or delete the job."`，并新增 trait 默认方法 `AgentScheduler::try_cancel`（xxl-job override 为 Err）；布尔版 `cancel()` 仍返回 `false` + warn（受 trait 签名限制，无法区分「不支持/未找到」，已在 §4.2 登记） |

## 3.5 2026-10-31 全量对齐复核清单（唯一编号）

> 复核基线：Java `main` @ `1524b3191`；Rust 工作树 HEAD 见仓库。本表为本轮逐文件核对结论，**存量 §3.1–§3.4 历史清单保持原样未重新编号**。
> 编号规则：`SCHED-NN`（缺口，状态列取值 `待办` / `进行中` / `已完成` / `设计决定（不迁移）` / `待裁定`）；§4.3 差异另有 `SCHED-D<NN>` / `SCHED-X<NN>` 系列。编号本模块内唯一、单调递增，不复用历史编号。
> **2026-10-31 裁定（汇总方已确认）**：本文件的 `待裁定` 项已全部裁定，状态列更新为 `待办` / `设计决定（不迁移）` / `已完成`；裁定结论、依据与执行批次见 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` §3.3。
> 正文其它位置出现的旧「待裁定 N 项」为复核时点快照，一律以 §3.5/§3.6 表格的状态列为准。
> 复核范围：Java 3 子模块 14 个主代码类型（3234 行）+ 13 个测试类（3178 行 / 180 `@Test`）；Rust `src/**/*.rs` 17 个文件（约 4.8k 行）+ `tests/quartz_execution.rs`，测试数沿用既有实测 **lib 100 + 集成 3**（本轮未重跑）。
> 一致性判定：`scheduler-common` 为本 crate 内共享层（§2.0），非独立 crate；quartz / xxl-job 两侧均为「语义模拟」而非真依赖（Java 依赖 quartz-scheduler、xxl-job-core，Rust 用 tokio 循环 + 手写 cron + 进程内 handler 表），等价性证据见 §4.3.2 `SCHED-X01`/`SCHED-X02` 与 §3.5.2 `SCHED-12`–`SCHED-19`。

### 3.5.1 缺口与待裁定

| 编号 | 优先级 | 缺口项（可执行描述） | Java 参考（文件#符号） | Rust 现状（文件:行 / rg 零命中） | 执行状态 | 下一动作 |
|---|---|---|---|---|---|---|
| SCHED-01 | P0 | 可序列化入口 `schedule(AgentConfig, ScheduleConfig)` 产出的任务在 Rust 侧**必然执行失败**：`RuntimeAgentConfig::from` 不带 model，而 `run_with` 在调用注入的 factory 之前就强制校验 model | `QuartzAgentScheduler.java#schedule`（`RuntimeAgentConfig.builder().modelConfig(agentConfig.getModelConfig())`）+ `BaseScheduleAgentTask.java#createAgent`（`getModelConfig().createModel()` 真造 model） | `common/config/spec.rs:227-235`（`From<AgentConfig>` → `model: None`）；`common/scheduler/task.rs:145-148`（`model().is_none() → Err("Model cannot be null.")`，先于 `factory.run`）；本 crate 内 `impl AgentFactory` 全部位于 `#[cfg(test)]`/`tests/`（`src/lib.rs:43`、`src/quartz/mod.rs:753`、`src/quartz/task.rs:444/455`、`src/common/scheduler/task.rs:197`、`src/xxljob/mod.rs:296/421`、`tests/quartz_execution.rs:38`），宿主工厂即使能自造 model 也无法绕开该校验 | 已完成【2026-10-31 落地：新增 `AgentFactory::resolve_model`（默认 `cfg.model().cloned()`）；`run_with` 仅当「配置与工厂都无 model」才报 `Model cannot be null.`，并把工厂解析的 model 注入传给 `run` 的配置（`common/scheduler/task.rs:39`、`:167`）；测试 `common::scheduler::task::tests::factory_resolved_model_is_accepted_and_handed_to_run`、`quartz::tests::schedule_default_runs_with_a_factory_resolved_model`】 | 在 `AgentFactory` 增加 `fn resolve_model(&self, cfg: &RuntimeAgentConfig) -> Option<Arc<dyn Model>>`（默认 `cfg.model().cloned()`），`run_with` 改为「配置与工厂都无 model 才报 `Model cannot be null.`」；验收：`schedule(AgentConfig{model_name:"qwen-max"}, NONE)` + 宿主 factory 成功执行 1 次且 `execution_count()==1`（现 `quartz/mod.rs:443 schedule_default_uses_none_mode_and_needs_a_runtime_model` 只断言失败） |
| SCHED-02 | P1 | `ScheduleAgentTask::cancel()` 未与所属调度器联动：Quartz 任务取消后仍留在 `jobs` 表，同名重排会拿回一个已取消的死任务 | `BaseScheduleAgentTask.java#cancel`（`cancelled.set(true); scheduler.cancel(getName())` → `deleteJob` + `tasks.remove`） | `common/scheduler/task.rs:154-162` `do_cancel` 只置标志 + 可选 `on_cancel` 回调；`with_on_cancel` 生产零调用（`rg 'with_on_cancel' src` 仅 `task.rs:94` 定义 + `:258` 测试）；`quartz/task.rs:396-398` `cancel()` = `shutdown()`，不摘 `QuartzAgentScheduler.jobs`；`quartz/mod.rs:128-133` 同名重排返回既有（已取消）任务 | 待办 | `QuartzScheduleAgentTask::new` 内以 `BaseScheduleAgentTask::with_on_cancel` 回填「从 scheduler 的 `jobs` 移除」（持 `Weak<QuartzAgentScheduler>` 避免引用环）；xxl-job 分支另判是否改为 `try_cancel` 同语义报错；验收：`task.cancel()` 后 `get_scheduled_agent` 为 `None`、同名可重新 schedule 且能执行（新增 2 用例） |
| SCHED-03 | P1 | 错过触发（misfire）策略未对齐：FIXED_RATE 会把每个错过的周期**逐个补触发**，CRON / 暂停恢复则完全不补 | `QuartzAgentScheduler.java#schedule`（`CronScheduleBuilder` / `SimpleScheduleBuilder` 均未设 misfire 指令 → Quartz SMART 策略：恢复后补一次再按原节奏） | `quartz/job.rs:139-160`：`tokio::time::interval(period)` 未设 `MissedTickBehavior`（`rg 'MissedTickBehavior\|misfire' src tests` 双零命中）→ tokio 默认 `Burst`，执行耗时超过周期时连续立即补触发 N 次；`quartz/job.rs:102-118` CRON 分支每次从 `Utc::now()` 重算 `next_fire_time`，错过槽位直接丢弃；`job.rs:155` 暂停恢复同理 | 待办 | FIXED_RATE 显式设 `MissedTickBehavior::Skip`（对齐 SMART「不补」）或按 SMART 补一次；CRON/暂停恢复在 `CronPlan` 上保留「上次计划触发点」，恢复后若仅错过 1 槽则补触发一次；验收：`fixed_rate=100ms` + 单次执行 350ms，断言总触发次数 ≤ 5（Burst 会到 8），并新增 misfire 用例 |
| SCHED-04 | P1 | FIXED_RATE **首次触发比 Java 晚一个周期**：Java 在 `startAt`（默认 now）立即首触发 | `QuartzAgentScheduler.java#schedule`（`SimpleScheduleBuilder...repeatForever()` + `startAt(startAt)`，startAt = now + initialDelay） | `quartz/job.rs:141-146`：`let mut ticker = interval(period); ticker.tick().await;` 消费掉「立即 tick」，循环内 `job.rs:149` 再 `tick()` → 首次落在 `initial_delay + fixed_rate` | 待办 | 消费首 tick 后立即 `fire` 一次（再进循环），或改 `sleep(initial_delay)` + 显式 `Instant` 推算；验收：`fixed_rate=500ms` 时 t≈initial_delay 处有首次触发（现 `quartz/mod.rs:816 fixed_rate_cancel_waits_for_the_in_flight_execution` 不断言首次时机） |
| SCHED-05 | P1 | 多实例/集群部署无互斥、无跨节点任务回读：每个进程各自触发同一 cron，重复执行 | `QuartzAgentScheduler.java#getScheduledAgent`→`loadTaskFromQuartz`、`#getAllScheduleAgentTasks`（按 `agentscope-quartz` group 从 Quartz store 读，JDBC store 集群下由 Quartz 集群锁保证单节点执行） | `quartz/mod.rs:337-345` 仅读本地 `jobs`；`rg 'lock\|leader\|election\|advisory\|cluster' src` 全零（`distributed_lock`/`acquire_lock` 亦零） | 已完成【2026-10-31 落地：①`src/lib.rs` 的 `# Deployment premise` + `src/common/scheduler/lock.rs` 模块文档明确「默认单实例、多实例由宿主注入锁」；②新增 `SchedulerLock` + `NoopSchedulerLock`（`common/scheduler/lock.rs:26`），`QuartzAgentSchedulerBuilder::scheduler_lock` 注入，`quartz/job.rs:209 fire()` 每次自动触发前 `try_acquire`、完成后 `release`，槽位键 `<scheduler_id>:<task>:<slot>`（CRON=计划触发时刻 / FIXED_RATE=周期桶 / FIXED_DELAY=触发时刻）；测试 `quartz::tests::{injected_scheduler_lock_gates_every_firing, a_denied_slot_skips_the_execution}`、`common::scheduler::lock::tests::noop_lock_grants_every_slot`】 | 二选一：①在 crate 文档与部署说明中明确「单实例前提」（`scheduler_type()` 不变）；②接入存储 crate 的原生锁（JDBC `NativeLockSession`/Redis `SET NX`）实现「同一 cron 槽位仅一实例执行」；验收：两实例同 cron 单槽位仅 1 次执行，或文档+`README` 明确声明 【裁定 2026-10-31：两步：文档声明单实例前提 + SchedulerLock trait（默认 no-op）由宿主注入】 |
| SCHED-06 | P1 | xxl-job **执行器协议**（`/api/registry` 注册、`/beat` 心跳、`/run` 调度回调、`/idleBeat`、`/kill`、`/log` 日志上报）整体缺失：当前只有进程内 `trigger`，无法被 admin console 调度 | `XxlJobAgentScheduler.java#registerJobHandler`（`XxlJobExecutor.registryJobHandler`，端点与心跳由 xxl-job-core SDK 提供） | `src/xxljob/mod.rs:135-201` 仅 `trigger`/`trigger_with_context`/`try_cancel`；`rg 'axum\|hyper\|reqwest' src` 全零（无 HTTP server/client）；`rg 'beat\|registry' src` 仅命中进程内注册与注释 | 已完成【2026-10-31 落地：新增 `src/xxljob/bridge.rs` —— `ExecutorBridge`（`run`/`idle_beat`/`kill`，由 `XxlJobAgentScheduler` 实现）+ `DispatchRequest`（Java `TriggerParam`，camelCase serde）→ `XxlJobContext` 映射（`jobId`→`job_id`、`executorParams`→`job_param`、`broadcastIndex/Total`→`shard_index/total`、`logId`）+ `ExecuteResponse`（Java `ReturnT`，200/500）；模块文档表格给出 registry/beat/run/idleBeat/kill/log 六端点映射；无 HTTP 依赖；测试 `xxljob::bridge::tests::{dispatch_request_maps_onto_the_execution_context, bridge_run_dispatches_with_the_mapped_context, bridge_run_reports_500_for_an_unknown_handler, idle_beat_and_kill_report_the_expected_codes}`】 | 二选一：①本 crate 实现最小 executor（新增 HTTP 依赖，需裁定依赖面）；②固化「宿主桥接」契约并在文档给出字段映射（`jobId`/`logId`/`shardIndex`/`shardTotal`/`handleCode` 已具备，见 `xxljob/context.rs:36-60`、`XxlJobHandleCode#code()` 200/500） 【裁定 2026-10-31：B：宿主桥接契约（ExecutorBridge + 字段映射文档），不内置 HTTP server】 |
| SCHED-07 | P2 | 错误分类丢失：Java 三类异常（`IllegalArgumentException` 参数非法 / `UnsupportedOperationException` 不支持 / `RuntimeException` 后端失败）在 Rust 收敛为同一 `SchedulerError(String)`，调用方无法分支处理 | `AgentScheduler.java#cancel`、`XxlJobAgentScheduler.java#schedule/cancel`、`QuartzAgentScheduler.java#schedule` | `common/scheduler/scheduler.rs:15-24`（`pub struct SchedulerError(pub String)`）；`quartz/mod.rs:112-114/126`、`xxljob/mod.rs:80-88` 全部返回同型 | 待办 | 改为 `enum SchedulerError { InvalidArgument(String), Unsupported(String), Backend(String), Shutdown }`，`Display` 文案保持与 Java 逐字一致（现有 `CANCEL_UNSUPPORTED`、`CronExpression '...' is invalid` 等）；验收：非法 cron → `InvalidArgument`、xxl-job `try_cancel` → `Unsupported`、关闭后 schedule → `Shutdown` |
| SCHED-08 | P2 | `TaskControl` 是无实现、无调用点的死 API（Java 无对应类型） | — （Rust 新增抽象） | `common/scheduler/task.rs:27-36` 定义；`rg 'TaskControl' crates` 仅 4 处：定义 + `common/scheduler/mod.rs:11` + `common/mod.rs:17` + `lib.rs:20` 三处 re-export，**无任何 `impl TaskControl` 或调用点** | 待办 | 删除该 trait，或让 `QuartzScheduleAgentTask` 实现并通过 `AgentScheduler` 暴露 `task_control(name)`；验收：`rg 'impl TaskControl'` ≥1 或类型完全移除（二者皆可，需 §4.2 留痕） |
| SCHED-09 | P2 | 任务间并发无上限：Java 受 Quartz 线程池（默认 10）约束，Rust 每任务一个 tokio task，同刻触发数不受限 | `QuartzAgentScheduler.java#Builder#build`（`StdSchedulerFactory` 默认 `threadPool.threadCount=10`） | `quartz/job.rs:76-95` 每任务 `tokio::spawn`；`rg 'Semaphore\|max_concurren' src` 零命中 | 已完成【2026-10-31 落地：`QuartzAgentScheduler` 新增 `concurrency: Arc<Semaphore>`，默认 `DEFAULT_MAX_CONCURRENCY = 10`（对齐 Quartz `threadPool.threadCount`，`quartz/mod.rs:60`），builder `max_concurrency(n)` / `concurrency_limiter(Arc<Semaphore>)`（`:304-313`）；`quartz/job.rs:209 fire()` 执行前取 permit（与 `task.cancelled()` 竞速，取消不泄漏槽位）；测试 `quartz::tests::concurrency_limit_bounds_simultaneous_executions`（0-permit gate + `watch`，无 sleep 竞态）】 | 决策：引入可配 `Arc<Semaphore>` 上限（默认对齐 10），或在文档声明「并发度由 tokio runtime 线程数约束」；验收：N 个同刻 CRON 任务在 limit 内不越界 【裁定 2026-10-31：A：可配 Arc<Semaphore>，默认 10】 |
| SCHED-10 | P2 | serde 字段名与 Java 不一致，跨语言/跨版本持久化不互通（Java JSON 为 `sysPrompt`/`modelConfig`/`scheduleMode`…） | `config/AgentConfig.java`（字段 `name`/`sysPrompt`/`modelConfig`）、`config/ScheduleConfig.java`（`scheduleMode`/`cronExpression`/`fixedRate`/`fixedDelay`/`initialDelay`/`zoneId`，Jackson 默认字段名） | `common/config/spec.rs:70-78`、`common/config/schedule.rs:74-90`；`rg 'rename_all\|serde\(rename' src` 双零命中 → 实际 JSON 为 `sys_prompt`/`model_name`/`schedule_mode`/`cron_expression` | 待办 | `ScheduleConfig` 加 `#[serde(rename_all = "camelCase")]`；`AgentConfig` 用 `#[serde(rename = "modelConfig")]` 包一层 `{modelName}` 或至少 `alias` 兼容；验收：Java 侧导出的 `AgentConfig`/`ScheduleConfig` JSON 可直接 `serde_json::from_str` 成功（新增 2 用例） |
| SCHED-11 | P2 | 测试密度与关键场景缺口（Rust 100 + 3 vs Java 180 `@Test`），FIXED_RATE 周期/首触发时机、cancel 后重排、`schedule(AgentConfig)` 成功路径均无用例 | `QuartzAgentSchedulerTest` 49、`XxlJobAgentSchedulerTest` 34、`ScheduleConfigTest` 19、`AgentQuartzJobTest` 11、`QuartzFixedDelayIntegrationTest` 1 | 现有覆盖见 §3.5.2；`quartz/mod.rs:816`、`tests/quartz_execution.rs:162` 只覆盖 fixed_delay 触发与 cancel 等待，无 fixed_rate 周期精度/首触发断言 | 待办 | 随 SCHED-02/03/04 一并补 3 组用例（fixed_rate 周期与首触发、cancel 后同名重排、`schedule`+factory 端到端）；验收：lib ≥ 110 全绿，且每个新缺口至少 1 条反向可失败用例 |

### 3.5.2 本轮复核确认已对齐（抽样，不求穷举）

| 编号 | 能力 | Java 参考 | Rust 证据（文件:行 + 测试名） | 执行状态 |
|---|---|---|---|---|
| SCHED-12 | Quartz cron 语义：6/7 字段、`1=SUN..7=SAT`、`JAN-DEC`/`SUN-SAT` 名称、`L`/`L-n`/`LW`/`nW`/`nL`/`n#k`、`a/1` 展开、`?` 仅限日字段、dom/dow 冲突拒绝 | `QuartzAgentScheduler.java#schedule`（`CronScheduleBuilder`）；Quartz `CronExpression` 语法 | `src/quartz/cron.rs`（904 行，手写解析器）；测试 `cron::tests::{parses_six_field_quartz, weekday_numbers_follow_quartz, rejects_zero_based_weekday_numbers, rejects_both_day_of_month_and_day_of_week, last_day_offset, nearest_weekday_does_not_cross_month_boundary, nth_day_of_week_in_month, bare_last_day_of_week_means_saturday, step_without_range_expands_even_for_step_one, question_mark_only_allowed_in_day_fields, year_field_range_matches_quartz}` | 已完成 |
| SCHED-13 | `zoneId` 生效：按配置时区的墙钟求值（含负偏移跨星期边界） | `QuartzAgentScheduler.java#schedule`（`csb.inTimeZone(TimeZone.getTimeZone(zoneId))`） | `common/config/zone.rs` + `quartz/job.rs#CronPlan::matches_at`；测试 `job::tests::{cron_matches_in_configured_zone, negative_offset_shifts_weekday_boundary, next_fire_time_honours_the_configured_zone}` | 已完成 |
| SCHED-14 | CRON 精确休眠（非轮询）+ 无未来触发置 `COMPLETE` | `AgentQuartzJob.java#execute`（Trigger 到期由 Quartz 调度）；`TriggerState.COMPLETE` | `quartz/job.rs:110-124`；测试 `quartz::tests::cron_loop_fires_on_next_fire_time`、`job::tests::next_fire_time_returns_none_when_years_are_exhausted`、`quartz/task.rs:565 completed_task_is_terminal_and_has_no_next_fire_time` | 已完成 |
| SCHED-15 | 三模式落地（CRON/FIXED_RATE/FIXED_DELAY），FIXED_DELAY 完成后再排下次 | `AgentQuartzJob.java#execute`（FIXED_DELAY → `rescheduleNextFixedDelay`） | `quartz/job.rs:97-182`；集成测试 `tests/quartz_execution.rs::fixed_delay_firing_reaches_factory` | 已完成 |
| SCHED-16 | 初始消息（`inputMsg`）每次触发重放 | `QuartzAgentScheduler.java#schedule`（JobDataMap）、`AgentQuartzJob.java#execute` | `quartz/task.rs:292-299`（`execute()` 重放 `initial_input`）；测试 `tests/quartz_execution.rs::schedule_with_input_delivers_initial_message_to_factory` | 已完成 |
| SCHED-17 | 同名重复注册返回既有任务（Java warn 后返回） | `QuartzAgentScheduler.java#schedule`（`tasks.containsKey`）、`XxlJobAgentScheduler.java#schedule` | `quartz/mod.rs:128-133`、`xxljob/mod.rs:91-93`；测试 `quartz::tests::duplicate_schedule_returns_existing`、`xxljob::tests::duplicate_registration_returns_existing` | 已完成 |
| SCHED-18 | pause / resume / interrupt / status（6 值 TriggerState） | `QuartzAgentScheduler.java#pause/#resume/#interrupt/#getStatus` | `quartz/task.rs:202-249`、`quartz/mod.rs:603-631`；测试 `quartz::tests::{pause_resume_and_status, trigger_state_covers_quartz_states, interrupt_reports_whether_a_run_was_in_flight}`、`quartz/task.rs::{pause_resume_status_transitions, interrupt_aborts_in_flight_execution, interrupt_while_idle_does_not_abort_the_next_run}` | 已完成 |
| SCHED-19 | cancel / shutdown 等待在途执行跑完（不丢弃） | `QuartzAgentScheduler.java#cancel`（`deleteJob` 不中断运行中作业）、`#shutdown`（`shutdown(true)`） | `quartz/mod.rs:338-352`、`:376-390`；`quartz/task.rs:345-380`（`join_with_grace`）；测试 `quartz::tests::{cancel_waits_for_the_in_flight_execution_to_finish, fixed_rate_cancel_waits_for_the_in_flight_execution, shutdown_waits_for_the_in_flight_execution, cancel_waits_for_the_job_loop_to_stop}` | 已完成 |
| SCHED-20 | 配置校验文案逐字对齐（cron 空 / fixedRate ≤0 / fixedDelay ≤0 / name 空 / ModelConfig 空 / `Model cannot be null.`） | `ScheduleConfig.java#validate`、`AgentConfig.java#validate`、`BaseScheduleAgentTask.java#createAgent` | `common/config/schedule.rs:136-152`、`common/config/spec.rs:132-155`；测试 `schedule::tests::{cron_requires_expression, fixed_rate_requires_positive, fixed_delay_requires_positive}`、`spec::tests::agent_config_requires_name_and_model`、`task::tests::missing_model_fails_with_the_java_message` | 已完成 |
| SCHED-21 | 非法规格快速失败且无副作用（不登记半成品任务） | `QuartzAgentScheduler.java#schedule`（异常早于 `tasks.put`） | `quartz/mod.rs:139`（`resolve_cron` 先于 `jobs.insert`）；测试 `quartz::tests::{invalid_cron_fails_without_registering_task, invalid_zone_id_fails_without_registering_task, reject_invalid_fixed_rate}` | 已完成 |
| SCHED-22 | xxl-job 协议面：`XxlJobContext`（jobId/jobParam/分片/logId）+ `XxlJobHelper.log/handleSuccess/handleFail` + 推理步日志 | `XxlJobAgentScheduler.java#executeAgent`、`#schedule`（`PostReasoningEvent` → `XxlJobHelper.log`、`handleSuccess`） | `xxljob/context.rs`（`tokio::task_local!` 替代 ThreadLocal）、`xxljob/log.rs#XxlJobLogMiddleware`（挂 `Middleware::on_reasoning_result`）；测试 `xxljob::tests::{dispatch_logs_each_reasoning_step_through_the_attached_middleware, trigger_passes_job_param_and_reports_success, trigger_reports_handle_fail_on_error, schedule_appends_the_reasoning_log_middleware}`、`context::tests::{helper_resolves_the_scoped_context, fail_code_is_500}`、`log::tests::{reasoning_message_reaches_the_dispatch_log, logging_outside_a_dispatch_is_a_noop}` | 已完成 |
| SCHED-23 | xxl-job `cancel` 不可用语义（Java 原文错误）与仅 `NONE` 模式 | `XxlJobAgentScheduler.java#cancel`、`#schedule`（非 NONE 抛 `UnsupportedOperationException`） | `xxljob/mod.rs:34-36`（`CANCEL_UNSUPPORTED` 逐字）、`:202-210`、`:84-88`（NONE 校验）；测试 `xxljob::tests::{try_cancel_mirrors_java_unsupported_operation, cancel_is_unsupported_shutdown_clears, rejects_non_none_mode}` | 已完成 |
| SCHED-24 | scheduler 注册表（按 id 查回调度器实例） | `QuartzAgentSchedulerRegistry.java#register/#get/#unregister` | `quartz/registry.rs`（`OnceLock<RwLock<HashMap>>` + `Weak` 句柄 + 剪除）；测试 `registry::tests::{register_lookup_unregister, entry_dies_with_the_scheduler_and_is_pruned, singleton_is_stable}`、`quartz::tests::builder_sets_a_deterministic_scheduler_id` | 已完成 |

## 4. 项目状态与下一步
### 4.1 当前状态
- **测试修复完工（2026-09-18）**：`cargo test -p agentscope-extensions-scheduler` 由「lib test 编译失败（4 个错误）」修至**全绿 37 passed / 0 failed**；`cargo build -p agentscope-extensions-scheduler` 通过（仅 7 条 warning，无 error）。
- **静态复核（2026-10-22，未编译）实测**：Rust 14 个 `.rs` / 2416 行 / 37 个测试属性（`#[test]` 19 + `#[tokio::test]` 18），分布：cron 8、quartz/mod 6、xxljob 5、schedule 5、spec 4、task(common) 4、task(quartz) 3、registry 2 → 与「37 passed」自洽。
- **Java 侧实测（静态）**：主 3234 行 / 14 类型，测试 3178 行 / 180 个 `@Test`。**已完成（2026-10-29 勘误）**：原记「Rust 测试数 37 vs Java 180（覆盖率约 21%），且 Rust **无端到端定时触发测试**（无任何测试断言 `fire()` 真的按 cron/fixed_rate 触发过一次；`QuartzAgentScheduler::trigger` 与 `job::spawn_job` 均零测试覆盖）」，实际 2026-10-28 已新增 `tests/quartz_execution.rs`（**3 passed**，真实按 fixed-rate/cron 触发断言）并补 36 个 lib 用例 → **合计 91 passed / 0 failed vs Java 180**（见 history/SCHEDULER__MIGRATION_HISTORY.md）；测试数量差距见 §4.2 第 8 条。
- **改动文件（仅本 crate）：**
  1. `src/quartz/mod.rs`：测试模块补 `use crate::common::config::ScheduleMode;`（修 `E0433: cannot find type 'ScheduleMode'`；类型本存在于 `common/config/schedule.rs:13`，未新建平行 enum）。
  2. `src/common/scheduler/task.rs` / `src/quartz/task.rs` / `src/xxljob/mod.rs`：`Msg::get_text_content()` → `Msg::text_content()`（3 处，修 `E0599`；正确 API 见 `agentscope-core/src/message/msg.rs:260`），其余逻辑未动。
  3. `src/quartz/cron.rs`：修 step 语法解析 bug——`parse_field` 对「裸 `a/step`（无范围）」原按单值 `(a,a)` 处理，导致 `0 0/30 * * * ?` 只匹配第 0 分钟，取数 `every_thirty_minutes` 断言失败；现按 cron/Quartz 语义展开为 `a..max`（step>1 时），`a` 仍为单值。
     - **静态复核补充（2026-10-22）**：该修复对 `step>1` 成立，但**遗留同类 bug**——`a/1`（step=1）仍走 `(v,v)` 单值分支，而标准 cron / Quartz 中 `0/1` 等价于「从 0 起每分钟」，应为 `(0,max)`。当前 `0/1` 只匹配第 0 分钟。`*/1` 因走 `range_part=="*"` 分支不受影响。
- **验证命令与结果：**
  - `cargo build -p agentscope-extensions-scheduler` → 成功（0 error）。
  - `cargo test -p agentscope-extensions-scheduler` → `test result: ok. 37 passed; 0 failed`（lib）+ doc-tests 0 failed。
- 未删除任何测试或功能；未改动根 `Cargo.toml`、core/harness 及其它 crate。

- **cron 语义对齐（2026-10-26）**：4 项 P1 语义分叉修复（星期基准 1-7、dom/dow 冲突报错、非法 cron 快速失败且无副作用、`zone_id` 生效）。新增文件 `src/common/config/zone.rs`；新增 `job::CronPlan` / `quartz::resolve_cron`；`cron::CronExpression::matches`/`next_after` 泛型到任意 `TimeZone`。测试名单与「改前如何失败」见 history/SCHEDULER__MIGRATION_HISTORY.md。注意：**行为变更** —— `0 0 9 * * MON-FRI` 这类两侧都指定的 6 字段表达式现在会报错（必须写 `0 0 9 ? * MON-FRI`），调用方与示例需同步。

- **2026-10-28 执行（7 项任务全部落地，见 history/SCHEDULER__MIGRATION_HISTORY.md 详表）**：cron `L`/`L-n`/`LW`/`nW`/`nL`/`n#k` + 7 字段年 + `a/1` + `?` 严格化；`next_after` 接入生产循环 + 查询 API；`interrupt()` 中止 in-flight；`TaskStatus` 6 值 + `is_terminal`/`as_str` + `trigger_state`；xxl-job `XxlJobContext`/`XxlJobHelper`/`handleSuccess`/`handleFail`/分片参数/日志 id；`cancel` 语义（quartz join 等待 + xxl-job `try_cancel` 原文报错）；`execution_count()`/`schedule_config()`/`agent_config()` 公开。
  - 实测：`cargo test -p agentscope-extensions-scheduler` → lib **88 passed / 0 failed**、`tests/quartz_execution.rs` **3 passed / 0 failed**、doc-tests 0 → 合计 **91 passed / 0 failed**（基线 52+3=55 全绿保留，新增 36）。
  - `cargo clippy -p agentscope-extensions-scheduler --all-targets`：本 crate 仅剩 2 条既有 warning（`module_inception`、`very_complex_type`）；本轮删除了 `next_seq` / `AsAny` / `interrupt_notify` / `JobContext::run_task` 四个死脚手架。
  - 未新增任何依赖（未引入 `chrono-tz`，未用 `tokio-util`；interrupt 用 `tokio::sync::watch` + `select!`）。

- **2026-10-31 loop 收口（8 项 = 原 6 项 + §3/见 history/SCHEDULER__MIGRATION_HISTORY.md 遗留 2 项，见 history/SCHEDULER__MIGRATION_HISTORY.md）**：core 新增推理产出 hook（additive）；`XxlJobLogMiddleware` + `RuntimeAgentConfig::with_middleware`；registry 改弱引用句柄反查；`run_with` 补 model 校验（`Model cannot be null.`）；`shutdown`/`cancel` 改「先等 in-flight 再退出循环」；删 `agentscope-harness`、`futures` 降 dev-dep；补 Builder/`schedule_default`。
  - **测试**：基线 lib 88 + 集成 3 = 91；本轮新增 **12** 个 scheduler lib 用例 → **实测 lib 100 + 集成 3 = 103**（另 core 新增 2 个用例，计入 core crate；见 history/SCHEDULER__MIGRATION_HISTORY.md）。
  - 未删除任何既有测试/断言；两处断言**语义更正**并写明理由：`cancel_aborts_the_in_flight_execution` → `cancel_waits_for_the_in_flight_execution_to_finish`（Java `deleteJob` 不中断运行中作业）；xxl-job `reply:hello` 日志断言 → 改为「调度器不再写最终回复日志，由 Hook 记录推理步」（见 history/SCHEDULER__MIGRATION_HISTORY.md）。

- **2026-10-31 复核裁定落地（4 项：SCHED-01/05/06/09）**：
  - `SCHED-01`（P0）：`AgentFactory::resolve_model(&RuntimeAgentConfig) -> Option<Arc<dyn Model>>`（默认 `cfg.model().cloned()`）；`run_with` 改为「配置与工厂都无 model 才报 `Model cannot be null.`」，并把工厂解析出的 model 注入传给 `run` 的配置（`src/common/scheduler/task.rs:39`、`:167-183`）。测试：`common::scheduler::task::tests::factory_resolved_model_is_accepted_and_handed_to_run`、`quartz::tests::schedule_default_runs_with_a_factory_resolved_model`（`schedule(AgentConfig{model_name:"qwen-max"}, NONE)` + 宿主 factory → 执行 1 次、`execution_count()==1`）。
  - `SCHED-05`（P1）：文档声明单实例前提（`src/lib.rs` `# Deployment premise` + `src/common/scheduler/lock.rs`）；新增 `SchedulerLock`（默认 `NoopSchedulerLock`）经 `QuartzAgentSchedulerBuilder::scheduler_lock` 注入，`quartz/job.rs::fire` 每次自动触发前 `try_acquire`、完成后 `release`。测试：`quartz::tests::{injected_scheduler_lock_gates_every_firing, a_denied_slot_skips_the_execution}`、`common::scheduler::lock::tests::noop_lock_grants_every_slot`。
  - `SCHED-06`（P1）：`src/xxljob/bridge.rs` 固化宿主桥接契约 `ExecutorBridge` + `DispatchRequest`→`XxlJobContext` 字段映射 + `ExecuteResponse`（200/500），`XxlJobAgentScheduler` 实现；registry/beat/log 纯宿主侧（模块文档表格给出六端点映射），不内置 HTTP server。测试：`xxljob::bridge::tests` 四个用例。
  - `SCHED-09`（P2）：`Arc<Semaphore>` 并发上限，默认 10，builder 可配；`fire` 执行前取 permit。测试：`quartz::tests::concurrency_limit_bounds_simultaneous_executions`（可控 gate + `watch`，无 sleep 竞态）。
  - 未新增依赖（仍为 core/serde/serde_json/thiserror/async-trait/tokio/tracing/chrono/uuid）；未改 core/harness/其它 crate；未删测试/未放宽断言。本轮为本 crate 新增 **10 个** lib 用例（task 1、lock 1、quartz 4、bridge 4，测试名见上）。

### 4.2 下一步与阻塞（2026-10-31 全部收口）

> 原 1–8 条全部关闭；剩余无未关闭条目。裁定依据见 history/SCHEDULER__MIGRATION_HISTORY.md。

| # | 原条目 | 状态 | 落点 / 依据 |
|---|---|---|---|
| 1 | P0 `with_factory` 未接线 | 已完成 | 2026-10-22 见 history/SCHEDULER__MIGRATION_HISTORY.md；`factory` 已非 dead field |
| 2 | `next_after` 生产零调用 | 已完成 | 2026-10-28 见 history/SCHEDULER__MIGRATION_HISTORY.md：`CronPlan::next_fire_time` 驱动精确休眠 + 查询 API |
| 3 | 未使用依赖 `agentscope-harness` / `futures`；`TaskControl` 脚手架 | 已完成 | 2026-10-31：删除 `agentscope-harness`（0 引用），`futures` 降为 `[dev-dependencies]`（仅测试桩模型需要）；`TaskControl` **保留**为公共控制面抽象（`pause/resume/interrupt` 三方法，后端可自选实现），是有意保留的 API 而非死代码 |
| 4 | cron 剩余（DST / `Calendar` / `misfire` / `nW`+列表混写） | 已完成（设计决定，不迁移） | 见 history/SCHEDULER__MIGRATION_HISTORY.md 逐项依据与替代 |
| 5 | xxl-job `PostReasoningEvent → XxlJobHelper.log` 无挂点 | 已完成 | 2026-10-31：core 新增 `Middleware::on_reasoning_result` + `ExtensionPoint::OnReasoningResult` + `MiddlewareChain::notify_reasoning_result`（`middleware.rs`），调用点在 `react_agent.rs::run_reasoning`；crate 侧 `XxlJobLogMiddleware` 由 `schedule_runtime` 经 `with_middleware` 注入（见 history/SCHEDULER__MIGRATION_HISTORY.md） |
| 6 | `cancel` 返回 bool（Java 抛异常） | 已完成 | 2026-10-28 `try_cancel`；2026-10-31 补充：`cancel` 语义改为「停循环 + 等在途执行跑完」（对齐 `deleteJob` 不中断运行中作业） |
| 7 | `shutdown` 用 `abort()` 丢弃 in-flight | 已完成 | 2026-10-31：`QuartzScheduleAgentTask::join_with_grace`（默认 30s，超时才 abort 防死等）；`job.rs` FIXED_RATE 分支把 `fire` 移出 `select!`（否则 cancel 会丢弃在途执行）；测试 `shutdown_waits_for_the_in_flight_execution`、`cancel_waits_for_the_in_flight_execution_to_finish`、`fixed_rate_cancel_waits_for_the_in_flight_execution` |
| 8 | 2026-10-29 剩余 6 项（日志 Hook / registry 句柄 / model 校验 / shutdown / 未用依赖 / 测试 91 vs 180） | 已完成 | 前 5 项同上；测试项见 §4.1 与 history/SCHEDULER__MIGRATION_HISTORY.md 映射表 |
| — | `RuntimeAgentConfig.middleware` 不可变（无法复现 Java hooks 追加） | 已完成 | 2026-10-31 新增复制构造 `with_middleware` / `with_model`（`src/common/config/spec.rs`），测试 `common::config::spec::tests::with_middleware_returns_a_copy_and_keeps_the_original` |
 
### 4.3 与 Java 的已知差异（有意为之，非缺口）

> 本小节为 2026-10-31 复核归并结果：原散落在 §2/§3 与 `history/SCHEDULER__MIGRATION_HISTORY.md` 的差异条目在此汇总为两表，原 §3.1–§3.4 文字保持原样。

#### 4.3.1 设计决定（不迁移）

| 编号 | 项 | Java 参考 | 不迁移理由（源码级依据） | 影响面 |
|---|---|---|---|---|
| SCHED-D01 | DST 时区（`Europe/London` 等含夏令时的 IANA 区） | `QuartzAgentScheduler.java#schedule`（`csb.inTimeZone(TimeZone.getTimeZone(zoneId))`，JVM 自带全量 tzdb） | 未引入 `chrono-tz`（`Cargo.toml` 依赖仅 core/serde/serde_json/thiserror/async-trait/tokio/tracing/chrono/uuid），`common/config/zone.rs` 只支持 UTC 别名 + 固定偏移 + 免 DST 白名单，DST 区**显式报错**（`zone::tests::rejects_unknown_and_dst_zones`）。引入 tzdb 会带来数百 KB 体积与数据更新问题，收益不抵成本 | 使用 DST 时区的 cron 表达式不可调度；中国区（UTC+8）、UTC、固定偏移场景不受影响 |
| SCHED-D02 | Quartz `Calendar`（节假日排除）、自定义 `Scheduler` 注入、`autoStart` | `QuartzAgentScheduler.java#Builder`（`scheduler(Scheduler)`、`autoStart(boolean)`）；`Scheduler#addCalendar` | 无 Quartz 依赖、无全局 scheduler 生命周期（每任务一个 tokio 任务，随 spawn 生效），`autoStart` 无对应语义；`QuartzAgentSchedulerBuilder` 只保留 `scheduler_id`（`quartz/mod.rs:257-296` 注释即此依据）。Calendar 可在外层用「cron + 代码内日历判断」实现 | 无法用 Quartz 原生日历批量排除日期；如需请勿依赖本 crate |
| SCHED-D03 | Quartz JDBC store 持久化与 `loadTaskFromQuartz` 集群回读 | `QuartzAgentScheduler.java#loadTaskFromQuartz`（从 Quartz store 重建 `QuartzScheduleAgentTask`）、`QuartzAgentSchedulerRegistry` + `JobDataMap.storeDurably(true)` | Rust 侧无共享 Quartz store，任务为进程内 `RwLock<HashMap>`（`quartz/mod.rs:337-345`），也无 `Task` 反序列化契约；`rg 'loadTaskFromQuartz\|load_task' src` 零命中。跨节点读取的净收益仅为「运维可见性」，需先有共享存储与稳定 schema（见 SCHED-05 待裁定） | 进程重启后调度任务丢失，需宿主重建；跨节点任务不可见 |
| SCHED-D04 | Quartz `JobKey`/Trigger 类型/`JobDataMap` 与 `inputMsg` 的 JSON 序列化（含反序列化失败时 `setUnscheduleFiringTrigger(true)`） | `QuartzAgentScheduler.java#schedule`（`JsonUtils.toJson(input)` 入 JobDataMap）、`AgentQuartzJob.java#execute`（`fromJson`，失败 `setUnscheduleFiringTrigger(true)`） | Rust 初始消息直接以 `Msg` 结构体存放（`quartz/mod.rs:135-147`、`quartz/task.rs:96-99`），不经过 JSON，故既无序列化失败路径也无「取消触发器」分支；身份用 `name` 单键（无 group 概念），`TaskStatus`/`next_fire_time` 已覆盖可观测需求 | 无跨语言 JobDataMap 兼容；`JobKey`/group 相关 API 不可用（`getJobKey()` 无对应，见 §2.2） |
| SCHED-D05 | xxl-job `XxlJobExecutor` 生命周期（`destroy()`）与版本自适应反射注册 | `XxlJobAgentScheduler.java#shutdown`（`executor.destroy()`）、`#registerJobHandler`（`registryJobHandler`，失败即抛） | Rust 无 executor 对象，`XxlJobAgentScheduler` 只有 `handler` 表，`shutdown()` 清表 + 置位（`xxljob/mod.rs:238-246`）；`rg 'destroy\|registryJobHandler' src` 零命中 | 无跨进程执行器注销（本就没有注册，见 SCHED-06）；宿主需自行管理执行器生命周期 |
| SCHED-D06 | `DashScopeModelConfig`（apiKey/modelName/stream/enableThinking/baseUrl + `createModel()`） | `config/DashScopeModelConfig.java`（5 字段 builder，`createModel()` 造 DashScope 模型） | DashScope provider 属 MODEL 模块且未迁移，本 crate 以 `StaticModelConfig`（包裹已构造的 `Arc<dyn Model>`）代替（`common/config/spec.rs:24-52`）；`rg 'DashScope' crates/agentscope-extensions-scheduler` 零命中 | 模型构造下移到宿主；`schedule(AgentConfig)` 因此无 model，见 SCHED-01 |

#### 4.3.2 设计决定与已知差异

| 编号 | 项 | Rust 实现 | 与 Java 的差异 | 为何保留（收益/依据） |
|---|---|---|---|---|
| SCHED-X01 | quartz 后端实现方式（等价性证据：三模式/misfire/并发/选主） | `quartz/job.rs`（tokio 循环）+ `quartz/cron.rs`（手写 Quartz 兼容解析器，904 行） | Java 用 quartz-scheduler 的 Job/Trigger/线程池；Rust 每任务一个 tokio task，CRON 睡到 `next_fire_time`、FIXED_RATE 用 interval、FIXED_DELAY 完成后再 sleep。**等价性**：三模式语义与 Java 一致、单任务串行（`fire` 内 await，与 Java 每 trigger 不并发执行一致）、misfire 差异已单列 SCHED-03；**选主/分布式锁两侧均无内置**——Java 依赖外部 Quartz 集群配置，本 crate 默认 `NoopSchedulerLock`（单实例前提），多实例由宿主注入 `SchedulerLock`（SCHED-05） | 免去 C 绑定/JVM 语义依赖，依赖面最小；解析器已用 30 条单测对齐 Quartz 语法 |
| SCHED-X02 | 调度器注册表存 `Weak` 句柄并在查找时剪除 | `quartz/registry.rs:19-46`（`RegistryEntry{id, scheduler_type, handle: Weak<..>}`）、`:101-112`（`get_handle` 升级失败即剪除死条目） | Java `ConcurrentHashMap<String, QuartzAgentScheduler>` 强引用，条目只能显式 `unregister`；Java 无「僵尸条目」场景 | 避免 registry 把已 drop 的调度器永久钉在内存里（Java 靠 `shutdown()` 的 finally 保证，漏调即泄漏）；`scheduler_type` 另修正为真实类型 `"quartz"` |
| SCHED-X03 | `TaskStatus` 7 值与 `Blocked` 语义 | `quartz/task.rs:36-70`（`None/Normal/Paused/Complete/Error/Blocked/Cancelled` + `as_str`/`is_terminal`） | Java 直接返回 Quartz `TriggerState`（6 值）。差异：①多 `Cancelled`（Java 取消后 job 被删，状态读 `NONE`）；②Rust `Blocked` = 「当前有在途执行」，而 Java 的 `BLOCKED` 仅在 `@DisallowConcurrentExecution` 时出现——`AgentQuartzJob` 无该注解，故 Java 实际**永不**返回 `BLOCKED` | 观测面更有信息量（在途/已取消可区分），不改变调度行为；`trigger_state` 对未知任务返回 `None` 而非 Java 的 `null`，避免 `Option` 双层 |
| SCHED-X04 | `shutdown`/`cancel` 的宽限期（30s）后 abort | `quartz/task.rs:348-380`（`SHUTDOWN_GRACE = 30s`，`join_with_grace` 超时才 `abort()`） | Java `scheduler.shutdown(true)` / `deleteJob` 无限等待在途作业 | 防「provider 挂死导致 shutdown 永不返回」；正常路径仍先等在途跑完，与 Java 可观测行为一致（仅极端超时差异） |
| SCHED-X05 | `interrupt` 返回值与实现机制；执行计数时机 | `quartz/task.rs:250-265`（`watch::Sender<u64>` 递增 + `notify`）、`:311-346`（`select!` 竞争，`Error::Interrupted`）、计数在 `common/scheduler/task.rs:145-150`（`run_with` 返回后 `+1`） | ①Java `scheduler.interrupt(JobKey)` 只投递中断、方法本身恒返回 `true`（代码忽略返回值），Rust 返回「是否真有在途执行」；②Java `BaseScheduleAgentTask#run` 在 `agent.call(...)`（冷 Mono）**返回时**即 +1，Rust 在执行结束后 +1；两者都做到「失败也计数」 | 真实 in-flight 反馈便于调用方判断；计数移到完成后可反映真实完成次数（Java 的提前计数在 Mono 未订阅时也会 +1，属隐患） |
| SCHED-X06 | xxl-job 执行计数与失败上报 | `xxljob/mod.rs:151-180`：`task.run(...)` 单次计 + 失败调 `handle_fail` | Java `XxlJobAgentScheduler#executeAgent` 先 `scheduleAgentTask.run(inputMsg).block()`（内部已 +1）再显式 `incrementExecutionCount()` → **每次执行双计**；失败路径只 log + 抛 `RuntimeException`，不调 `handleFail` | 修正 Java 双计 bug；显式 `handleFail` 让 admin 侧拿到 500 结果（`XxlJobHandleCode::Fail.code()==500`，测试 `context::tests::fail_code_is_500`、`trigger_reports_handle_fail_on_error`） |
| SCHED-X07 | 非法 `zone_id` 的处理 | `common/config/zone.rs#resolve_zone` → `ScheduleZoneError`，`quartz/mod.rs:139` 解析期报错 | Java `TimeZone.getTimeZone(id)` 对未知 id **静默回落 GMT**，任务照常注册但按错误时区触发 | 快速失败优于静默错时区（同 SCHED-21 的「无副作用失败」原则） |
| SCHED-X08 | 执行入口抽象与错误类型 | `AgentFactory`（`common/scheduler/task.rs:17-22`）注入执行；Java 异常 → `Error::Agent(<Java 文案>)`，如 `Model cannot be null.` | Java `BaseScheduleAgentTask#createAgent` 直接 `ReActAgent.builder()` 造 agent（`IllegalArgumentException` 抛给调用方）；Rust 把「造 agent + 跑一次」整体委托给工厂，校验失败以 `Err(Error::Agent(..))` 返回而非 panic | 让 crate 不依赖具体 agent 类型、可在测试/宿主替换（`tests/quartz_execution.rs` 即用 `RecordingFactory` 断言语义）；`Model cannot be null.`/`Parameter configuration error: ...` 文案逐字保留，便于跨语言日志比对 |

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-scheduler` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-scheduler` / `cargo test -p agentscope-extensions-scheduler` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-scheduler/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. 执行历史（已归档）

- 执行记录 / 静态复核 / 勘误 / 反向验证明细：[`history/SCHEDULER__MIGRATION_HISTORY.md`](./history/SCHEDULER__MIGRATION_HISTORY.md)
- 本文件只保留现行状态（§0–§5）；历史条目一律写进上述归档文件，不再回填本文。
