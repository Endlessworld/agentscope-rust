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
| 当前状态 | **总判：进行中**（仍有 P1：PostReasoningEvent 挂点 / registry 句柄）—— 进行中（2026-10-28：7 项执行任务已全部落地，cron/中断/状态机/xxl-job 协议/cancel 语义对齐；`cargo test` 91 passed / 0 failed，见 §8） |

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
| `config.RuntimeAgentConfig` | 追加运行时对象（model/toolkit/hooks） | `common::config::RuntimeAgentConfig` | Rust 侧用 `Arc<dyn Model>` + `Toolkit` + `Vec<Arc<dyn Middleware>>`（Rust 无 hooks，走 middleware）；**继承改为内嵌**（Rust 无类继承） |
| `config.ModelConfig`（interface） | getModelName + createModel | `common::config::ModelConfig`（trait） | `create_model() -> Arc<dyn Model>`（Java `Model` → core `Model` trait） |
| `config.DashScopeModelConfig` | DashScope 模型配置实现 | `common::config::StaticModelConfig`（占位实现，`src/common/config/spec.rs`） | Rust 无 DashScopeChatModel（属 MODEL 模块，未迁移）；实际提供的是 `StaticModelConfig`（包裹已构造的 `Arc<dyn Model>`），**不存在名为 `ModelSpec` 的类型**，见 §4.2 |
| `config.ScheduleConfig` | 调度参数（mode/cron/fixedRate/fixedDelay/initialDelay/zoneId），builder + 按 mode 校验 | `common::config::ScheduleConfig` | serde 往返；ms 语义对齐（Java Long ms → Rust `Option<u64>`） |
| `config.ScheduleMode` | 枚举 NONE/CRON/FIXED_RATE/FIXED_DELAY | `common::config::ScheduleMode` | 变体名天然一致（`NONE`/`CRON`/...），**未使用任何 `#[serde(rename)]`**（grep 命中 0），靠变体同名而非显式 rename 对齐 |
| `quartz.AgentQuartzJob` | Quartz Job：查 scheduler → 取 task → run（FIXED_DELAY 完成后重排） | `quartz::job::spawn_job`（**模块私有，无公开 `AgentJob` 类型**） | 定时循环体内执行；FIXED_DELAY 在 run 完成后 sleep 再排下次。**勘误：不存在 `quartz::AgentJob`**，`job` 为 `mod job;`（私有），仅导出 `spawn_job` |
| `quartz.QuartzAgentSchedulerRegistry`（package-private） | schedulerId → scheduler 静态注册表 | `quartz::SchedulerRegistry` | 进程内 `RwLock<HashMap>` 单例；Rust 无 JVM 静态，用 `OnceLock`。**偏差：Java 存 `scheduler` 引用（Job 可回查执行），Rust 只存 `RegistryEntry{id, scheduler_type}` 字符串，无反向查找能力** |
| `quartz.QuartzScheduleAgentTask`（package-private） | 包装 JobKey + Quartz Scheduler，pause/resume | `quartz::QuartzScheduleAgentTask` | 包装 `JoinHandle` + `AtomicBool` 控制位 + 2 个 `Notify`；pause/resume/**无 reschedule**（Java 的 `rescheduleNextFixedDelay` 在 Rust 中由 job 循环 sleep 替代） |
| `xxljob.XxlJobAgentScheduler` | XXL-Job 实现：注册 JobHandler（仅支持 NONE）、由控制台触发、cancel 抛 Unsupported | `xxljob::XxlJobAgentScheduler` | Rust 无 xxl-job SDK；提供 **JobHandler 注册表 + 本地触发 `trigger(name, params)`**，对齐「只注册 handler、调度由外部控制台决定」语义。**cancel 语义不等价**（Java 抛异常，Rust 返回 false）；无日志 Hook 注入 / 无 `handleSuccess` 回调，见 §6 |

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| reactor-core (Mono/Flux) | 异步结果 | `tokio` async/await（`async fn -> Result<Msg>`） | 已在本 crate Cargo.toml 声明 |
| quartz-scheduler | 调度引擎 | 自研 `quartz::cron::CronExpression`（`src/quartz/cron.rs`，293 行）+ `tokio::time`；**不引 Quartz C 绑定** | 勘误：**未使用** `tokio-cron-scheduler`（`Cargo.toml` 无此依赖，代码中 `tokio_cron` 引用数 = 0）；cron 为手写解析器，支持面见 §6 |
| xxl-job-core | 分布式调度 | 无 SDK；自建 `JobHandlerRegistry` + 本地 `trigger` | JobHandler 名 = agent name |
| jackson-databind | JSON 序列化（inputMsg 存 JobDataMap） | `serde` / `serde_json` | 已声明 |
| slf4j | 日志 | `tracing` | 已声明 |
| java.util.UUID | 任务 id | `uuid`（workspace v1） | 勘误：`uuid = { workspace = true }` **已在** `Cargo.toml:19` 声明，无需再补 |
| java.time / TimeZone | zoneId | `chrono`（已声明） | **已修复（2026-10-26，§6.1）**：新增 `common/config/zone.rs`（`resolve_zone` → `chrono::FixedOffset`）+ `ScheduleConfig::zone_offset()`，`quartz::job::CronPlan::matches_at` 按配置时区求值。**限制**：未引入 `chrono-tz`，只支持 UTC 别名 / 固定偏移 / 免 DST 的 IANA 白名单；`Europe/London` 等 DST 区报明确错误 |
| agentscope-core (Model/Toolkit/Hook/ReActAgent/Msg) | 核心类型 | `agentscope-core`（已声明） | `Model`/`Toolkit`/`Msg`/`ReActAgent` 直接复用 |
| agentscope-extensions-model-dashscope | DashScopeChatModel | **无对应**（MODEL 模块未迁移该 provider） | 依赖注入 `Arc<dyn Model>`，见 §4.2；`RuntimeAgentConfig::model` 为 `Option`，缺失时 `BaseScheduleAgentTask::run_with` **只校验 name/sysPrompt，不校验 model**（Java `createAgent` 会抛 `Model cannot be null`） |

### 2.2 Java 类清单逐项核对（3 个子模块，14 个主代码类型）

**scheduler-common（9 个类型）**
| Java 类 | Rust 是否存在 | 行为覆盖 |
|---|---|---|
| `AgentScheduler` | ✅ trait | 6 个方法齐全；Java 的 `schedule(a,b,Msg)` 默认抛 Unsupported → Rust `schedule_with_input` 默认同语义 |
| `ScheduleAgentTask<T>` | ✅ trait | run/cancel/getId/getName 齐全；Java 无 `as_any`/`run_empty` |
| `BaseScheduleAgentTask` | ✅ struct | 见 §2.0 表格；校验缺口为 model 非空 |
| `config.AgentConfig` | ✅ struct | 字段少 `ModelConfig` 对象（降级为 `model_name` 字符串），serde 字段名不兼容 |
| `config.RuntimeAgentConfig` | ✅ struct | toolkit/middleware/model 齐全；**Java 的 `hooks` 为可变 `CopyOnWriteArrayList`（xxl-job 依赖此可变性追加日志 Hook），Rust 为不可变 `Vec`，无法复现该模式** |
| `config.ModelConfig` | ✅ trait | `model_name` + `create_model` 对齐 |
| `config.DashScopeModelConfig` | ⚠️ 替代为 `StaticModelConfig` | 无 DashScope 语义（无 apiKey/modelName/stream 等字段）；Java 侧 15 个 `@Test`，Rust 侧 0 个对应测试 |
| `config.ScheduleConfig` | ✅ struct | 6 字段齐全；Java 19 个 `@Test`，Rust 5 个 |
| `config.ScheduleMode` | ✅ enum | 4 变体对齐；Java 6 个 `@Test`，Rust 0 个独立测试（仅散落在 schedule 测试中） |

**scheduler-quartz（4 个类型）**
| Java 类 | Rust 是否存在 | 行为覆盖 |
|---|---|---|
| `QuartzAgentScheduler`（657 行 / 26955B） | ✅ struct | 缺：Builder（`autoStart`/`schedulerId`/自定义 `Scheduler`）、`schedule(AgentConfig)` 单参便捷法、`loadTaskFromQuartz` 集群回读、`rescheduleNextFixedDelay` 公开能力、真实 interrupt |
| `AgentQuartzJob` | ⚠️ 仅 `job::spawn_job`（私有） | 缺 `InterruptableJob` 语义（interrupted 标志）、`inputMsg` JSON 反序列化、反序列化失败时 `setUnscheduleFiringTrigger(true)` |
| `QuartzAgentSchedulerRegistry` | ✅ struct | 缺「id → scheduler 句柄」映射（见 §2.0 表格） |
| `QuartzScheduleAgentTask` | ✅ struct | 缺 `getJobKey()` 等价物（Rust 无 JobKey 概念，可接受） |

**scheduler-xxl-job（1 个类型）**
| Java 类 | Rust 是否存在 | 行为覆盖 |
|---|---|---|
| `XxlJobAgentScheduler`（363 行 / 15762B） | ✅ struct | 见 §2.0 表格与 §8；**协议层已于 2026-10-28 补齐**：`XxlJobContext`（job id / job param / 分片 / 日志 id）+ `XxlJobHelper`（`log`/`handleSuccess`/`handleFail`）+ `try_cancel` 报错。仍无 executor `destroy()`（Rust 无 executor，设计决定）；`PostReasoningEvent` 日志 Hook 降级为「记录最终回复」（core 无 post-reasoning 挂点，§4.2-5） |

## 3. 详细缺口清单
### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 依赖（uuid, tokio-cron-scheduler/chrono, async-trait 等） | pom.xml ×3 | 已完成 | `Cargo.toml` 已含 core/harness/serde/serde_json/thiserror/async-trait/tokio/futures/tracing/chrono/uuid。实测 `tokio_cron` 引用 0、`futures::` 引用 0、`agentscope_harness` 引用 0 → `futures`/`agentscope-harness` 为未使用依赖（可裁剪，非阻塞） |
| `common` 共享层骨架（本 crate 内） | scheduler-common | 已完成 | 实为 7 个文件而非 4：`src/common/{mod,config/mod,config/schedule,config/spec,scheduler/mod,scheduler/scheduler,scheduler/task}.rs`（`config/spec.rs` 承载 AgentConfig+RuntimeAgentConfig+ModelConfig+StaticModelConfig） |
| ScheduleMode / ScheduleConfig | 同名 | 已完成 | serde + builder + 按 mode 校验；5 个 Rust 单测（非「6 个 Java 测试对应」，Java ScheduleConfigTest 实为 19 个 `@Test`） |
| AgentConfig / RuntimeAgentConfig / ModelConfig | 同名 | 已完成 | RuntimeAgentConfig 内嵌 AgentConfig；注意 **AgentConfig 无 `description`/`agentFactory` 字段**（Java 已无 description，doc 注释残留），`model_name: String` 取代 `ModelConfig` 对象 |
| AgentScheduler / ScheduleAgentTask trait | 同名 | 已完成 | async_trait；泛型 T→Msg；额外提供 `run_empty()` 便捷方法与 `as_any()` 下转型钩子 |
| BaseScheduleAgentTask | 同名 | 已完成 | 每次执行新建 agent（经 `AgentFactory`）+ 计数 + cancel；**偏差：`run_with` 只校验 name/sysPrompt，不校验 model 非空**（Java 抛 `Model cannot be null.`）——2026-10-29 复核仍存在（§4.2 第 8 条） |

### 3.2 quartz 后端（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| QuartzAgentScheduler schedule/cancel/getAll/shutdown/getSchedulerType | QuartzAgentScheduler(26955B) | 已完成 | tokio 定时循环 + 手写 cron 解析。**缺口：无集群回读**（Java `loadTaskFromQuartz` 从 Quartz store 重建任务；Rust 仅进程内 `HashMap`，属设计决定但 §2 表格「集群回读」表述需删） |
| CRON / FIXED_RATE / FIXED_DELAY 三模式 | 同名 | 已完成 | 三模式均已实现。cron 4 项 P1 语义于 2026-10-26 对齐（§6.1）；**Quartz 扩展关键字与年字段于 2026-10-28 落地（§8）**：`L`/`L-n`/`LW`/`nW`（dom）、`L`/`nL`/`n#k`（dow）、7 字段年（1970-2199）、`a/1` 展开、`?` 仅限日字段。`next_after` **已接入生产循环**（`CronPlan::next_fire_time` → 精确休眠，替换 1s 轮询），并补查询 API `QuartzAgentScheduler::next_fire_time` / `QuartzScheduleAgentTask::next_fire_time`（对齐 `Trigger#getNextFireTime`）。余额：无（DST 时区仍不支持，见 §4.2） |
| pause / resume / interrupt / getStatus | 同名 | 已完成 | **2026-10-28 补齐**：`interrupt()` 经 `tokio::sync::watch` 中止 in-flight 执行（`execute_with` 内 `select!` 竞争，回落 `Error::Interrupted`），FIXED_DELAY 被中断时跳过重排（对齐 `AgentQuartzJob:49/107`）；返回「是否真有 in-flight」对齐 `Scheduler#interrupt`。`TaskStatus` 扩为 Quartz 6 值 + Rust 专用 `Cancelled`（`None/Normal/Paused/Complete/Error/Blocked/Cancelled`，新增 `as_str`/`is_terminal`），并新增 Java 同形的 `trigger_state(name) -> TaskStatus`（未知任务 = `None`）。`get_status` 保留 `Option<TaskStatus>` |
| SchedulerRegistry | QuartzAgentSchedulerRegistry | 部分实现 | `OnceLock<RwLock<HashMap>>`；登记值为 `scheduler_id` 字符串（`self_handle_hint`），**不持有 scheduler 句柄**，无法反向查找（Java 存 `QuartzAgentScheduler` 实例）。2026-10-28 删除了原 `JobContext::run_task`（其 `contains` 探测结果为 `let _` 丢弃，纯装饰）→ 注册表现阶段只用于 `contains/get/len` 与测试可见性 |
| QuartzScheduleAgentTask | 同名 | 已完成 | P0（`with_factory` 未接线）已于 2026-10-22 修复（§7）；**2026-10-28 补 `schedule_config()` / `agent_config()`**（Java `getScheduleConfig`/`getAgentConfig`，§7.4 遗留），并持有 `CronPlan` 供 `next_fire_time` 查询。`cancel` 现在 `shutdown()` + `join().await`（`join` 不再是死代码），`cancel` 返回后 job 循环与 in-flight 执行均已终止 |
| AgentJob | AgentQuartzJob | 已完成 | 循环体 + `inputMsg` 已接（P0-2，§7）；2026-10-28 起 CRON 分支改为「睡到 `next_fire_time`」而非按秒轮询，无未来触发时置 `Complete`；interrupt 语义按 Java 短路 |

### 3.3 xxl-job 后端（P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| XxlJobAgentScheduler（JobHandler 注册 + 本地 trigger） | XxlJobAgentScheduler(15762B) | 已完成 | 仅 NONE 模式；**2026-10-28 补齐协议层（§8）**：本 crate 新增 `xxljob::context` — `XxlJobContext`（job_id / job_param / shard_index+total / log_id / 日志与 handle 结果）+ `XxlJobHelper`（`log` / `handle_success` / `handle_fail` / `get_job_param` / `get_job_id` / `get_shard_index` / `get_shard_total` / `get_log_id`），用 `tokio::task_local!` 替代 Java 的 ThreadLocal；`trigger_with_context(name, Arc<XxlJobContext>)` 在上下文作用域内执行：`getJobParam()` → `Msg::user`，成功后 `log(reply)` + `handleSuccess`，失败 `handleFail`。**已知差异（§4.2）**：Java 的 `PostReasoningEvent` 日志 Hook 无对应（Rust core 未向 middleware 暴露「推理产出」事件），改为记录最终回复；`cancel` 见下行 |
| JobHandler 注册表 | XxlJobExecutor.registryJobHandler | 已完成 | 进程内 `RwLock<HashMap<String, JobHandler>>`；**无 executor 生命周期**（设计决定：Rust 无 executor，`shutdown()` 清 map）；注册失败无异常路径（纯内存 insert 不会失败，属设计差异）。**cancel 语义（2026-10-28）**：新增 `XxlJobAgentScheduler::try_cancel()` 返回 Java 原文错误 `"Cancel operation is not supported by XxlJobAgentScheduler. Please use XXL-Job admin console to pause or delete the job."`，并新增 trait 默认方法 `AgentScheduler::try_cancel`（xxl-job override 为 Err）；布尔版 `cancel()` 仍返回 `false` + warn（受 trait 签名限制，无法区分「不支持/未找到」，已在 §4.2 登记） |

## 4. 项目状态与下一步
### 4.1 当前状态
- **测试修复完工（2026-09-18）**：`cargo test -p agentscope-extensions-scheduler` 由「lib test 编译失败（4 个错误）」修至**全绿 37 passed / 0 failed**；`cargo build -p agentscope-extensions-scheduler` 通过（仅 7 条 warning，无 error）。
- **静态复核（2026-10-22，未编译）实测**：Rust 14 个 `.rs` / 2416 行 / 37 个测试属性（`#[test]` 19 + `#[tokio::test]` 18），分布：cron 8、quartz/mod 6、xxljob 5、schedule 5、spec 4、task(common) 4、task(quartz) 3、registry 2 → 与「37 passed」自洽。
- **Java 侧实测（静态）**：主 3234 行 / 14 类型，测试 3178 行 / 180 个 `@Test`。**已完成（2026-10-29 勘误）**：原记「Rust 测试数 37 vs Java 180（覆盖率约 21%），且 Rust **无端到端定时触发测试**（无任何测试断言 `fire()` 真的按 cron/fixed_rate 触发过一次；`QuartzAgentScheduler::trigger` 与 `job::spawn_job` 均零测试覆盖）」，实际 2026-10-28 已新增 `tests/quartz_execution.rs`（**3 passed**，真实按 fixed-rate/cron 触发断言）并补 36 个 lib 用例 → **合计 91 passed / 0 failed vs Java 180**（§8）；测试数量差距见 §4.2 第 8 条。
- **改动文件（仅本 crate）：**
  1. `src/quartz/mod.rs`：测试模块补 `use crate::common::config::ScheduleMode;`（修 `E0433: cannot find type 'ScheduleMode'`；类型本存在于 `common/config/schedule.rs:13`，未新建平行 enum）。
  2. `src/common/scheduler/task.rs` / `src/quartz/task.rs` / `src/xxljob/mod.rs`：`Msg::get_text_content()` → `Msg::text_content()`（3 处，修 `E0599`；正确 API 见 `agentscope-core/src/message/msg.rs:260`），其余逻辑未动。
  3. `src/quartz/cron.rs`：修 step 语法解析 bug——`parse_field` 对「裸 `a/step`（无范围）」原按单值 `(a,a)` 处理，导致 `0 0/30 * * * ?` 只匹配第 0 分钟，取数 `every_thirty_minutes` 断言失败；现按 cron/Quartz 语义展开为 `a..max`（step>1 时），`a` 仍为单值。
     - **静态复核补充（2026-10-22）**：该修复对 `step>1` 成立，但**遗留同类 bug**——`a/1`（step=1）仍走 `(v,v)` 单值分支，而标准 cron / Quartz 中 `0/1` 等价于「从 0 起每分钟」，应为 `(0,max)`。当前 `0/1` 只匹配第 0 分钟。`*/1` 因走 `range_part=="*"` 分支不受影响。
- **验证命令与结果：**
  - `cargo build -p agentscope-extensions-scheduler` → 成功（0 error）。
  - `cargo test -p agentscope-extensions-scheduler` → `test result: ok. 37 passed; 0 failed`（lib）+ doc-tests 0 failed。
- 未删除任何测试或功能；未改动根 `Cargo.toml`、core/harness 及其它 crate。

- **cron 语义对齐（2026-10-26，代码已落盘、子智能体未跑 cargo）**：4 项 P1 语义分叉修复（星期基准 1-7、dom/dow 冲突报错、非法 cron 快速失败且无副作用、`zone_id` 生效）。新增文件 `src/common/config/zone.rs`；新增 `job::CronPlan` / `quartz::resolve_cron`；`cron::CronExpression::matches`/`next_after` 泛型到任意 `TimeZone`。测试名单与「改前如何失败」见 §6.1。注意：**行为变更** —— `0 0 9 * * MON-FRI` 这类两侧都指定的 6 字段表达式现在会报错（必须写 `0 0 9 ? * MON-FRI`），调用方与示例需同步。

- **2026-10-28 执行（7 项任务全部落地，§8 详表）**：cron `L`/`L-n`/`LW`/`nW`/`nL`/`n#k` + 7 字段年 + `a/1` + `?` 严格化；`next_after` 接入生产循环 + 查询 API；`interrupt()` 中止 in-flight；`TaskStatus` 6 值 + `is_terminal`/`as_str` + `trigger_state`；xxl-job `XxlJobContext`/`XxlJobHelper`/`handleSuccess`/`handleFail`/分片参数/日志 id；`cancel` 语义（quartz join 等待 + xxl-job `try_cancel` 原文报错）；`execution_count()`/`schedule_config()`/`agent_config()` 公开。
  - 实测：`cargo test -p agentscope-extensions-scheduler` → lib **88 passed / 0 failed**、`tests/quartz_execution.rs` **3 passed / 0 failed**、doc-tests 0 → 合计 **91 passed / 0 failed**（基线 52+3=55 全绿保留，新增 36）。
  - `cargo clippy -p agentscope-extensions-scheduler --all-targets`：本 crate 仅剩 2 条既有 warning（`module_inception`、`very_complex_type`）；本轮删除了 `next_seq` / `AsAny` / `interrupt_notify` / `JobContext::run_task` 四个死脚手架。
  - 未新增任何依赖（未引入 `chrono-tz`，未用 `tokio-util`；interrupt 用 `tokio::sync::watch` + `select!`）。

