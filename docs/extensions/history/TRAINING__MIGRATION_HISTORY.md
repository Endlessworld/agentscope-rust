# TRAINING 迁移执行历史（归档）

> 本文件是 [`../TRAINING__GAP_TRACKER.md`](../TRAINING__GAP_TRACKER.md) 的执行历史归档（截止 2026-10-31）。
> **现行状态以 tracker 为准**；本文件只读取证，不再更新。
> 内容来源：原 GAP_TRACKER 中 §5 之后各节，原文未改写（节标题：6. 静态复核记录（2026-10-22） · 7. §4.3 P0 修复记录（2026-10-22，汇总方） · 8. 2026-10-28 执行记录（P1-1 / P1-4 / P1-2 / P1-3 / P2-2 / P2-4） · 9. 2026-10-31 收口（注册表去全局化 / 队列容量 / hook 卸载））。

## 6. 静态复核记录（2026-10-22）

### 6.1 复核方式
- **纯静态核对，未编译、未运行 `cargo build` / `cargo test`**，未修改任何 Rust 代码；本节唯一写入文件为本 tracker。
- 方法：PowerShell 逐文件行数统计（含 `Get-Content`/`Measure-Object -Line`/`[IO.File]::ReadAllLines` 三种口径交叉验证）+ `Select-String` 模式检索（`#[test]`/`#[tokio::test]`/`@Test`/`import`/`class|interface|enum|record`/`sharegpt|conversations|trajectory` 等）+ 逐类读取 Java 源码与 Rust 源码做行为对照 + codegraph 索引交叉验证。
- 口径说明：**总行 = 物理行（含空行/注释）**；**有效行 = 剔除空行、纯注释行（`//` 与 `/* */` 块）、`import`/`package` 行**（该口径可完全复现 tracker 原文的 1463 / 2008）。

### 6.2 实测统计
| 指标 | 原 tracker 声明 | 实测 | 结论 |
|---|---|---|---|
| Java 主代码行 | 3275（有效 1463） | 20 文件 / 3275 总行 / 1463 有效行 | ✅ 一致 |
| Java 测试行 | 3457（有效 2008） | 19 文件 / 3457 总行 / 2008 有效行 | ✅ 一致 |
| Java 主类数 | 20 顶层 + 8 嵌套 | 20 顶层（`class`/`interface` 各计）+ 8 嵌套（Builder×5、`RegistryStats`、`CopyOnWriteArrayList` 引用不计） | ✅ 一致 |
| Java 测试类 | 17 | 17 个含 `@Test` 的测试类 + 2 个工具类（`TrainingTestConstants`/`TrainingTestUtils`，无 `@Test`） | ✅ 一致 |
| Java `@Test` 数 | 166 | 166（逐文件求和一致） | ✅ 一致 |
| Rust 文件数 | 5 | 5（`backend.rs`/`lib.rs`/`reward.rs`/`runner.rs`/`strategy.rs`） | ✅ 一致 |
| Rust 总行 | 3079 | 3079（`ReadAllLines` 口径）；非空 2767 | ✅ 一致 |
| Rust 各文件行 | 748/31/107/1473/720 | 748/31/107/1473/720 | ✅ 一致 |
| Rust 测试数 | 64 | `#[test]` 57 + `#[tokio::test]` 7 = **64**（backend 15 / reward 3 / runner 28 / strategy 18） | ✅ 一致；且无 `#[ignore]`、无 doc-test |
| 「64 test 通过」 | 64 passed | 源码内测试函数数 **恰好 64**，分布与声明吻合 | ✅ 与代码自洽（未重跑 cargo） |
| Java 类 → Rust 映射 | 21 类全覆盖 | 20 个 Java 主类 + `util/JsonUtils`（引用）**全部有真实 Rust 对应**且存在同名可定位实现 | ✅ 无虚假映射 |

