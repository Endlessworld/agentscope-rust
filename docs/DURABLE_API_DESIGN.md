# Durable（M2/M3）契约设计 — Rust 版

> 对应 Java `agentscope-durable/durable-runtime` + `durable-adapters/durable-adapter-harness-agentscope`。
> 目标：保留语义（原子提交、幂等 receipt、lease/fence、工具三态、UNKNOWN、事件游标），
> **不**翻译 Java 的线程/Reactor 结构与 2,628 行的 `TransactionalSessionBackend` 实现。

## 1. crate 与依赖方向（D5 已拍板）

```
crates/agentscope-durable/                  # 不依赖 harness
  src/{lib,runtime,session,thread,turn,types,session_backend,executor,harness_adapter,
        context,protocol,recovery,events,projection}.rs
  src/backend/{memory,file}.rs
  src/executor/{in_process}.rs

crates/agentscope-durable-adapter-agentscope/  # 依赖 durable + harness + core
  src/{adapter,backend_thread_journal,execution_host,tool_executor,
        messages,checkpoints,tool_events,workspace}.rs
```

`agentscope-harness` 不新增对 durable 的依赖；adapter 自己适配。

## 2. 五个原语（后端只暴露原语，不做业务）

```rust
#[async_trait]
pub trait SessionBackend: Send + Sync {
    fn capabilities(&self) -> BackendCapabilities;          // shared, idempotent_receipts, event_replay
    fn wakeups(&self) -> Option<Arc<dyn Wakeups>>;           // 可选提示，不是正确性来源

    // 身份
    async fn create_session(&self, spec: SessionSpec) -> Result<SessionView>;
    async fn session(&self, key: &SessionKey) -> Result<Option<SessionView>>;
    async fn create_thread(&self, thread: ThreadSpec) -> Result<ThreadView>;

    // 回合
    async fn submit(&self, thread: &ThreadKey, request_id: &str, input: TurnInput) -> Result<TurnReceipt>;
    async fn admission(&self, session: &SessionKey, request_id: &str) -> Result<Option<TurnReceipt>>;
    async fn turn(&self, thread: &ThreadKey, turn_id: &str) -> Result<Option<TurnView>>;
    async fn control(&self, thread: &ThreadKey, command: ControlCommand) -> Result<CommandReceipt>;
    async fn signal(&self, thread: &ThreadKey, turn: &str, signal: Signal) -> Result<Receipt>;

    // 所有权与提交
    async fn claim(&self, thread: &ThreadKey, owner: &str, ttl: Duration) -> Result<Option<Lease>>;
    async fn renew(&self, lease: &Lease, ttl: Duration) -> Result<()>;
    async fn release(&self, lease: &Lease, delay: Duration) -> Result<()>;
    async fn frame(&self, lease: &Lease) -> Result<Frame>;         // turn 状态 / 等待条件 / 工具三态
    async fn commit(&self, lease: &Lease, commit: Commit) -> Result<CommitReceipt>;  // 原子：事实+状态+checkpoint+工具记录

    // 事件（公共，带游标）
    async fn events(&self, session: &SessionKey, after: Cursor, limit: usize) -> Result<EventPage>;
}

#[async_trait]
pub trait Executor: Send + Sync {
    fn capabilities(&self) -> ExecutorCapabilities;  // lookup_by_execution_id / idempotent_start / cancellation
    async fn start(&self, req: &ExecutionRequest) -> Result<ExecutionResult>;
    async fn query(&self, req: &ExecutionRequest) -> Result<ExecutionResult>;
    async fn cancel(&self, req: &ExecutionRequest) -> Result<ExecutionResult>;
}

#[async_trait]
pub trait HarnessAdapter: Send + Sync {
    fn capabilities(&self) -> AdapterCapabilities;   // state_version, recovery_boundary, event_delivery
    async fn execute(&self, ctx: HarnessContext<'_>) -> Result<()>;
    async fn settle(&self, ctx: HarnessContext<'_>, status: TurnStatus, error: Option<String>) -> Result<()> { ... }
}
```

