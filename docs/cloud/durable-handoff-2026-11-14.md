# 交接文档：进程可消失 / Durable 能力评估

> 交接日期：2026-11-14
> 仓库：`D:\IdeaProjects\agentscope-rust`（HEAD `5f34db4 feat(acp): 慢工具调用转后台，不再被 core 的 300s 默认取消`）
> 对照仓库：`D:\IdeaProjects\agentscope-java`（main，HEAD `a4306d63d feat(durable): add recoverable runtime and streamline public APIs (#4154)`）
> 本文性质：**纯分析，未改动任何生产代码**。所有结论均附代码位置，可直接复核。

---

## 0. TL;DR

1. **ACP 已经解决「会话可以不存在」，没有解决「工作可以不存在」。**
   - `session/resume` / `session/load` 能把 agent 重新接到旧会话（身份、历史、catalog、MCP 都回来），
   - 但**没有**「谁有权继续这个 turn」「工具结果如何归位」「外部副作用是否可确认」这三件事。
2. **RSS/commit 真正归零只有一条路：进程退出。** Rust 无 GC、堆不能回收；Windows 无 per-process hibernate；
   `K32EmptyWorkingSet` 只是页级 trim（本仓库已实现，`mem.rs`）。
3. **跨天等待不能建模为「工具内部阻塞」**，当前 1800s 硬超时会杀掉；必须改成
   「作业化（ticket + 查询）」或「suspend + 外部 respond」。
4. **不建议整体迁移 Java durable-runtime**；建议只迁「语义层」，执行框架层按部署形态决定，
   基础设施层（rocketmq/http/objects）现阶段不迁。
5. 眼前最该做的三件小事（P0）：**工具结果落盘 + 后台 task 可取消 + wakeup 不丢未注册会话**。

---

## 1. 用户原始诉求（两轮提问）

### 1.1 第一轮

> 分析一下 acp agent 内存归零的可能性。例如工具调用外部系统需要跨天响应，在此期间应用内存归零直到被激活。
> 即工具调用超过指定时间比如 10 分钟，则进程进入休眠释放全部内存（只保留工具调用的响应监听），
> 工具调用成功之后重新激活进程恢复内存，在此期间内存全部释放。

### 1.2 第二轮

> Java 版的最后一个提交实现了 durable 相关功能，分析一下。
> durable 核心模块要迁移吗？rust 现在有 ACP 协议，是不是已经基本实现了它解决「进程可以不存在」？

### 1.3 本文要回答的四个问题

| 编号 | 问题 | 结论位置 |
|---|---|---|
| Q1 | 进程「休眠 + 内存归零 + 结果到达后激活」可行吗 | §6 / §7 |
| Q2 | Rust 现有机制能支持到哪一步 | §4 / §5 |
| Q3 | Java durable 提交做了什么、有没有可借鉴 | §8 |
| Q4 | ACP 是否等价于 durable；durable 要不要迁移 | §9 / §10 / §11 |

---

## 2. 术语（本仓库语境，务必统一）

| 术语 | 含义 | 代码位置 |
|---|---|---|
| ACP | Agent Client Protocol，客户端 ↔ agent 的 stdio JSON-RPC；本仓库只有 **stdio** 传输 | `crates/agentscope-extensions-acp` |
| Turn | 一次逻辑任务（一个 prompt / 一个 wakeup） | `core/session/views.rs::Turn` |
| Run | 一次 native 调用（≈ 一个 ReActAgent loop 实例） | `core/session/recorder.rs` 的 run 概念 |
| journal | 追加式事实日志（`turn/*`、`tool/*`、`interaction/*`、`state/checkpoint`） | `core/session/journal.rs` |
| checkpoint | 序列化后的 `AgentState`（`stateJson`，MB 级） | `core/session/recorder.rs` |
| offload | 工具超过阈值后转后台继续跑，模型先拿到占位结果 | `harness/.../middleware/async_tool.rs` |
| wakeup | 后台完成后请求「给空闲会话一个无输入的 turn」 | `bus/message_bus.rs::WAKEUPS_KEY` |
| inbox | 后台结果/提示的持久队列（文件系统后端，**跨进程可用**） | `bus/workspace_message_bus.rs` |
| uncertain tool call | 已派发但结果未提交的工具调用（崩溃后不能猜） | `core/session/views.rs` |
| suspend | 工具主动声明「等外部执行」，turn 落 `turn/suspended` | `core/message/block.rs::ToolResultBlock::suspended` |
| respond | 外部应答挂起交互并继续 turn | `ext.rs` + `harness/.../session.rs::respond_all` |
| lease | 写入者租约，30s；过期可被他人接管 | `core/session/recorder.rs`（`LEASE`） |

---

## 3. 结论速览（可直接引用）

```
ACP   = transport & session lifecycle      → 「用户能回到这个会话」
Durable = execution lifecycle              → 「系统能接着干这件事」
```

- ACP 的 `resume`：把 agent 重新接上会话（无 replay）。
- ACP 的 `load`：重放整段历史给 client（有 replay）。
- Durable 的 `claim`：拿到某个 turn 的推进权（带 fence）。
- Rust 现状：有前两者，没有后者；跨进程所有权仅存在于 `controlplane` 扩展（依赖平台服务）。

---

## 4. 当前 Rust 实现事实清单

> 复核方式见 §14。以下每条都是可以直接 grep 出来的事实，不是推测。

### 4.1 ACP 传输与会话生命周期

| 事实 | 位置 |
|---|---|
| 只声明并实现标准 ACP 方法：new/load/resume/prompt/cancel/list/delete/close/archive/status/title/additionalDirectories | `acp/src/coding_agent.rs::capabilities()` |
| `load_session(true)` 已声明；`session/load` 会 `replay_notifications()` 重放整段对话 | `coding_agent.rs:1474` 附近 |
| `session/resume` 已声明并实现；会话不在内存时 `create_session` 后从 journal 重建 | `coding_agent.rs:1493` |
| 只有 stdio 传输；进程由客户端 spawn。无 socket/daemon 模式 | `acp_harness_agent.rs::main` → `agent.run_stdio()` |
| 连接断开 → `shutdown_sessions()`：`close()` + `cancel_and_wait(5s)`，**在飞 turn 被取消，不是暂停** | `coding_agent.rs:726` |
| `session/wakeup` 循环每 500ms tick，只遍历**内存中已注册**的会话 | `coding_agent.rs::spawn_wakeup_loop` |
| 未注册/已淘汰会话的 wakeup 条目被 `continue` 丢弃（`deferred` 已被 `mem::take` 清空） | 同上，`let Some(mut target) = target else { continue; }` |
| 会话注册表容量默认 `max_sessions = 1`；满了淘汰最久未活动会话（优先空闲） | `coding_agent.rs:631`、`:399` |