### 6.3 对齐结论表
| Java 类 | Rust 对应 | 行为覆盖判定 |
|---|---|---|
| `backend/dto/CommitRequest` | `backend::dto::CommitRequest` | ✅ 字段名 snake_case 一致；**差异**：Java Jackson 默认序列化 `null` 字段，Rust `skip_serializing_if = "Option::is_none"` **省略 null** → 请求体字段集不同（Java 会发 `"task_id":null`，Rust 不发）。Trinity 侧若按缺省容忍则无碍 |
| `backend/dto/FeedbackRequest` | `backend::dto::FeedbackRequest` | ✅ 同上（`msg_ids`/`reward`/`task_id`/`run_id` 对齐） |
| `backend/dto/StatusResponse` | `backend::dto::StatusResponse` | ✅ `is_success()` 复刻 `"success".equalsIgnoreCase`（Rust 用 `eq_ignore_ascii_case`，对 ASCII 等价） |
| `backend/TrinityClient` | `backend::TrinityClient` | ✅ `/feedback`、`/commit` POST + 非 2xx → `Error`；❌ 差异见 P0-2（模型侧非流式）；另 Java 用 OkHttp 三段 timeout（connect/read/write），Rust 用 reqwest 单 `timeout`（整体超时）——P2 级差异 |
| `backend/TrinityModelAdapter` | `backend::TrinityModelAdapter` | ❌ **不 1:1**（P0-2）：Java 强制非流式 → 1 msg_id/run；Rust 恒流式 → 同一 id 按 chunk 数重复写入 |
| `reward/RewardCalculator` | `reward::RewardCalculator` | ✅ trait 语义一致（Java 参数 `Agent` → Rust `&ReActAgent` 具体类型，已在 §4.1 记为设计决定） |
| `runner/AgentCloner` | `runner::AgentCloner::clone_with_model` | ❌ 部分：name/description/toolkit/`-shadow` 后缀/空 memory 对齐；`sys_prompt`/`max_iters` **默认丢失**（P1-1）；3 个 ExecutionConfig 字段未迁移（P1-3）。另 Rust 多一行 `.todo_tool(false)`——与 Java `taskListEnabled` 默认 false 等价，非偏差 |
| `runner/RunExecutionContext` | `runner::RunExecutionContext` | ✅ 字段/方法/异常语义一致（空 taskId/runId → panic 对应 Java `IllegalArgumentException`）；`eq`/`hash`/`Display` 格式一致；⚠️ Rust **额外**在 shadow 执行后 `set_messages(...)`（Java 不填 `msgs`）→ Rust 更完整，非缺陷 |
| `runner/RunRegistry` | `runner::RunRegistry` | ✅ 自增 runId / cleanup / clearAll / size / `cleanup_old_tasks` no-op 全部对齐；⚠️ Java `allocateRunId(null)` 抛异常，Java `allocateRunId("")` **正常分配**；Rust 空串也 panic → Rust 更严（P2 级） |
| `runner/TaskExecutionRegistry` | `runner::TaskExecutionRegistry` | ✅ 注册/按 runId 数值排序/单查/ids/计数/cleanup/clearAll/`RegistryStats` 全部对齐；⚠️ Java `Integer.parseInt(runId)` 非数字会抛，Rust `unwrap_or(0)` 更宽容（P2 级） |
| `runner/TaskIdGenerator` | `runner::task_id_generator` | ✅ `task-{uuid}` / `task-{ts}-{rand8}` 格式与随机位数一致 |
| `runner/TrainingConfig` | `runner::TrainingConfig`(+Builder) | ✅ 默认值逐项一致（apiKey `dummy`、model `training-model`、interval 300s、timeout 300s、autoCommit true、pool 10/1000、repeatTime 1、默认 10% 采样）；❌ `enable_auto_commit` **生效语义不一致**（P1-4）；⚠️ `shadow_pool_size`/`shadow_pool_capacity` 存值未生效（P2-2） |
| `runner/TrainingRouter` | `runner::TrainingRouter` | ✅ 语义调整已声明：`-shadow` 递归保护、selection 分支、`taskId` 透取 metadata、repeatTime 循环 + runId 自增 + 末次 cleanup、msgIds 空则跳过 reward/feedback、reward 后再 feedback、feedback 成功才注册 registry —— **逐条与 Java `handlePostCall`/`runShadowAgent` 对齐**；⚠️ Java 为异步非阻塞（boundedElastic subscribe），Rust `run_shadow` 为阻塞 await（P2-2 同源） |
| `runner/TrainingRunner` | `runner::TrainingRunner` | ✅ start/stop 幂等、周期 commit（跳过首次 tick，对齐 `scheduleAtFixedRate` 的 initialDelay）、stop 时最终 commit 一次；❌ `enable_auto_commit` 差异（P1-4）；❌ 无 `addSystemHook`/`removeSystemHook` 等价物（P1-2，已记） |
| `strategy/TrainingSelectionStrategy` | `strategy::TrainingSelectionStrategy` | ✅ trait 方法集一致（`should_select`/`priority` 默认 100/`name`）；⚠️ `name()` 默认值 Java 为 `getClass().getSimpleName()`，Rust 为固定 `"TrainingSelectionStrategy"`（内置策略均已覆写，影响面小，P2 级） |
| `strategy/SamplingRateStrategy` | `strategy::SamplingRateStrategy` | ✅ 概率比较 `<`、accept reason `sampling-rate` + label `sampled`、reject reason `not-sampled`、priority 200、name 格式、`getSampleRate` 全一致；PRNG 实现不同（Java `java.util.Random` vs Rust xorshift）——分布等价、可接受 |
| `strategy/ExplicitMarkingStrategy` | `strategy::ExplicitMarkingStrategy` | ✅ 无标记→`no-explicit-marking`、过期→`explicit-marking-expired`、accept reason `explicit-marking`、labels 透传、metadata 合并 + `taskId` 注入、priority 10、name `ExplicitMarking`、默认 TTL 1 分钟 —— **完全一致** |
| `strategy/SelectionDecision` | `strategy::SelectionDecision` | ✅ accept/reject 工厂、labels/metadata 不可变语义、reason —— 一致 |
| `strategy/TrainingAnnotation` | `strategy::TrainingAnnotation` | ✅ builder 字段集（enabled/taskId/labels/metadata/timestamp）、静态工厂（`enabled`/`withLabels`/`withLabelsAndMetadata`）、`isExpired`（`>` 边界一致）—— 一致；Rust 额外 `Serialize/Deserialize` |
| `strategy/TrainingContext` | `strategy::TrainingContext` | ✅ 四个 `mark` 重载（无参/labels/labels+metadata/metadata-only）语义一致；⚠️ Java 是 Reactor `Context` 写入函数（隐式传播），Rust 改为显式携带（已在 §4.1 记为设计决定） |
| `util/JsonUtils`（引用） | `serde_json` 直用 | ✅ 无独立类需迁；Java `JacksonJsonCodec` 的 `FAIL_ON_UNKNOWN_PROPERTIES=false` 对应 Rust `serde` 默认忽略未知字段 |

