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
| Rust 规模（2026-10-28 实测） | 5 个源文件；`runner.rs` 1722 行（其余未变：`backend.rs` 748、`lib.rs` 32、`reward.rs` 107、`strategy.rs` 720）；**74 个测试**（2026-10-28：`#[test]` 61 + `#[tokio::test]` 13） |
| 迁移波次 | C |
| 当前状态 | 进行中（2026-09-18 曾收尾：build ✅ / test 64 passed；2026-10-22 静态复核发现 2 项 P0 + 3 项 P1 + 4 项 P2 缺口，见 §4.2 / §6） |

## 2. Java 清单与依赖映射
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `backend/dto/CommitRequest` | Commit API 请求体（task_id/run_id/time_threshold） | `backend::dto::CommitRequest`（serde, snake_case） | builder 语义对齐 |
| `backend/dto/FeedbackRequest` | Feedback API 请求体（msg_ids/reward/task_id/run_id） | `backend::dto::FeedbackRequest` | 同上 |
| `backend/dto/StatusResponse` | 通用状态响应（status/message/isSuccess） | `backend::dto::StatusResponse` | `is_success()` 复刻 `"success".equalsIgnoreCase` |
| `backend/TrinityClient` | Trinity Feedback/Commit HTTP 客户端（OkHttp） | `backend::TrinityClient`（`reqwest`，async） | 只有 `/feedback`、`/commit`；Chat 走 OpenAI 兼容模型 |
| `backend/TrinityModelAdapter` | 组合包装 OpenAIChatModel，拦截响应收集 msg_id → RunExecutionContext | `backend::TrinityModelAdapter`（实现 core `Model` trait，组合 `Arc<dyn Model>`） | **不重建模型**：实现 core `Model` trait 并委派给内层 model；`stream` 中把每个 `ChatResponse.id` 记入 `RunExecutionContext`。**Java 强制 `stream(false)`，Rust 建的 `OpenAiChatModel` 恒为流式且无 stream 开关**（见 §4.2 P0-2） |
| `reward/RewardCalculator` | 依据 shadow Agent 执行结果算 reward | `reward::RewardCalculator`（trait，`fn calculate(&self, agent: &ReActAgent) -> f64`） | Rust 用 `ReActAgent` 具体类型（等价 Java `Agent` 参数） |
| `runner/AgentCloner` | 反射提取配置 + Builder 重建 Agent（换 model，新建空 memory） | `runner::AgentCloner::clone_with_model`（基于 `ReActAgent`/`ReActAgentBuilder` 显式重建） | Rust 无反射：name/description/toolkit 由 `Agent` trait 回读，`sys_prompt`/`max_iters` **已通过 core getter 回读**（`ReActAgent::sys_prompt()`/`max_iters()`），`ShadowAgentSpec` 降级为可选覆盖；`modelExecutionConfig`/`toolExecutionConfig`/`toolExecutionContext` **设计决定（不迁移）**（core 无字段/无 setter，见 §4.2 P1-3） |
| `runner/RunExecutionContext` | 单次 Run 的执行上下文（msgIds/msgs/timing） | `runner::RunExecutionContext` | `Arc<Mutex<...>>` 内部可变，线程安全 |
| `runner/RunRegistry` | taskId → Run 计数（自增 runId） | `runner::RunRegistry`（静态全局，`Mutex<HashMap>`） | Java 为静态单例；Rust 用 `OnceLock` 全局 |
| `runner/TaskExecutionRegistry` | taskId → Vec<RunExecutionContext> 注册与统计 | `runner::TaskExecutionRegistry` | 同上；查询按 runId 数值排序 |
| `runner/TaskIdGenerator` | 生成 `task-{uuid}` / `task-{ts}-{rand}` | `runner::task_id_generator`（uuid crate） | `uuid` 已在 workspace deps |
| `runner/TrainingConfig` | 训练配置（endpoint/模型/策略/reward/commit 间隔/池/repeatTime） | `runner::TrainingConfig` + Builder | 默认值对齐 |
| `runner/TrainingRouter` | 训练路由 Hook：PreCall 存输入、PostCall 选择+shadow 执行+feedback | `runner::TrainingRouter`（**不实现 core `Middleware`**，提供显式驱动 API `decide(...)` + `run_shadow(...)`，另附 `ShadowRunReport`） | 见 §4.2：Java Hook 的 `PreCall/PostCall` 语义在 Rust 无等价钩子，改为显式驱动（保留 `-shadow` 递归保护 / repeatTime / runId / feedback 全流程） |
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
| reactor-core（Mono/Flux/Schedulers） | 响应式 + 有界弹性线程池跑 shadow | `tokio`（async fn / spawn / interval） | shadow 跑在 tokio 任务，池大小由 Tokio 运行时承载 |
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
| TrinityModelAdapter（组合 Model + 收集 msg_id） | TrinityModelAdapter | 已完成 | 实现 core `Model` trait，委派内层 `Arc<dyn Model>`；对流内每个 `ChatResponse.id` 记入 `RunExecutionContext` |

