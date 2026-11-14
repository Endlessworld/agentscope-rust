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
| **M3 adapter + harness 接线** | `BackendThreadJournal`（实现 core `ThreadJournal`，与 durable 共事务）、`AgentScopeExecutionHost`、工具三态、结果按 `tool_call_id` 归位 | **三进程** suspend/resume/duplicate（run=2、模型调用=2、`tool/result`=1；模型次数对齐 Java `ProcessRecoveryTest` 的 `model-calls`）；halt 后取回结果且副作用只 1 次 | 同 D7 |
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


### 2.2 M3-b 执行宿主落地（2026-11-15，未提交）

core `ReActAgent` 主循环接上 `AgentExecutionHost`；无宿主时行为不变（core lib 533 passed）。

| 位置 | 行为 |
|---|---|
| 入口 | `execution_host(ctx)` + `execution_position(ctx)`；`resume_result()` 命中则跳过推理/工具循环直接收尾 |
| 迭代预算 | 起点取 `position.reasoning_iteration`（缺省 0），`set_cur_iter` 同步 |
| 边界 | `reasoning_phase` 开头 Reasoning、模型调用前 Model、`acting_phase` 开头 Acting、续体开头 Finalization |
| 收尾 | 写回共享 state 包进 `FinishContinuation`；`HostPaused` 原样上抛，不转工具错误 |
| 状态锁 | hook 作用在循环私有的 `AgentState` 副本（`&mut`）上，不跨 await 持 `MutexGuard` |

适配层：新增 `crates/agentscope-durable-adapter-agentscope/src/execution_host.rs`——`AgentScopeExecutionHost::open`（读 committed `step/start` 得位置、读 `durable/finalization_started|completed` 得待收尾 reply）、`before_boundary`（控制检查 + input 注入）、`finish`（reply 与 checkpoint 同一次 commit）、`pin()`；5 条集成测试全绿。

遗留缺口：①`input:steer|inject` 只注入 state，未 durable 消费（需与 checkpoint 同事务的 `consumed_signals`，journal 桥未暴露）；②`acting_iteration` 无消费者（core 未迁 pending-tool resume 分支）；③`finish` 需 native recorder，缺则报错；④durable 模型调用 replay（Java `durable/finalization_model`）未迁。


### 2.3 M3-c 工具三态落地（2026-11-15，未提交）

工具三态（意图 / 已派发 / 结果 + applied）接进 durable 事务：一次 commit 同时落 journal 与工具事实。

| 位置 | 行为 |
|---|---|
| 组合入口 | `Commit` 新增 `dispatches: Vec<String>`、`results: Vec<ToolResultRecord>`；`SessionBackend::commit` 在 `apply_tools`（意图）之后、`derive_status`（终态门禁）之前按「意图 → 派发 → 结果 → applied」顺序应用 |
| 事务原语 | `tools::apply_dispatch` / `tools::apply_result` 为「已加载 state/turn」形态；public `begin_dispatch` / `accept_result` 改为「开一笔事务 → 调原语」，对外行为不变 |
| 执行身份 | `ToolCall::execution_id(turn_id)` = `{turnId}:{callId}`，`apply_tools` 改用同一函数（原 `exec_<digest>`），一个 call 只有一条记录 |
| 桥接推导 | `tool/requested` → 意图（`call` 字段，server tool 跳过）；`tool/dispatch` → 派发（`toolCallId`）；`tool/result` → 结果（`message` 里 `ToolResultBlock.id` + `state`）。无 callId 如实报错；`with_tool_facts(false)` 可关掉推导（退化成纯 journal 写入） |
| 状态映射 | Success→Succeeded、Error→Failed、Denied→Cancelled、Interrupted→Unknown、Suspended→Pending、Running→Running；**永不**映射 NOT_FOUND（`Commit::validate` 就拒） |
| log-before-do | `AgentScopeExecutionHost::before_boundary(Acting)` 从最后一个 assistant 消息取未应答 tool_use，先登记意图再派发（commit id = `dispatch:{turn}:{revision}`，重放不新增事实） |

