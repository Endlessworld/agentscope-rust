# Durable 差距收敛方案（P0/P1/P2）

> 输入：`docs/DURABLE_MIGRATION_PLAN.md`（M6 以后的独立验收项）+ 2026-11-20 差距核对清单。
> 状态：**方案待拍板**（本文只描述做什么、怎么做、怎么验收；未改任何代码）。
> 原则：内核语义不动摇，只补「外围装配层」与「使用入口」。

## 0. 一句话

Java 与 Rust 的语义内核已经同构；本方案把差距清单的 10 项拆成 10 个可独立验收的里程碑，
按 **P0（多实例能力）→ P1（能力面）→ P2（使用体验）** 顺序落地。
每个里程碑一个或多个线性提交（子项各自可独立回退；`dev` 上只 cherry-pick，不出现 merge commit）。

## 1. 不变量（每个里程碑都必须遵守）

1. **依赖方向不变**：`agentscope-durable` 不依赖 harness、不依赖 tokio/futures runtime；
   新增的 `futures` 只用于 `join_all`（无 runtime），常驻循环放在 `durable-worker`。
2. **wire 兼容**：不改既有 `durable.v1` 字段语义；新增字段一律 `#[serde(default)]`；
   新增端点属于可加性扩展，不升版本（若需与 Java 共用一个服务端再单独决策）。
3. **失败开放（fail-open）**：wakeup 发布、绑定写入、池化回收失败都只记 `tracing`，
   不得让一次提交、一次 tick 或一次工具效果因其失败。
4. **默认零行为变化**：所有新能力默认关闭或等价于当前行为（`parallelism=1`、
   `wakeups=None`、`executor` 仍为单例），现有测试基线不得下降。
5. **可测**：每个新端口都有内存实现 + 至少一条「重复/崩溃/丢回复」的反向用例。

## 2. 差距清单 → 里程碑

| # | 差距 | 优先级 | 里程碑 | 交付边界 |
|---|---|---|---|---|
| 1 | 无 `Wakeups`（trait/本地/MQ/后端提交后发布） | P0 | M1 | trait + `LocalWakeups` + `TxnBackend` 注入 + 宿主唤醒 |
| 2 | 无 `RocketMqRemoteExecutor`/`RocketMqWorker` 装配 | P0 | M3 | 三通道（request/reply/wakeup）+ 去重 + 闭环测试 |
| 3 | 无持久 inbox/outbox（`ExecutionStore`/`QueuedExecutor`） | P0 | M2 | store + queued executor + sweep，复用 `RecordStore.schedule/ready` |
| 4 | 单一 executor，`binding` 未校验 | P1 | M0 + M4 | `ExecutorRegistry` + 工具绑定目录 + 按名查询 |
| 5 | 无 `DelegationExecutor`、无 `fork` | P1 | M5 | adapter 的 `delegate` 工具路径 + `fork_thread`（复用 branch/restore） |
| 6 | 无 `SandboxBindings` | P1 | M6 | 绑定存储 + keepalive + 删除屏障 + 跨进程 attach |
| 7 | HTTP 仅 5 调用 `BACKEND` 客户端，无 `EXECUTOR`/`USER` 端点与 SSE | P1 | M7 | ✅ 已实施：`wire` + axum server + SSE + 全量 `HttpSessionBackend`（含资源/委派/恢复） |
| 8 | 无 quickstart；`agentscope-rs` 不含 durable | P2 | M8 | `examples/durable-quickstart` + features + 文档 |
| 9 | 无 `EventStreamOptions.deltas`；`scan` 串行 | P2 | M1 + M8 | `scan_with(parallelism)`、`events_with(options)`、SSE 过滤 |
| 10 | 无 `LocalBackend` 门面；objects 无 S3/OSS；mcp 无连接池 | P2 | M0 + M9 | 门面 + `ObjectTransport`/S3 形状 + `McpConnectionPool` |

**实施状态（2026-11-21）**：第 1–10 项全部落地，M0–M9 均已完成；M7 按用户要求的完整范围实施（不消减）。

## 3. 里程碑

### M0 门面与路由底座（小）

**目标**：让「接入」少一层手工装配，为 M4 的按名路由留好唯一入口。

- `crates/agentscope-durable/src/backend/local.rs`（新）
  - `pub struct LocalBackend`，`LocalBackend::open(root) -> Result<Arc<dyn SessionBackend>>`
    （内部 `TxnBackend::with_store(Arc::new(FileStore::open(root)?))`）；
  - `LocalBackend::in_memory()` 同样返回 `Arc<dyn SessionBackend>`。
- `crates/agentscope-durable-sqlite/src/lib.rs`（追加）
  - `pub fn open_backend(path) -> Result<Arc<dyn SessionBackend>>`（`SqliteStore::open` + `TxnBackend`）。
- `crates/agentscope-durable/src/executor.rs`（追加）
  - `pub struct ExecutorRegistry`：`register(name, Arc<dyn Executor>)`、`resolve(name) -> Option<Arc<dyn Executor>>`、
    `default_executor()`；未注册名字返回 `None`（由调用方报 `UNAVAILABLE`，**不**静默回退到默认）。