### 4.2 下一步与阻塞
1. ~~P0（`with_factory` 未接线）~~ → **已修复（2026-10-22，§7）**；`factory` 不再 dead field。
2. ~~`next_after` 生产零调用~~ → **已接入（2026-10-28）**：`job::CronPlan::next_fire_time` 驱动 CRON 循环精确休眠，并按字段跳跃优化（月/日/时/分逐级跳，不再逐秒扫）；查询 API `QuartzAgentScheduler::next_fire_time(name)`。
3. 未使用依赖：`agentscope-harness`（引用数 0）建议裁剪；`futures` 仍为 0 引用。`TaskControl` trait 仍为未被使用的脚手架（保留，属公共抽象）。
4. cron 剩余（本轮明确不做，非缺口）：**DST 时区**（`Europe/London` 等需 `chrono-tz`，当前 `resolve_zone` 明确报错而非静默降级）；Quartz 的 `Calendar` 排除日 / `misfire` 策略；`nW`/`n#k` 与列表混写（Quartz 本身亦不支持）。
5. **跨模块需求（记入此处，未改 core/harness）**：xxl-job 的 `PostReasoningEvent → XxlJobHelper.log` 逐条日志 Hook 在 Rust 侧无等价挂点（`Middleware::on_reasoning` 只能看到**入参**，`on_acting` 为死代码，core 未向 middleware 暴露推理**产出**事件）→ 当前以「执行结束记录最终回复 + handleSuccess/handleFail」替代；若后续 core 暴露 post-reasoning 事件，可在本 crate 内补 `XxlJobLogMiddleware` 并注入 `RuntimeAgentConfig.middleware`（`spec.rs` 的 `Vec` 不可变，需先增加 `with_middleware` 之类的复制构造）。
6. `AgentScheduler::cancel` 返回 `bool`（Java 抛异常）→ 已通过新增默认方法 `try_cancel() -> Result<bool, SchedulerError>` 补足；调用方需迁移到 `try_cancel` 才能区分「不支持」与「未找到」。
7. `QuartzAgentScheduler::shutdown` 现在 `shutdown + join().await`（对齐 `scheduler.shutdown(true)` 的等待语义），但 `join` 用 `abort()` 实现 → in-flight 执行被丢弃而非跑完；Java `shutdown(true)` 会等作业完成。语义差异已登记，若需精确对齐应改为「先等 in-flight，再 abort 循环」。
8. **真实剩余（2026-10-29 复核，8 项任务均已关闭，以下为剩余项）**：
   - xxl-job `PostReasoningEvent → XxlJobHelper.log` 逐条日志 Hook **无挂点**（跨模块：core 未向 middleware 暴露推理**产出**事件；现以最终回复 + `handleSuccess`/`handleFail` 替代）——第 5 条。
   - `SchedulerRegistry` 只存 `scheduler_id` 字符串、**无句柄反查**（Java 存 `QuartzAgentScheduler` 实例，§3.2）。
   - `BaseScheduleAgentTask::run_with` **不校验 model 非空**（Java 抛 `Model cannot be null.`，§3.1）。
   - `shutdown` 用 `abort()` **丢弃 in-flight**（第 7 条）。
   - 未使用依赖：`agentscope-harness`（0 引用）、`futures`（0 引用）；`TaskControl` 为保留的公共抽象（第 3 条）。
   - 测试 **91 vs Java 180**（§4.1 / §8）。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-scheduler` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-scheduler` / `cargo test -p agentscope-extensions-scheduler` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-scheduler/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. Cron 语义差异（Quartz vs 自研解析器）

