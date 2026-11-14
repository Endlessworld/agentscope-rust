# Durable 迁移计划（Java `a4306d63d` → Rust）

> 基准：Java `a4306d63d feat(durable): add recoverable runtime and streamline public APIs (#4154)`
> 分析依据：`docs/cloud/durable-handoff-2026-11-14.md` + 本计划附带的逐文件对照（见 §4）
> 状态：**已拍板，执行中**。本文件是本次迁移的单一事实来源，里程碑完成后就地更新。

## 1. 已拍板的七个决策（2026-11-14）

| # | 决策 | 选择 | 直接后果 |
|---|---|---|---|
| D1 | 交付边界 | **D 全量对齐** | 含语义层 + durable 最小集 + 全量重命名 + 删 AgentSession（ACP 迁 durable）+ worker/扩展 + 删 controlplane |
| D2 | 重命名 | **全改，不留别名** | `core::session` → `core::journal`；`Session*` → `Thread*`/`Journal*`/`Invocation*`；`HarnessAgent` 的 session API 换成 thread API；**破坏性公共 API**，需 0.2.0 |
| D3 | `AgentSession` | **删除并把 ACP 迁到 durable** | harness 少 3,860 行；ACP 的 turn 驱动改走 `DurableRuntime` |
| D4 | 持久格式 | **强切 v2** | `JournalEvent.schemaVersion = 2`、`ThreadJournal::Head.format_version = 2`，**v1 会话文件不可读**（不做迁移器，照 Java「零兼容」） |
| D5 | crate 形态 | **两 crate** | `agentscope-durable`（**不依赖 harness**）+ `agentscope-durable-adapter-agentscope`（依赖 durable + harness） |
| D6 | controlplane | **本次删除** | 删 40 文件 / 714 KB，移除 `agentscope-rs` 的 `controlplane` feature |
| D7 | 多进程验收 | **必须** | Rust 版三进程 `suspend/resume/duplicate` + `halt` 后取回结果的测试作为验收门 |

### 1.1 依赖方向（不可违背）

```
agentscope-durable  ──→  (core only)      # 不得依赖 harness
agentscope-durable-adapter-agentscope ──→ durable + harness + core
agentscope-harness  ──→  core             # 不得依赖 durable
```

## 2. 里程碑

| 里程碑 | 内容 | 验收门 | 反向验证 |
|---|---|---|---|
| **M0 删除 controlplane** | 删 crate + workspace member + `agentscope-rs` feature/文档 | `cargo check --workspace --all-targets`、`cargo test -p agentscope-rs`、全量测试 0 failed | — |
| **M1 core 语义 + 重命名** | `session`→`journal`（目录 + 类型）；新增 `agent::{ThreadKey, RunOutcome, AgentExecutionHost, AgentExecutionPosition}`；`GenerateReason::HostPaused`；`ToolCallParam::propagate_errors` + call context；事件/头 v2 强切 | core 单测全绿、workspace 绿、fmt 绿 | 事件 v1 必被拒；HostPaused→`RunOutcome::Suspended` |
| **M2 durable 骨架 + 最小集** | `agentscope-durable`：`DurableRuntime/Session/Thread/Turn`、`SessionBackend`（InMemory + File）、`Executor`（InProcess）、`HarnessAdapter`、`HarnessContext`、公共事件流/游标、`ExecutionRecovery` 判定树 | durable 自测：单进程 submit→tick→completed；`UNKNOWN` 分支 | 去掉 `query` 分支 → 恢复测试 FAILED |
| **M3 adapter + harness 接线** | `BackendThreadJournal`（实现 core `ThreadJournal`，与 durable 共事务）、`AgentScopeExecutionHost`、工具三态、结果按 `tool_call_id` 归位 | **三进程** suspend/resume/duplicate（run=2、模型调用=1、`tool/result`=1）；halt 后取回结果且副作用只 1 次 | 同 D7 |
| **M4 ACP 迁 durable + 删 AgentSession** | ACP 会话驱动改 `DurableRuntime`；删 `AgentSession`/`SessionAnswer`、`core::journal::{inbox, execution, turns}`；`/compact`、wakeup、respond 全部接 durable | ACP 全测试绿 + stdio smoke；durable 恢复路径覆盖 ACP 断线 | 关掉 durable 后端 → ACP 用例失败 |
| **M5 全量对齐 + 清理** | 删 controlplane 残留引用与文档；`docs`/tracker 更新；版本 0.2.0；`cargo fmt`；发布说明 | 全量 `cargo test --workspace` 0 failed；`fmt --check` 0 | — |
| **M6 后置** | `durable-worker`（Brain/Hands）、`durable-{jdbc,http,mcp,rocketmq,objects,sandbox}`、`Postgres` 后端、protocol JSON-Schema 全量 | 各自独立验收 | — |