### 6.4 新发现缺口（本轮复核产出）
| 编号 | 级别 | 缺口 | 证据 | 建议 |
|---|---|---|---|---|
| P0-1 | **P0**（结论为「非缺口」，但需登记以免重复追查） | 「训练数据生成 / 格式转换（ShareGPT / OpenAI-messages）」与「轨迹采样」两域在 Java 侧**不存在** | 全仓 `*.java` 检索 `sharegpt`/`ShareGPT`/`conversations`/`toTrainingData`/`trainingSample`/`sft`/`rft` → Java 源码 0 命中（`conversations` 的 50 处命中全在 node_modules `package-lock.json`）。数据格式化由外部 Trinity-RFT（Python）承担 | **不建 Rust 对应物**；若产品要求 Rust 侧直接产出 ShareGPT/OpenAI-messages，属新增需求（Java 亦无），须单独立项 |
| P0-2 | **P0** | `TrinityModelAdapter` msg_id 采集与 Java 不 1:1：Rust 恒流式 → 同一 completion id 按 chunk 数**重复**写入 `msg_ids`；且向 Trinity 发 `stream: true`（Java 显式规避） | Java `TrinityModelAdapter.java:177` `.stream(false)`；Rust `agentscope-extensions-model/src/openai/mod.rs:381` `stream: Some(true)` 硬编码，`options.stream` 在该 provider 内从未被读取；`StreamIncrementer.accumulate` 中 `self.id` 仅首个非空 chunk 赋值、后续 chunk 复用同 id；`backend.rs:303-305` 对每个 response 都 `add_msg_id` | 需在 `OpenAiChatModel` 暴露 stream 开关（或 `TrinityModelAdapter` 内强制非流式 + id 去重）。**本轮不改代码**，登记待办 |
| P1-1 | P1 | `AgentCloner` 丢失 `sys_prompt`/`max_iters`（`ShadowAgentSpec::default()` → 空 prompt + 默认 max_iters 10） | core `react_agent.rs:283` `pub fn sys_prompt()`、`:288` `pub fn max_iters()` **已存在**；`runner.rs:629-634` 仅在 spec 为 `Some` 时设置 | 直接回读 `original.sys_prompt()`/`original.max_iters()`，无需改 core |
| P1-3 | P1 | `modelExecutionConfig`/`toolExecutionConfig`/`toolExecutionContext` 未迁移 | Java `ReActAgent.java:5625/5630/5635` 有三个 getter；Rust `AgentCore` 无对应字段、builder 无 setter | 需 core 支持或归档为「设计决定（不迁移）」 |
| P1-4 | P1 | `enable_auto_commit` 生效语义不一致 | Java `startCommitScheduler()` 只判 `interval<=0`，`enableAutoCommit` 从未被读（Java 侧为死配置）；Rust `runner.rs:907` 判 `interval<=0 \|\| !enable_auto_commit` | 裁定统一口径 |
| P2-2 | P2 | `shadow_pool_size`/`shadow_pool_capacity` 存值未生效 | 全 crate 检索仅命中定义/getter/builder/测试断言，无消费点；Java 用于 `Schedulers.newBoundedElastic` | 实现限流调度，或明确标注「预留、由调用方自行 spawn」 |
| P2-4 | P2 | `Cargo.toml` 4 个依赖零引用（`agentscope-harness`/`chrono`/`async-trait`/`thiserror`） | `src/**/*.rs` 引用计数均为 0 | 移除，缩小编译面 |