- `crates/agentscope-durable/src/runtime.rs`
  - `with_executor` 等价于 `with_executor_named("local", ...)`；新增 `with_executor_named`、`executors()`；
  - `DurableRuntime` 实现 `Clone`（`events` 改 `Arc<Mutex<Option<EventSink>>>`，clone 共享 sink），
    新增 `DurableSession::shared(Arc<DurableRuntime>, ThreadKey)`（保留现有 `new`）。

**验收**：`LocalBackend::open` 建会话→提交→tick 全流程；同一 store 由两个 `Arc` 句柄共享；
registry 未注册名返回 `None`；`cargo test -p agentscope-durable --lib` 与 `fmt` 干净。

**回退**：全部是新增 API + `Clone` 改造，删除新文件/还原 `events` 字段即可。

---

### M1 Wakeups 与并行扫描（中）

**目标**：多实例不再只能靠定时轮询；`scan` 可用受控并发。

- `crates/agentscope-durable/src/wakeups.rs`（新，无 tokio/futures 依赖）
  ```rust
  pub struct Wakeup { pub thread: ThreadKey, pub at: i64, pub reason: WakeupReason }
  pub type WakeupSink = Arc<dyn Fn(&Wakeup) + Send + Sync>;
  pub trait Wakeups: Send + Sync {
      fn publish(&self, wakeup: &Wakeup) -> Result<()>;
      fn subscribe(&self, sink: WakeupSink) -> SubscriptionId;
      fn unsubscribe(&self, id: SubscriptionId);
  }
  pub struct LocalWakeups; // Mutex<Vec<Option<WakeupSink>>>，发布时逐个调用
  // 契约：sink 必须非阻塞；需要异步的宿主在 sink 里投递到自己的队列（例如 tokio::spawn）。
  ```
- `SessionBackend` 新增默认方法 `fn wakeups(&self) -> Option<Arc<dyn Wakeups>> { None }`。
- `TxnBackend::with_wakeups(Arc<dyn Wakeups>)`；`commit` 成功后发布 `WakeupReason::Committed`，
  `submit` 成功后发布 `WakeupReason::Submitted`（只发线程键，不携带状态）。
- `DurableRuntime::scan_with(selectors, limit, parallelism)`：
  - `parallelism <= 1` 走现有串行路径（字节级等价）；
  - `> 1` 用 `futures::future::join_all` 分批，按 `ready` 顺序回填 `Vec<TickOutcome>`；
  - `scan` 委托 `scan_with(.., 1)`。
- `agentscope-durable-worker/src/resident.rs`（新，worker 加 tokio 依赖 `rt/time/sync`）
  - `ResidentRunner::new(runtime, selectors, options)`，`options { interval, parallelism, limit }`；
  - `run_until(shutdown)`：先 `scan_with`，再 `select!`（interval / wakeup sink / shutdown）；
  - `EdgeRunner`：只跑一次 `scan_with`（对应 `EdgeDriver`）。

**验收**：两个 runtime 共享同一 `RecordStore`，A `submit` 后 B 的 sink 在一个 interval 内收到线程键；
`parallelism=4` 时 8 个 ready 线程全部被 tick 且每个只被 tick 一次（用一个记录 claim 次数的 store 断言）；
`parallelism=1` 与旧 `scan` 结果逐项相同。

**风险**：SQLite 单写者下并行 `claim` 会争锁（有 10s busy timeout）→ 默认 1，并行收益集中在多线程分片 store；
文档明确「并行不是吞吐承诺，是显式选择」。

---

### M2 持久 inbox/outbox（中）

**目标**：工具效果的重投、去重、补发有载体；崩溃后不重复执行。

- `crates/agentscope-durable/src/execution_store.rs`（新；**放核心**，因为 rocketmq 也要用，
  避免 `rocketmq → worker → durable` 的反向依赖；worker 只 re-export）
  ```rust
  pub enum DispatchState { Pending, Dispatched, Settled, Unknown }
  pub struct DispatchRecord { execution_id, request: ExecutionRequest, state, attempts,
                              dispatched_at, result: Option<ExecutionResult> }
  pub trait ExecutionStore: Send + Sync {
      fn put(&self, record: &DispatchRecord) -> Result<()>;
      fn get(&self, execution_id: &str) -> Result<Option<DispatchRecord>>;
      fn pending(&self, limit: usize) -> Result<Vec<DispatchRecord>>;
      fn settle(&self, execution_id: &str, result: &ExecutionResult) -> Result<()>;
  }
  ```
  - `MemoryExecutionStore`；
  - `RecordStoreExecutionStore`：scope `"hand:executions"`，part `exec:<id>`；`pending` 用
    `RecordStore::schedule/ready`（复用现有就绪索引，不新造扫描协议）；
    queue 名必须带 scope（`exec:<scope>`），因为 `ready(queue, ..)` 是全局的，不能混会话。
