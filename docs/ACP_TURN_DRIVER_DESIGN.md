# ACP TurnDriver 抽象层设计

> 状态：设计稿（未实施）。目标：一份 ACP 代码支持「durable 驱动」与「原生会话驱动」两种运行模式，**功能不降级**，可一键切换。
> 相关提交：`ad50390`（ACP 改由 durable runtime 驱动，M4-e-3）、`3928e12`/`d608f22`（B3-phase2）。

## 0. 结论摘要

- 两套实现的差异**只集中在 turn 执行**：`session.rs` 4484 行中，协议层（session 信息、model/thought level、config sidecar、archive、elicit、权限、事件渲染）在 `ad50390` 前后**几乎逐行相同**；差异集中在 `SessionRuntime` / `open_runtime` / `build_runtime` / `drive_turn` / `settle_*` / `reconcile_*`。
- 因此抽象层只需要一个 **`TurnDriver` trait（6 个方法）** + 一次 `AcpSession` 字段拆分，其余代码不分叉。
- `NativeDriver` **不降级**：协议能力（load/resume/status/cancel/HITL/subagents/usage/plan）全部实现；它自带一个轻量会话历史载体（消息级快照），因此不依赖 `agentscope-durable`。
- 两者真正的差异是**物理保证**而不是功能：跨进程 lease、崩溃原子性、多 host 协作只有 durable 驱动提供；文档 §4 逐项列出，且 `capabilities()` 只暴露这些物理位，不用于隐藏功能。
- 切换支持运行期（配置项）与编译期（`feature = "durable"`）两种形态，可叠加。

## 1. 背景与目标

### 1.1 现状

- ACP 硬依赖 `agentscope-durable` + `agentscope-durable-adapter-agentscope`（`Cargo.toml` 非 optional）。整个 crate 只有 3 个源文件引用它们：`src/session.rs`、`src/deferred_effects.rs`、`tests/acp_live_memory_test.rs`。
- `session.rs` 中 `durable|SessionRuntime|TxnBackend|ThreadJournal` 共 69 处引用，集中在：
  - `SessionRuntime`（约 645–950 行）：admit / tick / lease / journal / settle 缓存；
  - `durable_root` / `durable_thread` / `durable_backend` / `open_runtime` / `build_runtime`（约 1302–1437 行）；
  - recorder 构造（约 4255–4278 行）。
- `deferred_effects.rs` 实现 durable 的 `Executor` 扩展点，只回答「后台 tool call 最终结果」；其注释明确「Tool calls are dispatched by the agent loop, not by the durable executor」（`deferred_effects.rs:56`）。
- durable 引入的内存成本（SQLite 20 轮，8KB+8KB）：`flush:done` 210.9MB、`djournal:facts` 171.0MB、`djournal:read` 96.3MB、`acp:settle` 44.1MB，合计 **522MB / 626MB（83%）** 的分配 churn 来自 durable 路径；`@commit=9.55MB`、`peak_commit=11.61MB` 为 durable 专属。

### 1.2 目标

1. 引入 `TurnDriver` 抽象，`AcpSession` 只保留协议状态。
2. 两个实现：`DurableDriver`（现有行为，逐字节不变）、`NativeDriver`（不依赖 durable）。
3. 协议能力在两个驱动下**全部可用**；不允许用 `capabilities()` 报告「不支持」。
4. 运行期可切换，编译期可裁剪。
5. 现有测试与 20 轮内存基线在 durable 模式下不变。

### 1.3 非目标

- 不追求 native 驱动提供跨进程 lease / 崩溃原子性（物理限制，见 §4.2）。
- 不改变 `durable.v1` wire 契约和 `FileStore`/SQLite 磁盘布局。

## 2. 耦合面证据

| 证据 | 数据 |
|---|---|
| 旧版 `session.rs`（`ad50390^`） | 3451 行，其中 `TurnStatus/admit/lease/tick/settle/finalization/checkpoint` 39 处 |
| `ad50390` 改动 | `session.rs +591/−257`（3451 → 3710），提交说明：「ACP 不再驱动 AgentSession」 |
| 当前 `session.rs` | 4484 行（后续 +774 行均为 durable 路径上的新能力） |
| 引用 durable 的文件 | 3 个（`Cargo.toml`、`session.rs`、`deferred_effects.rs`）+ 1 个测试 |
| `session/load` 回放源 | `ThreadJournal`（`session.rs:650` 注释） |
| `session/list` / `status` / `resume` | 读 workspace 内的 config sidecar（`session.rs:1283`） |
| finalization 对 recorder 的硬要求 | `execution_host.rs:391`：缺失即报 `Durable finalization requires a native recorder` |
| `persist_state` 对 recorder 的可选性 | `execution_host.rs:331`：缺失直接 `Ok(())` |
| recorder 注入点 | `harness_adapter.rs:279`（无条件创建并注入） |