### 4.2 执行与等待

| 事实 | 位置 |
|---|---|
| ACP 侧工具 offload 阈值 `TOOL_OFFLOAD_TIMEOUT = 240s` | `coding_agent.rs:134` |
| 单次工具执行硬上限 `TOOL_EXECUTION_TIMEOUT = 1800s`（必须大于 offload 阈值） | `coding_agent.rs:147` |
| `with_tool_execution_policy()` 把上面两个值装进 builder | `coding_agent.rs:150` |
| offload 用 `tokio::spawn` 在**进程内**继续跑；返回值未绑定，**没有 AbortHandle**，会话卸载后仍持有 Arc | `middleware/async_tool.rs::on_acting` |
| offload 时写入占位 `ToolResultBlock` 并 `results_handle.record_placeholder()`，保证 `tool_use`/`tool_result` 配对 | 同上 |
| 后台完成后 `inbox_push` + `enqueue_wakeup` | 同上 |
| `InboxMiddleware` 默认 `stale_ttl = 10min`；超期 RUNNING 记录被标 `TIMEOUT` 并作为 hint 注入模型 | `middleware/inbox.rs:81`、`:152` |
| `WorkspaceAsyncToolRegistry::complete(&self, id, _result)` **丢弃结果文本**，只改状态 | `bus/async_tool.rs` |
| `wait_async_results` 是轮询屏障（默认 60s，最长 600s） | `harness/src/tool/wait_async.rs` |

### 4.3 持久化与恢复

| 事实 | 位置 |
|---|---|
| journal 事实由 caller 驱动 flush；后台只有租约续期 tick | `core/session/recorder.rs:4-16` |
| 写入者租约 `LEASE = 30s` | `core/session/recorder.rs:44` |
| `Turn::ended()` 只认 completed/cancelled/failed；`interrupted` 与 `suspended` **未结束**，可继续 | `core/session/views.rs:49` |
| `suspended` turn 写 `turn/suspended`，不写 `turn/output` | `core/session/recorder.rs`（`terminal != "suspended"` 分支） |
| `resume(turn_id)` 要求：turn 是 latest、未结束、无 pending interaction、无 uncertain tool calls | `harness/.../session/session.rs:546` |
| 提交新 prompt 时若存在 open turn：可恢复则先 resume；suspended/uncertain 则 `abandon_turn`（记 interrupted） | `session.rs:484-537` |
| `abandon_turn` 会把 pending interaction 写成 `CANCELLED_PENDING_MESSAGE`，并把 unknown 工具结果记 `Interrupted` | `core/session/recovery.rs::close_turn_locked` |

### 4.4 挂起与外部应答（已具备，但 ACP 使用面很窄）

| 事实 | 位置 |
|---|---|
| `ToolResultBlock::suspended(reason, id, name)` 存在 | `core/message/block.rs:665` |
| `GenerateReason::ToolSuspended` → turn `suspended` | `message/msg.rs`、`recorder.rs` |
| pending interaction 由 journal 的 `interaction/requested|resolved` 持久化，**纯日志重放可得** | `core/session/views.rs::pending_interactions` |
| 外部应答入口 `_agentscope/session/respond`（ACP 私有扩展方法） | `acp/src/ext.rs::RESPOND_METHOD` |
| `respond_all` 通过 inbox command `"respond"` 提交，**进程重启后同样可用** | `session.rs:665` |
| 限制：`respond` 需要 session 已注册到内存（`get_session`），未 load 时返回 `session_not_found` | `coding_agent.rs::handle_ext_respond` |

### 4.5 所有权

| 事实 | 位置 |
|---|---|
| `SessionTurnGate` 是**进程内** `HashMap<String, Arc<Gate>>` + 异步 Mutex，不跨进程 | `harness/.../gateway/session_turn_gate.rs:141` |
| 跨进程 fenced lease 只存在于 `controlplane` 扩展（`ControlPlaneTurnLease`），依赖 controlplane 服务 | `crates/agentscope-extensions-controlplane/src/store/session_turn_gate.rs` |
| journal writer lease 提供「写入者互斥」，不提供「谁继续这个 turn」的调度语义 | `core/session/journal.rs` |

### 4.6 内存治理（已有，均为页级/会话级）

| 机制 | 默认 | 位置 |
|---|---|---|
| 每轮结束 `HeapCompact`（Windows）/ `mi_collect`（unix） | 开启 | `mem.rs::collect` |
| 空闲 N 秒 `K32EmptyWorkingSet` 丢工作集 | 3s（`AGENTSCOPE_ACP_MEM_TRIM_IDLE`） | `mem.rs::schedule_idle_trim` / `:183` |
| 实测空载基线 | ~26MB RSS / 29MB commit（Windows） | `mem.rs:11-15` 注释 |
| 实测本机两个活进程 | WS 9.7MB / 35.1MB；Private 105.0MB / 37.3MB | 本次会话 `Get-Process` |
| 模型 catalog 可释放（~15MB） | — | `coding_agent.rs::release_catalog_if_unused` |
| 会话级卸载（drop `AcpAgentSession`） | 仅在新建会话触发 | `coding_agent.rs:631` |

> 结论：`trim` 只降 RSS，不降 commit；进程里还有 500ms wakeup tick 与 30s lease heartbeat，不可能「静默休眠」。

### 4.7 超时语义

| 事实 | 位置 |
|---|---|
| 工具执行超时由 core 包装（默认 300s），`is_long_running()` 工具可跳过 | `core/tool/executor.rs:194` |
| shell 工具是 `tokio::process::Command` + piped stdout；Windows 未绑 job object | `filesystem/sandbox.rs` / `tool/shell_execute_tool.rs` |
| 进程退出后子进程在 Windows 上会存活但管道断裂 → 输出不可回收 | 同上（`kill_process_tree` 仅主动杀时使用） |