- `crates/agentscope-durable-worker/src/queued_executor.rs`（新）
  - `QueuedExecutor { inner: Arc<dyn Executor>, store: Arc<dyn ExecutionStore> }`；
  - `start`：`Settled` → 返回缓存；`Dispatched` → 返回 `Pending`（**不重发**）；`Unknown` → 返回 UNKNOWN；
    否则先 `put(Dispatched)` 再 `inner.start`：成功写回结果，**失败写 `Unknown`（不是 `Pending`）**——
    一次失败的发布也可能已经到达对端，重发就是第二个效果；
  - `query`：先 store，未命中再 `inner.query`；
  - `cancel`：写 `Unknown`（取消本身不可证明）并调 `inner.cancel`；
  - `sweep(max_age, limit)`：对超过 `max_age` 仍 `Dispatched` 的记录用 `query` 查证，
    查不到 → `Unknown`（永不判成功）。

**验收**：`start` 两次只调用 inner 一次；模拟「写完 Dispatched 后崩溃」（重建 `QueuedExecutor`）→
第二次 `start` 返回 `Pending` 且 inner 调用次数仍为 1；`query` 返回 UNKNOWN 后 `sweep` 不重发；
`RecordStoreExecutionStore` 在 FileStore 与 SqliteStore 各跑一遍。

---

### M3 RocketMQ 三通道装配（大）

**状态**：✅ 已实施（2026-11-21）：`hands_route.rs`（三通道命名）、`hands.rs`（信封）、`remote_executor.rs`、`reply_inbox.rs`、`worker.rs`、`wakeups.rs`；`cargo test -p agentscope-durable-rocketmq` = 40 lib passed + 5 闭环测试。

**目标**：Brain–Hands 链路在 Rust 里有可运行的装配，而不仅是端口。

`crates/agentscope-durable-rocketmq/src/`（追加；不引入 SDK，继续走 `Broker` 端口）

- `hands_route.rs`：`request_topic(ns) = durable-<slug>-hands-req`，`reply_topic(ns) = …-reply`，
  `wakeup_topic(ns) = …-wakeup`；key = session storage key，tag = session。
- `hands.rs`：`HandsEnvelope { execution_id, kind: Request|Reply|Cancel, request, result, attempt }`
  （`serde`，`body` 复用现有 `Envelope` 的编码方式）。
- `remote_executor.rs`：`RemoteExecutor<B: Broker>`
  - `start`：发布 Request（`key = execution_id`），返回 `Pending { handle: execution_id }`；
  - `query`：从 `ReplyInbox` 读（见下），未命中 → UNKNOWN；
  - `cancel`：发布 Cancel；
  - `capabilities`：`lookup_by_execution_id = true`，`idempotent_start = true`（去重靠 M2）。
- `reply_inbox.rs`：`ReplyInbox<B>`：按 `execution_id` 持久化最近一条 Reply（复用核心的
  `ExecutionStore`，不新建存储）；游标按 reply 分区持久化（`RecordStore` part `cursor:reply`）。
- `worker.rs`：`RocketMqWorker<B, E: Executor>`：`poll_once()` 拉取 Request → `ExecutionStore` 去重 →
  `local_executor.start` → 发布 Reply / 记录 Pending；由 `ResidentRunner` 的 interval 驱动。
- `wakeups.rs`：`RocketMqWakeups<B>`：`publish` 发 wakeup 消息；`subscribe` 注册 sink，`poll_once` 拉取并分发。
- `dyn_broker.rs`：`DynBroker`（`async_trait` 包装），让 `Wakeups`/`Executor` 的 `Arc<dyn>` 可用。

**验收**（全部用 `InMemoryBroker`）：
1. Brain 侧 `submit` → `RemoteExecutor.start` 发一条 Request → Worker `poll_once` 执行 → 发布 Reply →
   Brain 侧 `query` 拿到 `Succeeded`；
2. 同一 Request 重复投递 3 次，本地 executor 只被调用 1 次；
3. Reply 丢失时 `query` 返回 UNKNOWN，`sweep` 不重发；
4. Wakeup 发布后同一进程的第二个 runtime 的 sink 收到线程键。

**风险**：RocketMQ 自身的重试/事务仍不做（与现有 crate 注释一致）；
本文档明确「有效一次」= 幂等键 + 消费端去重 + 持久 inbox，而不是 broker 事务。

---

### M4 按名路由与绑定校验（中）

**状态**：✅ 已实施（2026-11-21）：`tool_binding.rs`（`ToolCatalog`/`ToolBinding`/`MapCatalog`）、adapter 按名路由、`begin_dispatch`/`apply_tools` 绑定校验；durable lib 96 passed、adapter 12 + `tool_binding` 3。

**目标**：`ToolCall.executor`/`binding` 从「未校验字符串」变成真实路由依据。

- `crates/agentscope-durable/src/tool_binding.rs`（新）
  ```rust
  pub struct ToolBinding { pub executor: String, pub digest: String }
  pub trait ToolCatalog: Send + Sync { fn binding(&self, tool: &str) -> Option<ToolBinding>; }
  ```
