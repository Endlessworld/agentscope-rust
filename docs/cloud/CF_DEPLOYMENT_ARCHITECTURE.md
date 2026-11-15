# Cloudflare 部署架构：边缘多副本 + 容器化 Agent

> 版本：v1.1（2026-11-14）
> 状态：**架构设计草案**，尚未开始实现
> 关联文档：`.agentscope/workspace/durable-handoff-2026-11-14.md`（能力评估与差距分析）、`docs/cloud/CF_MIGRATION_TASKS.md`（任务拆解）
> 读者：实现者、评审者、运维
>
> **v1.1 变更**：新增 §18.1 端口清单（Backend 只是七分之一）、§18.2 双形态兼容（同一 tick + 两个 Driver）、§18.3 渐进路径；补充 §1 第 4 条判断、§17 两条反例、§19 两条未决问题。

---

## 0. 这份文档要解决什么

把 `agentscope-rust` 的 agent 运行时搬到 Cloudflare 全栈云上，满足以下部署形态：

| 形态 | 核心约束 |
|---|---|
| 全球边缘多副本 | 同一 Turn 只能有一个推进者；副本可被随时替换 |
| 跨天等待 / 人工审批 | 等待是持久状态，进程（容器）可以不活着 |
| API-only 部署 | 不做模型调用、不启动执行循环，也能提交/查询/应答 |
| 本地 / 客户端嵌入 | 无云服务时可降级为文件后端 |
| 移动端内置 | 进程随时被杀，冷启动恢复预算 < 1s 首字节 |

**一句话结论：Cloudflare 把「所有权」和「常驻进程」两件事托管掉了，我们只需自己写业务语义与薄状态机。但状态必须与计算彻底分离，否则容器的 sleep 会退化成「每次都重跑」。**

---

## 1. 四句话判断

1. **Durable Objects 承接 claim/fence**：每个 Session 一个 DO，天然单写者、全球唯一、就近激活、事务存储。不再自研 lease/ fencing/扫描。
2. **Containers 承接「进程可不存在」**：容器按需唤起、可睡到零；这正是最初诉求里的「10 分钟无响应则释放内存，结果到达再唤醒」，不需要自研 supervisor。
3. **语义层一行也省不掉**：三态（completed/accepted/applied）、`UNKNOWN`、`WaitCondition`、执行身份前置持久化、恢复边界、finalization 可恢复，平台不提供。
4. **本地与边缘是同一个 tick 的两种调度器**：一套 `tick(thread)` 循环 + 七个端口 + 两个 Driver（`LocalDriver` / `EdgeDriver`）。`Backend` 只解决「状态在哪」，不是兼容的全部（详见 §18.1 / §18.2）。

---

## 2. 角色与边界

| 角色 | 是什么 | 不做什么 |
|---|---|---|
| **Gateway（Worker）** | 鉴权、路由、WS 升级、限流 | 不持有状态、不做调度 |
| **SessionDO** | 一个会话的权威状态：Turn 状态机、等待条件、pending interaction、executionId、事件 cursor、幂等 receipt | 不跑模型、不跑工具、不反序列化大 state |
| **Brain Container** | 跑一次 Run：hydrate → ReActAgent → commit → 允许睡 | 不持有唯一状态；不假设自己是常驻的 |
| **Hands Container** | 执行长工具/沙箱；持有 executionId + handle，可被 query | 不参与推理；不决定 Turn 终态 |
| **R2** | checkpoint、大工具输出、artifact、workspace 快照 | 不承担事务/锁 |
| **Queues** | 「该推进了」「结果到了」「审批完成」 | at-least-once，消费方必须幂等 |
| **D1（可选）** | 会话目录、审计、跨会话查询 | 不做状态机、不进关键路径 |

**边界原则**：SessionDO 是唯一权威；容器与 Hands 都是可丢弃的计算；R2 只存不可变数据；任何跨角色通信都带幂等键。

---

## 3. 组件映射

| Cloudflare | 承接的 durable 概念 | 需要自己写 |
|---|---|---|
| Durable Object（每 Session） | Session / Thread / Turn、WaitCondition、事件 cursor、receipt | 核心：状态机与迁移规则 |
| DO 单线程 + 输入门控 | claim / fencing | 无（白送） |
| DO alarms | deadline、超时、唤醒节流 | alarm 与状态的对账 |
| DO storage（SQLite-backed） | 小状态与强一致事实 | schema + 查询 |
| R2 | checkpoint / `stateJson` / artifact | 压缩、分块、hash 指针 |
| Containers（绑定 DO） | Brain / Hands | 镜像、hydrate、`POST /run` |
| Queues | 结果投递、通知 | 幂等消费 |
| D1 | 目录与审计 | 只读副本查询 |
| KV | 配置、模型 catalog | — |
| Workflows（可选） | 固定多步长流程（审批后自动跑） | 只承载流程段 |

---

## 4. 拓扑