### 6.5 勘误列表（原文 → 实际）
| # | 位置 | 原文 | 实际 |
|---|---|---|---|
| 1 | §1 模块概览 | `20 个顶层类（+8 个嵌套类）；17 个测试类、166 个 @Test` | 表述可保留但需限定：嵌套类实为 **5 个 Builder + 1 个 RegistryStats = 6 个**（`CopyOnWriteArrayList` 等为 JDK 类型非嵌套类）；测试文件 19 个 = 17 个含 `@Test` 的测试类 + 2 个工具类（无 `@Test`） |
| 2 | §1 模块概览 | `Rust 规模：5 个源文件 / 3079 行` | ✅ 正确，但需补「非空 2767 行」（`Measure-Object -Line` 口径）以免与 rollup 的 2767 混淆 |
| 3 | §2 `AgentCloner` 行 + §4.2 P1 | 「`sysPrompt`/`maxIters` 不可回读，core 无 getter」 | **不成立**：core 已有 `ReActAgent::sys_prompt()` / `ReActAgent::max_iters()`。真实问题是**本 crate 未回读**（P1-1） |
| 4 | §4.1 已知差异 | 「core 无通用 `Hook`/`PreCallEvent`/`PostCallEvent` 系统（仅有语义不同的 `middleware::Middleware`）」 | **部分不准确**：core `middleware::Middleware` 有 `on_pre_call`，注释明确「Mirrors Java `PreCallEvent`」。准确表述应为「core 无 post-call 钩子，`PreCall` 有等价物」 |
| 5 | §4.2 首句 | 「**P0：无。**」 | **不成立**：本轮新增 P0-2（msg_id 采集不 1:1）；另 P0-1 为「经核实 Java 亦无、故非缺口」的登记项 |
| 6 | §3.6 测试行 | 「strategy 12 / backend 9 / reward 2 / runner 21」 | **实际**：strategy **18** / backend **15** / reward **3** / runner **28**（合计仍为 64，故总数无误但分模块明细错误） |
| 7 | §2.1 依赖映射 | 「shadow 跑在 tokio 任务，池大小由 Tokio 运行时承载」 | 掩盖事实：`shadow_pool_size`/`shadow_pool_capacity` **配置值未被任何代码消费**（P2-2） |
| 8 | §4.1 依赖 | 「仅本 crate `Cargo.toml` 新增 `reqwest`、`uuid`」 | 事实无误，但该 `Cargo.toml` 另有 4 个零引用依赖（`agentscope-harness`/`chrono`/`async-trait`/`thiserror`），P2-4 |
| 9 | §2 `TrainingConfig` 行 | 「默认值对齐」 | 默认值逐项对齐 ✅，但 `enable_auto_commit` 的**生效语义**与 Java 不一致（P1-4），原表述未区分 |
| 10 | §3.4 backend | 「DTO（…）snake_case serde」 | 字段名对齐 ✅，但 Java Jackson 会序列化 `null` 字段、Rust `skip_serializing_if` 省略 → **请求体字段集不同**，原表述未说明 |
| 11 | §4.1 | 「21 个 Java 类全部有 Rust 对应」 | ✅ 成立（20 主类 + `util/JsonUtils` 引用项），无虚假映射；本轮逐类核对均找到真实实现 |

### 6.6 复核结论
- **规模类声明全部可复现**：Java 2890/2924（`Measure-Object -Line` 口径）与 3275/3457（`ReadAllLines` 口径）、有效行 1463/2008、20 主类 + 17 测试类 + 166 `@Test`、Rust 5 文件 3079 行 64 test —— **均与 tracker 一致**（原 §1「Java 主代码 3275 行（有效行 1463）/ 测试 3457 行（有效行 2008）」准确；rollup 记 2890/2918 中的 2918 与实测 2924 相差 6 行，属 rollup 侧取数差异，**非本 crate 问题**）。
- **「64 test 通过」与代码自洽**：源码测试函数恰为 64（57 + 7），分布 strategy 18 / backend 15 / reward 3 / runner 28，无 `#[ignore]`、无 doc-test。
- **映射表无虚假条目**：21 个 Java 类逐个核对，全部有真实 Rust 对应实现。
- **真实剩余缺口**：P0×2（其中 P0-1 经核实为「Java 亦无 → 非缺口」，P0-2 为真实行为不 1:1）、P1×3（P1-1/3/4）、P2×4。
- **未改动任何 Rust 代码**；本轮仅写入本 tracker 文件。


