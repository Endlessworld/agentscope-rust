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
| Java 规模（初测） | 主代码 ~2890 行 / 测试 ~2918 行 |
| Rust crate | `crates/agentscope-extensions-training` |
| 迁移波次 | C |
| 当前状态 | 已完成（2026-09-18 收尾：build ✅ / test 64 passed, 0 failed） |

## 2. Java 清单与依赖映射
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `backend/dto/CommitRequest` | Commit API 请求体（task_id/run_id/time_threshold） | `backend::dto::CommitRequest`（serde, snake_case） | builder 语义对齐 |
| `backend/dto/FeedbackRequest` | Feedback API 请求体（msg_ids/reward/task_id/run_id） | `backend::dto::FeedbackRequest` | 同上 |
| `backend/dto/StatusResponse` | 通用状态响应（status/message/isSuccess） | `backend::dto::StatusResponse` | `is_success()` 复刻 `"success".equalsIgnoreCase` |
| `backend/TrinityClient` | Trinity Feedback/Commit HTTP 客户端（OkHttp） | `backend::TrinityClient`（`reqwest`，async） | 只有 `/feedback`、`/commit`；Chat 走 OpenAI 兼容模型 |
| `backend/TrinityModelAdapter` | 组合包装 OpenAIChatModel，拦截响应收集 msg_id → RunExecutionContext | `backend::TrinityModelAdapter`（实现 core `Model` trait，组合 `Arc<dyn Model>`） | **不重建模型**：实现 core `Model` trait 并委派给内层 model；`stream` 中把每个 `ChatResponse.id` 记入 `RunExecutionContext` |
| `reward/RewardCalculator` | 依据 shadow Agent 执行结果算 reward | `reward::RewardCalculator`（trait，`fn calculate(&self, agent: &ReActAgent) -> f64`） | Rust 用 `ReActAgent` 具体类型（等价 Java `Agent` 参数） |
| `runner/AgentCloner` | 反射提取配置 + Builder 重建 Agent（换 model，新建空 memory） | `runner::AgentCloner::clone_with_model`（基于 `ReActAgent`/`ReActAgentBuilder` 显式重建） | Rust 无反射：name/description/toolkit 由 `Agent` trait 回读，`sysPrompt`/`maxIters` 经 `ShadowAgentSpec` 显式传入；`modelExecutionConfig`/`toolExecutionConfig`/`toolExecutionContext` **未能迁移**（core 无 getter，见 §4.2 P1） |
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
| AgentCloner（换 model 重建 shadow） | AgentCloner | 部分完成 | 基于 `ReActAgentBuilder` 重建（name+`-shadow`/description/toolkit/model）；Rust 无反射，`sysPrompt`/`maxIters` 由 `ShadowAgentSpec` 传入；**`modelExecutionConfig`/`toolExecutionConfig`/`toolExecutionContext` 未迁移**（core 无回读器，见 §4.2 P1） |
| TrainingRouter（选择 + shadow + feedback） | TrainingRouter | 已完成（语义调整） | 不实现 core `Middleware`，提供显式 `decide`/`run_shadow` API；保留 `-shadow` 递归保护、repeatTime/runId/feedback 全流程（见 §4.2） |
| TrainingRunner（start/stop/周期 commit） | TrainingRunner | 已完成 | tokio 任务 + interval；start/stop 幂等 |

### 3.6 测试（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| strategy 单测 | *Test | 已完成 | 12 项：annotation（builder/insertion/serde 往返/isExpired）、context（mark/mark_with_labels/mark_with_metadata/with_annotation）、decision 工厂、sampling 边界与非法值、explicit（无标记/未启用/过期/优先级/名称/metadata+taskId 透传） |
| backend DTO + client 单测 | *Test | 已完成 | 9 项：DTO snake_case 往返、缺省字段不序列化、StatusResponse 反序列化、endpoint/timeout 校验、loopback mock HTTP（feedback/commit 请求体+非 2xx）、adapter 收集 msg_id / 无 context 警告路径 |
| reward 单测 | RewardCalculatorTest | 已完成 | 2 项：闭包代理、Zero 常量 |
| runner 单测 | *Test | 已完成 | 21 项：RunExecutionContext（收集/空 id panic/eq/display/set_messages）、RunRegistry（自增/清理/空 id）、TaskExecutionRegistry（注册/排序/空查询/统计/ids）、TaskIdGenerator、TrainingConfig（默认/校验/可选 setter/repeatTime/sampleRate）、TrainingRunner（builder 校验/start-stop 幂等/混用忽略）、router（recursion/sampling/explicit）、AgentCloner、run_shadow 未选中短路 |
| **合计** | | | **64 passed / 0 failed**（`cargo test -p agentscope-extensions-training`） |