```
Client (web / desktop / mobile)
   │  HTTPS + WS（鉴权在 Worker）
   ▼
Worker Gateway ──────────────► SessionDO（每 session 一个）
   │                              │  权威状态：Turn / Wait / Interactions /
   │  WS 事件订阅（hibernation）     │             executionId / cursor / receipts
   │                              │  大对象只存 R2 指针
   │                              ├── alarm：deadline / 重试 / 心跳
   │                              ├── Queues：推进通知、结果投递
   │                              ▼
   │                        Container: Brain（一次 Run）
   │                              │  启动 → hydrate → 推进到安全点 → commit → 睡
   │                              ▼
   │                        Container: Hands（工具/沙箱）
   │                              └── executionId + handle，可 query/cancel
   └──────────────────────────────┘
```

---

## 5. 数据放置（决策表）

| 数据 | 位置 | 理由 |
|---|---|---|
| Turn 状态、`WaitCondition`、pending interaction、executionId、cursor、receipt | DO storage | 强一致 + 单写者 + 体积小 |
| checkpoint / `stateJson` | **R2**，DO 只存 `{key, sha256, size, schemaVersion}` | MB 级全量写会把 DO 的成本与延迟放大 |
| 大工具输出 / artifact / workspace 快照 | R2 | 不可变、可按需取 |
| 会话目录、审计、跨会话检索 | D1（只读副本） | 允许最终一致 |
| 配置、模型 catalog 缓存 | KV / Worker 环境 | 无需强一致 |
| 执行方指针与结果 | DO（指针）+ Queues（投递） | 结果必须能跨容重生重放 |

> **硬规则**：任何超过 64KB 的对象都不进 DO storage。

---

## 6. SessionDO schema（草案）

```sql
-- 会话元信息（一行）
session(id TEXT PRIMARY KEY, namespace TEXT, created_at INT, schema_version INT)

-- Thread（一个会话可有多个：main / 子任务）
thread(id TEXT PRIMARY KEY, session_id TEXT, definition TEXT, definition_version TEXT,
       state_version TEXT, created_at INT)

-- Turn 队列（Thread 内按 seq 串行）
turn(id TEXT PRIMARY KEY, thread_id TEXT, seq INT, status TEXT,
     revision INT, request_id TEXT UNIQUE,              -- 幂等 admission
     input_ref TEXT,                                    -- R2 指针
     wait_kind TEXT, wait_key TEXT, wait_deadline INT,  -- WaitCondition
     cancel_requested INT, pause_requested INT,
     run_id TEXT, error TEXT,
     created_at INT, updated_at INT)
CREATE INDEX turn_queue ON turn(thread_id, seq);

-- Run（一次容器调用；恢复时沿用 Turn，新调用记新 Run）
run(id TEXT PRIMARY KEY, turn_id TEXT, started_at INT, ended_at INT,
    status TEXT,           -- RUNNING / COMPLETED / INTERRUPTED / FAILED / SUSPENDED
    outcome TEXT,          -- RunOutcome 序列化
    position TEXT,         -- AgentExecutionPosition(reasoning, acting)
    container_instance TEXT)

-- 工具执行（三态）
 tool_exec(id TEXT PRIMARY KEY, turn_id TEXT, run_id TEXT, call_id TEXT,
          name TEXT, executor TEXT,                          -- inproc / hands:<name>
          execution_key TEXT UNIQUE,                         -- 幂等 identity
          state TEXT,          -- DISPATCHED / COMPLETED / ACCEPTED / APPLIED / UNKNOWN / CANCELLED
          handle TEXT, result_ref TEXT, applied_at INT,
          created_at INT, updated_at INT);

-- 交互（人工审批 / 外部执行应答）
interaction(id TEXT PRIMARY KEY, turn_id TEXT, call_id TEXT, kind TEXT,
            status TEXT,          -- PENDING / RESOLVED / CANCELLED
            request_ref TEXT, answer_ref TEXT, resolved_at INT);

-- 事件流（对外可 replay）
event(id INTEGER PRIMARY KEY, thread_id TEXT, turn_id TEXT, type TEXT,
      revision INT, payload_ref TEXT, created_at INT);

-- 幂等 receipt（控制指令与提交）
receipt(request_id TEXT PRIMARY KEY, kind TEXT, result_ref TEXT, created_at INT);

-- 订阅游标（客户端 Last-Event-ID 基线，可选）
cursor(client_id TEXT PRIMARY KEY, thread_id TEXT, last_event INT);
```

设计要点：
- `turn.request_id UNIQUE` 提供 admission 幂等（重试同一 `requestId` 返回原 Turn）。
- `tool_exec.execution_key UNIQUE` 保证「派发前先落盘身份」，崩溃后可 `query` 查证而不是猜。
- `event.id` 单调递增，作为 `Last-Event-ID` 的来源；payload 大时只存 R2 指针。
- schema 带 `schema_version`，不兼容直接拒绝，**不做转换**（与 Java durable 的策略一致）。

---

## 7. Turn 状态机

| 状态 | 含义 | 谁能写 |
|---|---|---|
| `QUEUED` | 已接收，排在 Thread 队列里 | DO |
| `RUNNING` | 有容器正在推进 | DO + Brain 回调 |
| `WAITING` | 有 `WaitCondition` 未满足（外部执行 / 人工审批 / 时间） | DO |
| `PAUSING` / `PAUSED` | 显式暂停（安全点生效后进入 PAUSED） | DO |
| `SUCCEEDED` / `FAILED` / `CANCELLED` | 终态 | DO |