顺带修的真实缺陷：`store::txn` 曾把闭包错误吞成 `Ok`，使任何「写了一半才失败」的事务仍然提交（journal 位置没进、条目却落盘）；现在错误交回 `RecordStore::transaction`，内存/文件两种 store 都真回滚（测试 `a_rejected_tool_fact_rolls_the_journal_batch_back` 守护）。

缺口：①`action/start` / `action/end` 故意不推导（与 `tool/dispatch`/`tool/result` 重复，且 `action/end` 与 returnDirect 占位结果可能不一致）；②core 的 acting continuation 未消费「已派发未完成」事实，恢复时仍会重新执行 pending call（需 core 侧 pending-tool resume 分支）；③转发工具（executor ≠ local）与 delegation 未接；④UNKNOWN 对账（reconcile + evidence）未做。



### 2.4 M3-d `AgentState` ↔ checkpoint 映射（2026-11-15，未提交）

新增 `src/checkpoint.rs`：checkpoint 值就是 `AgentState` 自身 JSON（`encode`/`decode`），去重判据是 backend 已有的 digest + `resource_position` 比较。

- **写**：`AgentScopeExecutionHost::before_boundary` 在 `Reasoning`/`Acting` 边界调 `recorder.checkpoint_now(state, "durable_boundary")`；`BackendThreadJournal::commit` 从批次里取最后一个 `state/checkpoint`/`migration/baseline`/`state/restored` 的 `stateJson` 作 `Commit.checkpoint`（与 journal 投影认的三种载体一致）。
- **必须携带**：backend 拒绝「已有 checkpoint 后 checkpoint 为 null」的提交，所以无 state 的批次（含 `dispatch_calls`）沿用已提交的 checkpoint（`frame().checkpoint`）；同一值写入不产生新位置。
- **读**：`open()` 用 `thread_state.checkpoint_metadata.position` → `backend.checkpoint()` → `decode`，得到 `state()`（无则 `None`）与 `AgentExecutionPosition::new(cur_iter, cur_iter-1)`（与 `step/start` 推导同一规则）；journal 事件（`step/start`、finalization）仍可覆盖为更精确的位置。
- **暴露**：`pin()` 写入 `EXECUTION_POSITION_CONTEXT_KEY` 的值即上述 checkpoint 重建结果，不是入参常量。
- 测试 `tests/checkpoint_test.rs`：`an_unchanged_state_does_not_create_a_new_checkpoint`（同上状态/无 state 批次 → 位置仍 `[1]`，journal 前进 2）、`a_changed_state_creates_a_new_checkpoint`（`[1,2]` 且位置 2 就是新状态）、`a_reopened_host_rebuilds_the_committed_state`（全字段相等 + `cur_iter`/`context`/四类上下文）、`a_reopened_host_keeps_the_committed_iteration_budget`（`position().reasoning_iteration=3`，`max_iters=3` → MaxIterations 且 0 次模型调用）。
- 缺口：①重建出的 state 只用于位置与暴露，loop 仍从 `core.state` 起跑（回填到 live state 属 M4 `AgentSession`）；②checkpoint 是整体快照（大对象外置未做）；③`state/checkpoint` 事件本身仍全量入 journal（journal 与 checkpoint 双份）。

### 2.5 M3-e 三进程验收（2026-11-15，未提交）

`crates/agentscope-durable-adapter-agentscope/tests/three_process_test.rs`：三个独立 `TxnBackend` 只共享 store（不传 frame/checkpoint/lease）。`suspend_resume_and_duplicate_share_only_the_store` → **run=2 / `tool/result`=1 / 副作用=1**；halt 后结论（`Succeeded` + reply `recorded`）由全新 backend 从 store 读出；重复进程 `claim` 返回 `None`，模型与 `tool/result` 计数不变。`the_store_keeps_the_state_after_the_writer_process_exits` 用 `std::process::Command` 以 `DURABLE_WRITER_ROOT` 跑本测试二进制的 writer 角色，子进程写完即退出，父进程**在其退出后**才打开同一 file store（file store 按事务加锁，先退出再读，避免把锁竞争混进结论），等待用 `try_wait` 有界轮询（120s 上限）而非固定 sleep。