## 7. §4.3 P0 修复记录（2026-10-22，汇总方）

### 7.1 已修（部分）：Trinity `stream` 约束与 `msg_ids` 去重（§4.3 #6）

- **定位更正**：`stream: Some(true)` 位于 **`crates/agentscope-extensions-model/src/openai/mod.rs:381`**，本 crate **从无 `src/model/` 目录**（`git log --all -- crates/agentscope-extensions-training/src/model/*` 为空）。本 crate 的真实问题是**委托任意 `Model` 时原样透传 options**。
- **修复**：
  1. `TrinityModelAdapter::stream` 向下传 `options.stream = Some(false)`（对齐 Java builder 的 `.stream(false)`）。
  2. `msg_ids` **连续去重**：流式 delegate 会把同一 completion id 在每个 chunk 重复上报，Java 非流式路径只记一次；改前会把同一 id 记 N 次（污染 feedback/commit）。
- **用例**：`adapter_forces_stream_off`、`adapter_records_duplicate_chunk_ids_once`。
- **验证**：`cargo test -p agentscope-extensions-training` → **66 passed / 0 failed**。

### 7.2 未闭环（依赖 MODEL）

- `OpenAiChatModel` 只有 SSE 路径、从不读 `options.stream`，因此 `base_url` 模式仍会向 Trinity 发 `stream:true`。
- **关闭条件**：MODEL 增加非流式请求/响应分支（`chat().create()` + `CreateChatCompletionResponse` → `ChatResponse` 映射 + 用例），并让 `build_request` 读取 `options.stream`。在此之前本条只能记为**部分修复**。
### 7.3 测试竞态修复（2026-10-22，全量回归发现）

- **现象**：`cargo test --workspace` 偶发 `run_registry_allocates_incrementing_ids` 失败（`get_current_run_count("task-a")` 得到 0 而非 2）。
- **根因**：`RunRegistry` / `TaskExecutionRegistry` 是进程级 `static`，而两条测试各自调用 `clear_all()`；cargo 默认并行跑测试时互相清空计数。属**既有测试缺陷**，非生产代码回归。
- **修复**：测试模块内加 `registry_test_lock()`，6 条触及全局注册表的测试串行化（`MutexGuard`，中毒时取回内部值）。
- **2026-10-31 已废弃该方案（根因消除）**：串行锁只是掩盖共享状态，锁外仍有访问路径（`run_shadow` 内部的 `allocate_run_id`/`register` 并不可见）。现注册表为实例句柄，测试全部改用 `RunRegistry::new()` / `TaskExecutionRegistry::new()`，**锁已删除**；仅 2 条测 `global()` 单例的用例共用进程级单例，且各自使用 `task_id_generator::generate()` 生成的唯一 taskId（不调 `clear_all()`）→ 任意并行度下无冲突。详见 §9。
- **验证**：`cargo test -p agentscope-extensions-training --lib` 连跑 3 次均 **66 passed**；全量 `cargo test --workspace` → **EXIT=0**。
## 8. 2026-10-28 执行记录（P1-1 / P1-4 / P1-2 / P1-3 / P2-2 / P2-4）

### 8.1 变更文件
| 文件 | 变更 |
|---|---|
| `crates/agentscope-extensions-training/src/runner.rs` | 1473 → 1722 行：`AgentCloner` 回读 `sys_prompt`/`max_iters`；`start_commit_scheduler` 只看 interval；`TrainingRouter` 增 shadow 信号量 + `available_shadow_slots()`；新增 `TrainingHook`、`TrainingRunner::attach` / `has_commit_scheduler`；+8 测试 |
| `crates/agentscope-extensions-training/src/lib.rs` | 导出 `TrainingHook` |
| `crates/agentscope-extensions-training/Cargo.toml` | 删除 4 个零引用依赖（`agentscope-harness`/`chrono`/`async-trait`/`thiserror`） |
| `docs/extensions/TRAINING__GAP_TRACKER.md` | 本记录；§1 / §2 / §3.5 / §3.6 / §4.1 / §4.2 状态更新 |

未改：根 `Cargo.toml`、`agentscope-core/**`、`agentscope-harness/**`、其它 crate（均为只读核查）。

### 8.2 逐项结果

