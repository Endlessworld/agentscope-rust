# SCHEDULER 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **SCHEDULER** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-scheduler\`
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
| Java 规模（初测） | 主代码 ~3009 行 / 测试 ~2589 行 |
| Rust crate | `crates/agentscope-extensions-scheduler` |
| 迁移波次 | C |
| 当前状态 | 进行中 |

## 2. Java 清单与依赖映射

### 2.0 子模块结构
Java 聚合模块 = 1 个 Rust crate；3 个 Java 子模块映射为 crate 内 3 个模块（`common` / `quartz` / `xxljob`，对齐 `EXTENSIONS_MIGRATION_PLAN.md` §1「子模块以 `src/<sub>.rs` 体现」）。
**`common` 抽象为本 crate 内共享层**（`src/common/`），quartz / xxljob 两个后端均实现其 trait；不新建独立 crate、不放到 core/harness。

| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `scheduler.AgentScheduler`（interface） | 调度器核心契约：schedule/cancel/getScheduledAgent/getAllScheduleAgentTasks/shutdown/getSchedulerType | `common::scheduler::AgentScheduler`（async_trait） | 直接映射；`schedule` 用 `Arc<dyn AgentConfig>` 取代 Java `AgentConfig`/`RuntimeAgentConfig` 双形态 |
| `scheduler.ScheduleAgentTask<T>`（interface） | 任务句柄：getId/getName/run/cancel | `common::task::ScheduleAgentTask`（async_trait） | `Mono<T>` → `async fn run(Vec<Msg>) -> Result<Msg>`；泛型 T 收敛为 `Msg` |
| `scheduler.BaseScheduleAgentTask` | 默认实现：UUID id、cancelled/executionCount 原子量、每次执行新建 agent、createAgent 校验 | `common::task::BaseScheduleAgentTask` | 每次执行经 `AgentFactory` 新建 `ReActAgent`；`AtomicBool`/`AtomicU64` 对齐 |
| `config.AgentConfig` | 可序列化元数据（name/sysPrompt/modelConfig），builder + 校验 | `common::config::AgentConfig` | serde 往返；name/modelConfig 必填（校验对齐） |
| `config.RuntimeAgentConfig` | 追加运行时对象（model/toolkit/hooks） | `common::config::RuntimeAgentConfig` | Rust 侧用 `Arc<dyn Model>` + `Toolkit` + `Vec<Arc<dyn Middleware>>`（Rust 无 hooks，走 middleware）；**继承改为内嵌**（Rust 无类继承） |
| `config.ModelConfig`（interface） | getModelName + createModel | `common::config::ModelConfig`（trait） | `create_model() -> Arc<dyn Model>`（Java `Model` → core `Model` trait） |
| `config.DashScopeModelConfig` | DashScope 模型配置实现 | `common::config::ModelSpec`（占位实现） | Rust 无 DashScopeChatModel（属 MODEL 模块，未迁移）；提供 `ModelSpec` 抽象 + 由 `Arc<dyn Model>` 注入，见 §4.2 |
| `config.ScheduleConfig` | 调度参数（mode/cron/fixedRate/fixedDelay/initialDelay/zoneId），builder + 按 mode 校验 | `common::config::ScheduleConfig` | serde 往返；ms 语义对齐（Java Long ms → Rust `Option<u64>`） |
| `config.ScheduleMode` | 枚举 NONE/CRON/FIXED_RATE/FIXED_DELAY | `common::config::ScheduleMode` | serde rename 对齐 Java 枚举名 |
| `quartz.QuartzAgentScheduler` | Quartz 实现：Job/Trigger、pause/resume/interrupt/getStatus、集群回读 | `quartz::QuartzAgentScheduler` | Rust 无 Quartz；**用 tokio 定时任务实现等价语义**（cron 用 `tokio-cron-scheduler` 的 cron 解析；fixed_rate/fixed_delay 用 `tokio::time::interval`/`sleep`）。pause/resume/interrupt/getStatus 保留 |
| `quartz.AgentQuartzJob` | Quartz Job：查 scheduler → 取 task → run（FIXED_DELAY 完成后重排） | `quartz::AgentJob` | 定时循环体内执行；FIXED_DELAY 在 run 完成后 sleep 再排下次 |
| `quartz.QuartzAgentSchedulerRegistry`（package-private） | schedulerId → scheduler 静态注册表 | `quartz::SchedulerRegistry` | 进程内 `RwLock<HashMap>` 单例；Rust 无 JVM 静态，用 `OnceLock` |
| `quartz.QuartzScheduleAgentTask`（package-private） | 包装 JobKey + Quartz Scheduler，pause/resume | `quartz::QuartzScheduleAgentTask` | 包装 `TaskHandle`（JoinHandle + 控制通道），pause/resume/reschedule |
| `xxljob.XxlJobAgentScheduler` | XXL-Job 实现：注册 JobHandler（仅支持 NONE）、由控制台触发、cancel 抛 Unsupported | `xxljob::XxlJobAgentScheduler` | Rust 无 xxl-job SDK；提供 **JobHandler 注册表 + 本地触发 `trigger(name, params)`**，对齐「只注册 handler、调度由外部控制台决定」语义 |

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| reactor-core (Mono/Flux) | 异步结果 | `tokio` async/await（`async fn -> Result<Msg>`） | 已在本 crate Cargo.toml 声明 |
| quartz-scheduler | 调度引擎 | `tokio-cron-scheduler`（cron 解析）+ `tokio::time`；**不引 Quartz C 绑定** | 见 §4.2；cron 用 `cron` 表达式解析 |
| xxl-job-core | 分布式调度 | 无 SDK；自建 `JobHandlerRegistry` + 本地 `trigger` | JobHandler 名 = agent name |
| jackson-databind | JSON 序列化（inputMsg 存 JobDataMap） | `serde` / `serde_json` | 已声明 |
| slf4j | 日志 | `tracing` | 已声明 |
| java.util.UUID | 任务 id | `uuid`（workspace v1） | 需在本 crate `[dependencies]` 补 `uuid = { workspace = true }` |
| java.time / TimeZone | zoneId | `chrono-tz` 或 `tokio-cron-scheduler` 的 `chrono` 集成 | zoneId 存字符串；cron 时区为 P2，见 §4.2 |
| agentscope-core (Model/Toolkit/Hook/ReActAgent/Msg) | 核心类型 | `agentscope-core`（已声明） | `Model`/`Toolkit`/`Msg`/`ReActAgent` 直接复用 |
| agentscope-extensions-model-dashscope | DashScopeChatModel | **无对应**（MODEL 模块未迁移该 provider） | 依赖注入 `Arc<dyn Model>`，见 §4.2 |