**执行顺序的理由**：删除（D3/D6）必须排在 durable 落地之后，否则 ACP 与 `agentscope-rs` 断链；重命名（D2）排在一切改动之前，避免同一文件被改两次。

### 2.1 M2 落地情况（2026-11-14，未提交）

新增 `crates/agentscope-durable`（13 单测 + 6 条契约测试，`cargo test -p agentscope-durable` 全绿）：

| 模块 | 内容 |
|---|---|
| `types/` | Java `DurableTypes` 全量类型（identity/turn/run/execution/commit/resource 五分）；camelCase 序列化、SCREAMING_SNAKE 枚举 |
| `store` | `RecordStore`+`Txn` 原语（scope 事务 + 存储权威时钟 + 就绪索引）；`memory`（默认）、`file`（单机持久，临时文件+rename，锁文件自旋） |
| `backend/` | `TxnBackend`：create/threads/submit/claim/renew/release/frame/record_failure/commit/receipt/journal/prepare_run/runs/steps/execution/begin_dispatch/accept_result/ready/checkpoints/checkpoint/events/snapshot |
| `runtime` | `DurableRuntime::{tick, scan, cancel, pause, resume, signal}`，单次推进、不 spawn 常驻循环（Q1=A） |
| `executor` | `Executor` trait + 能力矩阵（lookup_by_execution_id/idempotent_start/cancellation） |
| `harness_adapter` | `HarnessAdapter`/`HarnessContext`（adapter 通过 backend+lease 自行提交，保证 checkpoint 与工具意图同事务，Q2=A） |

已严格实现的不可退让不变量：范围事务原子性、receipt 先于围栏的重放、revision CAS、thread 级 epoch 围栏、终态不可逆、效果结算前置（`Succeeded` 需全部 applied）、adherence 到 `appliedAt` 不可变、事件按条目递增序号 + eventId 幂等 + 游标绑定 session、checkpoint 不丢弃/相同内容不产生新位置、就绪五分支语义、工具三态（attempted/result/applied）与 `NOT_FOUND` 不可记录。

**M2 有意留下的缺口**（后续里程碑补齐，均已在此登记）：

1. `history()` 返回空页——消息投影由 adapter（M3）写入，backend 暂未存 message part。
2. `resources`/`delegate`/`restore`/`prune_events` 未入 trait（原语已预留 `resource_position`、`journal_branch`）。
3. 模型事实（`ModelCall/ModelRequest/TokenUsage`）只有类型，未接线（M3+）。
4. `Steer`/`Inject` 控制返回 `unavailable`（ACP 不需要；M3 与 adapter 输入队列一起做）。
5. 大对象外置（>16 KiB → object store）未做：`checkpoint` 全量内联存放。
6. `settled` 前缀未物化（`current_turn` 每次队列扫描），`receipt`/`tool` 内嵌在 `TurnData` 而非独立 part。
7. digest 用「键排序 canonical JSON + SHA-256」，与 Java RFC 8785（含 `{"value":…}` 外壳）不等价——两边数据不互通，故不影响正确性；如需跨语言互通再对齐。


## 3. 禁止事项（沿用 Java `agentscope-durable/AGENTS.md`）

- 不做兼容层：不留别名、不保留 deprecated 转发、不写旧格式读取器、不做版本协商/降级回退。
- 不迁 `agentscope-service`（Rust 无对应物）、不迁 `examples`、不迁 `.github` 改动。
- 不整体翻译 `TransactionalSessionBackend`（2,628 行）与 `DurableRuntime`（904 行）的 Java 线程/Reactor 结构，按 §2 的契约用 tokio 重写。
- `Backend` 只暴露原语，不出现 `submitPrompt` 一类业务方法。
- 恢复边界只承诺到 **工具边界**（`RecoveryBoundary::ToolBoundary`），不承诺任意异步栈恢复。

## 4. 命名对照（core）