**与 Java 的差异（有意，需确认）**：
1. Java 的 `SessionBackend` 是**阻塞** SPI（59 个方法）+ 调度器 hop；Rust 全 `async_trait`，由调用方选择 `spawn_blocking`。
2. Java 的 `DurableOperations`/`SessionEventClient` 是包内实现；Rust 先做成 `DurableRuntime` 的方法，等 API 稳定再拆。
3. `TransactionalSessionBackend` 不与存储无关地重写：**状态机写成 `SessionStore`（KV + 事务）之上的一层**，File/InMemory/未来 SQL 共用一个实现。

## 3. Turn 状态机

```
Queued ──claim──> Running ──commit(terminal)──> Succeeded | Failed | Cancelled
                    │  │
                    │  └──suspend──> Waiting(WaitCondition{key,deadline}) ──signal──> Queued
                    └──pause──> Paused ──resume(observed_revision)──> Queued
```

持久数据（不可省）：`turn { status, revision, wait: Option<WaitCondition>, outcome: Option<RunOutcome>, runs: Vec<RunView> }`。

判据（同交接文档 §3）：`kill -9` 之后，只看 store 必须能回答「这个 turn 在等什么」。

## 4. 工具三态与 UNKNOWN

```rust
pub enum ToolState { Completed, Accepted, Applied, Unknown { reason: String }, Failed { retryable: bool } }
```

- `Completed`：外部操作结束、结果已拿到，尚未写进 native state；
- `Accepted`：结果已可靠落盘（receipt），尚未被某次 Run 消费；
- `Applied`：已合并进 native 输入/state（带 `run_id + step` 引用，防重复应用）；
- `Unknown`：无法查证（`query` 不支持 / 传输失败）→ 不重试、不猜。

恢复判定树照抄交接文档 §8.5（`ExecutionRecovery::resolve`），包括「只有 `idempotent_start` 才能重启」。

## 5. 事件与游标

- 公共事件与 native journal 事实**同一次 commit**；native 事实是内部事实，公共事件是投影后的稳定契约。
- 游标：`Cursor(u64)`，单调、可重放；`events(after)` 返回 `Committed` 项带游标，`Live` 项不带（最佳努力）。
- 过期游标 → `ResyncRequired`（客户端重新拉快照）。

## 6. File backend 存储布局（M2 唯一持久后端）

```
<root>/sessions/<ns>/<session>/session.json          SessionSpec + primary Thread
<root>/sessions/<ns>/<session>/threads/<id>.json     ThreadSpec
<root>/sessions/<ns>/<session>/turns/<turn>.json     Turn 状态机 + runs
<root>/sessions/<ns>/<session>/commit                             单文件提交链（seq/epoch/lease）
<root>/sessions/<ns>/<session>/objects/<digest>.bin  checkpoint 大块对象
```

原子性：同目录临时文件 + `rename`（Windows 上 `ReplaceFile` 语义用 `std::fs::rename` 覆盖 + 失败重试）；单写者用 `<root>/locks/<thread>.lock` 文件锁 + `epoch` 递增。

## 7. 需要拍板的三个点（M2 开工前）

| # | 问题 | 选项 |
|---|---|---|
| Q1 | `DurableRuntime::tick` 的并发模型 | A. 单 `tick(thread)` + 调用方循环（最简，契合 EdgeDriver）／B. 内置 `tokio::spawn` 扫描循环（贴合 Java `start()`）／C. 两者都有 |
| Q2 | native checkpoint 与 durable facts 的“同一事务”落地方式 | A. adapter 把 core `ThreadJournal::commit` 转到 `SessionBackend::commit`（真正同事务，需改 harness 存储）／B. 先两个提交、靠 receipt 幂等（弱）| 
| Q3 | 工具执行身份何时生成 | A. 模型发出 tool_use 时就生成并落盘（崩溃后可 query，**推荐**）／B. dispatch 前生成（窗口更小但漏掉模型侧崩溃）|

默认建议：Q1=A（后续加 B）、Q2=A（否则 D7 的多进程测试过不了）、Q3=A。