## 3. 详细缺口清单
### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 依赖（uuid, tokio-cron-scheduler/chrono, async-trait 等） | pom.xml ×3 | 进行中 | 只写本 crate `[dependencies]`；不改根 `[workspace.dependencies]` |
| `common` 共享层骨架（本 crate 内） | scheduler-common | 进行中 | `src/common/{mod,config,scheduler,task}.rs` |
| ScheduleMode / ScheduleConfig | 同名 | 进行中 | serde + builder + 按 mode 校验（6 个 Java 测试对应） |
| AgentConfig / RuntimeAgentConfig / ModelConfig | 同名 | 进行中 | RuntimeAgentConfig 内嵌 AgentConfig |
| AgentScheduler / ScheduleAgentTask trait | 同名 | 进行中 | async_trait；泛型 T→Msg |
| BaseScheduleAgentTask | 同名 | 进行中 | 每次执行新建 agent + 计数 + cancel |

### 3.2 quartz 后端（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| QuartzAgentScheduler schedule/cancel/getAll/shutdown/getSchedulerType | QuartzAgentScheduler(26955B) | 进行中 | tokio 定时循环 + cron 解析 |
| CRON / FIXED_RATE / FIXED_DELAY 三模式 | 同名 | 进行中 | cron 表达式解析；fixed_rate=interval；fixed_delay=完成后 sleep |
| pause / resume / interrupt / getStatus | 同名 | 进行中 | 控制通道 + `AtomicBool` paused；status 枚举 |
| SchedulerRegistry | QuartzAgentSchedulerRegistry | 进行中 | `OnceLock<RwLock<HashMap>>` |
| QuartzScheduleAgentTask | 同名 | 进行中 | 包装 JoinHandle/控制句柄 |
| AgentJob | AgentQuartzJob | 进行中 | 循环体；inputMsg serde 往返 |

### 3.3 xxl-job 后端（P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| XxlJobAgentScheduler（JobHandler 注册 + 本地 trigger） | XxlJobAgentScheduler(15762B) | 进行中 | 仅 NONE 模式；cancel 抛 Unsupported（对齐 Java） |
| JobHandler 注册表 | XxlJobExecutor.registryJobHandler | 进行中 | 进程内注册表 |

## 4. 项目状态与下一步
### 4.1 当前状态
- **测试修复完工（2026-09-18）**：`cargo test -p agentscope-extensions-scheduler` 由「lib test 编译失败（4 个错误）」修至**全绿 37 passed / 0 failed**；`cargo build -p agentscope-extensions-scheduler` 通过（仅 7 条 warning，无 error）。
- **改动文件（仅本 crate）：**
  1. `src/quartz/mod.rs`：测试模块补 `use crate::common::config::ScheduleMode;`（修 `E0433: cannot find type 'ScheduleMode'`；类型本存在于 `common/config/schedule.rs:13`，未新建平行 enum）。
  2. `src/common/scheduler/task.rs` / `src/quartz/task.rs` / `src/xxljob/mod.rs`：`Msg::get_text_content()` → `Msg::text_content()`（3 处，修 `E0599`；正确 API 见 `agentscope-core/src/message/msg.rs:260`），其余逻辑未动。
  3. `src/quartz/cron.rs`：修 step 语法解析 bug——`parse_field` 对「裸 `a/step`（无范围）」原按单值 `(a,a)` 处理，导致 `0 0/30 * * * ?` 只匹配第 0 分钟，取数 `every_thirty_minutes` 断言失败；现按 cron/Quartz 语义展开为 `a..max`（step>1 时），`a`/`a/1` 仍为单值。
- **验证命令与结果：**
  - `cargo build -p agentscope-extensions-scheduler` → 成功（0 error）。
  - `cargo test -p agentscope-extensions-scheduler` → `test result: ok. 37 passed; 0 failed`（lib）+ doc-tests 0 failed。
- 未删除任何测试或功能；未改动根 `Cargo.toml`、core/harness 及其它 crate。

### 4.2 下一步与阻塞
1. 无阻塞。crate 依赖（§3.1 uuid / async-trait 等）如仍需补 `[workspace.dependencies]`，由汇总方处理（本 crate 未新增依赖）。
2. 建议后续：cron `next_after` 当前按秒扫描（上限约 4 年 × 每分钟），若后续追求性能可优化为字段跳跃；非阻塞。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-scheduler` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-scheduler` / `cargo test -p agentscope-extensions-scheduler` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-scheduler/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