- **模型调用 = 2，与 Java 一致**：Java `ProcessRecoveryTest` 断言 `model-calls` = 2 且 `effects` = 1，Rust 同值（`a_resumed_process_repeats_the_tool_decision_but_not_its_effect`）。本表原先写「模型调用=1」是判据笔误，已更正。真实缺口：core 无 acting 阶段续跑入口（`acting_iteration` 无消费者），Java 的 `resumeAgent()`（`ReActAgent.java:3172-3177`）尚未迁移——它影响「上一轮遗留 pending tool calls 的续跑」，不影响本验收。
- 确定性让步：①进程 1 停在 Acting 边界（意图+派发已提交、工具未运行）而非在工具体内被杀（同步工具无法从内部中断，durable 记录与「死在工具里」相同）；②进程 1 显式 `close()` 交还租约而非任其过期（单进程内无法持有不续租的租约）；③`settle()` 由用例代做（`HarnessAdapter::settle` 尚无实现），只记录真实发生过的 `applied` 与终态；④「效果已发生、结果未落盘」的 UNKNOWN 对账未覆盖，故工具只在恢复进程执行一次。
- 反向验证：去掉 Acting 边界 halt → 用例 FAILED；不 `settle` → 重复进程 `claim` 成功、用例 FAILED。

### 2.6 M4-a 语义映射与可达边界（2026-11-15，未提交，**无代码改动**）

M4-a 原定「先删 `AgentSession`，ACP 改持 `Arc<dyn ThreadJournal>`」。逐处核对后结论是：**删除动作不可单独执行**——`AgentSession`（`crates/agentscope-harness/src/agent/session/session.rs`，2259 行，含 1915 起的单测块）是本仓库唯一的会话驱动器，ACP 是它唯一的消费者；把 ACP 降级为「只持 journal」会同时失去准入、派发、receipt、resume、respond 五条语义，属于行为回退而非迁移。因此本步只产出映射表与阻塞清单，代码按 M4-b 一起改。

#### 2.6.1 职责映射（`AgentSession` → 新宿主）

| `AgentSession` 职责（行号） | 现实现 | 新归属 |
|---|---|---|
| 读取视图 `transcript`/`inspect`/`pending`/`head`（341-374） | journal 读函数 | 不变：`journal::{transcript, JournalProjection::read, pending_interactions}`，或 durable `SessionBackend::{journal, checkpoints}` |
| 准入 `submit`/`submit_msg`/`submit_with_key(_msg)`/`wakeup`（375-467） | `SessionInbox::accept` + `Command` + `seed_receipt_if_new` | `DurableRuntime::submit`（`request_id` 幂等 + terminal 校验已具备）；wakeup = 空 `input` 的 submit（需 ACP 侧显式提交空输入，durable 无 `wakeup` 概念） |
| 准入前收尾 `prepare_open_turn_before_submit`（487-546） | 可续跑→`resume`，否则 `abandon_turn`（523） | durable：非终态 turn 的 `resume_turn` / `cancel`；**「可续跑 vs 放弃」的判据要重写**（现有判据 = pending 为空且无 uncertain tool） |
| 派发 `start`/`dispatch_once`/`launch`（875-936、1218-1800） | 50ms 轮询 + 直接驱动 `ReActAgent` 流 | `DurableRuntime::{tick, scan}` + `Executor`/`AgentScopeExecutionHost`；`launch` 约 410 行不再需要 |
| receipt `seed_receipt_if_new`/`receipt`/`ReceiptCache`/`Task`/`task`/`tasks`/`await_task`（937-1139） | 反复读 journal 折叠 receipt | `SessionBackend::receipt` → `TurnReceipt`；ACP 依赖的 `Task{status, settled(), run_id, error}` 形态需由 `TurnView`+receipt 投影（形状不同） |
| 活跃判定 `has_active_execution`（954） | 读 journal | `TurnView.status` / `backend.runs` |
| 事件渲染 `set_event_sink`（365） | 每次执行捕获 sink | core 流式事件 + durable `events(cursor)`；**语义不同**（见 2.6.3 第 6 条） |
| 中断 `interrupt`/`interrupt_run`/`stop_active_run`（468-486、828-874） | `RunControl` + journal | 立即中断仍走 `RunControl`；持久意图 `DurableRuntime::cancel`（写 `Cancelling→Cancelled`） |
| 续跑 `resume(turn_id)`（547-595） | `resume` 命令入 inbox | `DurableRuntime::resume` / `resume_turn`（`TickOutcome`） |
| 交互答复 `respond`/`respond_all`（659-767） | `answer` 命令入 inbox + 派发 | **无等价物**：durable `signal` 的 `Steer`/`Inject` 仍返回 `unavailable`，且没有「答复 pending interaction」原语（§2.1 缺口 4） |
| `steer`/`inject`（768-827） | 命令入 inbox，run 前一次性应用 | 同上，**无等价物** |
| 放弃 `open_turn`/`abandon_unfinished`（596-658） | `TurnViews` + `journal::abandon_turn` | **无等价物**：durable 只有 `cancel`→`Cancelled`，没有「记 interrupted 且以后仍可续跑」的放弃语义 |
| `/compact` | 不在 `AgentSession`：ACP `session.rs:2075` → `HarnessAgent::compact_now()`（`harness_agent.rs:324` 直接读 `state_handle()`） | 不变，M4 无需迁移 |
| wakeup 队列与重试 | ACP 自持（`coding_agent.rs:1695 spawn_wakeup_loop` → `run_wakeup` → prompt `TurnInput::Wakeup`） | 不变，只需「无输入 turn」的准入（见上） |