> 高价值发现：tracker §4.1 声称「cron 解析 bug 已修」，实际只修了 `a/step`（step>1）一种情形，**Quartz 扩展语法整体缺失**。Java 侧直接用 `org.quartz.CronScheduleBuilder.cronSchedule(...)`，语义由 Quartz 保证。

| Quartz 能力 | Java | Rust `quartz::cron` | 影响 |
|---|---|---|---|
| 6 字段 `sec min hour dom mon dow` | ✅ | ✅ | — |
| 5 字段（缺秒） | ❌ Quartz 拒绝 | ✅ 自动补秒=0 | Rust 更宽松（非严格对齐，但无害） |
| 7 字段（带年） | ✅ 支持 `1970-2199`（2.3.2 运行时为 `1970..当前年+100`，`MAX_YEAR`） | ✅ **已支持（2026-10-28，§8）**：`sec min hour dom mon dow year`，年范围 `1970-2199`，支持 `a,b` / `a-b` / `*/n`；`CronExpression::years()` 暴露 | **P1 → 已修复**：`0 0 0 1 1 ? 2030` 可调度；年耗尽后 `next_after` 返回 `None` → 任务状态置 `Complete`（Quartz `TriggerState.COMPLETE`） |
| `L`（last day of month / last weekday） | ✅ | ✅ **已支持（2026-10-28，§8）**：dom `L` = 当月最后一天；dom `L-n` = 倒数第 n 天；dow `L` = `7`（SAT，Quartz 语义）；dow `nL` = 当月最后一个星期 n | **P1 → 已修复** |
| `W`（nearest weekday，如 `15W`） | ✅ | ✅ **已支持（2026-10-28，§8）**：`nW` = 距第 n 天最近的周一至周五，**不跨月边界**（`1W` 落在周六 → 下周一即 3 号；月末落在周日 → 前一个周五）；当月不存在该日（如 4 月 `31W`）则当月不触发 | **P1 → 已修复** |
| `LW` | ✅ | ✅ **已支持（2026-10-28，§8）**：当月最后一个工作日（最后一天为周六 → 周五，周日 → 周五） | **P1 → 已修复** |
| `#`（第 n 个星期几，如 `6#3`） | ✅ | ✅ **已支持（2026-10-28，§8）**：`n#k`，`k ∈ [1,5]`，支持名称（`FRI#2`）；当月无第 k 个该星期几则不触发 | **P1 → 已修复** |
| `?` 仅在 dom/dow 合法 | ✅ 严格（其他字段用 `?` → `ParseException("'?' can only be specified for Day-of-Month or Day-of-Week.")`） | ✅ **已修复（2026-10-28，§8）**：`parse_field(..., allow_question=false)`，非日字段出现 `?` 直接报同义错误；`? ?` 亦拒绝（Quartz 同样拒绝） | **P1 → 已修复** |
| dom/dow 同时被指定时的行为 | ❌ **直接拒绝**：`ParseException("Support for specifying both a day-of-week AND a day-of-month parameter is not implemented.")`（Quartz 2.3.2 `CronExpression.buildExpression`；判定条件是「dom 与 dow 均非 `?`」，与是否写成 `*` 无关） | ✅ **已修复（2026-10-26，§6.1）**：`resolve_day_selector` 在解析期直接返回 `CronParseError`；6 字段要求 dom/dow **恰有一个** `?`（`*` 也算「已指定」），5 字段扩展仅在两侧都是具体值时拒绝 | **P1 → 已修复**：`schedule_runtime` 在登记前报 `Err`，与 Java 「快速失败且不登记」一致。历史：原为 OR 语义（`(false,false) => dom_ok \|\| dow_ok`）→ 同一表达式 Java=失败 / Rust=触发 |
| `a/step`（裸起点+步长） | ✅ 展开到字段上限（Quartz `addToSet(val, end=-1, incr)` → `stopAt=59`；**`0/1` 亦展开为 0..59**） | ✅ **已修复（2026-10-28，§8）**：`parse_field` 区分「有无 `/`」——有 `/` 一律展开到字段上限（含 `step == 1`），无 `/` 才是单值 | **P1 → 已修复**：`0 0/1 * * * ?` 现在每分钟触发（历史：`0/1` 只匹配第 0 分钟） |
| `*/step` | ✅ | ✅ | — |
| `a-b/step` | ✅ | ✅ | — |
| 逗号枚举 `a,b,c` | ✅ | ✅ | — |
| 月份/星期名称 `JAN-DEC`/`SUN-SAT` | ✅（大小写不敏感，`dayMap`/`monthMap`） | ✅（`to_ascii_uppercase`；名称→值 `min==1 ? idx+1 : idx` → 月 `JAN=1`、星期 `SUN=1…SAT=7`） | **对齐**（名称路径与数字路径共用同一套 1 基编号） |
| 星期 **数字** `1-7` = SUN-SAT（Quartz 约定） | ✅ `dayMap`: SUN=1, MON=2, TUE=3, WED=4, THU=5, FRI=6, SAT=7 | ✅ **已修复（2026-10-26，§6.1）**：`parse_field(dow, 1, 7, &DOW_NAMES)`；`matches` 用 `weekday().num_days_from_sunday() + 1`，已删除 `dow==0 && matches(7)` 回绕分支 | **P1 → 已修复**：`0` 现报 `value 0 out of range [1,7]`（对应 Quartz `Day-of-Week values must be between 1 and 7`）。历史：原按 `0-7`（`1`=MON、`7`=SUN）静默错位一天 |
| 时区 `zoneId` | ✅ `csb.inTimeZone(TimeZone.getTimeZone(zoneId))` | ✅ **已修复（2026-10-26，§6.1）**：`ScheduleConfig::zone_offset()` → `chrono::FixedOffset`；`job::CronPlan::matches_at` 用 `now.with_timezone(&offset)` 求值 | **P1 → 已修复**：`Asia/Shanghai` 的 `0 0 8 * * ?` 现在真的在北京时间 08:00 触发。限制：无 `chrono-tz`，DST 区（`Europe/London` 等）报明确错误（Java 静默降级 GMT，Rust 故意快速失败） |
| 触发精度 | 毫秒级、由 Quartz 计算下次触发时刻 | ✅ **已对齐（2026-10-28，§8）**：`CronPlan::next_fire_time`（= `CronExpression::next_after` 在配置时区求值）驱动 CRON 循环 `sleep_until`；无未来触发时置 `Complete`。`next_after` 改为字段级跳跃（年/月/日/时/分），不再逐秒扫描 | **P1 → 已修复**：不再有「1 秒轮询抖动漏掉整分钟」窗口；即使睡过头也只会晚触发一次而不会丢 |
| 表达式非法时的行为 | `CronScheduleBuilder.cronSchedule()` 捕获 `ParseException` → 抛 **未受检** `RuntimeException("CronExpression '…' is invalid.", e)`；该异常**不被** `schedule()` 的 `catch (SchedulerException e)` 捕获 → 直接逸出 `schedule()`，且 `tasks.put(jobName, task)`（Java `:307`）**永不执行 → 任务未登记** | ✅ **已修复（2026-10-26，§6.1）**：`QuartzAgentScheduler::schedule_runtime` 在 `jobs.insert` **之前**调 `resolve_cron()`（解析表达式 + 解析时区），失败返回 `SchedulerError("CronExpression '…' is invalid: …")`；`spawn_job` 改收 `Option<CronPlan>`，循环内不再有解析/静默分支 | **P1 → 已修复**：Java=快速失败且无副作用，Rust 现同。历史：原仅 `tracing::error!` 且任务已登记、永不触发 |