**P1-1 已完成（真缺口已修）**
- 代码：`AgentCloner::clone_with_model` 改为 `spec.sys_prompt.unwrap_or_else(|| original.sys_prompt().to_string())` + `spec.max_iters.unwrap_or_else(|| original.max_iters())`，然后**无条件**写入 builder；`ShadowAgentSpec` 语义降级为「可选覆盖」。
- 测试：`agent_cloner_inherits_prompt_and_max_iters_by_default`（原 agent 的 prompt=`you are the production agent`、max_iters=3，默认 spec 克隆后一致）、`agent_cloner_spec_overrides_inherited_values`（spec 覆盖生效）。

**P1-4 已完成（对齐 Java）**
- 代码：`start_commit_scheduler` 的条件去掉 `|| !self.config.is_enable_auto_commit()`；字段、getter、builder setter 三处文档标注「与 Java 一致：不参与调度器启停，仅作配置记录」。
- 新增观测点 `TrainingRunner::has_commit_scheduler()`（避免测试只能靠 `is_running()` 猜）。
- 测试：`commit_scheduler_ignores_enable_auto_commit_flag`（`enable_auto_commit(false)` + interval=3600 → 调度器仍安装，stop 后卸载）、`commit_scheduler_skipped_for_non_positive_interval`。

**P1-2 已收口（显式装配 API，不改 core）**
- 核查结论：core `Middleware::on_agent` 已经包住整次调用（`middleware.rs:23`，`react_agent.rs` `wrap_agent` 调用点），因此「`AGENT_RESULT` 之后」就是 post-call 位置 —— 不需新增 core 扩展点。
- 新增 `TrainingHook`（`Middleware`，`active_points()=[OnAgent]`）：用 `StreamExt::inspect` 抓 `AgentEvent::AgentResult(data).fields["reply"]`，在流尾接一个 `stream::once(...).filter_map(|_| None)` 跑 `router.run_shadow(...)`；agent 以 `Weak<ReActAgent>` 持有，避免「agent 持有自己的 middleware chain」形成引用环。
- 新增 `TrainingRunner::attach(&mut MiddlewareChain)`（≈`AgentBase.addSystemHook(router)`）。
- 用法（同 `TrainingHook` 文档示例）：① `let mut chain = MiddlewareChain::new();` ② `let hook = runner.attach(&mut chain);` ③ 用该 chain 建 agent（`.middleware_chain(chain)`）并 `Arc::new(...)` ④ `hook.bind_agent(&agent);` ⑤ `runner.start();`（只跑生命周期，不注册 hook）。
- 与 Java 的差异（均已写入代码文档）：① **逐 agent 装配**而非进程全局；② post-call **内联 await**（Java 在 boundedElastic 上 fire-and-forget）→ 不希望阻塞生产调用时自行 `tokio::spawn`；③ 无 `AGENT_RESULT` 的失败/中断调用直接跳过；④ 无 `removeSystemHook` 对应物（chain 在 build 后不可变，卸载需重建 agent）。
- 测试：`runner_attach_installs_hook_on_chain`、`training_hook_drives_post_call_after_agent_completion`（走真实 `ReActAgent::call`，`completed_calls()==1`）、`training_hook_without_bound_agent_is_a_passthrough`。

**P1-3 设计决定（不迁移）**
- 只读核查：core `AgentCore`（`react_agent.rs:77` 起）与 `ReActAgentBuilder`（`:1220` 起）均无这三个字段/setter；`rg 'model_execution_config|tool_execution_config|tool_execution_context'` 在 core 仅命中自由函数 `tool_execution_context()`（构造**单次工具调用**的上下文，与 Java 的 agent 级 getter 不同物）。
- 依据已写入 §4.2 P1-3 追加段；`AgentCloner` 文档同步标注「not migrated」并指向 §4.2。

**P2-2 部分接线**
- `shadow_pool_size` → `TrainingRouter::shadow_slots: Arc<tokio::sync::Semaphore>`；`run_shadow` 每轮 `acquire_owned()`；新增 `available_shadow_slots()`。
- 测试：`router_shadow_pool_size_is_wired`（size=3 → 3 slots；size=0 → 兜底 1 slot，不阻塞）。
- `shadow_pool_capacity` 保留为**预留字段**（tokio 信号量无有界等待队列），getter/builder 不变，文档标注「not consumed at runtime」。

**P2-4 已完成**
- 删除 `agentscope-harness` / `chrono` / `async-trait` / `thiserror`；删除前四项 `Select-String` 引用计数均为 0。