### 3.5 runner（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| RunExecutionContext | RunExecutionContext | 已完成 | taskId/runId/msgIds/msgs/startTime + duration |
| RunRegistry（runId 自增 + cleanup） | RunRegistry | 已完成 | 全局 `OnceLock<Mutex<HashMap>>` |
| TaskExecutionRegistry（注册/查询/统计） | TaskExecutionRegistry | 已完成 | 含 `RegistryStats` |
| TaskIdGenerator | TaskIdGenerator | 已完成 | `task-{uuid}` / `task-{ts}-{rand}` |
| TrainingConfig + Builder | TrainingConfig | 已完成 | 默认值对齐；build 校验 endpoint+rewardCalculator |
| AgentCloner（换 model 重建 shadow） | AgentCloner | 已完成（2026-10-28） | 基于 `ReActAgentBuilder` 重建（name+`-shadow`/description/toolkit/model/`todo_tool(false)`）；**`sys_prompt`/`max_iters` 默认从原 agent 回读**（`original.sys_prompt()`/`original.max_iters()`），`ShadowAgentSpec` 仅在显式提供时覆盖（§4.2 P1-1 已关闭）；`modelExecutionConfig`/`toolExecutionConfig`/`toolExecutionContext` = **设计决定（不迁移）**（core 无关字段与 setter，见 §4.2 P1-3） |
| TrainingRouter（选择 + shadow + feedback） | TrainingRouter | 已完成（语义调整） | 提供显式 `decide`/`run_shadow` API，以及 **`TrainingHook`（core `Middleware`, `OnAgent`）** 自动驱动；保留 `-shadow` 递归保护、repeatTime/runId/feedback 全流程；`shadow_pool_size` **已接线**（`tokio::sync::Semaphore` 并发上限，`available_shadow_slots()` 可观测），`shadow_pool_capacity` 为**预留字段**（见 §4.2 P2-2） |
| TrainingRunner（start/stop/周期 commit） | TrainingRunner | 已完成 | tokio 任务 + interval；start/stop 幂等；**调度器只看 `commit_interval_seconds`**（`enable_auto_commit` 仅记录，对齐 Java，§4.2 P1-4）；`has_commit_scheduler()` 可观测；`attach(&mut MiddlewareChain)` 提供显式 hook 装配（§4.2 P1-2） |

### 3.6 测试（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| strategy 单测 | *Test | 已完成 | 18 项（strategy.rs）：annotation（enabled/过期/labels+metadata）、context（mark/mark_with_labels/mark_with_metadata/with_annotation）、decision 工厂+Display、sampling（边界/非法值 panic/priority/name）、explicit（无标记/未启用/过期/优先级/名称/metadata+taskId 透传）、`is_expired` 辅助 |
| backend DTO + client 单测 | *Test | 已完成 | 15 项（backend.rs，含 5 个 `#[tokio::test]`）：DTO snake_case 往返（commit/feedback）、缺省字段不序列化、StatusResponse 反序列化+大小写不敏感成功、endpoint/timeout 校验、loopback mock HTTP（feedback/commit 请求体+非 2xx）、adapter 收集 msg_id / 无 context 警告路径 / 由 base_url 自建模型 |
| reward 单测 | RewardCalculatorTest | 已完成 | 3 项：闭包代理、Zero 常量、闭包读取 agent 字段 |
| runner 单测 | *Test | 已完成 | 36 项（runner.rs，含 6 个 `#[tokio::test]`；2026-10-28 新增 8 项：cloner 继承/覆盖、pool 接线、commit 调度两个、hook 装配与有/无绑定两种路径）：RunExecutionContext（收集/空 id panic/eq/display/set_messages）、RunRegistry（自增/清理/空 id panic/cleanup_old_tasks）、TaskExecutionRegistry（注册/排序/空查询/统计/ids/cleanup）、TaskIdGenerator、TrainingConfig（默认/校验/可选 setter/repeatTime/sampleRate）、TrainingRunner（builder 校验/start-stop 幂等/混用忽略）、router（recursion/sampling/explicit）、AgentCloner+ShadowAgentSpec、run_shadow 未选中短路 |
| **合计** | | | **74 passed / 0 failed**（2026-10-28 实测 `cargo test -p agentscope-extensions-training`：strategy 18 / backend 17 / reward 3 / runner 36；源码内 `#[test]` 61 + `#[tokio::test]` 13 = 74；无 `#[ignore]`，无 doc-test） |