## 6.1 本轮 cron 语义对齐（2026-10-26）

> 范围：仅改 `crates/agentscope-extensions-scheduler/**`（含本 crate `Cargo.toml` 未动），未触碰 core/harness/根 `Cargo.toml`。子智能体按约定**未执行 cargo**，编译/测试由主智能体统一验证。

| # | 缺陷（改前行为） | 修复 | 新增 / 更正测试 |
|---|---|---|---|
| 1 | 星期数字按 `0-7`（`1`=MON、`7`=SUN），`0` 静默当作周日 → 与 Quartz **静默错位一天** | `parse_field(dow, 1, 7, &DOW_NAMES)`；`matches` 改为 `weekday().num_days_from_sunday() + 1`；删除 `dow==0 && matches(7)` 回绕 | 新增 `quartz::cron::tests::weekday_numbers_follow_quartz`、`weekday_names_follow_quartz`、`rejects_zero_based_weekday_numbers`；更正 `parses_six_field_quartz` / `weekday_range` / `named_months` 为 `?` 写法 |
| 2 | dom+dow 同时指定时按 **OR** 触发（Java 直接 `ParseException`） | 新增 `DaySelector {Any, DayOfMonth, DayOfWeek}` + `resolve_day_selector`：6 字段要求 dom/dow **恰有一个 `?`**（`*` 视为已指定，同 Quartz，故 `0 0 9 * * MON-FRI` 被拒）；5 字段扩展仅在两侧都具体时报错 | 新增 `quartz::cron::tests::rejects_both_day_of_month_and_day_of_week` |
| 3 | 非法 cron 仅 `tracing::error!`，任务**已登记但永不触发** | `schedule_runtime` 在 `jobs.insert` 之前调 `resolve_cron()`；`spawn_job(ctx, task, schedule_config, Option<CronPlan>)`；job 循环不再解析 | 新增 `quartz::tests::invalid_cron_fails_without_registering_task`（同时断言 `is_err()`、`get_scheduled_agent` 为 `None`、`all_schedule_agent_tasks()` 为空） |
| 4 | `zone_id` 纯死字段，一律 `Utc::now()` | 新增 `common/config/zone.rs`（`resolve_zone` / `ScheduleZoneError`）+ `ScheduleConfig::zone_offset()`；`job::CronPlan::matches_at(now) = expression.matches(now.with_timezone(&offset))`；`CronExpression::matches`/`next_after` 泛型化到任意 `TimeZone` | 新增 `common::config::zone::tests::{absent_zone_defaults_to_utc, resolves_utc_aliases, resolves_iana_aliases_and_fixed_offsets, rejects_unknown_and_dst_zones}`、`schedule::tests::zone_offset_defaults_to_utc_and_rejects_unknown`、`quartz::job::tests::{cron_matches_in_configured_zone, cron_matches_in_utc_by_default, negative_offset_shifts_weekday_boundary}`、`quartz::tests::{invalid_zone_id_fails_without_registering_task, cron_with_zone_id_is_scheduled}` |

**改前为何失败（反向验证口径）**：
- ① `weekday_numbers_follow_quartz`：`0 0 9 ? * 1` 旧实现匹配周一而非周日；`rejects_zero_based_weekday_numbers` 旧实现 `is_err()` 为 false。
- ② `rejects_both_day_of_month_and_day_of_week`：旧实现能解析成功，`unwrap_err()` 直接 panic。
- ③ `invalid_cron_fails_without_registering_task`：旧实现 `schedule_runtime` 返回 `Ok` 且 `get_scheduled_agent("A")` 为 `Some`。
- ④ `cron_matches_in_configured_zone` / `negative_offset_shifts_weekday_boundary`：旧实现恒按 UTC 求值，+08:00 的 00:00Z 不命中、08:00Z 反而命中。

**未做（仍为待办，未改动）**：`L`/`W`/`LW`/`#`、7 字段年、按秒轮询改精确休眠（`next_after` 仍零调用）、`interrupt()` 中止 in-flight、`get_status` 六值、xxl-job 回调协议；时区未引入 `chrono-tz`（DST 区需显式传偏移）。

## 静态复核记录（2026-10-22）

### 复核方式
- **纯静态**：通读 tracker + Rust crate 全部 14 个 `.rs`（2416 行）+ Java 3 子模块 28 个 `.java`（6412 行）。
- **未执行** `cargo build` / `cargo test` / `mvn`；所有「实际统计」均为文件行数、`#[test]` 属性计数、`grep` 引用计数。
- 统计口径：行数 = `ReadAllLines().Length`（含空行与注释）；测试数 = 行首 `#[test]` + `#[tokio::test]`（Rust）/ 行首 `@Test|@ParameterizedTest|@RepeatedTest`（Java）。

### 实测统计

| 项 | tracker 原文 | 实测 | 判定 |
|---|---|---|---|
| Java 主代码行数 | ~3009 | **3234** | 偏差 +225（7.5%） |
| Java 测试行数 | ~2589 | **3178** | 偏差 +589（22.8%） |
| Java 主代码类型数 | 未记 | **14** | 补充 |
| Java 测试 `@Test` 数 | 未记 | **180** | 补充 |
| Rust `.rs` 文件数 | 未记（§3.1 隐含 4 个 common 文件） | **14**（common 7 / quartz 5 / xxljob 1 / lib 1） | 补充 |
| Rust 总行数 | 未记 | **2416** | 补充 |
| Rust 内联测试数 | 37 passed | **37**（`#[test]` 19 + `#[tokio::test]` 18） | ✅ 一致 |
| 测试分布 | 未记 | cron 8 / quartz-mod 6 / xxljob 5 / schedule 5 / spec 4 / task(common) 4 / task(quartz) 3 / registry 2 | 补充 |
| Rust : Java 测试比 | 未记 | 37 : 180 ≈ **21%** | 补充 |

**分模块行数（Java 主/测）**：common 1868/1094 · quartz 975/1445 · xxl-job 391/639。

### 对齐结论表

| 子模块 | Java 类型数 | Rust 有对应 | 行为等价 | 结论 |
|---|---|---|---|---|
| scheduler-common | 9 | 8 ✅ + 1 ⚠️（DashScopeModelConfig → StaticModelConfig 占位） | 约 80% | 契约层可用；model 校验缺失、serde 字段名不兼容 |
| scheduler-quartz | 4 | 4（AgentQuartzJob 降级为私有 `spawn_job`） | 约 45% | **核心执行链路断裂**（P0）+ cron 语义大面积缺失 |
| scheduler-xxl-job | 1 | 1 | 约 60% | 注册表可用；回调/日志/上下文协议未实现 |

### 新发现缺口

**P0（核心链路断裂，必须修）**
1. **`QuartzScheduleAgentTask` 永远拿不到真实 `AgentFactory`**：`quartz/mod.rs:107-120` 构造任务后直接 `attach(handle)`，从未调用 `with_factory(self.factory)`；`task.rs:55` 的 `NoopFactory` 恒定返回 `Err("No agent factory attached to this scheduled task")`。→ Quartz 后端的**任何**执行（定时 fire、`trigger()`、`task.run()`）100% 失败。`QuartzAgentScheduler.factory` 因此成为 dead field。37 个测试无一断言执行成功，故 CI 全绿。
2. **`schedule_with_input` 的初始消息被丢弃**：`runtimes` map 只写不读（`mod.rs:113` 写、`:238` 删，无读取点），`fire()` → `task.execute()` → `run_with(Vec::new())`。Java 侧通过 JobDataMap `inputMsg` + JSON 反序列化传入 `t.run(inputMsg)`。→ 初始 prompt 语义完全丢失。

**P1（语义不等价，影响正确性）**
3. cron 缺 `L`/`W`/`LW`/`#`/年字段（详见 §6）。
4. cron 星期编号按 `0-7` 直读，与 Quartz `1-7`(SUN-SAT) 约定**静默错位一天**。
5. `zone_id` 为死字段，cron 恒按 UTC 求值。
6. `a/1` 仍退化为单值（§4.1 修复不完整）。
7. cron 非法表达式静默失败（登记成功但永不触发），Java 抛异常。
8. 秒级轮询 + `Utc::now()` 有漏触发风险；`next_after` 生产零调用。
9. `interrupt()` 仅 `notify_waiters()`，不中止 in-flight 执行（Java `InterruptableJob` 会置 `interrupted` 标志）。
10. `get_status` 只有 3 值，Java `TriggerState` 有 6 值（NORMAL/PAUSED/COMPLETE/ERROR/BLOCKED/NONE）。
11. xxl-job 无 `XxlJobHelper.log`/`handleSuccess`/`handleFail` 回调、无 `XxlJobContext` 参数读取、无 executor `destroy()`。
12. `cancel` 在 xxl-job 返回 `false` 而非抛错，调用方无法区分「不支持」与「未找到」。

**P2（增强/清理）**
13. 无集群回读（`loadTaskFromQuartz` 无对应）——设计决定，但 §2 表格曾声称「集群回读」。
14. 未使用脚手架：`next_seq`、`interrupt_notify()`、`AsAny`、`TaskControl`；未使用依赖：`futures`、`agentscope-harness`。
15. `QuartzAgentScheduler` 缺 Builder（`autoStart`/`schedulerId`/注入自定义 Scheduler）；`scheduler_id` 恒为随机 UUID，无法指定。
16. serde 字段名未做 camelCase 对齐，Java 侧持久化的 JSON 无法直接被 Rust 反序列化。
17. 无端到端定时触发测试（Java `QuartzFixedDelayIntegrationTest` 有 1 个集成测试，Rust 无对应）。

### 勘误列表