迁移表：

| 从 | 到 | 触发 | 前置条件 | 幂等键 |
|---|---|---|---|---|
| — | QUEUED | `submit(requestId, input)` | 无 | `requestId` |
| QUEUED | RUNNING | alarm / 队列推进 | 前一个 Turn 非终态则等待 | `turnId + revision` |
| RUNNING | WAITING | Brain 报告 `SUSPENDED` + `WaitCondition` | 工具调用已落盘 identity | `runId + revision` |
| RUNNING | SUCCEEDED | Brain 报告 `COMPLETED` | checkpoint 已提交 | `runId + revision` |
| RUNNING | FAILED | Brain 报告不可重试失败，或重试预算耗尽 | — | `runId + revision` |
| WAITING | RUNNING | 等待条件满足（respond / 结果 ACCEPTED / deadline） | 条件值与 `wait_key` 匹配 | `waitKey + revision` |
| WAITING | CANCELLED | 显式取消 | 无未确认副作用（否则先 CANCELLING） | `requestId` |
| RUNNING | PAUSING | 暂停请求 | — | `requestId` |
| PAUSING | PAUSED | Brain 在安全点确认 | — | `runId` |
| PAUSED | RUNNING | 显式 resume + 观察到的 revision 匹配 | 重新校验等待条件 | `requestId + revision` |
| 任意非终态 | CANCELLED | 所有工具已终态且无 UNKNOWN 未决 | — | `requestId` |

> **UNKNOWN 不可直接取消**：必须先 `query` 得到确定结果，或显式接受 `UNKNOWN` 并把 Turn 置 `FAILED`（记原因）。

---

## 8. 接口契约

### 8.1 客户端 / API-only（Worker → DO）

```
POST /sessions/{id}/turns            {requestId, threadId?, input}      -> {turnId, status}
GET  /sessions/{id}/turns/{turnId}                                     -> TurnView
POST /sessions/{id}/turns/{turnId}/respond  {requestId, interactionId, answer}  -> receipt
POST /sessions/{id}/turns/{turnId}/steer    {requestId, text}           -> receipt
POST /sessions/{id}/turns/{turnId}/cancel   {requestId}                 -> receipt
POST /sessions/{id}/turns/{turnId}/pause    {requestId}                 -> receipt
POST /sessions/{id}/turns/{turnId}/resume   {requestId, revision}       -> receipt
GET  /sessions/{id}/events?after=<cursor>   (SSE，支持 Last-Event-ID)
WS   /sessions/{id}/stream                   (hibernation 友好)
```

规则：
- 所有写操作带 `requestId`，重复返回同 receipt。
- `resume` 必须带观察到的 `revision`，不匹配则拒绝（防止基于过期快照的恢复）。
- API-only 部署只提供上述接口，**不启动执行循环**（无 Brain 容器绑定）。

### 8.2 Brain 容器契约

```
POST /run
  { sessionId, threadId, turnId, runId, revision,
    checkpointRef | null,      // R2 指针
    inputs: [...],             // 待应用输入 / 交互结果 / 已接受工具结果
    waitCondition: {...} | null,
    position: {reasoning, acting} }
  -> 202 Accepted（异步执行，立即返回）

POST /internal/commit（容器 -> DO，走 DO RPC）
  { runId, revision, expectedRevision,
    facts: [...],              // 增量事实（不含大对象）
    checkpoint: {key, sha256, size, schemaVersion} | null,
    turnStatus, waitCondition | null, position,
    toolExecutions: [{executionKey, state, handle, resultRef}] }
  -> { accepted: bool, newRevision }   // CAS：expectedRevision 不匹配则拒绝

POST /internal/failure
  { runId, kind: RETRYABLE | INVALID | FENCED, message }
```

约束：
- **`POST /run` 只派发不阻塞**。一次 turn 可能跑几十分钟，不能由一个 HTTP 请求承载。
- 容器不信任自己持有的状态：每次启动都从 `checkpointRef` hydrate；`position` 由 DO 给出（不是容器自己记）。
- commit 用 `expectedRevision` 做 CAS：DO 单写者 + revision 双保险，防止「休眠前的旧容器」写回。
- 容器可随时被平台回收：回收前不强求 flush，DO 的 Turn 仍是 `RUNNING`，由租约超时（alarm）判定后重派一个新 Run。

### 8.3 Hands 契约

```
trait Executor {
    fn start(&self, req) -> ExecutionResult;   // DISPATCHED + handle
    fn query(&self, req, handle) -> ExecutionResult;
    fn cancel(&self, req, handle) -> ExecutionResult;
    fn capabilities(&self) -> { lookupByExecutionId, idempotentStart, cancellation };
}
```

判定树（直接照抄 Java `ExecutionRecovery.resolve` 的思路）：

```
if terminal -> return
if !attempted -> start()
else if queryable -> query()
     if NOT_FOUND && idempotentStart -> start()
else if idempotentStart -> start()
else -> UNKNOWN
任何异常 -> UNKNOWN
```

> 「查不到」永远不等于「没发生」。`UNKNOWN` 是终态语义，不是重试信号。

### 8.4 事件与 cursor