### 8.3 验证
- `cargo build -p agentscope-extensions-training` → EXIT=0；本 crate 0 警告（仅 core 既有 2 条 `dead_code` 警告）。
- `cargo test -p agentscope-extensions-training` → **74 passed / 0 failed / 0 ignored**（lib），doc-test 0；连跑 3 次一致。
- `cargo clippy -p agentscope-extensions-training --all-targets` → EXIT=0；本 crate 2 条警告均为**既有**（`runner.rs` cloner `map_err` 的 `useless_conversion`、`strategy.rs` builder 的 `derivable_impls`），本轮未新增。
- 反向验证（逐个回退实现 → 跑定向用例 → 恢复；均确认 FAILED）：

| 回退内容 | 定向用例 | 结果 |
|---|---|---|
| cloner 改回「仅 spec 为 `Some` 时设置」 | `agent_cloner_inherits_prompt_and_max_iters_by_default` | FAILED（2 passed / 1 failed） |
| 调度器条件加回 `!is_enable_auto_commit()` | `commit_scheduler_ignores_enable_auto_commit_flag` | FAILED（1 passed / 1 failed） |
| `bind_agent` 改为空实现（hook 不绑定） | `training_hook_drives_post_call_after_agent_completion` | FAILED（1 passed / 1 failed） |
| 信号量许可数硬编码为 1 | `router_shadow_pool_size_is_wired` | FAILED（0 passed / 1 failed） |

### 8.4 遗留（2026-10-31 已更新，见 §9）
- **P0-2**（Trinity 非流式）**已完成（2026-10-29 勘误）**：原记「仍为部分修复，待 MODEL 侧让 `build_request` 读 `options.stream`（§7.2）」，实际已闭环——`backend.rs:305-308` 强制 `options.stream = Some(false)` + 重复 id 去重，MODEL `openai/mod.rs:271/320` 已读 `options.stream` 并走非流式分支（§4.2 P0-2）。
- ~~**P2-1** live 联调、**P2-3** 全局单例隔离不变。~~ → P2-3 已于 2026-10-31 关闭（实例注册表），P2-1 仍待环境。
- ~~`shadow_pool_capacity` 仍为预留~~ → 2026-10-31 已接线为有界准入队列（§4.2 P2-2）。
- ~~`TrainingHook` 无「卸载」~~ → 2026-10-31 增加 `disable()` 且 `TrainingRunner::stop()` 自动 disable（§4.2 P2-5）。

## 9. 2026-10-31 收口（注册表去全局化 / 队列容量 / hook 卸载）

### 9.1 变更文件
| 文件 | 变更 |
|---|---|
| `crates/agentscope-extensions-training/src/runner.rs` | 1985 → 2272 行：①`RunRegistry`/`TaskExecutionRegistry` 改**实例句柄**（私有 `Mutex<HashMap<..>>` 字段 + `new()` + `global()`，删除两个 `static OnceLock<Mutex<..>>` 与其自由函数）；②`TrainingRouter` 增 `run_registry`/`task_registry`/`shadow_in_flight`，新增 `with_registries`/`with_global_registries`/`run_registry()`/`task_registry()`/`pending_shadow_runs()`/私有 `try_admit_shadow_run()` + `ShadowAdmission` RAII 守卫；③`TrainingHook` 增 `enabled: AtomicBool` 与 `set_enabled/enable/disable/is_enabled`（禁用时 `on_agent` 直通）；④`TrainingRunner` 增 `hooks: Mutex<Vec<Weak<TrainingHook>>>`、`disable_attached_hooks()`、`attached_hook_count()`，`attach()` 记录弱引用、`stop()` 调用 disable；⑤测试全部改用实例注册表、**删除 `registry_test_lock`**，+8 用例 |
| `docs/extensions/TRAINING__GAP_TRACKER.md` | §1 / §2 / §3.5 / §3.6 / §4.1（含已知差异 6、7）/ §4.2（P2-2 / P2-3 / P2-5 / 真实剩余）/ §7.3 / §8.4 + 本 §9 |

未改：根 `Cargo.toml`、`agentscope-core/**`、`agentscope-harness/**`、其它 crate；本 crate `Cargo.toml` 亦未变动（无新增依赖）。

### 9.2 逐项处置
| 编号 | 处置 | 证据 |
|---|---|---|
| P2-3 全局单例隔离 | **已完成（落代码）** | `RunRegistry`/`TaskExecutionRegistry` 为实例；`TrainingRouter::new` 与 `with_registries`/`with_global_registries` 三种装配；`run_shadow`/`run_single_shadow` 只用 `self.run_registry`/`self.task_registry`（`runner.rs` 内已无 `RunRegistry::allocate_run_id(` 等静态调用，也无线程级 static 可变状态） |
| P2-2 `shadow_pool_capacity` | **已完成（落代码）** | `try_admit_shadow_run()` 用 `shadow_in_flight.fetch_add` 与 `capacity.max(1)` 比较，超限退回并丢弃该轮；`ShadowAdmission::drop` 释放；`run_shadow` 每轮先准入后取信号量 |
| P2-5 hook 卸载 | **已完成（落代码）** | `TrainingHook::disable()` + `on_agent` 早退直通；`TrainingRunner::stop()` → `disable_attached_hooks()`（弱引用列表，不延长 agent 生命周期） |
| P2-1 live 联调 | **未关闭（环境）** | 无 Trinity 端点；不引入 `#[ignore]`/env-gated 空跑用例（硬约束） |
| 可选后续 | 未立项 | 若需 Java `RejectedExecutionException` 等价的硬失败 / 真正阻塞排队，需宿主层改造；当前「超容量丢弃」是训练数据采集场景下的有意选型 |