## 3. 抽象层

### 3.1 `TurnDriver`

```rust
/// 一个 ACP 会话的 turn 执行器。协议层只经过这个 trait 与驱动交互。
#[async_trait]
pub trait TurnDriver: Send + Sync {
    /// 跑一个 turn。事件经 `EventSink` 写入 session 的 event_queue。
    async fn run(&self, input: TurnInput, sink: EventSink) -> Result<TurnOutcome, DriverError>;

    /// 请求停止当前 turn；返回时 turn 已到终态（或已排队停止）。
    async fn cancel(&self, turn_id: &str) -> Result<(), DriverError>;

    /// 当前 turn 视图：status / waiting / 用量 / 停止原因。
    async fn view(&self) -> Result<TurnView, DriverError>;

    /// session/load 的历史回放（按时间顺序，至少含全部消息）。
    async fn replay(&self) -> Result<Vec<ReplayItem>, DriverError>;

    /// session/resume：把上次未完成的 turn 标为 interrupted，并收尾未决 tool call。
    async fn resume(&self) -> Result<ResumeOutcome, DriverError>;

    /// 物理保证（不是功能开关）。协议层据此决定是否允许跨进程/多 host 场景。
    fn guarantees(&self) -> Guarantees;
}
```

配套类型：

```rust
pub struct TurnInput { pub message: Msg, pub turn_id: String, pub accepted_at_ms: i64,
                       pub model: String, pub thought_level: String }
pub struct TurnOutcome { pub stop_reason: StopReason, pub usage: Option<Usage> }
pub struct TurnView { pub status: TurnStatusView, pub waiting_for: Option<String>,
                      pub usage: Option<Usage>, pub stop_reason: Option<StopReason> }
pub enum ReplayItem { Message(Msg), TurnBoundary { turn_id: String, status: String } }
pub struct ResumeOutcome { pub resumed_turns: usize, pub reconciled_tools: usize }
pub struct Guarantees { pub cross_process: bool, pub crash_atomic: bool, pub multi_host: bool }
```

**关键约束**：`run` 是唯一的执行入口，事件通过 `EventSink` 推给协议层——这与现有 `event_queue` 结构一致，协议层不需要知道事件来自 journal 还是 agent stream。

### 3.2 数据流

```
ACP client ──prompt──▶ AcpSession（协议层）
                          │ TurnDriver::run(input, sink)
                          ▼
                 ┌────────┴────────┐
        DurableDriver          NativeDriver
        SessionRuntime          HarnessAgent::stream_events
        TxnBackend + Journal    SessionHistory（轻量快照）
                 └────────┬────────┘
                          ▼
                   EventSink ──▶ ACP notifications
```

### 3.3 `AcpSession` 字段拆分

| 留在 `AcpSession`（协议状态） | 移入驱动实现（turn 状态） |
|---|---|
| `session_id` / `cwd` / `additional_directories` | `runtime: Mutex<Option<Arc<SessionRuntime>>>` |
| `model` / `thought_level` / `mode` / `config sidecar` | `durable_backend` / `durable_thread` / recorder |
| `client_support` / `event_queue` / `cancel epoch` | `journal` / `lease` / settle 缓存 |
| `title` / `archived` / `auth` | `drive_turn` 的 tick 循环与 actor |

这一步是纯搬移：协议层方法（`session_info`、`model`、`set_model`、`persist_config`、`advisory_notification`、`respond` 的协议部分）签名不变。

### 3.4 `run` 的交互契约（P1.5b）

`run_prompt_inner` 不是「执行」一段代码，而是**协议层驱动执行**的 600 余行循环：它同时做渲染通知、判定取消、驱动 tick/settle 三件事。整段搬进 driver 会让 driver 变成第二个 session，所以先把三件事分开：