| # | tracker 原文 | 实际 |
|---|---|---|
| 1 | Java 规模 主 ~3009 / 测 ~2589 行 | 主 **3234** / 测 **3178** 行（14 类型 / 180 `@Test`） |
| 2 | `config.DashScopeModelConfig` → `common::config::ModelSpec` | 类型名为 **`StaticModelConfig`**，不存在 `ModelSpec` |
| 3 | cron 用 `tokio-cron-scheduler` 的 cron 解析 | **未引入该依赖**；为自研 `quartz::cron`（`tokio_cron` 引用 0 处） |
| 4 | 需补 `uuid = { workspace = true }` | **已声明**（`Cargo.toml:19`） |
| 5 | zoneId → `chrono-tz` 或 tokio-cron 的 chrono 集成 | 仅有 `chrono`；`zone_id` 无消费点 |
| 6 | `schedule` 用 `Arc<dyn AgentConfig>` | `AgentConfig` 为具体 struct 按值传入，无 `Arc<dyn>` |
| 7 | `common` 骨架为 `src/common/{mod,config,scheduler,task}.rs` | 实为 **7 个文件**（config/ 与 scheduler/ 均为目录） |
| 8 | quartz 对应 `quartz::AgentJob` | 无公开 `AgentJob`；为私有 `job::spawn_job` |
| 9 | `QuartzScheduleAgentTask` 支持 reschedule | 无 reschedule 能力（由 job 循环 sleep 替代） |
| 10 | §3.2「集群回读」保留 | Rust 无集群回读 |
| 11 | §3.3「cancel 抛 Unsupported（对齐 Java）」 | Rust 返回 `false` + warn，**不对齐** |
| 12 | §4.1「cron 解析 bug 已修」（含 `a/1` 仍为单值） | `a/1` 仍是 bug（应为 `a..max`） |
| 13 | §3.1 全部 6 项「进行中」 | 骨架均已落地，应改「已完成」（但 §3.2/3.3 存在实质缺口） |
| 14 | §4.2「无阻塞」 | **存在 P0 阻塞**（factory 未接线 + input 丢弃） |
| 15 | §2 表格 `BaseScheduleAgentTask`「每次执行新建 `ReActAgent`」 | Rust 经 `AgentFactory` 间接创建，本身不 new `ReActAgent` |
| 16 | §2 表格 `ScheduleMode`「serde rename 对齐 Java 枚举名」 | **无任何 `#[serde(rename)]`**，靠变体同名 |
| 17 | §3.1「6 个 Java 测试对应」 | Java `ScheduleConfigTest` 实为 19 个 `@Test`；Rust 侧 5 个 |

### 第一轮 rollup 建议
**建议 rollup 状态：`进行中`**（不可标 `已完成`）。
理由：① 编译/测试层面虽 37 passed 全绿，但 P0 缺陷使 Quartz 后端执行链路**运行时 100% 失败**，属功能性未完成；② cron 语义与 Quartz 差距大（`L`/`W`/`#`/年/时区/星期基准），无法声称行为对齐；③ xxl-job 回调协议未 1:1。建议在 `EXTENSIONS_GAP_TRACKER.md` 第 17 行将描述从「迁移完成，37 test 通过」修正为「骨架完成 / 37 test 通过，但存在 P0 执行链路断裂与 cron 语义缺口」。

### 第二轮补充复核（2026-10-22）

> **范围**：对第一轮 §「静态复核记录」的结论逐条**验证**（而非重做），重点核 P0①② 与 P1 全部条目；核对统计口径；补查 `common` 子模块 7 文件与 `spec`/`task`/`registry` 的 Java 对应类。**纯静态**，未执行 `cargo build` / `cargo test` / `mvn`，未改动任何 Rust 代码（仅本文件）。

#### 一、逐条验证判定

| 编号 | 第一轮结论 | 判定 | 证据 |
|---|---|---|---|
| **P0①** | `QuartzAgentScheduler::schedule*` 未调用 `with_factory(self.factory)` → 任务内持 `NoopFactory` → 所有触发返回 `Err("No agent factory attached…")`；`factory` 字段只写不读 | **✅ 成立（证据完全吻合）** | `quartz/mod.rs:107-111` 构造 `QuartzScheduleAgentTask::new(runtime, schedule_config, name)`，`:119-120` 仅 `spawn_job` + `task.attach(handle)`，**全程无 `with_factory` 调用**；`quartz/task.rs:55` `let factory = Arc::new(NoopFactory)`，`task.rs:229-238` `NoopFactory::run` 恒返 `Err(Error::Agent("No agent factory attached to this scheduled task"))`；`quartz/task.rs:176` `execute()` → `self.inner.run_with(Vec::new())` → `common/scheduler/task.rs:141` `self.factory.run(...)` → 必失败。全 crate `with_factory` 命中仅 3 处，**全在 `quartz/task.rs` 的 `#[cfg(test)]` 内**（`:263/:280/:293`），生产路径 0 处。`self.factory` 在 `quartz/mod.rs` 仅 `:48`（定义）`:63`（`new` 赋值）出现，**无任何读取点** → 确为 dead field。补充：`quartz/mod.rs:187` `trigger()`、`:203` `JobContext::run_task`、`:124` `job::fire` 三条执行路径全部汇聚到 `execute()`，故**定时 fire / 手动 trigger / trait `run()` 三者 100% 失败**。 |
| **P0②** | `schedule_with_input` 初始消息被丢弃（`runtimes` 只写不读，`fire()` 走 `run_with(Vec::new())`） | **✅ 成立（证据完全吻合）** | `quartz/mod.rs:219-228` `schedule_with_input` 把 `input` 传入 `schedule_runtime(..., Some(input))`；`mod.rs:113-116` 写入 `runtimes` map（`SchedulerRuntime{ runtime, input }`）；全 crate `runtimes` 命中仅 4 处（`:52` 定义、`:65` 初始化、`:113` 写、`:238` 删）→ **无读取点**。`job.rs:123-127` `fire()` → `task.execute()` → `quartz/task.rs:176` `self.inner.run_with(Vec::new())` → 初始 `Msg` 被丢弃。Java 对照：`AgentQuartzJob.java:67-101` 从 JobDataMap 取 `inputMsg` → `JsonUtils.fromJson` → `t.run(inputMsg).block()`。 |
| **P1-3** | cron 缺 `L`/`W`/`LW`/`#`/年字段 | **✅ 成立** | `cron.rs:194-210` `resolve()` 仅接受数字或名称，命中即 `CronParseError("unrecognised token '{t}'")`；`cron.rs:60-67` 仅接受 5/6 字段，7 字段 → `"expected 5 or 6 fields, got {n}"`。 |
| **P1-4** | 星期按 `0-7` 读取 vs Quartz `1-7`(SUN-SAT)，静默错位一天 | **✅ 成立（且已量化到逐值）** | Quartz `CronExpression.java` v2.3.2 静态块 `dayMap`: SUN=1, MON=2, TUE=3, WED=4, THU=5, FRI=6, SAT=7；`addToSet` 对 `val==0` 抛 `ParseException("Day-of-Week values must be between 1 and 7")`。Rust `cron.rs:79` `parse_field(dow, 0, 7, &DOW_NAMES)` + `cron.rs:99-100` `now.weekday().num_days_from_sunday()`（SUN=0…SAT=6）→ `1`=MON、Quartz `1`=SUN，**错位 1 天**。 |
| **P1-5** | `zone_id` 未使用、恒 UTC | **✅ 成立** | `schedule.rs:70/81/118-119/177-178` 仅定义/存取；`job.rs:70` `let now = Utc::now();`、`cron.rs:86` `matches(&self, now: DateTime<Utc>)` 全程 `Utc`；`Cargo.toml:18` 仅 `chrono`，**未引入 `chrono-tz`** → 无法解析 `Asia/Shanghai`。 |
| **P1-6** | `a/1` 退化为单值 | **✅ 成立（已对照 Quartz 源码确认修复方向）** | `cron.rs:164-172`：裸值分支 `if step > 1 {(v,max)} else {(v,v)}` → `0/1` 只产出 `{0}`。Quartz `checkNext` 的 `'/'` 分支 → `addToSet(val, end=-1, incr=1, type)` → `addToSet` 内 `stopAt==-1` 时按类型补上限（MINUTE→59）且 `incr==1≠0/-1` 故走循环展开 → **`0/1` = 0..59（每分钟）**。修复方向应为**去掉 `step > 1` 条件**（裸 `a` 仍需走单值，故须把「无 `/`」与「`/1`」两种情形分开）。 |
| **P1-7** | 非法 cron 静默失败 | **✅ 成立（且异常链已修正）** | `job.rs:34-42` 解析失败仅 `tracing::error!` 并置 `cron = None`，`job.rs:61` `let Some(cron) = cron else { return }` → 任务已登记（`mod.rs:117`）但**永不触发**。Java 侧 `QuartzAgentScheduler.java:272-284` `CronScheduleBuilder.cronSchedule(...)` → `ParseException` → **未受检** `RuntimeException("CronExpression '…' is invalid.", e)`（Quartz `CronScheduleBuilder.cronSchedule(String)` 内部 `catch (ParseException e) → throw new RuntimeException(...)`）→ **不匹配** Java `schedule()` 的 `catch (SchedulerException e)`（`:310-313`）→ 直接逸出 `schedule()`，且 `tasks.put(jobName, task)`（`:307`）**永不执行** → **Java=快速失败且无副作用，Rust=登记成功但静默不触发** |
| **P1-8** | 秒级轮询漏触发风险；`next_after` 生产零调用 | **✅ 成立** | `job.rs:62-78` CRON 分支 `sleep(Duration::from_secs(1))` + `cron.matches(now)`；`next_after` 全 crate 仅 `cron.rs:112`（定义）与 `cron.rs:259`（测试）→ **生产零调用**。 |
| **P1-9** | `interrupt()` 不中止 in-flight | **✅ 成立** | `quartz/task.rs:134-136` `interrupt()` 仅 `self.notify.notify_waiters()`；`notify` 被 `wait_if_paused()`（`:153-157`）消费，与执行中的 `run_with` 无关联；`running` 标志（`:118-120`）只读不写。Java `AgentQuartzJob.java:39/121-123` 置 `interrupted=true`，`:49-51` 与 `:107-109` 两处短路（跳过 run / 跳过 FIXED_DELAY 重排）。 |
| **P1-10** | `get_status` 3 值 vs Java 6 值 | **✅ 成立** | Rust `quartz/task.rs:22-30` `TaskStatus{Scheduled, Paused, Cancelled}`（3 值）；Java `QuartzAgentScheduler.java:535-551` 返回 Quartz `Trigger.TriggerState`（`NONE/NORMAL/PAUSED/COMPLETE/BLOCKED/ERROR`，6 值），且 `BLOCKED`/`ERROR` 来自 Quartz 运行时状态，Rust 无对应概念。 |
| **P1-11** | xxl-job 缺 `XxlJobHelper`/`handleSuccess`/`XxlJobContext` 协议 | **✅ 成立** | Java `XxlJobAgentScheduler.java:189-201`（`PostReasoningEvent` → `XxlJobHelper.log`）、`:253/265`（`XxlJobContext.getXxlJobContext()` → `getJobParam()`）、`:282`（`XxlJobHelper.handleSuccess(...)`）；Rust `xxljob/mod.rs:84-92` `trigger` 仅 `Msg::user(job_param)` + 返回 Msg，**无任何回调/上下文协议**。 |
| **P1-12** | xxl-job `cancel` 返回 `false` 而非抛错 | **✅ 成立** | Java `XxlJobAgentScheduler.java:299-304` `throw new UnsupportedOperationException(...)`；Rust `xxljob/mod.rs:111-119` `tracing::warn!` + `false`（注释亦自承 "Rust has no exceptions"）。 |

**判定小结**：P0 ①② 与 P1 全部 10 条**均成立**，无一条需要推翻。唯一需要**修正**的是第一轮 §6 表格中「dom/dow 同时非 `*` 时的 OR 语义 → 对齐」一栏（见下表第 1 行）。