- `HarnessContext` 增加 `pub executors: Arc<ExecutorRegistry>`（保留 `executor` 字段 = 默认执行器，
  既有 adapter 行为不变）。
- `agentscope-durable-adapter-agentscope/src/tool_facts.rs`
  - `tool_call(&ToolUseBlock, Option<&ToolCatalog>)`：目录命中时写 `executor`/`binding`，未命中维持 `"local"`；
  - `resolve_offloaded_effects` / `settle_cancelled_effects` 用 `ctx.executors.resolve(&tool.request.call.executor)`，
    未注册则该效果记 `UNAVAILABLE`（不猜成功）。
- `AgentScopeHarnessAdapter::with_tool_catalog(Arc<dyn ToolCatalog>)`（新增构造器，不改 `new`）。
- `TxnBackend::begin_dispatch`：若 `ToolCall.executor` 为空串或 `binding` 非 64 位十六进制 → `INVALID`
  （`ToolCall::validate` 已有 digest 规则，这里补「非空 executor」）。

**验收**：两个具名 executor，`binding` 指向第二个时 `query` 走第二个（用记录调用的 stub 断言）；
未注册名字 → `UNAVAILABLE` 且不产生效果；非法 digest 在 `submit`/`commit` 被拒；
`tool_facts_test` 现有用例全部保持绿。

---

### M5 委派与 fork（中）

**状态**：✅ 已实施（2026-11-21）：`delegation.rs`（`DelegationExecutor`）、`DurableRuntime::fork_thread`、adapter `delegate` 拦截；`delegation` 2 passed、adapter 12。

**目标**：子 Thread 有正式入口，父 turn 恢复后能找到它。

- `crates/agentscope-durable/src/runtime.rs`
  - `delegate(&self, lease, execution_id, request_id, target) -> DelegationView` 的 runtime 包装；
  - `fork_thread(&self, parent: &ThreadKey, request_id, spec) -> Result<ThreadView>`：
    以父线程当前 checkpoint 为基线，`Restoration { expected_position }` + `create_thread`，
    子线程 id 由 `child-<sha256(parent.thread_id + request_id)[..12]>` 生成（与 Java 命名对齐）；
    幂等：同 `request_id` 返回同一子线程。
- `crates/agentscope-durable/src/delegation.rs`（新）
  - `DelegationExecutor<'a> { backend: &'a dyn SessionBackend, lease: &'a Lease }`：**不是** `Executor`，
    而是「本 tick 绑定了 lease 的委派入口」，提供 `delegate(execution_id, request_id, target) -> DelegationView`；
  - 不塞进 `ExecutorRegistry`：`Executor` 没有 lease，而 `backend.delegate` 必须围栏；
    用 `Executor` 承载委派会让「谁持有 lease」变得不可判定（这是 Java 里由线程作用域解决、Rust 必须显式传的问题）。
- `AgentScopeHarnessAdapter`：识别 `delegate`/`fork` 工具名（可配置前缀），在本地 journal 已记录 intent 之后、
  调用普通工具之前拦截，用 `DelegationExecutor` 调 `backend.delegate`，把 `child.thread/turn` 作为
  `Succeeded` 结果写回 `accept_result`；其余工具不受影响。

**验收**：父 turn 调一次 delegate → 子线程存在且 `DelegationView.parent.execution_id` 正确；
重复 `request_id` 不建第二个子线程；父 turn 崩溃恢复后仍指向同一子线程；
直接用 `DelegationExecutor` 的单测覆盖「未知 target → INVALID」。

---

### M6 SandboxBindings（中）

**状态**：✅ 已实施（2026-11-21）：`bindings.rs`（`BindingRecord`/`BindingStore`/`FileBindings`）、keepalive、`Attached→Releasing→Gone` 屏障；`bindings_test` 5 + `sandbox_executor_test` 16 全过。

**目标**：沙箱身份跨进程可恢复，keepalive 与删除有屏障。

- `crates/agentscope-durable-sandbox/src/bindings.rs`（新）
  ```rust
  pub struct BindingRecord { execution_id, thread, handle: Handle, spec: ExecSpec,
                             deadline_ms: Option<u64>, state: Attached|Releasing|Gone, updated_at }
  pub trait BindingStore: Send + Sync {
      fn put(&self, record: &BindingRecord) -> Result<()>;
      fn get(&self, execution_id: &str) -> Result<Option<BindingRecord>>;
      fn release(&self, execution_id: &str) -> Result<()>; // 先写 Releasing，kill 成功后写 Gone
      fn live(&self, limit: usize) -> Result<Vec<BindingRecord>>;
  }
  pub struct SandboxBindings<S: BindingStore> { … }
  ```
  - `MemoryBindings` + `FileBindings`（`bindings.json` + 原子 rename，与 `SessionHistory` 同法）；
- `SandboxExecutor::with_bindings(bindings: Arc<dyn BindingStore>)`：
  - `start` 前写 `Attached`；`poll` 命中终态写 `Gone`；
  - 进程内 `live` miss 时读 store：`Attached` → 用 `handle` 继续 `poll`（跨进程 attach）；`Releasing` → 重试 `kill`；
  - `kill` 幂等：`Gone` 直接返回 `Cancelled`；
  - `keepalive(limit)`：对 `Attached` 且超过 `deadline_ms` 的执行 `poll`/`kill`，返回处理数。