- 事件 JSON 带 `{id, threadId, turnId, type, revision, payload|payloadRef}`。
- `id` 单调递增，`Last-Event-ID` 直接用它。
- 游标过期（DO 已裁剪）→ 返回 `snapshotRequired`，客户端重新拉一次快照。
- delta 类事件可合并写（每 N 条或每 T 毫秒提交一次），避免 DO 往返放大。

---

## 9. 时序

### 9.1 正常 Turn

```
client  → DO      submit(requestId, input)
DO      → client  {turnId, QUEUED}
DO      → Brain   POST /run（container.fetch，立即返回）
Brain   → DO      commit(facts..., checkpoint, RUNNING)
Brain   → client  模型 delta（WS 直连，不经过 DO）
Brain   → DO      commit(turn=SUCCEEDED, revision+1)
DO      → client  turn/succeeded 事件
```

### 9.2 跨天人工审批

```
Day 1
Brain  → DO    commit(turn=WAITING, wait={kind:interaction, id})
DO     →      allowSleep；容器缩容到 0
Day 2
审批人 → Worker respond(requestId, interactionId, approve)
DO     →      幂等 receipt；wait 满足；alarm(now)
DO     → Brain POST /run（新 runId，携带已接受的 answer）
Brain  → DO    commit(tool_result 归位到原 call_id, turn=RUNNING→SUCCEEDED)
```

要点：**等待期间没有任何常驻进程**；`pending interaction` 在 DO，Day 2 的容器是全新实例。

### 9.3 长工具三态

```
Brain  → DO     commit(tool_exec=DISPATCHED, executionKey, handle)   ← 先落盘身份
Brain  → Hands  start(executionKey)
Brain  → DO     commit(turn=WAITING, wait={kind:execution, key})
Hands  → Queue 结果
Queue  → DO    accept(executionKey, resultRef)   // COMPLETED -> ACCEPTED
DO     → Brain new Run: apply → APPLIED；turn→RUNNING
```

> `ACCEPTED` 与 `APPLIED` 分离，保证「宿主在两者之间崩溃」不会重跑工具（对应 Java 的 accepted/applied 区分）。

### 9.4 容器崩溃

```
Brain 崩溃（无 commit）
DO: Turn 仍 RUNNING，run 有 startedAt，无心跳
DO alarm: 超过 runTimeout → run=INTERRUPTED，turn 回 QUEUED（或 WAITING）
DO: 新 run 携带同一 turnId + checkpointRef + position
新容器: hydrate → 若工具已 ACCEPTED 则 apply，不重派
```

> 这里用「超时 + 重新派发」代替 lease/fence：DO 是单写者，只需判断「上一次 Run 是否还活着」。

---

## 10. 恢复语义

| 能恢复 | 不能恢复 |
|---|---|
| Turn 身份、队列位置、等待条件 | 任意 Rust 异步栈 / 中间件副作用 |
| 已提交的对话与工具结果 | 未提交的模型输出（需重跑该次调用） |
| `AgentExecutionPosition`（迭代预算） | 容器内进程内存、临时文件 |
| 工具执行身份与 handle（可 query） | 已发生的外部副作用（只能幂等 + 查证） |
| finalization（已产出 reply 未收尾） | 被裁剪的历史窗口之外的内容 |

**恢复边界 = 工具边界**。不要在文档或 API 里承诺「断点续跑任意位置」。

---

## 11. 容器生命周期与恢复预算

| 参数 | 建议 | 说明 |
|---|---|---|
| `sleepAfter` | 30–60s（交互期）；0（长等待立即睡） | 与成本直接相关 |
| 热路径 | 同会话 30s 内连续 turn 不睡 | 交互体验优先 |
| 镜像大小 | 目标 < 50MB | 冷启动主要项 |
| hydrate 预算 | < 200ms（本地 R2 同区域） | 分块 + 只读前缀复用 |
| 首字节预算 | < 1s（移动端） | 容器启动 + hydrate + 模型首 token |
| 单 Run 上限 | 由产品定（如 30min） | 超过则落 WAITING 并续 Run |

检查点策略：
- 全量 checkpoint 只在「工具边界 / Turn 结束」写；
- 中间只写增量事件；
- checkpoint 按块存 R2（前缀 hash 复用），避免每轮 MB 级重写。

---

## 12. 平台约束清单

| 约束 | 设计应对 |
|---|---|
| 单 HTTP 请求承载不了长任务 | `POST /run` 异步 + 回调 commit |
| DO 单线程 | 长 CPU 工作（大 state 反序列化）放容器；DO 只做索引与状态迁移 |
| DO 只有一个待触发 alarm | deadline / 超时 / 节流合并为「下一次该做什么」的计算 |
| Queues at-least-once | 消费以 `executionKey` 幂等 |
| 容器磁盘临时 | workspace 从 R2 物化；不依赖本地文件持久 |
| DO 有 home location | token 流式由容器直连客户端，事件另走 DO |
| D1 写走 primary | 目录异步写，不进关键路径 |
| DO storage 写入放大 | 大对象一律 R2 + 指针 |

> 具体配额（并发实例、vCPU/内存、单请求时长、DO 存储上限）以部署时的官方限额为准。本架构只要坚持「容器无状态 + 异步执行 + 状态外置」就不会被限额卡死。