**内部 4 处 `abandon_turn` 的归属**：

| 行 | 场景 | 新归属 |
|---|---|---|
| 523 | 提交前遇到孤儿 turn（suspended / uncertain tool） | 准入职责：durable `submit` 前的非终态处理，或带证据的 `reconcile`（比 `cancel` 更准，见 `docs/cloud/durable-handoff-2026-11-14.md` 第 9/11 条） |
| 634 | `abandon_unfinished`：宿主显式放弃 | **需新增 host-facing abandon**；durable `cancel` 只写 `Cancelled`，丢掉「interrupted 尚可续跑」，不是等价替换 |
| 1262 | `dispatch_once`：有 open turn 时先把 submit 排到下一 tick | 派发职责：durable `claim` 成功后对非终态 turn 的处置（`resume` 或 `cancel`），不应在准入层重复一遍 |
| 1774 | `launch` 中 `finish` 失败后的兜底关闭 | **可删**：durable 的终态门禁在 backend（`derive_status` + 原子 commit），commit 失败即未落盘，只需保证释放 claim/lease，不需要事后 abandon |

#### 2.6.2 ACP 6 处的处理方式

`session.rs:3110/3137/3149/3222/3245/3265` 六处形状一致，都是「本轮结束但 turn 未终态 → 记 interrupted 关闭，避免下一个 prompt 被拒」，且只需 `turn_id` + journal。它们**本次未改**：改掉 `session.log()` 只会让 ACP 多出一个与 `runtime` 并存的 journal 字段，不减少任何依赖（`submit`/`task`/`resume`/`respond` 仍要 `AgentSession`），属于纯 churn。迁到 durable 后统一收敛为一次 `cancel` 调用——**但必须先决定「宿主放弃」写 `interrupted` 还是 `Cancelled`**，否则 ACP 的「interrupted 自动续跑」（`MAX_INTERRUPTED_RESUMES`，`session.rs:2794`）会失效。

#### 2.6.3 为什么停在文档层（阻塞证据）