## 4. 项目状态与下一步
### 4.1 当前状态
- **状态：进行中（波次 C；2026-09-18 收尾重验，2026-10-22 静态复核发现 2 项 P0 + 3 项 P1 + 4 项 P2 缺口，见 §4.2 / §6）**。21 个 Java 类全部有 Rust 对应（详见 §2），划分为 4 个模块：
  - `strategy.rs`：`TrainingAnnotation`(+Builder)/`TrainingContext`/`SelectionDecision`/`TrainingSelectionStrategy`(trait)/`SamplingRateStrategy`/`ExplicitMarkingStrategy`/`is_expired`
  - `reward.rs`：`RewardCalculator`(trait) + `FnRewardCalculator` + `ZeroRewardCalculator`
  - `backend.rs`：`dto::{CommitRequest,FeedbackRequest,StatusResponse}` + `TrinityClient`(+Builder) + `TrinityModelAdapter`(+Builder)
  - `runner.rs`：`task_id_generator`/`RunExecutionContext`/`RunRegistry`/`TaskExecutionRegistry`(+`RegistryStats`)/`TrainingConfig`(+Builder)/`AgentCloner`(+`ShadowAgentSpec`)/`TrainingRouter`(+`ShadowRunReport`)/`TrainingRunner`(+Builder)
- **验证命令与结果**（2026-09-18 收尾重验；2026-10-22 静态复核确认测试数自洽）：
  - `cargo build -p agentscope-extensions-training` → 通过（Finished，exit 0，无 error）
  - `cargo test -p agentscope-extensions-training` → **64 passed; 0 failed**（2026-09-18 基线；经 §7.1 增至 66，2026-10-28 增至 **74**，见下行）
  - 首轮基线 37 passed；本轮补齐 27 个离线单测（无网络、无 Trinity 依赖）。
  - **2026-10-28 复验**：`cargo build -p agentscope-extensions-training` → exit 0（本 crate 无新警告）；`cargo test -p agentscope-extensions-training` → **74 passed / 0 failed**（lib 74，doc-test 0）；连跑 3 次一致。新增 8 项测试见 §8。
  - 静态复核（2026-10-22，未编译）：源码内 `#[test]` 57 + `#[tokio::test]` 7 = **64**，与声明一致；无 `#[ignore]`、无 doc-test 块，故「64 test 通过」与代码一致（未重跑 cargo）。
- **复用既有 trait（未重建平行类型）**：
  - `core::Model` trait：`TrinityModelAdapter` 实现它并委派内层 `Arc<dyn Model>`（组合模式，对齐 Java）。
  - `core::ReActAgent` / `ReActAgentBuilder`：`AgentCloner` 用 builder 重建 shadow；name/description/toolkit 经 `Agent` trait 回读。
  - `Msg`/`ChatResponse`/`GenerateOptions`/`ToolSchema`/`ExecutionConfig` 全部复用 core。
  - 与 harness 无新增耦合（未触及 `DistributedStore` 等；`Cargo.toml` 中 4 个零引用依赖已于 2026-10-28 删除，见 §4.2 P2-4）。
  - 已确认：core 无独立 `Hook` 类型；`middleware::Middleware` 的 `on_pre_call` 对齐 Java `PreCallEvent`，而 **`on_agent` 的洋葱包装（可观察 `AGENT_RESULT`）能承载 post-call** → 2026-10-28 已由本 crate 的 `TrainingHook` 落地（见 §4.2 P1-2 / §8）。