---

## 5. 「进程突然消失」四场景行为矩阵

| 场景 | 现状结果 | 是否可能重复副作用 | 恢复动作 |
|---|---|---|---|
| 空闲，无 turn | 无损 | — | 客户端 `resume`/`load` |
| 模型流式中 | turn 停在 `interrupted` | **会重跑模型（重复计费）** | `resume` 重跑 / `abandon_turn` |
| 工具在进程内执行 | 子进程孤儿、结果丢失；tool_use 已提交、tool_result 未提交 → uncertain | 可能（只能记 interrupted，不查证） | 人工 reconcile |
| 工具已 offload | 后台 task 随进程消亡；registry RUNNING 记录 10min 后判 TIMEOUT | 结果丢失，模型被告知超时 | 无自动路径 |
| 结果已进 inbox 但会话未注册 | wakeup 条目被丢弃 | 静默丢失 | 无 |

---

## 6. 八个硬约束（设计时必须逐条满足）

1. **跨天等待不能发生在工具内部**：1800s 硬上限会杀；断线即失败。必须作业化或 suspend。
2. **进程在工具执行中途退出 = 结果不确定**：facts 是 caller-driven flush，`uncertain_tool_calls` 非空时禁止 resume。退出必须发生在安全点。
3. **挂起期间用户发新消息会作废 pendings**：`prepare_open_turn_before_submit` 遇到 suspended turn 会 `abandon_turn`。跨天场景必须用「独立 job + inbox 事件」，不要用「turn 挂起」。
4. **重启后 10 分钟判定**：`stale_ttl = 10min` 会把后台记录判为 TIMEOUT 并注入模型，即「重启 = 判超时」。
5. **连接断开 = 取消**：`shutdown_sessions` 是 cancel 不是 pause。要做「休眠」必须先改成「可挂起 + 落盘」。
6. **租约 30s**：休眠 > 30s 后他人可接管（好事），但原进程回来必须先重新 acquire，否则 flush 报 `fenced`。
7. **结果必须自己存**：`AsyncToolRegistry::complete` 丢弃 result；目前唯一持久载体是 inbox。
8. **offload 的后台 task 不可取消**：`tokio::spawn` 无 handle，drop 会话后仍持有 Arc，内存释放不掉。

---

## 7. 三个「归零」层级与可行性

| 层级 | 手段 | 效果 | 可行性 |
|---|---|---|---|
| L1 页级 | `K32EmptyWorkingSet` + HeapCompact（已实现） | RSS ↓ 到几 MB，**commit 不变**，线程照跑 | 今天就有，但不是「释放」 |
| L2 会话级 | 从 sessions map 移除（drop `AcpAgentSession`/`HarnessAgent`） | 会话级内存（历史、toolkit、模型、投影缓存）归零 | 可行，需先修 §6-8 与 wakeup 丢弃 |
| L3 进程级 | 进程 exit + 外部拉起 + `session/resume` | RSS 与 commit 真正归零 | 唯一真解；需 supervisor + 客户端支持 resume |

**Windows 上不可行的做法**（已论证）：
- `NtSuspendProcess`：只冻结线程，不减内存；
- `EmptyWorkingSet`：只是 trim；
- 依赖 pagefile 换出：commit 不降，且定时器会反复唤醒；
- 句柄转交（把 socket 留给另一个进程再退出）：Windows 无 `SCM_RIGHTS` 等价物。

**因此「只保留工具调用的响应监听」必须外置**，三选一：
1. 外部系统把结果写进 workspace（现有 `WorkspaceMessageBus` 文件队列即可，天然跨进程）；
2. 极小的 supervisor/代理进程持有 stdio 与端口，按需拉起 agent；
3. 系统服务/CI 侧持有作业，agent 只存 ticket。

---

## 8. Java 提交 `a4306d63d` 分析

### 8.1 元信息与规模

```
commit a4306d63d8ac737b535657f56a0ad5496503d4d8
Author: Ken Liu   Date: Fri Nov 13 16:47:31 2026 +0800
title:  feat(durable): add recoverable runtime and streamline public APIs (#4154)
stat:   2553 files changed, +122,990 / -294,516
```

构成：

| 部分 | 内容 |
|---|---|
| core 重命名 | `core/session/**` → `core/journal/**`；`SessionLog→ThreadJournal`、`SessionProjection→JournalProjection`、`SessionRecovery→ThreadRecovery`、`SessionStateAccess→ThreadStateAccess`、`SessionModelPolicy→ModelCallPolicy` |
| core 新增 | `AgentExecutionHost`、`AgentExecutionPosition`、`RunOutcome`、`ThreadKey`、`ThreadInteractions`、`InvocationRecorder`、`RecordedModels`、`RecordedModelRequests`、`ModelInvocation` |
| 新模块 | `agentscope-durable/{durable-runtime, durable-worker, durable-adapters, protocol, examples}` |
| 新扩展 | `durable-{jdbc,http,sandbox,rocketmq,objects,mcp}` |
| 删除 | 整个 `agentscope-service`（平台层） |
| schema | `session-event-v1` → `journal-event-v2` |

**注意**：这是一次仓库级重构，不是增量特性；`agentscope-durable/AGENTS.md` 明确要求「不做兼容层、不留旧格式读取器、不保留 deprecated 转发」。

### 8.2 概念模型

```
Session（应用身份 + Thread 目录）
  └ Thread（上下文 + 固定 agent 定义 + Turn 队列）
      └ Turn（持久逻辑任务）
          └ Run（一次 native 调用；恢复时沿用 Turn，新调用记新 Run）
              └ Step（reasoning / result-application 步）
```

关键语义：**恢复保留 Turn 身份，进程重启不产生新用户任务**。

### 8.3 四类边界

```
DurableRuntime  组装 / 生命周期 / claim / tick / recovery（构造不需要 start，API-only 部署可用）
HarnessAdapter  native 边界 ↔ durable 提交（RecoveryBoundary: NONE | CHECKPOINT | TOOL_BOUNDARY）
SessionBackend  事实、原子提交、幂等 receipt、lease/fencing、事件 replay
Executor        start / query / cancel + capabilities（lookupByExecutionId / idempotentStart / cancellation）
```