| 归属 | 内容 |
|---|---|
| `AcpAgentSession`（协议层） | `emit`（`EventSink` 的接收端）、取消意图（推进 `TurnState::cancel_epoch`）、`turn.usage` 归集、权限/客户端/subagent/model 选择 |
| `DurableDriver`（turn 状态） | store、`SessionRuntime`、tick loop、settle、turn 记录的读取 |

契约：

1. `run(input, sink)` 自己完成 admit（写输入）→ 起 tick → 逐轮 tick/settle → 每次状态变化经 `sink` 发通知 → 结束时返回 `TurnOutcome { stop_reason, usage }`。
2. 取消不是 `run` 的参数：`DriverInput.cancel_epoch` 是开始前捕获的代数；session 收到 cancel 时推进代数，driver 在 tick 边界比较，一致才继续。语义与现有 `cancel_epoch_moved` 相同，只是比较动作从 session 移到 driver。
3. `view()` 只给 `run` 自己与「取消后确认终态」用，不接协议（§4.1 已更正）。
4. 权限/elicit 不需要新契约：工具执行时由 harness 的 permission store 同步回调，driver 只是把 `OffloadedToolEffects` 交给 runtime，与现在一致。
5. `sink` 是同步闭包，session 侧的实现就是「把通知塞进 `event_queue`」——入参与现在的 `emit` 完全相同。

按此契约，P1.5c 的改动是：把 `run_prompt_inner` 里 `open_runtime` 之后到 settle 结尾的一段移入 `DurableDriver::run`，session 侧只剩「构造 `DriverInput` + 传入 `sink` + 归集 usage + 返回它给的 stop reason」。

### 3.5 P1.5c 的搬移清单

入口与边界（`session.rs`，行号以 `da847af` 为准）：

- 函数：`run_prompt_inner`（3163 – 约 4010）。
- **留在 session（协议部分）**：函数签名与 `F: Fn(SessionNotification)` 约束、`prompt_lock` 等待与 `cancel_epoch` 捕获（3163 – 3230）、`turn.usage` 的归集与 `prompt_response` 构造。
- **移入 `DurableDriver::run`**：`open_runtime` 之后（3277）到 settle 结尾的全部逻辑，含 `MAX_INTERRUPTED_RESUMES`（3451）、`TickLoop::spawn`（3461）、`"interrupted"` 续跑（3913）、`failed`/`Refusal` 文案。
- **需改写**：`emit(x)` → `sink(x)`；`self.session_id` → driver 自有的 `session_id`；`self.turn.usage` → 局部 `ChatUsage` 累加，结束时经 `TurnOutcome.usage` 返回。
- **不需改写**：`open_runtime`（driver 已有）、`reconcile_*` / `settle_*`（模块级函数）、`TickLoop`（模块级）。
- **验收**：ACP lib 138 passed + 20 轮 SQLite 诊断逐项基线不变（`flush:done` 210.9MB / `djournal:facts` 171.0MB / `djournal:read` 96.3MB / `acp:settle` 44.1MB / `peak_live` 3.57MB）。

### 3.6 `NativeDriver::run` 的渲染可以共享（P3b 的关键结论）

`ad50390^`（durable 接管之前的驱动）里的渲染循环与 `DurableDriver::run` 现在持有的段体**同构**：同样处理 `AgentEventType::TextBlockDelta`（`event::map_delta_event`）、`AgentResult`（`event::map_result_event`，含 `Interrupted` 的特殊文案）、`ModelCallEnd`（usage 累计 + `usage_notification`）、`Custom`（`compaction_marker` → `compaction_notifications`，受 `supports_compaction` 门控）。两者的唯一区别是事件来源：

| 驱动 | 事件来源 |
|---|---|
| `DurableDriver` | `SessionRuntime::set_event_sink`（recorder 写完后回放） |
| `NativeDriver` | `HarnessAgent::stream_events(Vec<Msg>, StreamOptions)` 的流 |

因此 P3b **不应复制**这段渲染，而应抽出一层：

```rust
struct RenderCtx<'a> {
    session_id: SessionId,
    message_id: Option<MessageId>,
    caps: PromptCaps,
    emitted_text: bool,
    turn_usage: &'a mut ChatUsage,
    last_input_tokens: &'a Arc<AtomicU64>,
}

// `Custom` 可能一次产出多条通知，故调用方传入接收器而不是返回 Option。
fn render_event(ctx: &mut RenderCtx<'_>, event: &AgentEvent, out: &mut dyn FnMut(SessionNotification));
```