- **本轮新增代码**：`TrainingContext::mark_with_metadata(...)`（补齐 Java `mark(Map)` 重载），以及 4 个业务文件测试模块扩写；无破坏性改动、未删除既有功能/测试。
- **依赖**：仅本 crate `Cargo.toml` 新增 `reqwest`、`uuid`（均走已有 workspace deps，对应 0.12 / 1，**未改根 Cargo.toml**）。未引 `rand`（用内部 xorshift PRNG）。
- **已知差异/设计决定**：
  1. Java `TrainingSelectionStrategy.shouldSelect(agent, inputs, output, ContextView)` 的 Reactor `ContextView` → Rust 改为显式 `Option<&TrainingAnnotation>`（无等价隐式调用域上下文）。
  2. Java `TrainingRouter` 是 `Hook`（`PreCallEvent`/`PostCallEvent`）；Rust 提供两种驱动方式：显式 API `decide(...)` + `run_shadow(...)`，以及 `TrainingHook`（`Middleware::on_agent` 包装）自动触发（2026-10-28 新增，见 §4.2 P1-2）；`-shadow` 递归保护、选择分支、repeatTime/runId/feedback 全流程保持一致。
  3. Java `AgentCloner` 用反射提取 `sysPrompt`/`maxIters` 等；Rust 无反射 → name/description/toolkit 从 `Agent` trait 读取，`sys_prompt`/`max_iters` **默认用 core getter 回读**，`ShadowAgentSpec` 仅作为显式覆盖（2026-10-28 修正，见 §4.2 P1-1）。
  4. Java `RewardCalculator.calculate(Agent)` → Rust `calculate(&ReActAgent)`（具体类型）。
  5. Java `TrinityModelAdapter` 强制 `stream(false)`（Trinity 不支持流式）；Rust 侧 `OpenAiChatModel` 恒为流式且无关闭开关（见 §4.2 P0-2）。

### 4.2 剩余缺口（分级）与阻塞 / 跨模块依赖
> 以下分级为 **2026-10-22 静态复核**结果（未编译、未运行 cargo test）。P0-1 / P0-2 为本轮新发现，详见末尾「静态复核记录」。

**状态总览（2026-10-28 执行后）**

| 编号 | 主题 | 2026-10-28 状态 |
|---|---|---|
| P0-1 | 训练数据格式/轨迹采样（Java 亦无） | 非缺口（不变） |
| P0-2 | `TrinityModelAdapter` 流式 vs 非流式 | 部分修复（§7.1/§7.2 不变，仍待 MODEL 侧非流式分支） |
| P1-1 | `AgentCloner` 丢 `sys_prompt`/`max_iters` | **已完成**（改回读，+2 测试） |
| P1-2 | 无法全局挂载 hook | **已收口**：不改 core，新增 `TrainingHook` + `TrainingRunner::attach`（+3 测试） |
| P1-3 | 3 个 ExecutionConfig 字段 | **设计决定（不迁移）**（core 无字段/setter） |
| P1-4 | `enable_auto_commit` 语义 | **已完成**（对齐 Java：只看 interval，+2 测试） |
| P2-1 | live 验证 | 待条件（不变） |
| P2-2 | `shadow_pool_size`/`capacity` | **部分接线**：size → 信号量；capacity = 预留字段 |
| P2-3 | 全局单例隔离 | 不变（对齐 Java） |
| P2-4 | 4 个零引用依赖 | **已完成**（已删除） |

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

**P1-1：`AgentCloner` 丢弃 `sys_prompt` / `max_iters`（core 已有 getter，本 crate 未回读）**
- 上一版 tracker 称「core 无 getter，故由 `ShadowAgentSpec` 显式传入」——**该前提不成立**：`agentscope-core/src/agent/react_agent.rs:283` 已有 `pub fn sys_prompt(&self) -> &str`，`:288` 已有 `pub fn max_iters(&self) -> i32`。
- 现状：`AgentCloner::clone_with_model` 只在 `ShadowAgentSpec.sys_prompt/max_iters` 为 `Some` 时才设置；`ShadowAgentSpec::default()` 两项皆 `None` → **shadow agent 的 system prompt 为空串、max_iters 回落到默认 10**。Java 则通过反射 100% 复制这两个字段。
- 影响：shadow agent 与生产 agent 行为不一致 → 采集到的训练轨迹不是「同一请求在训练模型上的真实表现」，reward 失真。`run_shadow` 要求调用方**记得**传 spec，缺省路径即静默降级。
- 建议：直接在 `clone_with_model` 内用 `original.sys_prompt()` / `original.max_iters()` 回读（无需 core 改动），`ShadowAgentSpec` 降级为可选覆盖。
- **已实施（2026-10-28）**：按此建议实现（`runner.rs` `AgentCloner::clone_with_model` 先取 `spec.*`，为 `None` 时回读原 agent），新增 `agent_cloner_inherits_prompt_and_max_iters_by_default` / `agent_cloner_spec_overrides_inherited_values` 两条用例。