`AgentScopeAdapter.capabilities()` 明确声明 `RecoveryBoundary.TOOL_BOUNDARY`——**不恢复任意 Java 栈**。

### 8.4 关键机制（逐条，与本项目关切对应）

| # | 机制 | 关键实现 | 解决我们的哪条约束 |
|---|---|---|---|
| 1 | Turn 持久状态机 + `WaitCondition(key, deadline)` | `DurableRuntime.process()`：waiting 不满足直接 `return`，不占线程 | §6-1 / §6-5 |
| 2 | 工具三态 `completed → accepted → applied` | README §3；`frame.tools()` 中 `applied` 单独判断 | §6-2 / §6-7 |
| 3 | `UNKNOWN` 而非重试 | `ExecutionRecovery.resolve()` 判定树 | §6-2 |
| 4 | claim / renew / fence | `backend.claim(key, claimId, leaseDuration)`；心跳 renew；`FENCED` 错误处理 | §6-6 |
| 5 | 宿主可暂停边界 + finalization 恢复 | `AgentExecutionHost{REASONING,MODEL,ACTING,FINALIZATION}` + `Paused` + `resumeResult()` + `finish()` | §5 场景 2（防重复计费） |
| 6 | 迭代位置恢复 | `AgentExecutionPosition(reasoningIteration, actingIteration)`；`ReActAgent` 在 resume 路径使用 | 恢复不重置 maxIters |
| 7 | 模型调用可重放/可观测 | `ModelInvocation` 接口 + `RecordedModels` 包装 + `RecordedModelRequests`（attempt 级） | 恢复后不重跑模型 |
| 8 | native 与 durable 同一事务 | `BackendThreadJournal implements ThreadJournal`（注释：No second scheduler） | 事实与执行状态不分裂 |
| 9 | 操作员显式调和 | `ThreadRecovery.reconcile(log, outcomes, reason)` 写 `recovery/applied` + checkpoint | 替代我们的 `abandon_turn` 单向放弃 |
| 10 | 幂等 receipt / 重复控制被拒 | `Turn.respond` 重复 requestId 抛错且 cursor 不变 | §6-3 |

### 8.5 `ExecutionRecovery.resolve` 判定树（值得照抄的算法）

```
if previous.terminal() -> return previous
if cancellationRequested && !attempted -> CANCELLED

if !attempted:
    observed = executor.startExecution(request)            # 首次派发
else:
    queryable = caps.lookupByExecutionId || observed.handle != null
    if cancellationRequested:
        if queryable: observed = query(...)                # 查证后再取消
        if observed.terminal(): return observed
        observed = caps.cancellation ? cancel(...) : UNKNOWN("Provider cannot confirm cancellation")
    else if queryable:
        observed = query(...)
        if observed == NOT_FOUND && caps.idempotentStart: observed = start(...)   # 仅幂等才重启
    else if caps.idempotentStart:
        observed = start(...)
    else:
        observed = UNKNOWN("Provider cannot locate the original operation")

return observed == NOT_FOUND ? UNKNOWN("outcome remains unconfirmed") : observed
# 任何异常 -> UNKNOWN("External operation outcome is unconfirmed")
```

核心思想：**「查不到」绝不等于「没发生」**。

### 8.6 可行性证据：两个多进程测试

**A. `HarnessExternalToolProcessRecoveryTest`（3 个独立 JVM）**

| phase | 行为 | 断言 |
|---|---|---|
| `suspend` | 模型返回 external tool 调用 → `runtime.tick()` | Turn = `WAITING`；`pendingInteractions[0].requestId == external-call-1`；checkpoint 已落盘；进程退出 |
| `resume` | 新 JVM：`turn.respond(...)` + `tick()` | `SUCCEEDED`；`runs.size()==2`；**模型调用次数 == 1** |
| `duplicate` | 重复 respond | 抛 `DurableException`；backend cursor 不变；模型调用 0；`tool/result` 事件仍为 1 条 |

**B. `ProcessRecoveryTest`（强杀 Brain）**

- `Runtime.getRuntime().halt(17)` 在 `acceptResult` 之后强杀；
- sandbox 里的 detached 进程继续跑；
- 第二个 JVM 通过 `executor.query` 取回原结果并应用；
- 断言沙箱副作用文件**只出现一次**。

这两条就是我们第一轮问题的端到端证明（把「跨天」缩成「跨秒」，机制相同）。

### 8.7 Java 自己声明的边界（不要过度期待）

1. **不解决内存归零**：README 原文「all hosts are stopped → supervisor or external scheduler must start one」。
2. **恢复只到工具边界**：`TOOL_BOUNDARY`；「工具内部发 5 封邮件」无法续跑，必须工具自己幂等/可查。
3. **`UNKNOWN` 是业务契约**：框架不猜结果，业务必须提供 idempotency key + operation lookup。
4. **不做旧格式兼容**：schema/事件/存储格式随时改。

---

## 9. Rust vs Java 差距表

| # | 能力 | Rust 现状 | Java 新现状 | 迁移价值 |
|---|---|---|---|---|
| 1 | 会话日志命名/语义 | `core/session/*`（Session*） | `core/journal/*` + `Thread*` | 低（纯改名） |
| 2 | 调用语义结果 | 只有 `GenerateReason` | `RunOutcome` + `GenerateReason::HOST_PAUSED` | **高** |
| 3 | 宿主协作钩子 | 无 | `AgentExecutionHost` 四边界 + `resumeResult`/`finish` | **高** |
| 4 | 迭代位置恢复 | 无（重启视为新调用） | `AgentExecutionPosition` | **高** |
| 5 | 模型 attempt 观测/重放 | 无 | `RecordedModels` / `RecordedModelRequests` / `ModelInvocation` | 中高 |
| 6 | 工具三态 | 只有 `uncertain_tool_calls` | `completed/accepted/applied` + `UNKNOWN` | **高** |
| 7 | 结果归位 | inbox hint（文本，模型自己理解） | 按 `tool_call_id` 归位为真实 tool_result | **高** |
| 8 | 跨进程 claim/fence | 进程内 `SessionTurnGate`；跨进程仅 controlplane | `SessionBackend.claim/renew/release` | 看部署 |
| 9 | 事件 cursor / replay | journal 可重放，但无对外 cursor 契约 | 独立版本化的 public events + `Last-Event-ID` | 中 |
| 10 | 执行方分离（Brain/Hands） | 无（offload 在进程内） | `Executor` + 独立 Worker | 看部署 |
| 11 | 操作员调和 | `abandon_turn`（只能记 interrupted） | `ThreadRecovery.reconcile`（带原因，写 `recovery/applied`） | 中 |
| 12 | 多主机接管 | 无 | claim + fence + 扫描 | 看部署 |