### 9.3 测试竞态「真正修掉」的依据（替代 §7.3 的串行锁）
- 旧方案的问题：`registry_test_lock()` 只能串行化**测试直接调用**注册表的那几行；`run_shadow` 内部的 `allocate_run_id` / `TaskExecutionRegistry::register` / `cleanup` 无法被该锁覆盖，任何新增「跑通一次 shadow」的测试都会重新引入竞态。
- 新方案：注册表**不是进程全局**了。默认路径（`TrainingRouter::new`）每 router 一份，测试里 `RunRegistry::new()` 每用例一份 → 用例之间零共享可变状态，`-j 2`（乃至默认并行度）下天然稳定。
- 仍共享 `global()` 的只有 2 条**只测单例语义**的用例：`run_registry_global_is_a_shared_singleton`、`task_execution_registry_global_is_a_shared_singleton`。二者用 `task_id_generator::generate()`（uuid）作为 taskId，**不调用 `clear_all()`**，只对自己创建的 key 断言并 cleanup → 与任何并行用例不冲突（不同 key）。
- `routers_own_isolated_registries_by_default` 断言 `Arc::ptr_eq` 反向性质（默认不同实例、`with_global_registries` 与 `global()` 同一实例），把「隔离是默认」与「Java 语义可选」都钉住。

### 9.4 P2 残留裁定（含「provider HTTP 400 / 迭代上限语义」口径）
- **「provider HTTP 400 中断」**：属**上次 live 联调的环境/协议现象**（provider 拒绝请求体），本 crate 无任何断言、无代码分支与之对应（`grep 400` 在 `src/**` 零命中）。离线层面真正相关的是 P0-2（Trinity 非流式），已闭环；因此本项**不新增代码**，结论：非缺口。
- **「迭代上限语义」**：`max_iters` 已由 `AgentCloner` 从生产 agent 回读（P1-1），`ShadowAgentSpec.max_iters` 仅覆盖；core 的迭代上限本身在 `ReActAgent` 内实现，本 crate 不重复实现也不改写 → 结论：非缺口（与 P1-3「三个 ExecutionConfig 字段 = 设计决定（不迁移）」同源：Rust 的执行调控按调用/按工具下发，不上提到 agent 级）。
- `shadow_pool_capacity` / 注册表全局单例 / hook 卸载三项旧残留已在本轮关闭（见 9.2）。

### 9.5 反向验证建议（主智能体执行；本轮按约束未跑 cargo）
| 回退内容 | 定向用例 | 预期 |
|---|---|---|
| `try_admit_shadow_run` 恒返回 `Some`（去掉容量判断） | `router_shadow_queue_capacity_bounds_admission` / `run_shadow_checks_queue_capacity_before_running` | FAILED |
| `run_shadow` 中删除准入调用 | `run_shadow_checks_queue_capacity_before_running` | FAILED |
| `on_agent` 去掉 `if !self.is_enabled()` 早退 | `training_hook_disable_makes_it_a_passthrough` | FAILED |
| `stop()` 去掉 `disable_attached_hooks()` | `runner_stop_disables_attached_hooks` | FAILED |
| `TrainingRouter::new` 改用 `global()` 注册表 | `routers_own_isolated_registries_by_default` | FAILED |
| 把 `RunRegistry::new()` 换回共享 static | `run_registry_instances_are_isolated` | FAILED |

### 9.6 未关闭项
- **P2-1 live Trinity 联调**（唯一未关闭项，环境依赖；离线面已覆盖到 loopback mock + `TrinityModelAdapter` 强制非流式 + msg_id 去重）。
- 回执约定：本轮只改 `crates/agentscope-extensions-training/**` 与本 tracker；`docs/extensions/EXTENSIONS_GAP_TRACKER.md` 的 TRAINING 行仍写着「已加测试内互斥锁」，**需汇总方同步改为「注册表实例化，无需串行锁」**（跨模块需求，本轮无写权限）。