#### 二、统计核对

| 项 | 第一轮数值 | 第二轮实测 | 判定 |
|---|---|---|---|
| Java 主代码行数 | 3234 | **3234**（14 个 `.java`，`[System.IO.File]::ReadAllLines().Length`） | ✅ 精确一致 |
| Java 测试行数 | 3178 | **3178**（13 个测试类 + `StubModelConfig`） | ✅ 精确一致 |
| Java 主代码类型数 | 14 | **14**（common 9 / quartz 4 / xxl-job 1） | ✅ 一致 |
| Java `@Test` 数 | 180 | **180**（`@Test` 180；`@ParameterizedTest`/`@RepeatedTest` 各 0） | ✅ 一致 |
| 分模块主/测 | common 1868/1094、quartz 975/1445、xxl-job 391/639 | **common 1868/1094、quartz 975/1445、xxl-job 391/639** | ✅ 三项全部一致 |
| Rust `.rs` 文件数 | 14（common 7 / quartz 5 / xxljob 1 / lib 1） | **14**（同分布） | ✅ 一致 |
| Rust 总行数 | 2416 | **2416** | ✅ 一致 |
| Rust 测试属性数 | 37（`#[test]` 19 + `#[tokio::test]` 18） | **37**（19 + 18） | ✅ 一致 |
| Rust 测试分布 | cron 8 / quartz-mod 6 / xxljob 5 / schedule 5 / spec 4 / task(common) 4 / task(quartz) 3 / registry 2 | **完全一致**（逐文件核对） | ✅ 一致 |
| Java 侧 `@Test` 逐类明细 | 未记 | `ScheduleConfigTest` 19、`DashScopeModelConfigTest` 15、`AgentConfigTest` 13、`BaseScheduleAgentTaskTest` 11、`AgentQuartzJobTest` 11、`ScheduleModeTest` 6、`QuartzScheduleAgentTaskTest` 5、`ScheduleAgentTaskTest` 4、`ModelConfigTest` 3、`RuntimeAgentConfigTest` 9、`QuartzAgentSchedulerTest` 49、`XxlJobAgentSchedulerTest` 34、`QuartzFixedDelayIntegrationTest` 1 | 补充 |

> 统计口径提示：Java 行数含空行与注释；Rust 行数同样含空行与注释。**两侧口径一致**，可直接比较。

#### 三、第二轮修正（第一轮结论 → 实际）

| # | 第一轮结论 | 实际 | 处置 |
|---|---|---|---|
| 1 | §6 表：「dom/dow 同时非 `*` 时的 OR 语义 → Java ✅ OR / Rust ✅ OR → **对齐**」 | **误判**。Quartz 2.3.2 `CronExpression.buildExpression` 在 `dom` 与 `dow` **均非 `?`** 时抛 `ParseException("Support for specifying both a day-of-week AND a day-of-month parameter is not implemented.")`——**Java 侧根本不会接受这类表达式**，`schedule()` 抛 `RuntimeException`。Rust `cron.rs:102-107` 却是 OR 语义 → **语义分叉（Java=失败 / Rust=触发）**，非「对齐」 | 已改写 §6 对应行；新增 P1-18（见下） |
| 2 | §6 表：7 字段年份范围 `2000-2099` | Quartz 2.3.2 javadoc 标 `1970-2199`，运行时 `MAX_YEAR = Calendar.getInstance().get(YEAR) + 100`（即 2026 年时为 `1970-2126`） | 已改为「`1970-2199`（2.3.2 运行时 `1970..当前年+100`）」 |
| 3 | §6 表：星期「按 `0-7` 数字直读，`1`=MON」 | 结论正确，但**表述可精确化**：Rust 额外接受数字 `0`（=SUN），而 Quartz 对 `0` 抛 `ParseException("Day-of-Week values must be between 1 and 7")`；且 Rust `7`=SUN 与 Quartz `7`=SAT **同时错位**（非单纯「错一天」而是双向回绕） | 已在 §6 行内补全逐值对照 |
| 4 | §6 表：「`a/step` → Java ✅ 展开到字段上限」 | 结论正确但**未指出 `step==1` 也应展开**。Quartz `addToSet(val, end=-1, incr=1)` 对 MINUTE 补 `stopAt=59` → `0/1` = 0..59 | 已在 §6 行内标注「**`0/1` 亦展开为 0..59**」 |
| 5 | §3.2「`QuartzScheduleAgentTask` — `with_factory` 未被 `QuartzAgentScheduler::schedule*` 调用」 | 表述为「未被调用」易被读成「调用了但没生效」。实际是**从未调用**（`with_factory` 生产路径 0 命中，3 处命中全在 `#[cfg(test)]`） | 措辞已在下方 P0-① 复核表中精确化（§3.2 原文保留） |
| 6 | §6 表 + P1-7：非法 cron「`schedule()` 抛 `RuntimeException`（**`SchedulerException` 包装**）」 | **异常链错误**。`CronScheduleBuilder.cronSchedule(String)` 把 `ParseException` 包成**未受检** `RuntimeException("CronExpression '…' is invalid.", e)`，**不是** `SchedulerException`；因此 Java `QuartzAgentScheduler.schedule()` 的 `catch (SchedulerException e)`（`:310`）**捕不到**，异常直接逸出，且 `tasks.put(jobName, task)`（`:307`）永不执行 → **Java 侧快速失败且无残留副作用**（与 Rust 的「登记成功但静默不触发」是两种不同的失败形态） | 已修正 §6 表与 P1-7 复核行 |

#### 四、第二轮新增缺口

**P1（新增）**

- **18. cron dom/dow 语义分叉**（详见 §三 第 1 行）：Java 拒绝、Rust 按 OR 触发。修复方向二选一：① Rust 侧改为「dom 与 dow 均非 `?` 时解析报错」，对齐 Quartz（更严格）；② 保留 OR 但**文档显式标注与 Quartz 不一致**。建议 ①，因 Java 侧 README/javadoc 示例（`ScheduleConfig.java:188-208`）明确要求用 `?`。
- **19. `RuntimeAgentConfig` 的 `model` / `toolkit` / `middleware` 全 crate 无消费点**：`spec.rs:184-194` 三个访问器定义了但**无任何调用方**（`rg '\.model\(\)|\.toolkit\(\)|\.middleware\(\)'` 命中仅 `spec.rs:325` 的 `assert!(rt.model().is_none())` 一处测试断言）。对照 Java `BaseScheduleAgentTask.java:231-256` `createAgent()`：`model` 缺失 → 抛 `IllegalArgumentException("Model cannot be null.")`；`toolkit`/`hooks` 缺失 → **静默不挂载**（工具/钩子能力整体丢失）。Rust 侧 `BaseScheduleAgentTask::run_with`（`common/scheduler/task.rs:126-144`）只校验 name/sysPrompt，**既不校验 model、也无法把 toolkit/middleware 传给 factory**（`AgentFactory::run` 只收到 `&RuntimeAgentConfig`，factory 自行取用——但由于 `AgentFactory` 由**调用方**提供，实际能否生效取决于外部实现，crate 内无任何示例/测试覆盖）。
- **20. `RuntimeAgentConfig` 的 `hooks` 语义缺口（Java 依赖其可变性）**：Java `RuntimeAgentConfig.java:84` 用 `CopyOnWriteArrayList`，xxl-job 在注册后 `getHooks().add(...)` 追加日志 Hook（`XxlJobAgentScheduler.java:189-201`）。Rust `spec.rs:158` `middleware: Vec<Arc<dyn Middleware>>` 不可变 → **该模式无法复现**（第一轮 §2 表格已提及，本轮补充：这是 xxl-job 协议缺口 11 的**根因**，而非仅是实现细节）。
- **21. `QuartzScheduleAgentTask` 未暴露 `execution_count` / `schedule_config` / `agent_config`**：`quartz/task.rs:86-120` 的公开方法中**无 `execution_count()`**（`inner` 为私有字段，测试靠 `task.inner.execution_count()` 访问私有字段——`quartz/task.rs:283`）。Java `QuartzScheduleAgentTask` 继承 `BaseScheduleAgentTask`，公开 `getExecutionCount()`/`getScheduleConfig()`/`getAgentConfig()`，且 `QuartzFixedDelayIntegrationTest.java:56` **正是靠 `qt.getExecutionCount()` 轮询断言「至少执行 2 次」**。Rust 侧无对应可观测点 → **即使 P0 修好，也无法写出等价的端到端断言**（这是 P1-17「无端到端测试」的**结构性原因**）。
- **22. `QuartzScheduleAgentTask::join()` 为死代码**：`quartz/task.rs:196-202` 定义了 `join()`（`h.abort()` + await），**全 crate 无调用点**；`cancel` 路径（`quartz/mod.rs:230-244`）只调 `task.shutdown()`（置标志 + notify），**不等待 job 循环退出** → `shutdown()`（`quartz/mod.rs:263-273`）后 job 循环可能仍在跑（`job.rs:63/67/76/89/93/98/105/109/115` 均以 `task.cancelled()` 退出，属协作式，但**无 join 保证**）。Java `scheduler.shutdown(true)` 等待正在执行的任务完成（`QuartzAgentScheduler.java:414`）→ Rust 语义弱一档。

**P2（新增）**

- **23. `QuartzAgentScheduler::schedule` 缺 Java 的单参便捷法 `schedule(AgentConfig)`**：Java `QuartzAgentScheduler.java:154-156` / `XxlJobAgentScheduler.java:144-146` 均提供单参重载（默认 `ScheduleConfig`），Rust 侧 `AgentScheduler` trait 只有双参 `schedule`（`common/scheduler/scheduler.rs:34-38`）→ API 面不对齐（低风险，调用方可自行 `ScheduleConfig::none()`）。
- **24. `AgentScheduler` trait 的 `schedule_with_input` 默认实现吞掉 `input`**：`common/scheduler/scheduler.rs:41-52` 默认体 `let _ = input;` 后返回 `Err(...)`（对齐 Java `AgentScheduler.java:122-126` 的 `UnsupportedOperationException`）。语义一致，但 **xxl-job 后端未 override** → Rust 侧 xxl-job `schedule_with_input` 返回 Err，Java 侧 `XxlJobAgentScheduler` **也没有** override `schedule(a,b,Msg)` → 同样落到默认 `UnsupportedOperationException`。**此项实为对齐**，此处仅作确认，非缺口。
- **25. `XxlJobAgentScheduler` 缺 executor 生命周期与注册失败异常路径**：Java `XxlJobAgentScheduler.java:343-348` `shutdown()` 调 `executor.destroy()`；`:229-232` 注册失败抛 `RuntimeException("Failed to schedule task: " + jobName, e)`。Rust `xxljob/mod.rs:141-147` `shutdown()` 仅 `handlers.clear()`，无 executor 概念（设计决定）；但**注册失败路径**：Rust `schedule_runtime` 无「注册失败」这一环节（纯本地 HashMap insert，永不失败）→ 无对齐目标，属**设计差异**而非缺陷。
- **26. `RegistryEntry.scheduler_type` 被写入为 `scheduler_id`**：`quartz/mod.rs:69` `scheduler_registry().register(sched.scheduler_id.clone(), sched.self_handle_hint())`，而 `self_handle_hint()`（`mod.rs:80-82`）**返回的也是 `scheduler_id` 字符串** → 注册表里 `scheduler_type` 字段存的其实是 id，**类型字段形同虚设**（`registry.rs:12` 文档注释声称是「Human-readable type (e.g. `"quartz"`)」）。Java `QuartzAgentSchedulerRegistry` 存的是**真实 scheduler 引用**（`QuartzAgentSchedulerRegistry.java`：`Map<String, QuartzAgentScheduler>`）→ Rust 注册表**既无类型值也无反向查找能力**，比第一轮描述更弱。