---

## 10. 「durable 要不要迁」的决策

### 10.1 三问捷径

```
Q1 只有单机、单客户端、桌面 IDE？
   → 不迁 durable。做 §11-P0（语义/缺口修复）即可支撑跨天等待 + 会话卸载。

Q2 需要多主机接管、跨天审批、对外的 HTTP/SSE 观察者？
   → 迁「语义层 + SessionBackend trait」，用 tokio 重写，不照抄实现。

Q3 只想让 RSS 归 0 省内存？
   → durable 帮不上；它只是让「进程可以不活着」变成可选。仍需外部拉起者。
```

### 10.2 三层，只有第一层是「必须」

**第 1 层 语义层（建议做，成本低）**
- `GenerateReason::HostPaused`
- `RunOutcome{COMPLETED,SUSPENDED,INTERRUPTED,FAILED,CANCELLED}`（与 turn 终态解耦）
- `AgentExecutionPosition(reasoning, acting)`
- 工具三态 + `UNKNOWN` + `query` 式查证
- finalization 可恢复（先落 reply 再 finish middleware）
- 结果按 `tool_call_id` 归位

**第 2 层 执行框架层（按部署形态）**
- 最小契约：
```rust
trait SessionBackend { claim/renew/release/fence; commit(原子); frame; events(cursor); receipt }
trait Executor { start/query/cancel; capabilities }
```
- 可直接复用的现有件：journal（事实）、checkpoint（native state）、`WorkspaceMessageBus`（跨进程通知）、`AsyncToolRegistry`（升级为 job store）、journal 30s writer lease（升级为 turn lease）。

**第 3 层 基础设施（现阶段不迁）**
- `durable-rocketmq` / `durable-http` / `durable-objects` / `durable-mcp` / examples / `NotifyingSessionBackend`。

### 10.3 明确不建议

- 整体移植 `DurableRuntime`（970 行）+ `TransactionalSessionBackend`（2762 行）：它们是 Java 线程/阻塞/Reactor 混合模型与平台历史的产物（同一提交还删掉了整个 `agentscope-service`，说明仍处于收敛期）。
- 逐行翻译 Reactor 的 `contextWrite` 到 `RuntimeContext`。
- 为旧格式写兼容层（Java 明确不做，我们也不必）。

---

## 11. 落地路线（每步都可独立验收）

### P0：修掉四个缺口（不改协议、不加 crate）

| 改动 | 文件 | 验收 |
|---|---|---|
| `complete()` 保存 result；job 记录含 `result/applied/deadline` | `harness/src/agent/bus/async_tool.rs` | 重启后能取回结果 |
| 保存并暴露 `AbortHandle`，会话卸载/关闭时取消后台 task | `middleware/async_tool.rs` | drop 会话后内存可回落（`mem.rs` 日志可观测） |
| wakeup 未注册会话的条目**保留**而不是丢弃 | `coding_agent.rs::spawn_wakeup_loop` | 卸载后结果到达仍能唤醒 |
| 结果按 `tool_call_id` 归位为真实 tool_result，而不是纯 hint | `middleware/inbox.rs` / `async_tool.rs` | 模型看到结构化结果 |

**反向验证方法（本项目惯例）**：临时把条件改成 `false && ...` 或还原修复，对应测试必须 FAILED，再还原。

### P1：turn 状态机 + 跨进程 claim

- 新增 `TurnStatus{WAITING,RUNNING,SUSPENDED,SUCCEEDED,FAILED,CANCELLED}` 与 `WaitCondition(key, deadline)`；
- claim 用文件锁或 journal lease（不引 controlplane）；
- 验收：两个进程同时 tick，只有一个推进；进程 kill -9 后另一进程接管同一 turn。

### P2：迭代位置 + finalization 恢复

- 验收：模型已返回、进程在 finish 前崩、重启后**模型调用次数不增加**（对照 Java `resumeResult()`）。

### P3：SessionBackend trait + 事件 cursor

- sqlite / postgres 两种实现即可；
- 验收：`HarnessExternalToolProcessRecoveryTest` 的 Rust 版（三进程 suspend/resume/duplicate，`tool/result` 恰好 1 条）。

---

## 12. 已知缺陷清单（可直接开 issue）

| # | 缺陷 | 位置 | 影响 |
|---|---|---|---|
| D1 | `complete(id, _result)` 丢弃结果 | `bus/async_tool.rs` | 结果无法跨进程取回 |
| D2 | offload 后台 task 无 AbortHandle | `middleware/async_tool.rs` | 会话卸载后内存不释放 |
| D3 | wakeup 对未注册会话 `continue`，条目被丢 | `coding_agent.rs::spawn_wakeup_loop` | 后台结果静默丢失 |
| D4 | `stale_ttl=10min` 把重启后的后台记录判 TIMEOUT | `middleware/inbox.rs:81` | 「重启 = 判超时」 |
| D5 | `shutdown_sessions` 是 cancel 不是 suspend | `coding_agent.rs:726` | 连接抖动即丢在飞工作 |
| D6 | `max_sessions` 默认 1，只在新建会话时淘汰 | `coding_agent.rs:399/631` | 多会话互相挤掉，非按空闲卸载 |
| D7 | `SessionTurnGate` 仅进程内 | `gateway/session_turn_gate.rs:141` | 多进程并发无保护 |
| D8 | 结果只能作为 hint 文本进入 reasoning，不是结构化 tool_result | `middleware/inbox.rs` | 模型可能误判 |
| D9 | suspend/respond 已具备但 ACP 侧几乎没有使用场景接线 | `acp/src/ext.rs` | 跨天等待没有可用入口 |
| D10 | 进程退出后 `_agentscope/session/respond` 需先 load 会话 | `coding_agent.rs::handle_ext_respond` | 外部回调前必须先重建会话 |