抽取顺序：先在 `DurableDriver::run` 内部把四个分支换成一个 `render_event` 调用（行为不变，138 lib + 20 轮基线回归），再让 `NativeDriver::run` 用它。直接复制 400 行会让两个驱动从此各自演化，而这正是本接缝要避免的事。

## 4. 能力矩阵（功能不降级）

### 4.1 协议能力

| ACP 能力 | DurableDriver | NativeDriver |
|---|---|---|
| `prompt` / 流式事件 / stop reason | `SessionRuntime` tick | `HarnessAgent::stream_events` |
| `cancel` | 写 control 意图 + 等 turn 终态 | 置 cancel epoch + 等 stream 结束 |
| `session/status` | 会话级 `Open`/`Saved`（注册表 + 工作区探测），与驱动无关 | 同左 |
| prompt 内部的 turn 视图 | `frame` 读 turn 记录 | 驱动内存中的 turn 视图 |
| `session/load` | `ThreadJournal` 回放 | `SessionHistory` 快照回放 |
| `session/resume` | reconcile unsettled + 续跑 | 从历史找出未决 tool call + 续跑 |
| HITL（elicit / permission） | 协议层回调 | 同左（与驱动无关） |
| subagents | harness | harness |
| `end_turn_token_usage` | 驱动返回 usage | 同左 |
| plan operations | 协议层 | 协议层 |
| `session/archive` / `set_title` / `auth` | config sidecar + 协议层 | 同左 |
| offload tool call 结果 | `deferred_effects::Executor` | agent loop 直接处理（`deferred_effects.rs:56` 已说明） |

> `session/status` 返回的是**会话级**状态（`SessionStatus::Open` / `Saved`，`coding_agent.rs:838`），不读 turn 记录；`TurnDriver::view` 的消费者是 `run` 自己（取消后确认终态、内层循环判断），不是协议层。设计初稿把二者混为一谈，此处更正：`view` 没有协议级入口，它的接管只能随 `run` 一起做。

### 4.1.1 `EventSink` 装的是什么

`EventSink` 发的是**已渲染的 ACP 通知**（`SessionNotification`），不是 core `AgentEvent`。一轮要报的东西——tool call 的状态迁移、失败的文案、stop reason——是从驱动产出的 frame 上读出来的，所以渲染是驱动的职责；若改成「驱动发 core 事件、协议层渲染」，等于把 `run_prompt_inner` 里全部 `super::event::*` 调用重写一遍，那是重写而不是本接缝要做的搬移，也与「功能不降级」冲突。协议层仍然不需要知道事件来历：它只把通知交给 `event_queue`。

### 4.2 物理保证（不是功能）

| 保证 | DurableDriver | NativeDriver |
|---|---|---|
| `cross_process` | ✅（lease + control） | ❌（进程内） |
| `crash_atomic` | ✅（事务 + 幂等重放） | ❌（无事务） |
| `multi_host` | ✅（scope 锁） | ❌ |

`Guarantees` 只表达这三项。协议层在 native 模式下遇到「另一进程正在写」这类场景时，按「单进程部署」语义处理，而不是报「不支持」。

## 5. NativeDriver 设计（不降级的关键）

### 5.1 会话历史载体 `SessionHistory`

不引入 durable，但需要一个能支撑 `load`/`resume` 的载体：

```
<cwd>/.agentscope/acp-sessions/<session_id>/
  ├─ messages.jsonl      # 追加写：{role, content, turn_id, ts}
  └─ turns.json          # turn 元数据：{turn_id, status, started_at, finished_at}
```

- 消息级快照，**不记录 journal 事件流**；一条消息一行，追加写 + fsync（沿用 `write_and_sync`）。
- 写入点：`NativeDriver` 在收到 `assistant`/`tool_result` 消息与 turn 起止时追加。
- 读取点：`replay()` 顺序读 `messages.jsonl`；`resume()` 读 `turns.json` 找非终态 turn。

### 5.2 `resume` 与未决 tool call

native 没有 journal，无法直接知道「哪些 tool call 未决」。规则：