---

## 13. 安全与多租户

- 鉴权在 Worker：`Authorization` → `(namespace, sessionId)` 映射；DO 不接受未鉴权请求。
- `namespace` 作为租户边界，写入所有表；跨租户查询在 DO 内拒绝。
- 容器只接受来自绑定 DO 的调用（DO RPC / 内部 token），不暴露公网入口。
- 数据主权：需要时给 DO 设置 jurisdiction；R2 bucket 按区域规划。
- 审批类操作记录 `actor`（谁批的），写入 D1 审计。

---

## 14. 版本与格式

- `schema_version`（DO）、`state_version`（native checkpoint）、`definition_version`（agent 定义）三者独立。
- 不匹配 → 拒绝并给出明确错误（`INCOMPATIBLE`），**不转换、不降级**（与 Java durable 策略一致）。
- 镜像版本钉死在 Thread 的 `definition_version` 上；升级通过新建 Thread 或显式迁移完成。

---

## 15. 可观测性与 SLO

必采指标：

| 指标 | 用途 |
|---|---|
| `turn.status` / `turn.waiting_ms` | 业务流程健康度 |
| `run.hydrate_ms` | 冷启动体验 |
| `run.duration_ms` / `container.awake_ms` | 成本主要项 |
| `container.start_total{hot|cold}` | 热路径命中率 |
| `tool_exec.state` 分布（含 UNKNOWN） | 副作用风险面 |
| `commit.cas_reject_total` | 旧容器写回 / 竞态 |
| `event.lag_ms` | 前端体感 |

建议 SLO（草案）：- Turn 推进延迟 P95 < 3s；- 冷恢复首字节 P95 < 1.5s；- `UNKNOWN` 比例 < 0.1%；- 重复副作用 = 0（观测 `tool/result` 恰好一次）。

---

## 16. 成本结构

| 项 | 量级 | 优化 |
|---|---|---|
| 容器 vCPU·s | **主要** | 尽快睡；一次 Run 只做必要工作 |
| DO duration / 请求 | 次要 | 事件批量提交，减少往返 |
| R2 存储 / 操作 | 低（无 egress） | 分块复用，避免每轮全量 |
| Queues | 很低 | 合并通知 |
| D1 | 低 | 只做目录/审计 |

倾向：**宁可多一次 R2 读，也不要让容器多醒 30 秒**。

---

## 17. 不推荐的做法

| 做法 | 为什么不行 |
|---|---|
| 把 ACP stdio 直接搬到云上 | ACP 是本地客户端↔进程协议；云端应是 Worker + WS API |
| 容器常驻并自管多会话 | 贵，且与 DO 单写者权威冲突，最终两套所有权 |
| 用 Workflows 当会话状态机 | 不适合交互式 steer/cancel；只在固定多步段使用 |
| checkpoint 直写 DO | 写放大 + 成本 + 唤醒 IO |
| 跨 DO 事务 | 平台不提供；用消息 + 幂等，一致性边界切在 session |
| 依赖容器本地磁盘 | 临时存储，缩容即丢 |
| 用 `UNKNOWN` 驱动重试 | `UNKNOWN` 是终态语义，重试会产生重复副作用 |
| 本地一套 `run_agent_turn()`、云上一套 `tick()` | 两条实现路径必然漂移；一边修的 bug 另一边没有，测试覆盖不了任何一边 |
| 用 feature flag 编译两套循环 | 同上，且 CI 矩阵翻倍，长期无人维护其中一套 |

---

## 18. 与现有 Rust 代码的映射

| 现有资产 | 复用方式 | 需要改的 |
|---|---|---|
| `core/src/session/{journal,recorder,views,recovery}.rs` | 作为 native 事实层，序列化为 checkpoint | 事实里补 `executionKey` / `position`；`abandon_turn` 增加 `reconcile` 语义 |
| `harness/src/agent/session/session.rs` | `submit/resume/respond` 语义可映射到 DO | 把「内存态」抽成 `hydrate/commit` 幂等接口 |
| `harness/src/agent/bus/*`（inbox/bus/async_tool） | 概念保留 | 本地文件后端 → DO/Queues 实现 |
| `harness/src/agent/middleware/async_tool.rs` | offload 语义保留 | 后台 task → Hands Executor；结果按 call_id 归位 |
| `harness/src/agent/gateway/session_turn_gate.rs` | 进程内互斥仍有用 | 跨进程所有权改由 DO |
| `acp` crate | 保留，服务桌面直连本地二进制 | 云上不用 |
| `mem.rs` | 指标与 trim | 云上以「容器 awake_ms」为主 |

### 18.1 端口清单：`Backend` 只是七分之一

只换 `Backend` 只能覆盖「状态在哪」，Cloudflare 还挑战另外四件事：谁去跑、什么时候跑、结果怎么出去、跑多久。最小端口集合：