---

## 13. 待确认的决策点（需要产品/架构拍板）

1. **外部系统能否作业化**（给 ticket 后轮询/回调），还是必须保长连接？前者 P0 即可，后者需常驻 watcher。
2. **挂起期间是否允许用户继续对话**？允许 → 必须用「独立 job + inbox 事件」（不能 turn 挂起）；不允许 → suspend 模型更省事。
3. **客户端是否支持 transport 断开后重连并 `session/resume`**（Zed 等会重启 agent 进程，但有的客户端会直接丢弃会话）。
4. **是否需要多主机/多副本**？这直接决定第 2 层要不要做。

---

## 14. 复现与验证命令

### 14.1 复核 Rust 事实

```powershell
cd D:\IdeaProjects\agentscope-rust

# offload 阈值与执行上限
rg -n 'TOOL_OFFLOAD_TIMEOUT|TOOL_EXECUTION_TIMEOUT' crates/agentscope-extensions-acp/src/coding_agent.rs

# 后台任务与结果落盘
rg -n 'tokio::spawn|inbox_push|enqueue_wakeup' crates/agentscope-harness/src/agent/middleware/async_tool.rs
rg -n 'fn complete|fn find_stale|stale_ttl' crates/agentscope-harness/src/agent/bus/async_tool.rs crates/agentscope-harness/src/agent/middleware/inbox.rs

# 恢复语义
rg -n 'uncertain_tool_calls|prepare_open_turn_before_submit|Only the latest unfinished task' crates/agentscope-harness/src/agent/session/session.rs

# 内存治理
rg -n 'K32EmptyWorkingSet|schedule_idle_trim|DEFAULT_TRIM_IDLE_SECONDS' crates/agentscope-extensions-acp/src/mem.rs

# 所有权
rg -n 'pub async fn acquire|try_acquire' crates/agentscope-harness/src/agent/gateway/session_turn_gate.rs
```

### 14.2 观测现有进程内存

```powershell
Get-Process -Name acp_harness_agent -ErrorAction SilentlyContinue |
  Select-Object Id, @{n='WS_MB';e={[math]::Round($_.WorkingSet64/1MB,1)}},
                      @{n='Private_MB';e={[math]::Round($_.PrivateMemorySize64/1MB,1)}}
```

### 14.3 复核 Java 提交

```powershell
cd D:\IdeaProjects\agentscope-java

git show --shortstat --format='' a4306d63d
git show --name-status --format='' a4306d63d | Select-String -Pattern '^R' | Select-Object -First 30

# durable 总览
gh 无；直接读文件
Get-Content agentscope-durable/README.md -TotalCount 120

# 关键实现
Get-Content agentscope-durable/durable-runtime/src/main/java/io/agentscope/durable/ExecutionRecovery.java
Get-Content agentscope-core/src/main/java/io/agentscope/core/agent/AgentExecutionHost.java
Get-Content agentscope-durable/durable-adapters/durable-adapter-harness-agentscope/src/test/java/io/agentscope/durable/agentscope/HarnessExternalToolProcessRecoveryTest.java
```

### 14.4 Rust 侧验证（改动后）

```powershell
# 单 crate 定向
cargo test -p agentscope-harness --lib -j 2 --no-fail-fast
cargo test -p agentscope-extensions-acp --lib -j 2 --no-fail-fast

# 全量（输出落文件，避免 PowerShell 管道爆栈）
cargo test --workspace -j 2 --no-fail-fast *> $env:TEMP\ws-test.log
then: Select-String -Path $env:TEMP\ws-test.log -Pattern 'test result: FAILED|^failures:'

# 格式（只跑全量，不要对单文件直接 rustfmt）
cargo fmt --all -- --check
```

> 注意：`target\debug\acp_harness_agent.exe` 被运行中的客户端占用时，含 bin 的测试会报 `os error 5`，用 `--lib` 规避。

---

## 15. 下一位 agent 的启动清单

1. 读本文 §0、§5、§6、§10、§11（其余按需）。
2. 若任务只是「让等待不占内存」：直接做 §11-P0，**不要动 durable 框架**。
3. 若任务涉及「跨进程/多主机/跨天审批」：先回答 §13 的四个决策点，再进 §11-P1。
4. 改动前先跑一次 `cargo test -p agentscope-harness --lib` 建立基线。
5. 任何修复都要做「反向验证」（还原 → 测试必须 FAILED → 再修回）。
6. 不要对单个文件跑 `rustfmt`（会绕开仓库 `rustfmt.toml` 造成大面积无关 diff）。

---

## 16. 参考文件索引

**本话题的后续设计文档（2026-11-14 新增）**

| 文档 | 内容 |
|---|---|
| `docs/cloud/CF_DEPLOYMENT_ARCHITECTURE.md` | Cloudflare 部署架构：DO schema、Turn 状态机、接口契约、4 个时序、恢复预算、平台约束、成本；**§18.1 七个端口（Backend 只占七分之一）、§18.2 双形态兼容（同一 tick + 两个 Driver + 四条硬规则）、§18.3 渐进路径** |
| `docs/cloud/CF_MIGRATION_TASKS.md` | M1–M6 任务拆解、验收标准、反向验证、阶段门、工作量估算、风险登记；**§1.1 端口归属表**（避免实现时只盯 Backend） |

### Rust（本仓库）

| 文件 | 作用 |
|---|---|
| `crates/agentscope-extensions-acp/src/coding_agent.rs` | ACP 方法实现、会话注册表、wakeup 循环、offload 阈值 |
| `crates/agentscope-extensions-acp/src/session.rs` | 单会话：turn 驱动、runtime 句柄、suspend/respond 接线 |
| `crates/agentscope-extensions-acp/src/ext.rs` | `_agentscope/session/respond` 扩展方法 |
| `crates/agentscope-extensions-acp/src/mem.rs` | 内存治理与观测（HeapCompact / trim / usage） |
| `crates/agentscope-harness/src/agent/middleware/async_tool.rs` | 工具 offload + 占位结果 + inbox/wakeup 投递 |
| `crates/agentscope-harness/src/agent/bus/async_tool.rs` | 异步工具记录（落盘，但丢 result） |
| `crates/agentscope-harness/src/agent/bus/workspace_message_bus.rs` | 文件系统消息总线（跨进程） |
| `crates/agentscope-harness/src/agent/middleware/inbox.rs` | inbox 消费 + stale 判定 |
| `crates/agentscope-harness/src/agent/session/session.rs` | `AgentSession`：submit/resume/respond/wakeup/abandon |
| `crates/agentscope-harness/src/agent/gateway/session_turn_gate.rs` | 进程内 turn 互斥 |
| `crates/agentscope-core/src/session/{journal,recorder,views,recovery}.rs` | 事实日志、checkpoint、projection、恢复 |
| `crates/agentscope-core/src/tool/executor.rs` | 工具执行超时包装 |