1. 读 `messages.jsonl`，建立 `tool_use_id → 已收到 tool_result` 集合；
2. assistant 消息里出现、但没有对应 `tool_result` 的调用 = 未决；
3. 对这些调用合成 `tool_result`（错误或取消），写入历史，并把 turn 标为 `interrupted`；
4. 与 durable 的 `RECONCILE_REASON`（`session.rs:620`）保持相同的用户可见语义。

### 5.3 与 durable 的边界

| 项 | DurableDriver | NativeDriver |
|---|---|---|
| 存储 | `FileStore` / SQLite（`scopes/`、`journal`） | `acp-sessions/<id>/`（消息级） |
| 恢复粒度 | 事件级（可重放到任意位置） | 消息级 + turn 终态 |
| 依赖 crate | `agentscope-durable` + adapter | 仅 `agentscope-core` / `agentscope-harness` |
| 内存成本 | 含 `djournal:*` / `commit` 路径 | 无 journal 分配 |

## 6. 切换机制

### 6.1 运行期

```
AGENTSCOPE_TURN_DRIVER=durable|native   # 默认 durable
```

- `AcpSession::new` 按配置构造驱动；两个实现都编译进二进制。
- 未编译 durable 时，配置 native 为唯一值（`AGENTSCOPE_TURN_DRIVER=durable` 返回明确错误）。

### 6.2 编译期

```toml
[features]
default = ["rusqlite", "durable"]
durable = ["dep:agentscope-durable", "dep:agentscope-durable-adapter-agentscope"]
durable-sqlite = ["durable", "dep:agentscope-durable-sqlite"]
```

- `--no-default-features` 时只编译 `NativeDriver`，ACP 二进制不含 durable/adapter/sqlite。
- 现有 `durable-sqlite` feature 依赖新增 `durable` feature。

## 7. 迁移步骤（每步可回退）

| 阶段 | 内容 | 验收 |
|---|---|---|
| P1 ✅ | 定义 `TurnDriver` 与配套类型；`runtime` 槽与 `open_runtime`/`build_runtime` 归 `DurableDriver`（`7abebd2`、`865d55e`） | 全量测试绿（ACP lib 138）；行为零变化 |
| P1.5 ✅ | 把 `AcpSession` 拆成 `ProtocolState` + `TurnState`（`cancel_epoch`/`closed`/`prompt_lock`/`running`/`turn_usage` 归后者），再把 `run_prompt_inner` 整体搬到 driver（`da847af`、`2157b23`） | 20 轮基线逐项不变（`djournal:facts`/`djournal:read`/`acp:settle`/`checkpoint:payload`/`peak_live` 均一致） |
| P2 ✅ | `SessionHistory`（`messages.jsonl` / `turns.json`）+ 单元测试（`2eb8740`） | 写入/回放/resume 单测（4 条） |
| P3 ✅ | `NativeDriver`：`run`/`view`/`replay`/`resume`（`2eb8740`、`afed959`）；`cancel` 走 prompt 准入时携带的 generation，trait 方法返回 `Unavailable` | `run`/`replay`/`resume`/`view` 单测全绿（3 条）；端到端验收随 P4（native 尚未被选中） |
| P4 ✅ | 运行期切换 + 错误路径：`TurnDriverKind` 选 `durable`/`native`（`a316500`）；native 补齐中断续跑、重启后历史下发、取消子代理任务、错误前缀去重 | 同一组 ACP 集成测试在 `AGENTSCOPE_TURN_DRIVER=durable|native` 下全绿（148 lib + 全部集成目标） |
| P5 ✅ | `feature = "durable"` 可选：依赖标 `optional`、`default = ["rusqlite", "durable"]`；`session.rs` 的 durable 块、`deferred_effects`、`event_queue`、`TurnDriverKind` 全部 `#[cfg]` | `--no-default-features` 全量绿（132 lib + 全部集成目标，`acp_live_memory_test` 经 `required-features` 跳过）；durable 20 轮基线逐项不变 |
| P6 | 回归：durable 20 轮基线、全量 workspace 测试、fmt | 与本文 §4 一致 |

**P1.5 不是「把方法搬到 driver」**：`run_prompt_inner` 是 `&mut self`，并深度读写 `cancel_epoch`/`closed`/`prompt_lock`/`running`/`turn_usage`/`client`/`permission_store`。直接搬方法会让 driver 变成第二个 session；正确顺序是先按「协议状态 / turn 状态」拆字段，再搬执行体。P1 的编译错误只出现在 import 与命名层面（`TurnInput` 与既有枚举同名、`async_trait` 需全路径），说明「字段归并 + 委托」是低风险的那一半，真正的成本在 P1.5。