| 端口 | 职责 | 本地实现 | 边缘实现 |
|---|---|---|---|
| `SessionBackend` | Turn/Thread 状态、原子 commit、事件存储、幂等 receipt | SQLite / 文件 | DO storage |
| `BlobStore` | checkpoint、大工具输出、artifact | 本地目录（原子 rename + sha256） | R2 |
| `WorkspaceFs` | 文件工具与 workspace（read/write/ls/grep/glob） | 本地 FS（现有 trait） | R2 / 容器临时盘 |
| `Clock` | 到期时间、超时判定、唤醒节流 | `tokio::time` + 定时任务 | DO alarm |
| `EventChannel` | 事件发布/订阅、cursor、`Last-Event-ID` | `broadcast` + 文件 bus | WS（hibernation）+ Queues |
| `Executor` | 工具执行 `start/query/cancel` + capabilities | `InProcessExecutor` | Hands 容器 |
| `Dispatcher` | 何时调用 `tick`、唤醒执行者 | `tokio::spawn` + 监听 | alarm / 请求即 tick |

读法：**`SessionBackend` 是唯一在 Cloudflare 上「变简单」的端口**（claim/fence 被 DO 吸收），其余六个要么必须新写，要么必须换语义。

```
Backend      解决「状态在哪」
Dispatcher   解决「谁来跑」
Clock        解决「什么时候跑」
EventChannel 解决「结果怎么出去」
Executor     解决「跑多久」
```

**只换 Backend 会在云上出现的具体故障**：Turn 卡在 `RUNNING` 无人推进；后台工具结果到了模型不知道；心跳/超时失效；单轮超过平台请求时长被切断；workspace 写不进去。

### 18.2 双形态兼容：同一 tick，两个 Driver

结论：**可以同时兼容本地常驻与边缘可睡，但前提是「一套循环 + 两个 Driver」，不是「两套跑法」。**

| 维度 | 本地 / 常驻 | Cloudflare / 可睡 |
|---|---|---|
| 谁驱动 | 常驻线程 + 定时器 | 事件（alarm / 请求 / 队列）即一次执行 |
| 一次执行能跑多久 | 不限 | 有上限，必须切安全点 |
| 内存能否保存状态 | 能（但**不许**） | 不能 |
| 时钟 | `sleep` | 只能「记到期时间 → 下次检查」 |
| 事件出去 | 进程内 channel | WS + Queues + cursor |

```
submit / wake ──► tick(thread) -> Commit   （唯一的一份业务逻辑）
                        │
   ┌──────┬──────┬───────┼────────┬────────────┬────────┐
Backend BlobStore  Fs   Clock  EventChannel  Executor
   │      │      │       │          │           │
LocalDriver:  SQLite/文件 / 本地FS / tokio / broadcast / inproc
EdgeDriver:   DO storage / R2 / R2+临时盘 / alarm / WS+Queues / Hands
```

四条硬规则（违反任一条，就会退化成两套）：

| # | 规则 | 原因 |
|---|---|---|
| R1 | **禁止内存真值**：本地也不许把 turn 状态只放内存 | 否则云上跑不了；本地也会重启丢工作 |
| R2 | **禁止无界阻塞**：一次 tick 只做「一次模型调用 / 一次工具 apply / 一次 finalization」 | 单轮可能超过平台执行上限 |
| R3 | **禁止隐式顺序**：正确性不能依赖线程调度或进程内锁 | 本地靠调度「碰巧对」，云上必露 |
| R4 | **禁止时钟依赖**：「睡 X 秒后立刻继续」不成立 | 边缘没有后台线程 |

两种装配（上层一行不分叉）：

| 端口 | `LocalAssembly` | `EdgeAssembly` |
|---|---|---|
| SessionBackend | SQLite / File（`shared=false`） | DO（`shared=true`） |
| BlobStore | 本地目录 | R2 |
| WorkspaceFs | 本地 FS | R2 / 容器临时盘 |
| Clock | tokio timer | DO alarm |
| EventChannel | broadcast + 文件 bus | WS + Queues |
| Executor | InProcess | Remote（Hands） |
| Dispatcher | LocalDriver（常驻） | EdgeDriver（事件） |

质量保障：契约测试（对应 Java durable 的 `SessionBackendContract` / `UserApiContract`）：

```
tests/contract/backend_contract.rs    → InMemory / File / SQLite / DO 同一组断言
tests/contract/executor_contract.rs   → InProcess / Remote
tests/contract/driver_contract.rs     → LocalDriver / EdgeDriver（都退化为「手动 tick 一次」）
tests/contract/recovery_contract.rs   → 六个恢复场景（崩溃点 × 三态）
```

`driver_contract` 的技巧：因为 `tick` 是纯函数、driver 只决定调用时机，两个 driver 在测试里都退化成「手动调用一次 `tick`」，于是「两种形态」在测试层变成同一个调用序列，差异只剩端口实现。

判据：**任何只在某个 Driver 里出现的业务分支，都视为设计缺陷。**

### 18.3 渐进路径（与 M1–M6 对齐）

| 阶段 | 形态 | 说明 |
|---|---|---|
| M1 | 只有端口定义 + 语义类型 | 不接 driver，纯类型与状态机单测 |
| M2 | `LocalAssembly` 最小版 | File backend + LocalDriver + inproc executor |
| M3 | 崩溃恢复契约 | 同一套 tick，换 InMemory / SQLite backend 跑 |
| M4 | `EdgeAssembly` | 只新增 DO backend / alarm driver / WS channel |
| M5 | Hands | 只新增 RemoteExecutor |
| M6 | 观测 | 两边共用同一组指标名 |

