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
 
## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-scheduler` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-scheduler` / `cargo test -p agentscope-extensions-scheduler` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-scheduler/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. 执行历史（已归档）

- 执行记录 / 静态复核 / 勘误 / 反向验证明细：[`history/SCHEDULER__MIGRATION_HISTORY.md`](./history/SCHEDULER__MIGRATION_HISTORY.md)
- 本文件只保留现行状态（§0–§5）；历史条目一律写进上述归档文件，不再回填本文。