1. **ACP 与 durable 之间零依赖**：`crates/agentscope-extensions-acp/Cargo.toml` 只有 `agentscope-core`/`agentscope-extensions-model`/`agentscope-harness`；`agentscope_durable` 在 `crates/` 下只被 durable 两个 crate 及其测试引用。
2. **durable 侧没有会话驱动器**：`agentscope-durable-adapter-agentscope` 只有 `BackendThreadJournal` / `AgentScopeExecutionHost` / `checkpoint` / `tool_facts`，没有 `Session`/`Driver` 门面；`DurableRuntime` 是「调用方驱动的一次 `tick`」（Q1=A）。目前唯一跑通全流程的 `three_process_test.rs` 是**用例自己**写了 `prepare_run`/`drive`（拼 `RECORDER`+`EXECUTION_HOST` extensions 再拉 `run_stream_with_extensions`）/`settle`（并注明 `HarnessAdapter::settle` 尚未实现）——这套循环正是 M4-b 要抽成运行时的东西。
3. **ACP 需要 11 个能力，durable 现覆盖 5 个**：submit ✅、tick ✅、resume ✅、receipt ✅（形状不同）、cancel ≈（语义差）、signal(wakeup) ✗、respond ✗、event sink ✗（需 cursor→`AgentEvent` 映射）、`execution_error` ✗、Task 投影 ✗、abandon ✗。
4. **重写面**：ACP 的 turn 循环（`session.rs:2780-3300`，约 470 行）与 `launch`（约 410 行）都要重写，且必须一次改完——半删会让 workspace 不编译，违反「不做兼容层/不留半成品」。

#### 2.6.4 M4-b 需要先做的六件事

1. `agentscope-durable` 增加 host-facing `DurableSession` 门面：`submit`/`wakeup`（空 input）/`task`（`TurnReceipt`→ACP 的 `Task` 形状）/`has_active_execution`/`resume`/`cancel_as_abandon`/`respond`/`execution_error`。
2. `SessionBackend` 补「答复 pending interaction」原语，并让 `Steer`/`Inject` 真正被消费（当前 `unavailable`）。
3. 拍板放弃语义：`interrupted`（可续跑）还是 `Cancelled`（终态）——决定 ACP 6 处与内部 523/634/1262 的归宿。
4. 事件流：durable `events(cursor)` → `AgentEvent` 的映射，以及「一次 tick 内如何边跑边推」的策略（回调 vs tick 后批量读 cursor）。
5. ACP `runtime` 字段改 `Arc<DurableSession>`；`respond`、`run_wakeup` 接新路径（`/compact` 已就绪）。
6. 最后才删 `AgentSession`/`SessionAnswer`/`HarnessAgent::session()`/`session::{answer, session}` 导出，并改写 `crates/agentscope-harness/tests/harness_session_test.rs`（15 处 `AgentSession::open`）与 `harness_subagent_wiring_test.rs:845` 的注释。

#### 2.6.5 门禁（第 4 步结论）

计划里「关掉 durable 后端 → ACP 用例失败」在当前结构下**不成立**：ACP 不引用 durable，移除/禁用 durable 后端对 ACP 无任何影响，ACP 仍走纯 journal 路径（`AgentSession` + `AtomicThreadJournal`）。这条门禁只能在 M4-b 完成后才有意义，届时写成「把 `DurableRuntime::tick` 换成 no-op / 去掉 durable backend → ACP prompt 不再产生事件且不终止 → 对应用例 FAILED」。当前真实可观测的门禁仍在 durable/harness 层：`agentscope-durable-adapter-agentscope/tests/three_process_test.rs`（去掉 Acting halt 或去掉 settle 即 FAILED，§2.5）与 `harness_session_test.rs` 的 journal 恢复用例。

### 2.7 M4-b durable 侧能力补齐（2026-11-15，未提交；ACP 与 `AgentSession` 均未改）