关键点：**M4 之前本地已能验证全部语义；CF 只是新增实现，不新增逻辑。** 这就是「同时兼容」的工程判据。

---

## 19. 未决问题（需要在实现前定）

| # | 问题 | 影响 |
|---|---|---|
| 1 | Brain 与 Hands 是否同容器（先同容器简化，后拆） | 拆分时机决定接口冻结时间 |
| 2 | token 流式走容器直连还是 DO 转发 | 延迟 vs 可重放 |
| 3 | checkpoint 分块策略（按消息前缀 / 按 hash 链） | 冷启动预算 |
| 4 | 是否引入 Workflows 承载「审批后自动多步」 | 编排复杂度 |
| 5 | 多租户限流放在 Worker 还是 DO | 公平性与成本 |
| 6 | 容器镜像是否需要 GPU（当前结论：不需要） | 成本与地域覆盖 |
| 7 | `BlobStore` 的缓存层是否允许（可丢弃的加速缓存，不得成为真值） | 本地性能 vs 正确性 |
| 8 | `EdgeDriver` 的 tick 粒度：一次 alarm 推进一个 turn 还是扫一批 | 吞吐 vs 单次执行预算 |

---

## 20. WASM / Workers 可行性调查（2026-11-16）

调查目的：若实现 `durable-cf-*` 系列，能否把 ACP agent 直接编译到 `wasm32-unknown-unknown`
跑在 CF Worker 上，从而省掉 CF Containers。

### 20.1 结论

**agent 主循环可以在 Worker + WASM 上跑，不需要 Container；但不是「编译一下就行」。** 两个前提
必须同时成立：

1. **ACP 的 stdio 传输必须换成 WebSocket**（§18 已规划的 WS + hibernation 方案即为此）；
2. **任意代码执行类工具（sandbox）在 Workers 上物理不可能**，必须外置到 Containers 或远端
   MCP server。省掉的是「agent 进程」的容器，不是「工具执行」的容器。

### 20.2 Workers 约束 vs 现有依赖（静态推断，待 P0 证伪）

| 约束 | Workers 现实 | 本项目冲突点 |
|---|---|---|
| 线程 | 无（无 pthreads、无跨 isolate 共享内存） | `tokio` 的 `rt-multi-thread` / `spawn_blocking`、任何 `std::thread` |
| 进程 | 无 | `std::process::Command`（sandbox 执行器）、MCP stdio 传输 |
| 文件系统 | 无 | file store 的 `std::fs`、workspace 本地 FS、`LocalFilesystem` |
| 网络 | 只能 `fetch` | `reqwest`（模型 HTTP 客户端） |
| 内存 | 128 MB | scope 整文档重写（实测可见 27 MB 级）→ 依赖 §6.1-4 与 L1-c 的 blob 外置 + 分页读 |
| CPU | 默认 30s 上限 | `LocalDriver` 的 `max_steps = 16` 会超预算 |
| 时间 | `std::time::Instant` 不可直接用 | tick 内的超时判断全部依赖 wall clock（见 R4） |
| 产物目标 | `wasm32-unknown-unknown`（V8 isolate） | **不是** `wasm32-wasip1`，构建目标不可混用 |

### 20.3 三个硬阻塞（按风险排序）

**① `tokio` feature 分裂（最难，改动面最大）**
`agentscope-durable` 本身轻（单 `tick`，符合 R2），但 `agentscope-core` + `agentscope-harness`
依赖 `tokio::spawn`、`spawn_blocking`、`net`、`fs`。需要在两个 crate 内做
`cfg(target_arch = "wasm32")` 的 feature 分裂，并要求 13 个 harness middleware 全部不再假设后台任务存在。

**② `Clock` 端口缺失（七端口里唯一既未实现又必然要碰的）**
CF 上只能用 `Date.now()`。R4（禁止时钟依赖）必须先落地，否则 tick 内的超时/退避/`ready_at`
判定全部失效。

**③ `EventChannel` 换 WS（而非 Queues）**
ACP 是 stdio 协议，CF 上只有 WS。`platform_channel.rs` 已是 WS 形状，ACP 的
`session/load`（重放）与 `session/resume`（不重放）语义正好对应 DO hibernation + WS 唤醒；
但这是传输层重写，不是编译选项。

### 20.4 已经就绪的部分

- `durable-worker` 的 `Worker::step` = 一次 tick，天然 Workers 友好（无常驻循环）。
- **`LocalDriver` 在 Workers 上不可用，只能 `EdgeDriver`**（每次唤醒 1 step）。L5 的双驱动等价
  测试已证明这是纯调度选择，不是业务分支。
- 六个适配器均为注入式端口（`Transport` / `Broker` / `Sandbox` / `ObjectStore` /
  `McpTransport`），CF 实现是**新增 impl**，不改现有代码。`durable-http` 的 `Transport`
  可直接接 service binding（同 isolate 零网络跳）。
- L2 的模型事实（554 B/次，仅元数据）在 128 MB 下是必需而非可选。