#### 五、第二轮 rollup 建议（最终）

**维持 rollup 状态：`进行中`**（与第一轮一致，且第二轮**强化**该结论，不建议改为 `已完成`）。

理由（更新后）：
1. **P0 双缺陷未被推翻**，证据比第一轮更精确：`quartz/mod.rs:107-120` 从未调用 `with_factory`（生产路径 0 命中），`quartz/task.rs:55` 恒为 `NoopFactory` → Quartz 后端 **定时 fire / 手动 trigger / trait run 三条路径 100% 失败**；`quartz/mod.rs:113-116` 写入 `runtimes` 的 `input` 无读取点 → 初始消息 100% 丢失。
2. **cron 语义差距比第一轮描述更大**：除 `L`/`W`/`LW`/`#`/年/时区/星期基准外，**dom/dow 语义本身分叉**（Java 拒绝 vs Rust OR 触发，第二轮新增 P1-18），无法声称行为对齐。
3. **xxl-job 回调协议未 1:1**（P1-11），且其根因（hooks 不可变）已定位到 `spec.rs:158`（P1-20）。
4. **可观测性缺口是结构性的**：Rust `QuartzScheduleAgentTask` 不暴露 `execution_count()`（P1-21），导致即便修好 P0 也难以写出等价于 Java `QuartzFixedDelayIntegrationTest` 的端到端断言。
5. **统计全部复核通过**（Java 3234/3178、14 类型、180 `@Test`；Rust 14 文件/2416 行/37 测试），本轮未发现第一轮统计造假或口径错误。

**给汇总方的建议动作**（不改代码，仅登记）：
- 在 `EXTENSIONS_GAP_TRACKER.md` 第 17 行把 SCHEDULER 描述由「迁移完成，37 test 通过」改为「**骨架完成 / 37 test 通过；存在 P0 执行链路断裂（factory 未接线 + 初始消息丢弃）与 cron 语义缺口**」。
- 修复优先级建议：**P0① → P0② → P1-21（补 `execution_count()` 公开访问器 + 端到端断言）→ P1-18（cron dom/dow 严格化）→ P1-6（`a/1`）→ P1-4（星期基准）→ P1-5（时区）→ P1-7（非法 cron 显式失败）**。P0①② 修复后 37 个既有测试仍应全绿（它们未断言执行成功），需**新增**回归测试锁死行为。

---

## 7. P0 修复记录（2026-10-22）

### 7.1 修复的缺陷

**P0-1：Quartz 任务未接入 scheduler 的 `AgentFactory`**

- 位置：`crates/agentscope-extensions-scheduler/src/quartz/mod.rs`（`schedule_runtime`）与 `src/quartz/task.rs`（`QuartzScheduleAgentTask::new`）。
- 修复前：`QuartzScheduleAgentTask::new` 内部硬编码 `NoopFactory`；`QuartzAgentScheduler::schedule_runtime` 构造任务时未传 `self.factory`；`with_factory` 仅在 `#[cfg(test)]` 中被调用。定时 fire、`trigger()`、trait `run()` 三条路径都会返回 `No agent factory attached to this scheduled task`。
- 修复：`QuartzScheduleAgentTask::new` 增加 `factory: Arc<dyn AgentFactory>` 参数，`schedule_runtime` 传入 `self.factory.clone()`；删除不再需要的 `NoopFactory`。`with_factory` 保留为公开 API（兼容），但生产路径不再依赖它。
- Java 依据：`QuartzAgentScheduler.java:243-244` 构造任务时传入 `this`，`BaseScheduleAgentTask` 在同一执行内完成 agent 构建；Rust 用注入 factory 等价实现（factory 即 agent 构建入口）。

**P0-2：`schedule_with_input` 的初始消息被丢弃**

- 位置：`src/quartz/mod.rs`、`src/quartz/task.rs`。
- 修复前：`schedule_runtime` 把 `input` 写入 `runtimes: RwLock<HashMap<String, SchedulerRuntime>>`，但全 crate 只写不读；`execute()` 固定调用 `run_with(Vec::new())`，factory 收不到任何初始消息。
- 修复：初始消息改由 `QuartzScheduleAgentTask` 持有（`initial_input: Option<Msg>`），在 `execute()` 中按 `vec![msg]` 重放；新增只读 getter `initial_input()`。删除只写不读的 `runtimes` 字段与 `SchedulerRuntime` 结构。`cancel()` 不再需要清理该 map。
- Java 依据：`QuartzAgentScheduler.java:233-241` 将 `input` 序列化进 Quartz `JobDataMap("inputMsg")`；`AgentQuartzJob.java:67-101` **每次触发**都读取并在非空时调用 `t.run(inputMsg)`。Rust 的 `execute()` 同样在每次自动/手动触发时重放，语义一致（重复调度同名任务仍返回既有任务、忽略新 input，与 Java `:219-222` 一致）。

**P0-3：`QuartzScheduleAgentTask` 缺少执行次数可观测点**

- 位置：`src/quartz/task.rs`。
- 修复：新增 `pub fn execution_count(&self) -> u64`，委托给已有的 `BaseScheduleAgentTask::execution_count()`（`AtomicU64`，`BaseScheduleAgentTask::run_with` 在 factory 调用返回后（无论成功/失败）`fetch_add`）。自动 fire、`trigger()`、trait `run()` 三条路径都经过 `run_with`，计数并发安全（`SeqCst`）。
- Java 依据：`BaseScheduleAgentTask.java:266-268 getExecutionCount()`；计数在 `run()` 调用 agent 后自增（`:164-167`），失败路径同样计数。Rust 现有 `run_with` 行为与之一致（取消/配置校验失败不计）。

**顺带清理（非新增缺口）**：删除 `NoopFactory`、`SchedulerRuntime`、`runtimes` map；未改动 xxl-job（其 `schedule_runtime` 已正确传入 `self.factory`，且 Java `XxlJobAgentScheduler` 本身不支持初始 input，Rust 默认 `schedule_with_input` 与 Java 默认 `UnsupportedOperationException` 对齐）。

### 7.2 新增回归用例

文件：`crates/agentscope-extensions-scheduler/tests/quartz_execution.rs`。使用记录型 `RecordingFactory`（`AtomicUsize` 记调用次数，`Mutex<Vec<Vec<Msg>>>` 记每次收到的消息）。

1. `trigger_and_trait_run_use_injected_factory_and_count`
   - 修复前失败原因：`scheduler.trigger()` 走 `NoopFactory`，返回 `Err("No agent factory attached ...")`，`unwrap()` 直接 panic；且 `execution_count()` 方法不存在（编译失败）。
   - 修复后断言：`trigger` 成功且 factory 调用 1 次、`execution_count() == 1`；再走 trait `task.run(vec![Msg::user("explicit")])`，factory 累计 2 次、计数 2，且 factory 收到的第 2 条消息文本为 `explicit`。
2. `schedule_with_input_delivers_initial_message_to_factory`
   - 修复前失败原因：`trigger()` 先因 NoopFactory 返回 Err；即使只修 P0-1，`execute()` 仍传空 vec，factory 收到 `0` 条消息，对消息文本的断言失败。该用例特意断言消息内容，单独修 P0-1 不足以通过。
   - 修复后断言：factory 被调用 1 次，收到恰好 1 条消息且文本为 `bootstrap`，`execution_count() == 1`。
3. `fixed_delay_firing_reaches_factory`（覆盖自动 job loop，非最低要求但用于锁死定时路径）
   - 修复前失败原因：`FIXED_DELAY` job 循环 fire → `NoopFactory` 报错 → `execution_count()` 始终为 0、factory 调用次数为 0，轮询超时后断言失败。
   - 修复后断言：`initial_delay=10ms`、`fixed_delay=20ms`，2 秒内 `execution_count() >= 1` 且 factory 至少被调用 1 次，无初始 input 时收到 0 条消息。

说明：以上用例均通过公开 API（`QuartzAgentScheduler::schedule_runtime` / `trigger` / `ScheduleAgentTask::run` / `as_any` 下转型）驱动真实 schedule→job→factory 链路，不是只验证新 getter 的空测试。

### 7.3 验证命令与真实结果

```
cargo check -p agentscope-extensions-scheduler
→ Finished `dev` profile [unoptimized + debuginfo] target(s) in 24.03s
→ EXIT=0（仅既有 dead_code 警告：next_seq、JobContext.scheduler_id、run_task、AsAny、interrupt_notify、join）

cargo test -p agentscope-extensions-scheduler
→ 单元测试：test result: ok. 37 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
→ 集成测试 tests\quartz_execution.rs：test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
→ Doc-tests：test result: ok. 0 passed; 0 failed
→ EXIT=0
```

（构建期间出现过 `Blocking waiting for file lock on build directory`，按要求等待，未 kill cargo/rustc。）

### 7.4 未完成项

- ~~P1-21~~ → **已补完（2026-10-28，§8）**：`QuartzScheduleAgentTask` 新增 `schedule_config()` / `agent_config()` / `runtime()`；连同已完成的 `execution_count()` / `initial_input()` 与 Java `getScheduleConfig()` / `getAgentConfig()` / `getExecutionCount()` 对齐。
- P1-18（cron dom/dow）、P1-4/5/6/7（cron 解析细化）→ **已完成**（2026-10-26 §6.1 + 2026-10-28 §8）；P1-22（`join()` 死代码 / shutdown 不等待 job 循环）→ **已完成**（§8：`cancel`/`shutdown` 均 `join().await`）。
- 未发现其它同源 P0：xxl-job 后端 factory 已正确接线，其 `trigger()` 通过 `BaseScheduleAgentTask::run` 正常执行并有既有测试覆盖。
- `schedule_with_input` 的输入在「同名任务重复调度」时仍被忽略（与 Java 返回既有任务的语义一致），未改为覆盖。

### 7.5 裁定：channel 回调控制器注册（不迁移，2026-10-26 登记）

**Java**（`34b209e86`，#3604）：`SchedulerApp` 的 `@ComponentScan` 原先只扫 builder 包，漏掉
`...channel.feishu` / `...wecom` / `...github` / `...gitlab` 四个扩展包，导致 channel 回调
控制器未注册成 bean、线上 404。修复是**把四个包加进扫描列表**，并用 `SchedulerAppContextLoadTest`
断言四个控制器都已注册。