| Java（HEAD） | Rust 旧 | Rust 新 |
|---|---|---|
| `journal::ThreadJournal` | `session::SessionLog` | `journal::ThreadJournal` |
| `journal::ThreadJournalStore` | `SessionLogStore` | `ThreadJournalStore` |
| `journal::AtomicThreadJournal` | `JournalSessionLog` | `AtomicThreadJournal` |
| `journal::AtomicJournalStorage` | `AtomicSessionStorage` | `AtomicJournalStorage` |
| `journal::InMemoryThreadJournalStore` | `InMemorySessionLogStore` | `InMemoryThreadJournalStore` |
| `journal::JournalEvent` | `SessionEvent` | `JournalEvent` |
| `journal::JournalEventTypes` | `SessionEventTypes` | `JournalEventTypes` |
| `journal::JournalException` | `session_error`/`Error::Session` | 保留（Rust 错误前缀） |
| `journal::JournalProjection` | `SessionProjection` | `JournalProjection` |
| `journal::JournalExportSink` / `JournalExporter` | `SessionExportSink` / `SessionLogExporter` | `JournalExportSink` / `JournalExporter` |
| `journal::InvocationRecorder` | `SessionRecorder` | `InvocationRecorder` |
| `journal::RecordedModels` | `SessionModels` | `RecordedModels` |
| `journal::ModelCallPolicy` | `SessionModelPolicy` | `ModelCallPolicy` |
| `journal::ThreadRecovery` | `recovery` 模块 | 保留 `recovery`（`reconcile` 等） |
| `journal::ThreadStateAccess` | `state_access` | 保留 `state_access::Snapshot` |
| `journal::ThreadInteractions` | `views::pending_interactions` | 保留（M3 扩充） |
| `agent::ThreadKey` | `session::SessionKey` | `agent::ThreadKey` |
| `agent::RunOutcome` | — | 新增 |
| `agent::AgentExecutionHost` | — | 新增 |
| `agent::AgentExecutionPosition` | — | 新增 |
| 已删除（Java） | `SessionInbox` / `SessionExecution` / `SessionTurns` / `SessionHistoryMode` | **M4 删除**（M1 暂留，标注 `TODO(M4)`） |

## 5. 附带修复（本文档承诺的最小集）

- `AsyncToolRegistry::complete(id, _result)` 丢弃结果 → M3 保存 `result/applied/deadline`。
- offload `tokio::spawn` 无 `AbortHandle` → M3 绑定句柄，会话关闭时 cancel。
- `spawn_wakeup_loop` 对未注册会话 `continue` 丢弃条目 → M4 保留条目。
- `stale_ttl=10min` 把重启判超时 → M3 由 durable 的持久执行身份取代。

## 6. 验证口径

- 每档：`cargo check --workspace --all-targets -j 2` → 0；`cargo test --workspace -j 2 --no-fail-fast` → 0 failed；`cargo fmt --all -- --check` → 0。
- 反向验证：每个新增语义至少一条「去掉修复 → 用例 FAILED」的证据。
- 结果写回本文档对应里程碑行与 `docs/MIGRATION_GAP_TRACKER.md` §8。

## 7. 进度

| 日期 | 里程碑 | 结果 |
|---|---|---|
| 2026-11-14 | **M0** | ✅ 删除 `crates/agentscope-extensions-controlplane`（40 文件）+ workspace member + `agentscope-rs` feature/doc/README；`cargo check -p agentscope-rs --all-features` 通过 |
| 2026-11-14 | **M1a 重命名** | ✅ `core/src/session` → `core/src/journal`（`journal.rs` → `atomic.rs`，`key.rs` → `agent/thread_key.rs`）；`SessionEvent→JournalEvent`、`SessionLog→ThreadJournal`、`JournalSessionLog→AtomicThreadJournal`、`SessionRecorder→InvocationRecorder`、`SessionProjection→JournalProjection`、`SessionModels→RecordedModels`、`SessionModelPolicy→ModelCallPolicy`、`SessionKey→ThreadKey`、`AtomicSessionStorage→AtomicJournalStorage`、`SessionTurns→TurnViews`、`SessionSnapshot→JournalSnapshot`、`SessionRetention→JournalRetention`、harness `*SessionStorage/*SessionLogStore→*JournalStorage/*ThreadJournalStore`；`Error::Session→Error::Journal`、`journal_error` 前缀改为 `journal: `。共 111 文件；core 533 / harness lib 659 通过 |
| 2026-11-14 | **M1b 语义类型** | ✅ `agent::{RunOutcome, RunOutcomeStatus, AgentExecutionHost, ExecutionBoundary, FinishContinuation, AgentExecutionPosition}` + `EXECUTION_HOST_CONTEXT_KEY`/`EXECUTION_POSITION_CONTEXT_KEY`；`GenerateReason::HostPaused`；`Error::HostPaused`（暂停是控制流，不得转成工具错误结果）；4+2 条单测（含「暂停时不执行 finalization continuation」） |
| 2026-11-14 | **M1c 格式 v2** | ✅ `JournalEvent::SCHEMA_VERSION = 2`（拒绝 v1）、`SessionHeader::FORMAT_VERSION = 2`（拒绝 v1），按 D4 硬切 |
| pending | **M1d** | ⬜ `Head.format_version`（Java `ThreadJournal.Head` 带 `formatVersion`）留到 M3 由 durable backend 接管时加，避免现在改存储头格式两次 |
| pending | **M2–M6** | ⬜ 见 §2 |