- **signal 落地**：`control` 不再对 `Steer`/`Inject` 返回 `unavailable`，两者与 `Signal` 共用 `accept_signal`（原先 `Signal` 分支在事务内二次调用 `with_session`，同一 scope 重复加锁会死锁，已合并为单事务）。对齐 Java `TransactionalSessionBackend.control`：两者都只作用于**活跃 turn**（`epoch != 0` 且为队首非终态），否则 `CONFLICT`；写 `input:steer` / `input:inject` 信号。**明确近似**：Java 在 durable 层也不区分生效时机，差别只在 key 与（Java 侧）`inputKind` 审计字段，折叠两种都在下一个 reasoning 边界——Rust 无对应审计事件，故只有 key 与回执 `operation` 可区分。
- **已交付标记**：`AgentScopeExecutionHost::apply_inputs` 折叠后，把「携带消息的 state」与 `consumed_signals` 放进**同一次 commit**（对齐 Java `log.consuming(ids)` + `checkpointedRecordNow`），此后 `frame.signals` 不再返回该信号；原来的「按消息 ID 去重」已删除，`signal:<id>` 只作消息的稳定 id，不再是投递依据。
- **suspend vs cancel**：不新增 `TurnStatus`。`DurableRuntime::suspend` = `Pause` 意图 + 完成握手：活跃写者看到 `Pausing` 交还租约后落 `Paused`；写者已死则由 `claim→release` 就地完成（bump thread epoch，fence 语义不变）。`Paused` 非终态，position/checkpoint/工具记录全保留，`resume` 原地续跑；`cancel` 仍写 `Cancelled`，`resume` 被 `CONFLICT` 拒绝。
- **settle 收编**：`harness_adapter::settle_turn` 先按「`Succeeded` 且未 applied」补 `applied_results`（`appliedAt` = 本 turn 最新 run），再写终态；同终态重复调用为 no-op，终态后换终态为 `CONFLICT`。`HarnessAdapter::settle` 增加 `output` 参数并以它为默认实现，`three_process_test.rs` 的手写 settle 已删除。
- **门面**：`DurableSession`（`src/session.rs`）只做组合，不含策略：`submit`/`admission`/`tick`/`cancel`/`suspend`/`resume`/`steer`/`inject`/`signal`/`settle`（需调用方自己的 `Lease`）/`turn`/`turns`/`active`/`state`/`events`。
- **仍未覆盖**（M4-c 前需补）：`wakeup`（空输入 turn）、`respond` 与 pending interaction 记录、`TurnReceipt`→ACP `Task` 投影、事件流实时推送（现只有 `events(cursor)` 读）、`history()` 仍返回空页、`execution_error` 投影；`suspend` 之后 native invocation 的续跑仍受 core「无 acting 阶段续跑」限制。
- 测试：新增 `agentscope-durable/tests/session_test.rs`（9 条）与 `execution_host_test.rs::a_delivered_signal_is_folded_once_and_marked_consumed`；durable 13+6+9、适配器 4+6+4+4+5 全绿。

### 2.8 M4-c（一）：门面补齐完成，ACP 换驱动受阻于两个新前置（2026-11-15，未提交）

**已落地（只在 durable 侧，均有测试）**：`DurableSession::{wakeup, respond, waiting, task, history, set_event_sink}` + `HarnessContext::publish`，`settle` 增 `error` 参数。语义：wakeup = `input` 为 `null` 的 admission（同 request id 幂等）；respond 以 **wait key 就是 interaction id** 为前提，先校验 `TurnView.waiting` 再把答案落成 `Signal`（不是新记录，所以重复答复幂等、终态回答 `CONFLICT`）；`task()` 投影出 `TurnTask{status, settled, run_id, error, output}`（`error` 同时覆盖 `record_failure` 与 settle 携带的错误，即 `execution_error` 的归宿）；`history()` 从空页改为「每个 turn 的 committed output 按队列序」；事件推送是 `HarnessAdapter` 在 `execute` 里 `ctx.publish(value)` → runtime 的 sink。
测试：`session_test.rs` 9→15（`a_wakeup_is_an_admission_without_input_and_it_is_idempotent`、`a_waiting_turn_is_answered_by_its_interaction_key`、`the_task_projection_reads_the_status_the_run_and_the_ending`、`history_projects_the_committed_outputs_in_queue_order`、`a_run_publishes_through_the_session_event_sink`、`a_run_without_a_host_sink_still_advances`）。durable 17+6+15、适配器 4+6+4+4+5 全绿。