### Java（对照仓库）

| 文件 | 作用 |
|---|---|
| `agentscope-durable/README.md` | 完整设计说明（推荐先读） |
| `agentscope-durable/AGENTS.md` | 设计决策与「不做兼容」约束 |
| `agentscope-core/.../agent/AgentExecutionHost.java` | 四边界宿主钩子 |
| `agentscope-core/.../agent/AgentExecutionPosition.java` | 迭代位置恢复 |
| `agentscope-core/.../agent/RunOutcome.java` | 调用语义结果 |
| `agentscope-core/.../journal/RecordedModels.java` | 模型调用记录/重放 |
| `agentscope-core/.../journal/ThreadRecovery.java` | 显式调和（`recovery/applied`） |
| `agentscope-durable/.../ExecutionRecovery.java` | UNKNOWN/查询/幂等重启判定树 |
| `agentscope-durable/.../DurableRuntime.java` | claim/tick/process 主循环 |
| `agentscope-durable/.../AgentScopeAdapter.java` | native loop 嵌入 + 恢复入口 |
| `agentscope-durable/.../HarnessExternalToolProcessRecoveryTest.java` | 三 JVM 端到端证据 |
| `agentscope-durable/.../ProcessRecoveryTest.java` | halt(17) 强杀 + detached 进程证据 |

---

## 17. 一句话留给下一位

> `ACP` 让用户能回到这个会话；`Durable` 让系统能接着干这件事。
> 我们目前只到「能读懂上次留下的现场」，还没到「能安全地接着干」。
> 优先级：**先修四个缺口（P0），再谈框架（P1+）**。


# 结论

**是，但不是「迁移 Java 的执行框架层」，而是「先固化执行语义（第 1 层），再自建一个薄执行层（第 2 层）」。**

判断依据不是「部署形态有几种」，而是三个**相互独立**的问题：

| 维度 | 问的问题 | 决定什么 |
|---|---|---|
| **所有权** | 同一时刻是否可能有 >1 个进程想推进同一 turn | 要不要 claim / lease / fence |
| **等待** | 等待的是「无」/「外部事件」/「人」 | 要不要持久 `WaitCondition` + 外部注入入口 |
| **进程生命周期** | 常驻 / 随时被杀 / 多副本 | 要不要外部拉起 + 冷启动恢复 |

六种形态就是这三个维度的不同组合。只要其中**任意一种**组合要求「进程可被杀且工作不丢、结果不重复」，你就必须把执行语义从内存搬到持久层——**这是语义问题，不是部署问题**，而语义一旦有两套实现（本地一套、服务端一套），就会长期漂移。所以答案偏向「是」。

但「迁移框架层」≠「移植 `DurableRuntime`(970) + `TransactionalSessionBackend`(2762) + worker(1300)`」。正解是自建，规模约为 Java 的 1/3。

---

# 一、六种形态 × 三维度

| 部署形态 | 所有权 | 等待 | 进程生命周期 | 需要的东西 |
|---|---|---|---|---|
| **本地单机**（CLI/桌面） | 单写者 | 可能跨天（人工/外系统） | 常驻，可被用户关掉 | 第 1 层 + 文件 store；**claim 可退化为文件锁** |
| **客户端内嵌**（IDE 插件/桌面 app） | 单写者 | 人工审批、外部作业 | **客户端一关就没**，还会重启 | 第 1 层 + 文件/SQLite store；必须支持「重启即恢复」 |
| **移动端内置**（Android/iOS） | 单写者 | 人工审批、推送回调 | **OS 随时杀、后台窗口极短（iOS ~30s）、Doze 冻结** | 第 1 层 + **嵌入式 store（体积/写放大敏感）**；不依赖任何后台长任务 |
| **API-only**（无模型、无 agent） | 单写者（只读写事实） | 人工审批 | 常驻或按需 | 第 1 层 + store；**Runtime 构造不得要求 start**（Java 这条很关键） |
| **多主机/多副本**（服务端） | **多写者** | 跨天 + 人工 | 副本随时替换 | 第 1 层 + **claim/renew/fence + 共享 backend** |
| **Brain/Hands 分离** | 多写者（两类角色） | 外部作业 | 两侧独立重启 | 上一层 + **Executor 契约（start/query/cancel + capabilities）** |

读法：

- 前四种只需要「**单写者 + 持久等待 + 冷启动恢复**」——不需要分布式协议，但**必须有持久状态机**。这恰恰是大多数团队最容易漏的：以为「单机就不用 durable」，结果客户端一崩就丢工作。
- 后两种才需要 claim/fence 与共享后端。**注意它们不是「更高级的版本」，而是「加了多写者约束的版本」**；如果只为后两种设计，前四种会被迫背上 DB、MQ、线程池的复杂度。

---

# 二、一套契约，多种 backend

薄执行层只需要 5 个原语，形态差异全部收进 `Backend` 的实现与能力位：

```rust
trait SessionBackend {
    // 所有权（单写者实现可以退化为文件锁，能力位 shared=false）
    fn claim(&self, thread: &ThreadKey, owner: &str, ttl: Duration) -> Result<Option<Lease>>;
    fn renew(&self, lease: &Lease, ttl: Duration) -> Result<()>;
    fn release(&self, lease: &Lease, delay: Duration) -> Result<()>;

    // 原子提交（native facts + turn 状态 + checkpoint + 工具执行记录 一次提交）
    fn commit(&self, lease: &Lease, commit: Commit) -> Result<Receipt>;

    // 读当前视图（turn 状态、等待条件、工具三态、信号）
    fn frame(&self, lease: &Lease) -> Result<Frame>;