**Rust 侧无对应工作**：Rust 没有 Spring 容器，也不存在「扫描包 → 注册 bean」的隐式装配路径。
控制器（axum router / handler）由调用方在组装 scheduler 服务时**显式挂载**；漏挂会在编译期
或首个请求时暴露，不会出现「编译通过但运行时 404」这类静默失败。

**裁定**：`设计决定（不迁移）`。若将来引入统一的装配入口，应在该入口的初始化测试中断言
四个 channel 路由均存在（对应 Java 的 context load test 意图），而非复刻包扫描。



## 8. 2026-10-28 执行记录（7 项任务）

> 范围：仅 `crates/agentscope-extensions-scheduler/**`（含本 tracker），未改根 `Cargo.toml`、`agentscope-core/**`、`agentscope-harness/**` 与其它 crate；**未新增任何依赖**。
> Java 依据：`agentscope-extensions-scheduler-{common,quartz,xxl-job}`（HEAD `16cb024ae`）。

### 8.1 逐项结果

| # | 任务 | 结果 | 关键实现 | Java 依据 |
|---|---|---|---|---|
| 1 | cron 扩展关键字 `L` / `W` / `LW` / `#` + 年字段 | ✅ 已完成 | `cron.rs` 重构：`DayOfMonthRule{Values,Last,LastOffset,LastWeekday,NearestWeekday}` + `DayOfWeekRule{Values,LastInMonth,NthInMonth}`；7 字段年（`1970-2199`）；`a/1` 展开；`?` 仅限日字段；`next_after` 改字段级跳跃 | `QuartzAgentScheduler.java:272-284`（`CronScheduleBuilder.cronSchedule` + `inTimeZone`）、Quartz 2.3.2 `CronExpression.buildExpression/getTimeAfter` |
| 2 | `next_after` 生产零调用 | ✅ 已完成（**接线 + 查询 API 双做**） | `job::CronPlan::next_fire_time` → CRON 循环 `sleep` 到下次触发（替换 1s 轮询），无未来触发 → `Complete`；新增 `QuartzAgentScheduler::next_fire_time(name)`、`trigger_state(name)`、`QuartzScheduleAgentTask::next_fire_time(from)` | `Trigger#getNextFireTime` / `CronExpression#getNextValidTimeAfter`；`QuartzAgentScheduler.java:535-551` |
| 3 | `interrupt()` 不中止 in-flight | ✅ 已完成 | `QuartzScheduleAgentTask` 增 `watch::Sender<u64>` 中断代次；`execute_with` 用 `select!` 与执行竞争，中断即丢弃执行 future 并返回 `Error::Interrupted`；`interrupted_count()` 可观测；FIXED_DELAY 被中断则跳过重排；`QuartzAgentScheduler::interrupt` 返回「是否真有 in-flight」 | `AgentQuartzJob.java:39/49-51/107-109/121-123`（`InterruptableJob`）、`QuartzAgentScheduler.java:479-503`（`scheduler.interrupt`） |
| 4 | `get_status` 3 值 → Java 6 值 | ✅ 已完成 | `TaskStatus::{None,Normal,Paused,Complete,Error,Blocked,Cancelled}` + `as_str()` + `is_terminal()`（`None/Complete/Cancelled`）；`get_status` 保留 `Option`，另加 Java 同形 `trigger_state() -> TaskStatus` | `QuartzAgentScheduler.java:535-551` 返回 `Trigger.TriggerState`（NONE/NORMAL/PAUSED/COMPLETE/ERROR/BLOCKED） |
| 5 | xxl-job 协议缺口 | ⚠️ 已完成（1 处因 core 限制降级，见 §4.2-5） | 新增 `src/xxljob/context.rs`：`XxlJobContext`（job_id/job_param/shard_index/shard_total/log_id/日志/handle 结果）+ `XxlJobHelper`（`log`/`handle_success`/`handle_fail`/`get_job_*`/`get_shard_*`/`get_log_id`），以 `tokio::task_local!` 模拟 Java ThreadLocal；`trigger_with_context` 在作用域内执行：job param → user msg、成功 `log(reply)`+`handleSuccess`、失败 `handleFail` | `XxlJobAgentScheduler.java:253/265`（`getJobParam`）、`:282`（`handleSuccess`）、`:189-201`（日志 Hook） |
| 6 | `cancel` 语义 | ✅ 已完成 | quartz：空名/不存在 → `false`（+warn），存在 → `shutdown()` + `join().await`（job 循环与 in-flight 均终止）→ `true`；xxl-job：新增 `try_cancel() -> Result` 返回 Java 原文 `UnsupportedOperationException` 文案，trait 新增默认 `AgentScheduler::try_cancel`（xxl-job override 为 Err），布尔 `cancel` 保留 `false`+warn | `QuartzAgentScheduler.java:321-345`、`XxlJobAgentScheduler.java:299-304` |
| 7 | `execution_count()` 未暴露 | ✅ 已完成（P0-3 已暴露，本轮补同族访问器） | `execution_count()` 已有；新增 `schedule_config()`/`agent_config()`/`runtime()`，并加 `config_accessors_are_exposed` 回归 | `BaseScheduleAgentTask.java:266-268`、`QuartzFixedDelayIntegrationTest.java:56` |

### 8.2 新增 / 变更测试（36 个新测试，全部离线）

| 文件 | 新增测试 |
|---|---|
| `src/quartz/cron.rs` | `last_day_of_month`、`last_day_offset`、`last_weekday_of_month`、`nearest_weekday_of_month`、`nearest_weekday_does_not_cross_month_boundary`、`nearest_weekday_never_fires_when_day_is_missing`、`last_day_of_week_in_month`、`nth_day_of_week_in_month`、`nth_day_of_week_skips_months_without_it`、`bare_last_day_of_week_means_saturday`、`rejects_malformed_day_extensions`、`year_field_limits_firing`、`year_field_range_matches_quartz`、`year_field_next_after_skips_years`、`step_without_range_expands_even_for_step_one`、`question_mark_only_allowed_in_day_fields`、`next_after_is_strictly_after`（17） |
| `src/quartz/job.rs` | `next_fire_time_honours_the_configured_zone`、`next_fire_time_returns_none_when_years_are_exhausted`（2） |
| `src/quartz/task.rs` | `config_accessors_are_exposed`、`status_reports_error_after_failed_execution`、`interrupt_aborts_in_flight_execution`、`interrupt_while_idle_does_not_abort_the_next_run`、`completed_task_is_terminal_and_has_no_next_fire_time`（5） |
| `src/quartz/mod.rs` | `next_fire_time_query_for_cron_task`、`trigger_state_covers_quartz_states`、`interrupt_reports_whether_a_run_was_in_flight`、`cron_loop_fires_on_next_fire_time`（端到端：每秒钟 CRON 真的触发 ≥2 次）、`cancel_aborts_the_in_flight_execution`、`cancel_waits_for_the_job_loop_to_stop`（6） |
| `src/xxljob/mod.rs` | `try_cancel_mirrors_java_unsupported_operation`、`trigger_passes_job_param_and_reports_success`、`trigger_reports_handle_fail_on_error`（3） |
| `src/xxljob/context.rs` | `helper_resolves_the_scoped_context`、`helpers_are_noops_outside_a_scope`、`fail_code_is_500`（3） |

既有测试的唯一改动：`pause_resume_status_transitions` / `pause_resume_and_status` 中 `TaskStatus::Scheduled` → `TaskStatus::Normal`（为对齐 Quartz `TriggerState.NORMAL` 而重命名变体；断言强度不变）。未删除任何测试、未加 `#[ignore]`。

### 8.3 验证命令与实测

```
cargo test -p agentscope-extensions-scheduler
→ lib:                       test result: ok. 88 passed; 0 failed; 0 ignored
→ tests/quartz_execution.rs: test result: ok.  3 passed; 0 failed; 0 ignored
→ Doc-tests:                 test result: ok.  0 passed; 0 failed
→ EXIT=0（合计 91 passed / 0 failed；基线 55 全绿保留）

cargo clippy -p agentscope-extensions-scheduler --all-targets
→ 本 crate 仅 2 条既有 warning（module_inception / very_complex_type），0 error
→ 本轮删除死脚手架 next_seq / AsAny / interrupt_notify / JobContext::run_task

cargo fmt -p agentscope-extensions-scheduler（已跑，无额外改动）
```

### 8.4 反向验证（逐项回退 → 确认 FAILED → 恢复）

| 回退的实现 | 期望失败的测试（实测均 FAILED） |
|---|---|
| `CronPlan::next_fire_time` 恒返回 `None` | `quartz::job::tests::next_fire_time_honours_the_configured_zone`、`next_fire_time_returns_none_when_years_are_exhausted`、`quartz::tests::next_fire_time_query_for_cron_task`、`quartz::tests::cron_loop_fires_on_next_fire_time` |
| `execute_with` 去掉 `select!`（直接 await，不响应中断） | `quartz::task::tests::interrupt_aborts_in_flight_execution`（1s timeout `Elapsed`） |
| `cancel` 去掉 `task.join().await` | `quartz::tests::cancel_aborts_the_in_flight_execution`（`left: 1`，in-flight 执行存活） |
| `DayOfMonthRule::Last` / `NearestWeekday` / `DayOfWeekRule::NthInMonth` 返回 false | `last_day_of_month`、`nearest_weekday_of_month`、`nearest_weekday_does_not_cross_month_boundary`、`nth_day_of_week_in_month`、`nth_day_of_week_skips_months_without_it` |

恢复后重跑：88 + 3 passed / 0 failed（§8.3）。

### 8.5 注记

- **任务示例表达式勘误**：需求给出的 `0 0 L * * ?`、`0 0 15W * * ?` 是 6 字段（Quartz `sec min hour dom mon dow`），第三个字段是**小时**，把 `L`/`15W` 放在小时位上 Quartz 同样会拒绝。正确写法为 `0 0 0 L * ?`、`0 0 0 15W * ?`，本轮测试即按此编写；`0 0 12 ? * 6#3`（第三个星期五 12:00）本身合法，已直接覆盖。
- **行为变更**：`?` 现仅允许出现在 dom/dow（其他字段报错）→ 历史表达式若在非日字段使用 `?`，需改写为 `*`。
- **Java 双计数问题未复刻**：Java `executeAgent` 在 `run()` 之后又调 `incrementExecutionCount()`（`XxlJobAgentScheduler.java:272`），实际每次执行 +2；Rust 保持 +1（`BaseScheduleAgentTask::run_with` 内），属**有意偏离**，已在 §4.2 记录。
- **编译环境注记**：本轮验证期间 `agentscope-extensions-model` 正被并行会话重构，出现临时编译失败（与本 crate 无关），等待其稳定后重跑；未改动任何本 crate 之外的文件。