**验收**：模拟「进程重启」——新 `SandboxExecutor` 只有 store 与同一个 `FakeSandbox`，
`query` 能继续 `FakeSandbox` 的脚本并得到 `SUCCEEDED`；`release` 两次只 kill 一次；
`Releasing` 记录在下次 `keepalive` 时被清理；`live` 不返回 `Gone`。

---

### M7 HTTP 服务端 + 全量远程 Backend（✅ 已实施 2026-11-21）

> 最终裁定（2026-11-21）：用户要求按完整 M7a/b/c 实施，**不消减**。下面的必要性评估保留为背景；
> 交付与验收记录见本节末尾「实施状态」。

**必要性评估（2026-11-21，结论：建议降为按需，不阻塞 P0/P1）**

先分清三者，它们不在同一层：

| 东西 | 协议/接口 | 消费者 | 解决什么 |
|---|---|---|---|
| **ACP** | 客户端 ↔ agent（JSON-RPC，现为 stdio） | IDE / CLI | 人机交互、会话 UI、权限确认 |
| **`agentscope-server`**（CF 规划 M4-1） | 应用 HTTP API（`POST /run` 异步派发）+ WS | 业务后端 / 云端网关 | 「把一次 turn 派给某个 agent 进程」 |
| **`durable-http`** | 执行宿主 ↔ `SessionBackend` / `Executor` 原语（claim/commit/frame/events、start/query/cancel） | 另一个 worker 或 Hands 进程 | 多进程共享**状态机**与**工具执行**，不面向人 |

**Java 源码核实（2026-11-21，`agentscope-extensions-durable-http` + `durable-runtime/DurableProtocol`）**

该模块不是「durable 连接远程 agent 实例」，而是把 durable 协议的三类内部端点各自搬上 HTTP，
三者共用版本化信封 `{version, operation, payload}` 与同一份 JSON Schema（`protocol/schema.json`）：

| Java 类型 | `Endpoint` | 方向 | 操作面 |
|---|---|---|---|
| `SessionBackendHandler` / `HttpSessionBackend` | `BACKEND` | Brain（执行宿主）→ 权威状态后端 | 完整内部 Backend（claim/commit/frame/journal/execution/delegate/restore/objects）；文档明确「not a public user endpoint」 |
| `ExecutorHandler` / `HttpExecutor` | `EXECUTOR` | Brain → Hands / Sandbox / Worker | `capabilities/start/query/cancel/output` 五个；远端是**执行器**，不是 agent |
| `DurableRuntimeHandler` / `DurableHttp` transport | `USER` | 前端 / API 进程 → 权威 Runtime | 26 个公共操作（create/submit/control/history/snapshot/events/fork/restore…），显式拒绝 claim/commit/native state/global discovery |
| `SessionEventHandler` + `SseEventWriter` | — | 观察者 → Runtime | committed SSE（`Last-Event-ID` 优先于 cursor）、live 预览帧不带 id、`CURSOR_EXPIRED` → 410 / `resync` |

`DurableProtocol.Endpoint` 只有这三个值；`DurableHttp` 的类注释即
"Versioned transport only; all transaction semantics remain in SessionBackend/ExecutionStore"。
大请求（>16 MiB）转 object store 引用 + `invokeObject`。

由此可判定：

1. **「durable 作为执行宿主直接连远程 agent 实例」不成立**：agent 始终由 Brain 本地
   `Harness`/Agent factory 构造并运行，远端只有**状态后端**与**工具执行器**。
2. 用户假设最接近的是 `EXECUTOR` 那条（Brain 把工具执行交给远端 Hands），它属于
   executor 体系，不需要 HTTP 服务端网关，也不是 ACP 的职责。
3. Rust 现有 5 个调用（`claim/commit/frame/events/receipt`）正对应 `BACKEND` 子集，
   缺的是 `EXECUTOR`（M3/M6 的 HTTP 变体）、SSE 与 `USER`。

因此：

1. **不与 ACP 重叠**：ACP 的 `session/update` 是 live 通知，没有 committed cursor、
   没有 claim/fence/commit；它无法替另一个 worker 托管 turn 状态。反过来，
   `durable-http` 也不能当客户端协议用（没有 `session/prompt`/权限语义）。
2. **但 M7 全量 backend 的必要性低**：当前唯一真实消费者（ACP 会话）用本地 FileStore/SQLite；
   同机多进程已有 SQLite 共享文件，跨机已有 `durable-jdbc`（Postgres）。
   只有当「worker 不能直连数据库、又不想引 JDBC」时，把 backend 放到 HTTP 后面才成立。
3. **成本与维护面很重**：`SessionBackend` 有 60+ 方法，每加一个原语都要同步一份 HTTP 映射；
   管理面（create/submit/history/turns）对 worker 来说通常不需要。