**ACP 未换驱动（也未删任何东西）：§2.6.4 的六项补齐后仍有两个硬前置**
1. **没有可执行的 `HarnessAdapter`**：`advance` 只按 `definition.harness()` 查适配器，未注册就 `record_failure`；仓库里 `HarnessAdapter` 只有测试桩的实现。把 `HarnessAgent` 跑成一次 turn（inputs → `stream_events_with_extensions`、checkpoint/工具事实、`settle`）的适配器必须新写。
2. **租约不可转交**：`InvocationRecorder::open_with(..., run_id, ...)` 用 `run_id` 当 owner 调 `journal.acquire` → `backend.claim(thread, run_id, ttl)`；tick 已以 runtime owner 持租且未过期，`claim` 直接返回 `None`，recorder 建不起来（`No turn is ready to execute`）。方案 A（推荐）：`BackendThreadJournal` 增「借用 tick 租约」模式（`acquire` 返回既有 lease、`release` 不真释放），runtime 仍是唯一 owner，不 bump epoch、不碰 durable 不变量；方案 B：core recorder 接受外部 `Writer`（改 core）。

**6 处 `abandon_turn` 的分流结论（第 2 步落地时按此写注释）**：`3110` 客户端 cancel / 会话已关闭 → `cancel`（用户要停）；`3137` admission 看门狗（turn 从未被取走或状态读不出，没有位置可续）→ `cancel`；`3149` 内部循环未拿到 receipt（可能已有副作用，位置值得留）→ `suspend`；`3222` 自动续跑失败（durable 下失败多为 `CONFLICT`，说明已在别处落终态）→ 只记日志，不再 abandon；`3245` 反复被打断（环境问题）→ `suspend`；`3265` 其它非终态（含 external tool 未回）→ `suspend`。判据一句话：**用户要停或没有可续位置 → `cancel`；跑过东西但没有结论 → `suspend`**。选 `suspend` 的几处要求 ACP 在下一个 prompt 前先把非终态 turn 处理掉（`AgentSession::prepare_open_turn_before_submit` 的职责，需搬进 ACP）。

**删除清单（第 3 步，未执行）**：`AgentSession`、`SessionAnswer`、`HarnessAgent::session()`、`session::{answer, session}` 导出、`harness_session_test.rs`（15 处 `AgentSession::open`）。`core::journal::{inbox, execution, turns}` 保留：删掉 `AgentSession` 后它们的生产消费者归零，但 `TurnViews` 仍在 `JournalProjection`/`recovery` 里，`inbox`/`execution` 仍被 core 自己的 `session_*` 集成测试与 `harness_session_test.rs` 使用（应随这些测试一起删）。

**剩余项**：可执行适配器 + 租约借用 → ACP `runtime` 字段换 `DurableSession`、turn 循环（`session.rs:2780-3300`）重写、`respond`/`run_wakeup` 接新路径 → 最后才是删除。`history()` 仍无消息转录（归 adapter）。

### 2.9 M4-d 可执行适配器 + 租约借用（2026-11-15，未提交；ACP 与 `AgentSession` 未改）

**已落地**：`AgentScopeHarnessAdapter`（适配 crate）通过 `DurableRuntime::with_adapter(harness, Arc<dyn HarnessAdapter>)` 注册，构造只收一个 `Fn() -> HarnessAgent` 工厂（每 tick 调一次：invocation 是建立在已提交事实上的新 agent，不是长活对象）。一次 tick 的顺序：open 执行宿主 → 用 checkpoint 回填 `ReActAgent` 的 state（不回填则恢复进程看到空历史）→ journal 借用 tick 租约 → `stream_events_with_extensions` 跑一轮 → 事件逐条 `ctx.publish` → flush、close recorder → `settle`。`HostPaused` 是让出（适配器返回 `Ok(())`，turn 仍 `Running`、不记 error，runtime 记 `TickOutcome::Settled`）；事件流里的其它错误以 `turn.failed` 落终态并带 error 文本。