### 20.5 切分方案：薄 Worker + 外部 Executor

```
Worker (WASM) —— 薄
  durable 状态机 + EdgeDriver + Brain/Hands 协调
  = agentscope-durable + durable-cf-do(DO) + durable-cf-clock(alarm) + durable-cf-queues
  单次 tick 一次模型调用，CPU 预算内

外部 —— 厚
  Executor 走 durable-mcp（远端 MCP server）或 durable-cf-executor（Containers）
  任意代码执行、CPU 密集、需要进程的全部在这里
```

即：Container 从「必须」降级为「按需」，由 `Executor` 端口选择，不写进业务分支。

### 20.6 六个 `durable-cf-*` 模块规划

| 端口 | 现有实现 | CF 绑定 | 建议 crate | 可离线验证部分 |
|---|---|---|---|---|
| `SessionBackend` | `durable-jdbc` / `-http` | DO storage（SQLite 后端；单写者天然承接 claim/fence） | `durable-cf-do` | tick 状态机、事件序列化、CAS 语义退化 |
| `BlobStore` | `durable-objects` | R2 | `durable-cf-r2` | SigV4 签名（公开测试向量）、key 布局、conditional put |
| `EventChannel` | `durable-rocketmq` | WS + Queues | `durable-cf-queues` | 信封、eventId 幂等、延迟/批量映射 |
| `Clock` | **缺**（隐式 `now`） | DO alarm | `durable-cf-clock` | `ready_at` 五分支计算 |
| `WorkspaceFs` | **缺**（harness 本地 FS trait） | R2 + 容器临时盘 | `durable-cf-workspace` | 路径映射、grep/glob 客户端实现 |
| `Executor` | `durable-mcp` / `-sandbox` | Containers / service binding | `durable-cf-executor` | 三态映射、超时 |
| `Dispatcher` | `durable-worker` | alarm / 请求内 tick | 已在 `durable-worker` | 双驱动等价 |

**命名与依赖方向**：另建 `agentscope-durable-cf-*` 命名空间，不要把 CF 实现塞进现有
`durable-*` 的 feature。理由：CF 版不依赖 file store 那条链（`std::fs`），依赖方向不同。

### 20.7 Queues 与 `durable-rocketmq` 的语义差异

| `durable-rocketmq` 现状 | Cloudflare Queues | 处理 |
|---|---|---|
| `Cursor` = offset（`u64`，顺序化） | 无 offset，ack-based | **cursor 权威移入 DO**（§18 已定：事件 cursor 存 DO），Queues 只携带 `eventId` |
| 乱序 → `OutOfOrder` 报错 | 无 FIFO 保证，批量乱序投递 | 顺序校验放消费侧，与 DO 内 `sequence` 比对；**不得用 Queues 顺序做 turn 顺序权威** |
| 失败批次不提交进度（不跳号） | 批量投递 + 自动重试 | 保留「批内幂等 + 失败不推进」，语义一致，现有测试可复用 |
| — | `delaySeconds` / `max_batch_size` / DLQ | 分别映射 `ready_at` 五分支、`limit 1..=1000` 纪律、不可重试失败直接 `Failed` |

**重要判断**：CF 上唤醒的第一选择是 **DO alarm**（单写者、无额外投递语义、无重复消费）。
Queues 只在「跨对象 fan-out / 解耦消费」时才需要。**Queues 不应排在前面。**

### 20.8 验证路线（每步单独可判定，不依赖全栈就绪）

| 阶段 | 做什么 | 判据 |
|---|---|---|
| **P0** | `cargo build -p agentscope-durable --target wasm32-unknown-unknown` | 是否通过 + WASM 体积（目标 < 2 MB）+ 依赖树中 wasm 不兼容项清单 |
| P1 | 给 `agentscope-durable` 加 `std-fs` feature（file store 可摘除）+ 落地 `Clock` trait | wasm 构建零 `std::fs`；`ready_at` 五分支测试仍绿 |
| P2 | `worker-rs` 跑最小 agent：1 次模型调用 + 1 个 MCP 工具 | 单次 tick 在 CPU 预算内返回 |
| P3 | ACP over WS + DO hibernation | 客户端断连后 `session/resume` 能续 |
| P4 | 评估 `core`/`harness` 的 tokio 分裂成本 | 决定「全量上 Worker」还是「Worker 只放状态机 + 驱动」 |

**P0 是唯一能立刻判定可行性的一步**，且不改变任何现有代码。P0 不成立则后续讨论无意义。

### 20.9 风险与明确不做什么

- **不留**「WASM 条件编译」与「原生路径」两套并行实现的分支扩散：所有平台差异必须收敛到
  §18 的七个端口 + `cfg` 只出现在端口实现内部，禁止渗进业务逻辑。
- **不做** 把任意工具执行搬到 Workers 上（无进程、无 FS）。
- **不做** 用 Queues 承担顺序权威。
- 已知结构冲突：file store 的「整文档重写」在 DO 上必然 OOM，必须改成 SQLite 行级写 +
  L1-c blob 外置；R2 无 rename，temp+rename 原子写要改为 conditional put；
  DO storage 与 R2 无跨事务，blob 引用靠 `BLOB_GC_GRACE` 保护读者。