4. **ACP 的 HTTP 也不是现成能力**：Rust SDK 5.0.0 只提供 `stdio`/`process`，
   `run_with_transport` 可注入自定义 `ConnectTo`；规范 v2 的 Streamable HTTP 仍在 RFD，
   v1 没有消息重放。接入它要自研连接/会话头 + SSE 双流（工作量与 M7a 相当，且规范未定）。

**建议的 M7 重定义**（待拍板）：

- M7 从 P1 主线移除，改为「按需里程碑」，不阻塞 M1–M6；
- **优先级最高的是 `EXECUTOR` 远程化**（`HttpExecutor` ↔ `ExecutorHandler`，5 个操作），
  让工具执行落到独立进程/Sandbox；它是 M3（MQ）的 HTTP 变体，随 M3/M6 实施，
  不需要 M7 的服务端网关；
- 若做 M7，只做**执行宿主子集**（现有 5 个调用 → claim/commit/frame/control_state/
  execution/journal/events/receipt，约 9–10 个）+ 只读管理面 + SSE，
  不做 60 方法全量；`HttpSessionBackend` 未实现的方法显式 `UNAVAILABLE`；
- 对外部署入口交给 `agentscope-server`（应用 API + WS）与 ACP-over-WS/HTTP（P3 单独立项），
  `durable-http` 只保持「内部 backend 远程化」的定位。

**目标**：HTTP 从「5 个调用的客户端」变成「可部署的执行后端」。

**实施状态（2026-11-21，全部完成）**

| 交付 | 位置 | 验收 |
|---|---|---|
| 共享 wire（路径/头/分页/错误码） | `src/wire.rs` | 客户端与服务端同一份；`409` 先读 body `code` 区分 `CONFLICT`/`FENCED` |
| 全量远程 Backend（M7b + M7c） | `src/client.rs` `HttpSessionBackend` | 43 个 `SessionBackend` 方法全部实现（含 resources/delegate/restore），无 `UNAVAILABLE` 占位 |
| axum 服务端（M7a） | `src/server.rs` `DurableService::router()` | 41 条路由；`server` feature（默认关），不把 axum 带进默认构建 |
| SSE（M7a） | `src/sse.rs` | committed 帧带 `id`、`Last-Event-ID` 优先、`CURSOR_EXPIRED` → `resync`、心跳 |
| 契约测试 | `tests/http_contract.rs` | 9 项：终态/replay、冲突、陈旧 epoch、waiting/signal、checkpoint 去重、工具结算、管理面、resources、SSE cursor |

验证：`cargo test -p agentscope-durable-http`（15 lib）与 `--features server`（15 lib + 9 集成 + 1 doc）全过；
`cargo fmt --all -- --check` 干净。

实现取舍（与上面「建议的 M7 重定义」不同处，已按用户裁定执行）：

- 走完整计划而非降级：`server` feature 引入 axum 0.7 + tokio + futures，默认构建不变；
- `HttpSessionBackend` 覆盖全量而非 9–10 个执行宿主子集；
- `ready` 用 `POST /ready` + JSON body（`selectors`/`limit`），因为重复 query 键在 axum 的 `Query` 下不可靠；
- fenced 调用额外携带 `X-Durable-Expires-At` / `X-Durable-Thread` / `X-Durable-Turn`，服务端据此重建 `Lease`
  （后端 `fence` 校验 `expires_at`，缺它会把所有调用判成过期）。

`crates/agentscope-durable-http/Cargo.toml`
- 新 feature：`server = ["dep:axum", "dep:tokio"]`（默认关）、`client`（默认开，保持现状）。

**M7a 共享 wire + server**（`src/wire.rs`、`src/server.rs`、`src/sse.rs`）
- `wire.rs`：把现有路径/头/分页规则抽成 `pub fn path_*` + `Header` 常量，client 与 server 共用一份；
- `server.rs`：`DurableService { backend: Arc<dyn SessionBackend> }` + `router()`（axum 0.7）：
  - 身份：`POST/GET /v1/sessions`、`POST/GET /v1/sessions/{ns}/{sid}/threads`、`GET …/state`；
  - 队列：`POST …/threads/{tid}/turns`、`GET …/turns/{turn}`、`GET …/turns`、`GET …/history`、
    `POST …/control`、`POST …/signals`；
  - 所有权/提交：`POST …/claim`、`POST …/lease/renew`、`POST …/lease/release`、`GET …/frame`、
    `POST …/commits`、`GET …/commits/{id}/receipt`；
  - 事实：`POST …/runs`（prepare）、`GET …/runs`、`GET …/steps`、`GET …/model-calls`、`GET …/journal`；
  - 工具：`GET …/executions/{id}`、`POST …/dispatch`、`POST …/results`；
  - 发现：`GET /v1/ready?harness=`、`GET …/checkpoints`、`GET …/events`、`GET …/snapshot`、
    `POST …/prune`、`resources`/`delegate`/`restore` 的 `POST` 端点。