**一次 tick 一次模型调用**：`TickBudget` 包住执行宿主数 `Model` 边界，第二次直接 `HostPaused`。暂停点选 `Model` 而非下一个 `Reasoning`：那一刻 core 已 `set_cur_iter(N+1)` 且 `recorder.step(N+1)` 已入队，适配器退出前 `recorder.flush()` 落盘，恢复位置就是 N+1，迭代预算跨 tick 前进；选 `Reasoning` 会每 tick 重跑同一迭代（实测把 2 迭代的 turn 拖成 3 tick / 3 次调用）。边界：**一个 tick = 一次模型决策 + 该决策触发的工具执行**（core 无 acting 续跑分支，停在决策与工具之间会丢决策），不是「一个工具 apply 一次 tick」；`max_iters` 仍是全局预算，超出后由 core 的 summarizing 收尾。

**租约借用**：`BackendThreadJournal::{attach_lease, detach_lease}`。借用期 `acquire` 直接返回该租约（owner/epoch 取 tick 的，不再 `claim`），`release` 空转（租约归 runtime），`detach_lease` 后恢复 claim 语义。不变量未动：epoch 不 bump、receipt 先于 fence、CAS/终态不可逆/工具三态全在 backend。

**测试**（`tests/harness_adapter_test.rs`，7 条）：`a_tick_makes_exactly_one_model_call`、`a_borrowed_lease_lets_the_tick_write_the_journal`、`the_borrow_ends_with_the_tick`、`a_paused_tick_yields_and_the_next_one_finishes_the_turn`、`a_turn_runs_across_ticks_on_its_committed_conversation`、`the_iteration_budget_carries_across_ticks`、`a_second_turn_on_the_thread_gets_its_own_input`。反向验证 7 处全部 FAILED 后还原：去 state 回填 / 去 `attach_lease` / 借用期真 release / 去 tick 预算 / 暂停点改 `Reasoning` / 去 `recorder.flush()` / `turn_started()` 退回 `state().is_some()`。

**剩余（M4-e）**：①ACP 仍走 `AgentSession`，需把 prompt→input、事件→`ctx.publish`、`respond`/`wakeup` 接到 `DurableSession`；②`history()` 消息转录；③输入投递用 `turn_started()`（= 本 turn 是否已有 journal 事实），若 ACP 在 submit 与首个 tick 之间写入本 turn 的控制事实需复核；④agent 工厂由 ACP 提供（模型/工具/中间件装配在 ACP 侧）；⑤`settle` 目前把非 `HostPaused` 错误一律记为终态 `Failed`，重试预算未用（ACP 若要重试由宿主重新 submit）。

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

| 2026-11-15 | **M3-b 执行宿主** | ✅ core 主循环接 `AgentExecutionHost`（位置/边界/resume/finish）+ adapter `AgentScopeExecutionHost`/`pin()`；5 条集成测试全绿；core lib 533 passed |
| 2026-11-15 | **M3-e 三进程验收** | ✅ `three_process_test.rs`：run=2 / `tool/result`=1 / 副作用=1；真实子进程验证状态外置；⚠️ 模型调用=2（D7 要求 1，缺 acting 续跑分支，见 §2.5） |
| 2026-11-15 | **M4-a 语义映射** | ⚠️ 已产出 §2.6 映射表，**未删代码**：`AgentSession` 是唯一会话驱动器、ACP 是其唯一消费者，而 ACP 与 durable 零依赖、durable 无会话门面（11 项能力只覆盖 5 项）。删 `AgentSession` 与「ACP 改 `DurableRuntime`」必须同批完成（M4-b 六项前置见 §2.6.4）；「关掉 durable → ACP 失败」在当前结构下不成立（§2.6.5） |
| 2026-11-15 | **M4-c（一）门面补齐** | ⚠️ `DurableSession::{wakeup,respond,waiting,task,history,set_event_sink}` + `HarnessContext::publish` 落地（session_test 9→15），**ACP 与 `AgentSession` 未改**：换驱动多了两个硬前置（无可执行 `HarnessAdapter`、租约不可转交），见 §2.8 |
| 2026-11-15 | **M4-d 可执行适配器 + 租约借用** | ✅ `AgentScopeHarnessAdapter`（工厂 `Fn() -> HarnessAgent`）注册给 `DurableRuntime`；`BackendThreadJournal::{attach_lease, detach_lease}`；tick = 一次模型决策；`harness_adapter_test.rs` 7 条全绿；ACP 与 `AgentSession` 未改 |