## 4. 项目状态与下一步
### 4.1 当前状态
- **状态：已完成（波次 C；2026-09-18 收尾重验）**。21 个 Java 类全部有 Rust 对应（详见 §2），划分为 4 个模块：
  - `strategy.rs`：`TrainingAnnotation`(+Builder)/`TrainingContext`/`SelectionDecision`/`TrainingSelectionStrategy`(trait)/`SamplingRateStrategy`/`ExplicitMarkingStrategy`/`is_expired`
  - `reward.rs`：`RewardCalculator`(trait) + `FnRewardCalculator` + `ZeroRewardCalculator`
  - `backend.rs`：`dto::{CommitRequest,FeedbackRequest,StatusResponse}` + `TrinityClient`(+Builder) + `TrinityModelAdapter`(+Builder)
  - `runner.rs`：`task_id_generator`/`RunExecutionContext`/`RunRegistry`/`TaskExecutionRegistry`(+`RegistryStats`)/`TrainingConfig`(+Builder)/`AgentCloner`(+`ShadowAgentSpec`)/`TrainingRouter`(+`ShadowRunReport`)/`TrainingRunner`(+Builder)
- **验证命令与结果**（2026-09-18 收尾重验）：
  - `cargo build -p agentscope-extensions-training` → 通过（Finished，exit 0，无 error）
  - `cargo test -p agentscope-extensions-training` → **64 passed; 0 failed**（strategy 12 / backend 9 / reward 2 / runner 21；doc-test 0）
  - 首轮基线 37 passed；本轮补齐 27 个离线单测（无网络、无 Trinity 依赖）。
- **复用既有 trait（未重建平行类型）**：
  - `core::Model` trait：`TrinityModelAdapter` 实现它并委派内层 `Arc<dyn Model>`（组合模式，对齐 Java）。
  - `core::ReActAgent` / `ReActAgentBuilder`：`AgentCloner` 用 builder 重建 shadow；name/description/toolkit 经 `Agent` trait 回读。
  - `Msg`/`ChatResponse`/`GenerateOptions`/`ToolSchema`/`ExecutionConfig` 全部复用 core。
  - 与 harness 无新增耦合（未触及 `DistributedStore` 等）。
  - 已确认：**core 无通用 `Hook`/`PreCallEvent`/`PostCallEvent` 系统**（仅有语义不同的 `middleware::Middleware`），故 `TrainingRouter` 不能照搬 Java Hook。
- **本轮新增代码**：`TrainingContext::mark_with_metadata(...)`（补齐 Java `mark(Map)` 重载），以及 4 个业务文件测试模块扩写；无破坏性改动、未删除既有功能/测试。
- **依赖**：仅本 crate `Cargo.toml` 新增 `reqwest`、`uuid`（均走已有 workspace deps，对应 0.12 / 1，**未改根 Cargo.toml**）。未引 `rand`（用内部 xorshift PRNG）。
- **已知差异/设计决定**：
  1. Java `TrainingSelectionStrategy.shouldSelect(agent, inputs, output, ContextView)` 的 Reactor `ContextView` → Rust 改为显式 `Option<&TrainingAnnotation>`（无等价隐式调用域上下文）。
  2. Java `TrainingRouter` 是 `Hook`（`PreCallEvent`/`PostCallEvent`）；Rust 无等价 post-call hook，改为显式驱动 API `decide(...)` + `run_shadow(...)`（保留 `-shadow` 递归保护、选择分支、repeatTime/runId/feedback 全流程）。
  3. Java `AgentCloner` 用反射提取 `sysPrompt`/`maxIters` 等；Rust 无反射 → name/description/toolkit 从 `Agent` trait 读取，不可回读项由 `ShadowAgentSpec{sys_prompt,max_iters}` 显式传入。
  4. Java `RewardCalculator.calculate(Agent)` → Rust `calculate(&ReActAgent)`（具体类型）。

### 4.2 剩余缺口（分级）与阻塞 / 跨模块依赖
**P0：无。** 核心链路（strategy/reward/backend/runner 四域 + 64 项离线测试）已全部落地且全绿。

**P1：`AgentCloner` 缺三个 ExecutionConfig 类字段的迁移（需 core 支持）**
- Java `cloneReActAgent` 反射提取 `modelExecutionConfig`/`toolExecutionConfig`/`toolExecutionContext` 并回注 builder；Rust `ReActAgent` 无对应 getter，`ShadowAgentSpec` 也未覆盖，这三个字段在 shadow agent 上回退为默认值。
- 裁定建议（择一，由汇总方决定；**本 crate 未改 core**）：(a) core `ReActAgent` 暴露 `model_execution_config()`/`tool_execution_config()`/`tool_execution_context()` 访问器，本 crate 再扩 `ShadowAgentSpec`；(b) 归档为「设计决定（不迁移）」，因默认 `ExecutionConfig` 对 shadow 训练无实质影响。当前不阻塞。

**P1：`TrainingRunner.start()` 无法全局挂载 hook（需 core/harness 支持）**
- Java `start()` 通过 `AgentBase.addSystemHook(router)` 全局注册；Rust 无等价机制。当前 `TrainingRunner` 仅管理自身生命周期 + commit 调度，**不会自动挂到 agent**；调用方需自行把 `TrainingRouter` 接入 agent 调用路径。
- 需跨模块裁定是否在 core 引入 hook 注册点，再由汇总方统一处理。

**P2：live 验证待条件**
- `TrinityClient`/`TrinityModelAdapter` 真实 HTTP 往返需 Trinity 服务端点；本轮单测均离线（loopback mock + 假 Model），未做真连。

**P2：全局单例隔离**
- `RunRegistry`/`TaskExecutionRegistry` 为进程内全局单例（对齐 Java 静态字段）；如需多实例隔离需后续改造。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-training` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-training` / `cargo test -p agentscope-extensions-training` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-training/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