- `sse.rs`：`GET /v1/sessions/{ns}/{sid}/events/stream?after=&deltas=`：
  先回放 `events(after)`，再按 `interval` 轮询新事件；支持 `Last-Event-ID` 头；
  未注入 live sink 时只发 committed 事件。
- 错误映射：`DurableError::code()` → HTTP 状态（`CONFLICT=409` 带 receipt、`FENCED=409`、
  `NOT_FOUND=404`、`INVALID=400`、`UNAVAILABLE=503`、`CURSOR_EXPIRED=410`），与现有 client 映射表对称。

**M7b `HttpSessionBackend`（执行宿主子集）**
- 实现 `SessionBackend` 的：`claim/renew/release/frame/control_state/commit/receipt/`
  `prepare_run/begin_dispatch/accept_result/execution/journal/read_journal/turn/thread_state/`
  `ready/events/snapshot`；其余方法显式返回 `UNAVAILABLE`（**不**静默成功）。
- 验收：`backend_contract.rs` 的同一套断言对 `TxnBackend` 与 `HttpSessionBackend` 各跑一次
  （axum `oneshot`，无需真实端口）。

**M7c 管理面补齐（仅当出现真实远程管理端时）**
- `create/session/close_session/create_thread/thread/close_thread/threads/submit/admission/`
  `turns/history/control/signal/record_failure/runs/steps/model_calls/checkpoints/checkpoint/`
  `prune_events/resources/delegate/restore`；
- 验收：`resources_delegation_test` 的远程版本；SSE 断线用 cursor 重连不丢不重。

**风险**：服务端是「把同一个 `SessionBackend` 暴露出去」，不新建业务；
`axum` 只在 `server` feature 下编译，不增加默认构建体积。

---

### M8 使用入口（中）

**状态**：✅ 已实施（2026-11-21）：`agentscope-rs` 四个 durable feature；`examples/durable-quickstart`（文件/SQLite 两种 backend 实跑两次通过）；`DurableSession::turn_handle`/`TurnHandle::wait_until_terminal`/`pending_interactions`；`EventStreamOptions`/`DeltaFilter` + `SessionBackend::events_with` + HTTP `deltas=` + SSE 过滤。验证：`event_stream` 3、http 契约 11、quickstart 首次/第二次/`--sqlite` 输出符合验收。

**目标**：文章第 7 节的 quickstart 在 Rust 中可跑。

- `crates/agentscope-rs`：新增 `durable`（re-export `agentscope_durable`）、
  `durable-sqlite`、`durable-adapter-agentscope`、`durable-worker` 四个 feature；`full` 收录。
- `examples/durable-quickstart/`（新 workspace member）
  - `main.rs`：`LocalBackend::open("./durable")` → 一个最小 `HarnessAdapter`（不依赖模型）→
    `ensure_session` → `submit` → `tick` → 打印 `events`；第二次运行从 checkpoint 继续；
  - `README.md`：`cargo run -p durable-quickstart -- <root>` 两条路径（首次 / 重启）；
  - 另附 `--sqlite` 分支，验证同一代码换 backend。
- `docs/DURABLE_QUICKSTART.md`：从零到「进程被杀后继续」的 30 行说明。
- `DurableTurn` 句柄（M8b，低成本）
  - `DurableSession::turn_handle(turn_id) -> TurnHandle`；`TurnHandle::view()`、
    `TurnHandle::wait_until_terminal(timeout, poll_interval)`（轮询 `turn()`，不引入定时器依赖）、
    `TurnHandle::result()`；
  - `DurableSession::pending_interactions()`：用 `waiting()` 投影出 `waiting.key`（`interactionId`），
    终于给 `Interaction` 类型一个消费者。
- `EventStreamOptions.deltas`（M8c）
  - `durable-http`：`DeltaFilter { None, Message, ToolArguments, All }` + `events_request_with(.., options)`；
  - `SessionBackend::events_with(.., options)`（默认委托 `events`，TxnBackend 过滤 `event_type`）；
  - SSE 的 `deltas=` query 映射到同一 filter；**不**把 token 级 delta 写入持久事件。

**验收**：quickstart 首次运行输出 1 个 turn；删除进程后第二次运行读到同一 turn 并继续；
`--sqlite` 输出相同；`deltas=message` 只收到消息事件、`deltas=none` 只收到终态事件；
`TurnHandle::wait_until_terminal` 在 `Succeeded` 后返回且不再轮询；`pending_interactions()` 与
`waiting()` 的 `waiting.key` 一一对应。

---

### M9 objects / mcp 外围（中）

**状态**：✅ 已实施（2026-11-21）：`transport.rs`（`ObjectTransport`）、`s3.rs`（`S3ObjectStore`，feature `s3`，If-None-Match 语义）、`pool.rs`（`McpConnectionPool`）+ `McpExecutor::with_pool`；objects 4 + mcp pool 5 全过。

**目标**：远程对象存储与 MCP 连接复用有正式入口，但不引入云 SDK。