    // 公共事件（给前端/观察者，带 cursor）
    fn events(&self, thread: &ThreadKey, after: Cursor, limit: usize) -> Result<Page>;
}

trait Executor {
    fn start(&self, req: &ExecutionRequest) -> Result<ExecutionResult>;
    fn query(&self, req: &ExecutionRequest, handle: Option<&Handle>) -> Result<ExecutionResult>;
    fn cancel(&self, req: &ExecutionRequest, handle: Option<&Handle>) -> Result<ExecutionResult>;
    fn capabilities(&self) -> Capabilities; // lookupByExecutionId / idempotentStart / cancellation
}
```

| Backend 实现 | 用于 | 必需/可省 |
|---|---|---|
| `FileBackend`（目录 + 原子 rename + `.lock`） | 本地、客户端、移动端 | **必需**（最小可用） |
| `SqliteBackend` | 客户端、移动端、单机服务 | **必需**（事务、体积可控） |
| `PostgresBackend` | 多主机/多副本 | 需要多写者时才做 |
| `InMemoryBackend` | 测试 | 必需（让所有测试离线） |

`InProcessExecutor`（工具就在本进程跑）+ `RemoteExecutor`（sandbox/作业服务）两种即可；MQ 传输是 `RemoteExecutor` 的一种实现，不是新概念。

---

# 三、五个「定错就返工」的接口决策

这五条与部署形态无关，但一旦定错，后面六种形态都要付代价：

1. **Turn 状态与等待条件是持久数据，不是内存对象。**
   判据：`kill -9` 之后，能否只看 store 说出「这个 turn 在等什么」。
2. **工具执行身份必须在 dispatch 之前生成并落盘**（`executionId` / `idempotencyKey`）。
   否则崩溃后只剩 `uncertain`，永远无法查证。这是 §5 场景 3 的唯一出口。
3. **结果三态与 `UNKNOWN` 必须进类型系统**（`Completed / Accepted / Applied` + `Unknown`）。
   用 `Option<Result>` 表达不了「已接受未应用」，也表达不了「不知道」。
4. **Backend 只暴露原语，不做业务**（不出现 `submitPrompt` 这种）。
   否则本地 backend 会被迫实现调度，移动端就背不动。
5. **恢复边界承诺到工具边界，不要承诺任意栈恢复。**
   Java 明确 `RecoveryBoundary::TOOL_BOUNDARY`；Rust 若承诺更多，异步栈根本做不到。

---

# 四、各形态的附加约束（框架之外的），漏了会翻车

| 形态 | 附加约束 |
|---|---|
| **移动端内置** | ① checkpoint 全量 `stateJson` 是 MB 级，写放大与耗电不可接受 → 需要**增量 checkpoint 或周期性全量 + 前缀摘要**；② 冷启动恢复必须有预算（目标：首字节 < 1s），否则用户体感是「卡死」；③ 不能依赖后台长任务（iOS ~30s 窗口、Android Doze）→ **每次唤醒都是「冷启动 + 从 store 恢复 + 一次 tick」**；④ store 需要加密。 |
| **客户端内嵌** | 没有 supervisor，拉起者就是客户端自己 → `session/resume` 必须够快；退出前要主动 flush（不能只依赖崩溃恢复）。 |
| **API-only** | 构造 Runtime **不得要求** model/executor；不 `start()` 也能 submit/query/observe/respond。Java 把这条写进 AGENTS.md，说明踩过坑。 |
| **多主机** | fence 必须能拒绝过期 owner 的写入；**fence 保护状态，不能撤销已发生的外部副作用**（README 原话）→ 业务侧仍要幂等。 |
| **Brain/Hands** | Hands 必须有独立 ledger 与投递进度（"result accepted" 可重试但不得换 identity）。 |
| **全形态共性** | 恢复时模型缓存（prompt cache）必然失效 → 冷恢复首次 turn 成本高，需要在产品上接受或加「恢复摘要」。 |

---

# 五、路线：三阶段，每阶段覆盖哪些形态

| 阶段 | 做什么 | 覆盖形态 | 验收 |
|---|---|---|---|
| **S1**（约 1/3 工作量） | 第 1 层语义 + `FileBackend`/`InMemoryBackend` + `InProcessExecutor` | 本地、客户端、**移动端**、API-only（只读） | 进程被 kill 后重启，悬挂的工具结果**被应用恰好一次**；模型调用次数不增加 |
| **S2** | `WaitCondition` + `respond` 落盘 + 审批无 Brain 也可应答 | 全部（跨天/人工） | 无 agent 进程时能 `respond`，下次 tick 继续同一 Turn |
| **S3**（仅当要多主机） | `claim/renew/fence` + `Sqlite→Postgres` + `RemoteExecutor` | 多主机/多副本、Brain-Hands | 两进程同时 tick 只有一个推进；强杀后另一副本接管；`tool/result` 恰好 1 条 |

关键点：**S1 就已经把你列的六种形态里四种（本地/客户端/移动端/API-only）覆盖住了**，而它并不需要分布式任何东西。多主机只是 S3 追加。

---

# 六、什么情况下可以不做

只有当你能**永久锁死**「单进程常驻 + 无跨天等待 + 崩了就让用户重来」这一种形态时，才不需要执行层。现实里只要有一个客户端版本或者一个跨天审批需求，这个前提就破了——而那时你已经把 `tokio::spawn` 的后台任务、inbox hint、`stale_ttl` 三套临时机制写进产品，改造成本远高于现在做 S1。

反过来说：**不要为了「未来可能多主机」现在就上 Postgres + claim**。S1 的 backend 契约留好 `claim` 方法（本地实现退化为文件锁、`shared=false`），未来加实现即可，这才是「不返工」的成本最低解。

---

# 七、一句话

> 你列的六种形态里，**移动端内置是最强约束**（进程随时死、后台窗口极短、写放大敏感），它逼你把「等待」和「执行身份」变成持久数据；
> 一旦它们是持久数据，多主机就只是**换一个 Backend 实现 + 打开 fence**，而不是换一套架构。
> 所以顺序应该是：**语义层 → 薄执行层 + 嵌入式 backend → 分布式 backend**，而不是反过来照搬 Java 的服务端框架。