**P1-2：`TrainingRunner.start()` 无法全局挂载 hook（需 core/harness 支持）**
- Java `start()` 通过 `AgentBase.addSystemHook(router)` 全局注册；Rust 无等价机制。当前 `TrainingRunner` 仅管理自身生命周期 + commit 调度，**不会自动挂到 agent**；调用方需自行把 `TrainingRouter` 接入 agent 调用路径。
- 复核补充：Rust core `middleware::Middleware` 现有 `on_pre_call`（语义对齐 Java `PreCallEvent`），但**无 post-call 钩子**，故 Java 的 `PostCallEvent`（选择 + shadow + feedback）确实无法用 core middleware 直接承载——原「core 无 PreCallEvent」表述不准确（见 §6 勘误）。
- 需跨模块裁定是否在 core 引入 post-call 扩展点，再由汇总方统一处理。
- **已收口（2026-10-28，未改 core）**：`Middleware::on_agent` 本身就是包住整次调用的洋葱拦截，包住 `AGENT_RESULT` 之后即可当 post-call 用，所以无需新扩展点。本 crate 新增 `TrainingHook`（`Middleware`，只参与 `ExtensionPoint::OnAgent`，`Weak<ReActAgent>` 绑定避免自持引用）与 `TrainingRunner::attach(&mut MiddlewareChain)` 作为**显式装配 API**；差异与用法见 §8.3。

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

**P2-2：`shadow_pool_size` / `shadow_pool_capacity` 存值未生效**
- Java 用 `Schedulers.newBoundedElastic(shadowPoolSize, shadowPoolCapacity, "training-shadow")` 调度 shadow 调用（异步、限流、队列有界）。
- Rust `TrainingConfig` 保留这两个字段并有 getter/builder，但**全 crate 无任何消费点**（检索仅命中定义与测试断言）→ shadow 并发度实际由调用方自行 `tokio::spawn` 决定，配置形同虚设。tracker §2.1 原表述「shadow 跑在 tokio 任务，池大小由 Tokio 运行时承载」掩盖了「配置值未被使用」的事实。
- **2026-10-28 部分接线**：`shadow_pool_size` → `TrainingRouter` 内 `Arc<tokio::sync::Semaphore>`（许可数 `shadow_pool_size.max(1)`，`run_shadow` 每轮 `acquire_owned()`），新增 `available_shadow_slots()` 观测点。`shadow_pool_capacity` **明确为预留字段**（tokio 信号量没有有界等待队列，无法 1:1 映射 Java `newBoundedElastic` 的 queue bound），既有 getter/builder 保留，文档已标注「not consumed at runtime」。
- 测试：`router_shadow_pool_size_is_wired`（size=3 → 3 slots；size=0 → 兜底 1 slot，不阻塞）。

**P2-3：全局单例隔离**
- `RunRegistry`/`TaskExecutionRegistry` 为进程内全局单例（对齐 Java 静态字段）；如需多实例隔离需后续改造。

**P2-4：无依赖清理**
- `Cargo.toml` 声明 `agentscope-harness`、`chrono`、`async-trait`、`thiserror` 四个依赖，但 `src/**/*.rs` 引用数均为 **0** → 可移除（减少编译面）。`agentscope-harness` 尤其值得清理，避免与「未触及 harness」的表述产生歧义。
- **2026-10-28 已清理**：先逐项 `Select-String` 复核（`agentscope_harness`/`chrono`/`thiserror`/`async_trait` 命中均为 0）再删除。剩余 `[dependencies]`：`agentscope-core`、`agentscope-extensions-model`、`serde`、`serde_json`、`tokio`、`futures`、`tracing`、`reqwest`、`uuid`；build/test 均通过。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-training` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-training` / `cargo test -p agentscope-extensions-training` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-training/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

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

### 8.4 遗留
- **P0-2**（Trinity 非流式）仍为部分修复，待 MODEL 侧让 `build_request` 读 `options.stream`（§7.2）。
- **P2-1** live 联调、**P2-3** 全局单例隔离不变。
- `shadow_pool_capacity` 仍为预留（若要真正的有界队列，需自建调度器，当前不阻塞）。
- `TrainingHook` 无「卸载」：middleware chain 在 `build()` 后不可变，宿主要停用需重建 agent；`TrainingRunner::stop()` 只停生命周期与 commit 调度，**不会**移除 hook（已在代码文档与本 tracker 注明，对应 Java `removeSystemHook` 的缺失）。