- `crates/agentscope-durable-objects/src/transport.rs`（新）
  ```rust
  pub trait ObjectTransport: Send + Sync {
      fn get(&self, scope: &str, digest: &str) -> Result<Option<Vec<u8>>>;
      fn put(&self, scope: &str, digest: &str, bytes: &[u8]) -> Result<()>;
      fn delete(&self, scope: &str, digest: &str) -> Result<()>;
      fn list(&self, scope: &str, after: Option<&str>, limit: usize) -> Result<Vec<String>>;
  }
  ```
- `S3ObjectStore<T: ObjectTransport>`（feature `s3`）：只做 `blobs/<scope>/<digest>.json` 的路径映射与
  `If-None-Match` 语义；签名/端点/凭证由 `ObjectTransport` 注入（与 `HttpTransport` 同思路）。
- `crates/agentscope-durable-mcp/src/pool.rs`（新）
  ```rust
  pub struct McpConnectionPool { max_connections, idle_timeout, … }
  impl McpConnectionPool {
      pub fn register(&self, server_key, Arc<dyn McpTransport>);
      pub fn get(&self, server_key) -> Result<Arc<dyn McpTransport>>;
      pub fn sweep(&self);
  }
  pub struct McpExecutor::with_pool(pool, server_key, catalog);
  ```
- 验收：`FakeTransport` 实现 `ObjectTransport` 跑通 put/get/list/delete 与「同内容不重写」；
  pool 复用同一 `Arc`、超过上限拒绝、idle 超时回收；`McpExecutor` 单测不变。

---

## 4. 需要拍板的 6 个点

| # | 决策 | 建议 | 备选 |
|---|---|---|---|
| A | HTTP 是否引入 axum | **先不引入**；M7 降为按需，若恢复再只在 `server` feature 下用（workspace 已统一 0.7）。`EXECUTOR` 远程化两端都可用现有自研 `Transport`，不需要 axum | 自研 transport 适配层 |
| B | `HttpSessionBackend` 覆盖范围 | **只做执行宿主子集（9–10 个调用）**；管理面不做，除非出现真实远程管理端。Java 侧虽有 60+ 方法全量实现，但文档自述「only for trusted execution hosts」 | 一次性全量 |
| C | RocketMQ 是否只做 `Broker` 端口映射 | 是（与现有 crate 一致，不引 SDK） | 引 SDK |
| D | objects 是否引云 SDK | 否，`ObjectTransport` 注入 | `aws-sdk-s3` |
| E | quickstart 放 `examples/` | 是，注册为 workspace member（CI 会编译） | 只放 tests/ |
| F | `DurableRuntime: Clone` 的语义 | 共享 sink/adapters，价格是 `Arc` 包一层 | 保持不可 Clone |

默认按「建议」执行；若某项要改，请在开工前指出。

## 5. 执行顺序与提交边界

```
M0 → M1 → M2 → M3 → M4 → M5 → M6 → M8 → M9
             ↘ (M3 依赖 M1/M2)      ↘ (M5 依赖 M4)   ↘ (M8 依赖 M0)

M7（HTTP 服务端 + 远程 backend）默认不进主线：它是按需里程碑，
只有在出现「worker 不能直连数据库」的部署约束时才启动；结论见 M7 开头的评估。
```

每个里程碑完成后：

1. `cargo test -p <crate> --lib` + 该 crate 的集成测试；
2. `cargo fmt --all -- --check`；
3. 涉及热路径（M0/M1/M4/M7）的里程碑追加 20 轮 ACP 诊断，
   对比 `flush:done`/`djournal:facts`/`acp:settle` 在 ±4MB 噪声内；
4. 全部里程碑完成后跑一次全量：
   `cargo test --workspace -j 2 --no-fail-fast`（输出落文件，检查 `failures:` 段与 `test result: FAILED`）。

## 6. 明确不做（避免重复入坑）

- 不做 RocketMQ 事务消息/broker 端 exactly-once；「有效一次」由幂等键 + 消费端去重 + 持久 inbox 保证。
- 不把 token 级模型 delta 写入 durable 事件；`deltas` 只是 SSE 的过滤粒度。
- 不做对象存储跨 scope 引用计数；沿用「一个 scope 一个目录」的布局。
- 不改 `FileStore` 的磁盘布局与 `formatVersion` 逻辑（B3-phase2 已收敛，见 `DURABLE_INFORMATION_MODEL_TRACKER.md`）。
- 不重写 `TxnBackend` 的状态机；新增能力全部走「端口 + 包装 + 默认方法」。
- 不做 `forReAct` 工厂：`ReActAgent` 没有 durable journal 宿主，接进来等于第二个 adapter
  （要另写 journal/checkpoint 桥），与「补装配层」不是一个量级；需要时单独立项。
- 不做 `SessionBackend` 60+ 方法的全量 HTTP 化：没有真实远程消费者，且每个新原语都要维护一份映射；
  需要远程 backend 时只做执行宿主子集（见 M7）。
- 不把 ACP 的 HTTP/WS 接入混进 M7：ACP 是客户端协议，其 Streamable HTTP 在 Rust SDK 5.0 尚未提供，
  属于独立的 P3 传输项（与 `durable-http` 的内部 backend 远程化不是同一件事）。