### 7.1 P4 的入口清单（已完成）

1. `AcpAgentSession` 增加 `driver: Arc<dyn TurnDriver>`，构造时按 `AGENTSCOPE_TURN_DRIVER`（`durable`|`native`，默认 `durable`）选择；两个驱动都实现 `TurnDriver`。**已落地**：没有 durable feature 时只有一个驱动，变量被忽略。
2. `AcpAgentSession::run_prompt_inner` 现在直接调 `DurableDriver::run`，改为经 `self.driver.run(...)`。
3. `session/load` 的 `replay`、`session/resume` 的 `resume`、`session/status` 的 `view` 改经 trait（调用点在 `coding_agent.rs`）。**纠正**：`view`/`cancel`/`resume` 在协议层没有消费者（`session/status` 返回的是会话级 `Open`/`Saved`），因此只接了 `run` 与 `replay`；其余留为 trait 上的 `Unavailable` 占位。
4. native 模式构造 `NativeDriver::open(cwd.join(".agentscope"), session_id, Some(agent))`；durable 模式沿用现有 `durable_root()`/`durable_thread()`。**已收敛**：两种模式共用一个 `native_driver()` 构造点。
5. `Cargo.toml`：`durable = ["dep:agentscope-durable", "dep:agentscope-durable-adapter-agentscope"]`、`default = ["rusqlite", "durable"]`、`durable-sqlite = ["durable", ...]`；用 `#[cfg(feature = "durable")]` 包住 `session.rs` 的 durable 方法与 `impl DurableDriver`。
6. 端到端：同一组 ACP 集成测试跑两遍（`AGENTSCOPE_TURN_DRIVER=durable|native`）。
7. native 侧待补：`session/list` 枚举 `acp-sessions/`；`resume` 的 tool 调用合成（`reconciled_tools` 现恒为 0）；`TurnView` 的 `usage`/`stop_reason`（现只填 `status`）。

## 8. 验收标准

1. `AGENTSCOPE_TURN_DRIVER=native` 下：prompt/cancel/status/load/resume/HITL/subagents/usage/plan 全部可用，无「不支持」分支。
2. `AGENTSCOPE_TURN_DRIVER=durable` 下：20 轮 SQLite/FileStore 基线（`flush:done`、`djournal:*`、`peak_live`、`sbytes`）与 P1 前逐项一致。
3. `cargo test -p agentscope-extensions-acp` 在三种 feature 组合下全绿：默认、`--no-default-features`、`--features durable-sqlite`。
4. `cargo fmt --all -- --check` 干净。

## 9. 风险与未决

| 风险 | 说明 | 缓解 |
|---|---|---|
| 双驱动行为漂移 | 旧路径历史 bug（取消卡死、中断落 failed）必须在 native 实现里带回 | 把 `ad50390` 的两个修复写成 native 驱动的验收用例 |
| `AcpSession` 拆分面大 | 协议方法大量读 turn 状态；`run_prompt_inner` 是 `&mut self` 且读写 7 个可变字段 | P1 探针已验证「字段归并 + 委托」低风险（编译错误只在 import/命名）；执行体搬移按 P1.5 先拆 `TurnState` |
| 历史快照一致性 | 进程崩溃时 `messages.jsonl` 可能有半行 | 追加写 + 读侧丢弃不完整尾行（与 `FileStore` 的 log 语义一致） |
| `messages.jsonl` 体积 | 长会话线性增长 | 先不做压缩；后续可加 turn 边界截断/压缩 |
| 运行期切换的测试矩阵 | 两种驱动 × 三种 feature | P4 用同一组集成测试参数化 |

### 未决问题

1. `SessionHistory` 是否复用 ACP 现有 `config sidecar` 目录（`session_config_path`），还是独立 `acp-sessions/`？**已定：独立 `acp-sessions/<session_id>/`**（已落地）。
2. native 模式下 `session/list` 是否需要枚举 `acp-sessions/`？**建议需要**，保持与 durable 一致。
3. `Guarantees` 是否要暴露给 ACP client（作为 capability 元数据）？**建议暂不暴露**，仅在内部用于多 host 场景判定。
