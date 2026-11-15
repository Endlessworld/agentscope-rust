# Cloudflare 迁移任务清单

> 版本：v1.1（2026-11-14）
> 配套文档：`docs/cloud/CF_DEPLOYMENT_ARCHITECTURE.md`（架构与契约）
> 状态：待评审。评审通过后按里程碑逐项开工。
>
> **v1.1 变更**：新增 §1.1 端口归属表（七个端口 + 两个 Driver 的负责里程碑）；M3 增加契约测试（M3-8/M3-9）；M4 明确只新增 `EdgeDriver` 不新增业务逻辑（M4-8）；G3/G4 与 DoD 加入「无 Driver 专属业务分支」判据。

---

## 0. 怎么用这份清单

- 每个里程碑（M1–M6）都有 **目标 / 范围 / 任务 / 验收 / 反向验证**，单独可交付。
- 每条任务带 `[P]` = 与平台无关（可先在本地做）、`[C]` = 需要 Cloudflare 环境。
- 验收必须可复现；**不接受「测试绿」作为唯一证据**，关键项要做反向验证（把修复还原 → 测试必须 FAILED → 再修回）。
- 能力项完成后，同步登记到 `docs/extensions/{CORE,HARNESS,ACP}_GAP_TRACKER.md`。

---

## 1. 阶段总览

```
M1 语义层 ──┬─► M2 状态外置 ──┬─► M4 容器化 Brain ──┐
            │                 │                     ├─► M6 观测与运维
            └─► M3 DO 状态机 ──┴─► M5 Hands ─────────┘
```

| 里程碑 | 一句话 | 覆盖部署形态 | 平台无关 |
|---|---|---|---|
| M1 | 执行语义（RunOutcome / 三态 / UNKNOWN / position / finalization） | 全部 | 是 |
| M2 | `hydrate/commit` 幂等 + 大对象外置 | 全部 | 是 |
| M3 | Turn 状态机 + 等待条件 + receipt + cursor | 本地、客户端、移动端、API-only | 是（本地 SQLite 等价物） |
| M4 | Brain 容器 `POST /run` + 回调 CAS | 全部 | 部分 |
| M5 | Hands Executor（三态 + query/cancel） | 多主机、Brain/Hands 分离 | 部分 |
| M6 | 指标、告警、成本与 SLO | 全部 | 部分 |

> 读完 M1–M3 即可满足「本地 / 客户端 / 移动端 / API-only」四种形态；M4–M5 才上云。

### 1.1 端口归属（谁负责哪个端口）

架构文档 §18.1 定义七个端口 + 两个 Driver。任务分配如下，避免实现时只盯 `Backend`：

| 端口 / Driver | 本地实现 | 边缘实现 | 负责里程碑 |
|---|---|---|---|
| `SessionBackend` | File / SQLite | DO storage | M2（接口）、M3（本地）、M4（DO） |
| `BlobStore` | 本地目录 | R2 | M2（本地）、M4（R2） |
| `WorkspaceFs` | 现有 `Filesystem` | R2 / 容器临时盘 | M2（接口）、M4（R2 版） |
| `Clock` | `tokio::time` | DO alarm | M2（接口）、M4（alarm） |
| `EventChannel` | `broadcast` + 文件 bus | WS + Queues | M3（本地）、M4（WS/Queues） |
| `Executor` | `InProcessExecutor` | Hands 容器 | M5 |
| `Dispatcher` | `LocalDriver` | `EdgeDriver` | M3（LocalDriver）、M4（EdgeDriver） |

原则：**M4 只新增实现，不新增业务逻辑**；任何逻辑只在某个 Driver 里出现，视为设计缺陷。

---

## 2. M1 执行语义层

**目标**：让「一次调用跑成什么样」和「任务终态」解耦，并让工具结果可归位、可查证。

### In
- `GenerateReason::HostPaused`
- `RunOutcome { Completed, Suspended, Interrupted, Failed, Cancelled }` + 与 `GenerateReason` 映射
- `AgentExecutionPosition { reasoning, acting }`，从 `RuntimeContext` 注入，`ReActAgent` 在 resume 路径使用
- 工具三态 `Dispatched / Completed / Accepted / Applied` + `Unknown`
- 工具执行身份（`execution_key`）在 **dispatch 之前**持久化
- `reconcile(execution_key, outcome, reason)` 替代单纯的 `abandon_turn`
- finalization 可恢复：已产出 reply 先落盘，再做 finish middleware

### Out
- 不做跨进程后端（M2/M3）
- 不改 ACP 协议

### 任务

| # | 任务 | 文件 |
|---|---|---|
| M1-1 | [P] 新增 `HostPaused` 变体并接入 `GenerateReason` 映射 | `crates/agentscope-core/src/message/msg.rs` |
| M1-2 | [P] 新增 `RunOutcome` 类型 + `from_generate_reason` | `crates/agentscope-core/src/agent/`（新文件） |
| M1-3 | [P] 新增 `AgentExecutionPosition`，在 ReActAgent 的 resume 分支读取（替换当前重置行为的逻辑） | `crates/agentscope-core/src/agent/react_agent.rs` |
| M1-4 | [P] 工具结果状态枚举扩展到五态；`ToolResultBlock` 增加 `Unknown` | `crates/agentscope-core/src/message/block.rs` |
| M1-5 | [P] `AsyncToolRegistry` 在 `register` 时先落盘 `execution_key`，再允许 dispatch | `crates/agentscope-harness/src/agent/bus/async_tool.rs`、`middleware/async_tool.rs` |
| M1-6 | [P] `complete(id, result)` 保存结果（修复 D1） | 同上 |
| M1-7 | [P] 新增 `reconcile` API（写 `recovery/applied` + checkpoint） | `crates/agentscope-core/src/session/recovery.rs` |
| M1-8 | [P] finalization 恢复：reply 先落盘，finish middleware 幂等 | `crates/agentscope-core/src/agent/`（推理循环收尾处） |
| M1-9 | [P] 结果按 `tool_call_id` 归位为真实 `tool_result`（而不是 hint 文本） | `crates/agentscope-harness/src/agent/middleware/inbox.rs` |

### 验收

1. 进程被 `kill` 中途后重启：悬挂工具结果被应用**恰好一次**（检查 `tool/result` 事件条数 = 1）。
2. 工具已 `Accepted` 但未 `Applied` 时崩溃：重启只做 apply，不再 dispatch。
3. 模型已返回、finalization 未完成时崩溃：重启后**模型调用次数不增加**。
4. 全量基线不回归：`cargo test --workspace -j 2 --no-fail-fast` → 0 failed。

### 反向验证

- 临时把 `reconcile` 改成直接 `abandon_turn` → M1 验收 1 必须 FAILED。
- 临时把 finalization 恢复关掉 → 验收 3 必须 FAILED。

---

## 3. M2 状态外置

**目标**：把「会话内存态」变成可幂等 hydrate/commit 的数据，为容器无状态化铺路。

### In
- `hydrate(blob|ref) -> AgentState` 与 `commit(state) -> blob|ref` 两个幂等接口
- 大对象（checkpoint / 工具大输出）走外部存储，DO 只存指针
- checkpoint 分块 + 前缀 hash 复用

### Out
- 不绑定具体云后端（先文件 / 本地 SQLite）

### 任务

| # | 任务 | 文件 |
|---|---|---|
| M2-1 | [P] 抽出 `StateBlobStore` trait（`put -> Ref`、`get(Ref) -> bytes`、`delete`） | `crates/agentscope-harness/src/agent/session/` 或新 crate |
| M2-2 | [P] `AgentSession` 增加 `hydrate_from(ref)` / `commit_to(store) -> ref` | `crates/agentscope-harness/src/agent/session/session.rs` |
| M2-3 | [P] `FileStateBlobStore` 实现（原子 rename + sha256 校验） | 同上 |
| M2-4 | [P] checkpoint 分块：按消息前缀切块，块内容 hash 复用 | `crates/agentscope-core/src/session/recorder.rs` |
| M2-5 | [P] 大工具输出外置（超过阈值写 blob，`tool_result` 存引用） | `crates/agentscope-harness/src/tool/output_compact.rs` 附近 |

### 验收

1. 对同一会话连续 hydrate/commit 两次，字节与内容一致（幂等）。
2. 300 轮会话的 checkpoint 写入量：启用分块后相比全量写下降 ≥ 5x（有测量日志）。
3. 大工具输出（> 1MB）不出现在内存态的序列化产物里。

---

## 4. M3 Turn 状态机（本地等价物）

**目标**：在不接 Cloudflare 的前提下，用**本地 SQLite + 单写者**跑通 DO 的语义，验证状态机与恢复。

### In
- `turn` / `run` / `tool_exec` / `interaction` / `event` / `receipt` 六张表（见架构文档 §6）
- Turn 状态机与迁移表（架构文档 §7）
- `WaitCondition` + 到期判定
- `submit/resume/respond/steer/cancel/pause` 的 receipt 幂等
- 事件 cursor + `snapshotRequired`

### Out
- 不做多写者 / 不做 DO
- 不做容器（用一个 in-process runner 代替）

### 任务

| # | 任务 | 文件 |
|---|---|---|
| M3-1 | [P] 新增 `durable` 模块（可独立 crate）：类型 `TurnStatus`/`WaitCondition`/`ToolExecState`/`Receipt` | `crates/agentscope-durable/`（新） |
| M3-2 | [P] SQLite backend：schema、CAS commit、receipt 冲突检测 | 同上 |
| M3-3 | [P] 状态迁移函数（纯逻辑，可单测）+ 非法迁移拒绝 | 同上 |
| M3-4 | [P] `tick(thread)`：取队首 Turn、检查等待条件、驱动一次 Run | 同上 |
| M3-5 | [P] 等待条件：interaction / execution / deadline 三类 | 同上 |
| M3-6 | [P] 事件流：append + cursor + 裁剪 + `snapshotRequired` | 同上 |
| M3-7 | [P] 与 `AgentSession` 对接：一次 tick = 一次 `hydrate → run → commit` | 同上 |
| M3-8 | [P] 契约测试：`backend_contract` / `driver_contract` / `recovery_contract`（一组断言跑遍 InMemory / File / SQLite） | `crates/agentscope-durable/tests/contract/` |
| M3-9 | [P] `LocalDriver` 最小实现：常驻循环 / 定时器 / 唤醒通知，均只调 `tick` 不含业务 | `crates/agentscope-durable/src/driver/local.rs` |

### 验收

1. 两进程同时 `tick` 同一 thread：只有一个推进（SQLite 写事务 + 单写者断言），另一个安全返回。
2. `submit` 用同一 `requestId` 重试 3 次 → 只产生 1 个 Turn，receipt 相同。
3. `respond` 同一 `requestId` 重复 → 第二次拒绝且 cursor 不变。
4. 等价于 Java `HarnessExternalToolProcessRecoveryTest` 的三阶段测试：suspend / resume / duplicate，最终 `tool/result` 恰好 1 条。
5. 崩溃注入：在 `commit` 前后各 kill 一次，重启后状态一致。
6. 同一组契约测试在 `InMemory` / `File` / `SQLite` 三种 backend 上全绿；`LocalDriver` 退化成「手动 tick 一次」后与「常驻跑」结果等价。

### 反向验证

- 去掉 `requestId UNIQUE` → 验收 2 必须 FAILED。
- 去掉 `expectedRevision` 的 CAS → 验收 1 必须 FAILED。
- 把某个业务判断从 `tick` 挪进 `LocalDriver` → `driver_contract` 必须 FAILED（证明 Driver 不含业务）。

---

## 5. M4 容器化 Brain

**目标**：Brain 变成可随时唤起/回收的一次性执行器。

### In
- `agentscope-server` 入口：`POST /run`（异步）+ `/internal/commit` + `/internal/failure`
- 镜像构建（静态链接、< 50MB）
- 容器内 hydrate 预算优化
- DO ↔ 容器的绑定与唤醒
- `EdgeDriver`：alarm / 请求即 tick（**只负责调度时机，不含业务逻辑**）

### Out
- 不拆 Hands（先用进程内 Executor）
- 不做多租户计费

### 任务

| # | 任务 | 文件 |
|---|---|---|
| M4-1 | [P] 新增 `agentscope-server` crate：HTTP 路由 + 幂等 `/run` | `crates/agentscope-server/`（新） |
| M4-2 | [P] run 上下文：`{turnId, runId, revision, checkpointRef, inputs, position}` 解析与校验 | 同上 |
| M4-3 | [P] commit 客户端：`expectedRevision` CAS、批量 facts、失败重试策略 | 同上 |
| M4-4 | [P] 单 Run 上限与安全点：超时主动落 WAITING 并返回 | 同上 |
| M4-5 | [C] 镜像与容器定义（healthcheck、graceful stop、日志到 stdout） | `deploy/` 或 `Dockerfile` |
| M4-6 | [C] DO 绑定：唤醒、`sleepAfter`、热路径策略 | CF 侧（TS 或 Rust/WASM） |
| M4-7 | [C] Queues 投递与幂等消费 | CF 侧 |
| M4-8 | [P] `EdgeDriver` 接口对齐：与 `LocalDriver` 共用同一个 `tick` 签名，禁止在 Driver 内做状态判断 | `crates/agentscope-durable/src/driver/edge.rs` |

### 验收

1. 冷启动到首字节 P95 < 1.5s（本地用容器模拟，记录 `hydrate_ms`）。
2. 同一 `turnId` 重复 `POST /run` 只推进一次（revision 不变时第二次直接返回 202 no-op）。
3. 容器在 commit 前被强杀 → DO 超时后重派，最终结果一致且副作用一次。
4. 旧容器（revision 落后）尝试 commit → 被 CAS 拒绝，不产生脏数据。
5. `LocalAssembly` 与 `EdgeAssembly` 跑同一组契约测试全绿（`backend` / `executor` / `driver` / `recovery`），且不存在「仅在某个 Driver 出现的业务分支」。

---

## 6. M5 Hands

**目标**：长工具与外部作业可以跑在 Brain 之外，跨越 Brain 重启。

### In
- `Executor` trait：`start / query / cancel / capabilities`
- 判定树（查询优先、`idempotentStart` 才重启、其余 `UNKNOWN`）
- `execution_key` 贯穿：DO 先落盘再派发
- 三态落地：`DISPATCHED → COMPLETED → ACCEPTED → APPLIED`

### Out
- 不做 MQ 多传输（先 HTTP 直连）
- 不做沙箱隔离强化

### 任务

| # | 任务 | 文件 |
|---|---|---|
| M5-1 | [P] `Executor` trait + `ExecutionResult`/`Capabilities` | `crates/agentscope-durable/src/executor.rs` |
| M5-2 | [P] `resolve()` 判定树（含 UNKNOWN 收敛），带单测覆盖 6 条分支 | 同上 |
| M5-3 | [P] `InProcessExecutor`（短工具，capabilities 全 false） | 同上 |
| M5-4 | [C] `RemoteExecutor`（HTTP / 容器）+ handle 持久化 | 同上 + `agentscope-server` |
| M5-5 | [P] `tool_exec` 状态机接线：ACCEPTED 与 APPLIED 分离 | `crates/agentscope-durable/src/` |
| M5-6 | [P] 结果按 `call_id` 归位（与 M1-9 对接） | `crates/agentscope-harness` |

### 验收

1. 工具 COMPLETED 后 Brain 崩溃，新 Run 只 apply，不再 dispatch（用副作用文件计数验证 = 1）。
2. provider 不支持 query 且不幂等 → 结果为 `UNKNOWN`，Turn 不得 SUCCEEDED。
3. cancel 请求在 provider 不能确认时 → `UNKNOWN`，不得假装 CANCELLED。

### 反向验证

- 把判定树改成「查不到就重启」→ 验收 2 必须 FAILED。

---

## 7. M6 观测与运维

| # | 任务 | 说明 |
|---|---|---|
| M6-1 | [P] 指标埋点：`turn.waiting_ms`、`run.hydrate_ms`、`container.awake_ms`、`tool_exec.state` 分布、`commit.cas_reject_total` | 统一 `tracing` 字段 |
| M6-2 | [P] 事件 lag 与 cursor 过期计数 | 前端体感 |
| M6-3 | [C] 告警：UNKNOWN 比例、cas_reject 突增、冷启动 P95 | — |
| M6-4 | [C] 成本看板：容器 vCPU·s / DO duration / R2 操作 | — |
| M6-5 | [P] 运维手册：如何手动 reconcile 一个 UNKNOWN、如何强制续跑 | `docs/cloud/CF_RUNBOOK.md` |

---

## 8. 阶段门（gate）

| Gate | 判据 | 不通过时的处理 |
|---|---|---|
| G1（M1 后） | 四项验收全绿 + 全量测试 0 failed | 不带病进 M2 |
| G2（M2 后） | hydrate/commit 幂等 + 写放大下降 ≥ 5x | 先优化 checkpoint 再上云 |
| G3（M3 后） | Java 三阶段测试的 Rust 等价版通过 + backend/driver/recovery 契约测试全绿 | 不上云（DO 语义会在云上放大问题） |
| G4（M4 后） | 冷启动预算达标 + CAS 拒绝生效 + `EdgeDriver` 无业务分支 | 调镜像/分块 |
| G5（M5 后） | 重复副作用 = 0（副作用计数） | 不得对外承诺 exactly-once |

> G3 是最关键的一道门：状态机在本地都跑不顺，接 DO 只会更难查。

---

## 9. 工作量粗估（人日，仅供排期，误差 ±50%）

| 里程碑 | 估算 | 说明 |
|---|---|---|
| M1 | 8–12 | 语义分散在多处，测试量大 |
| M2 | 6–10 | 分块与写放大是主要工作量 |
| M3 | 15–25 | 状态机 + SQLite backend + 崩溃注入测试 |
| M4 | 10–15 | server crate + 容器化 + 预算调优 |
| M5 | 8–12 | Executor + 判定树 + 结果归位 |
| M6 | 4–6 | 指标与手册 |
| 合计 | **51–80** | 不含 Cloudflare 侧（TS）工作与多租户 |

Cloudflare 侧（DO 状态机移植、Worker 网关、Queues、部署流水线）另计：**15–25 人日**。

---

## 10. 风险登记

| 风险 | 影响 | 缓解 |
|---|---|---|
| M1 语义改动面积过大，回归风险 | 高 | 逐项落地，每项独立反向验证；用 feature flag 灰度 |
| checkpoint 分块设计返工 | 中高 | 先做 M2-1 幂等接口，分块策略可替换 |
| 容器冷启动不达标（移动端） | 高 | 热路径 + 镜像瘦身 + 分块 hydrate；必要时同会话保热 |
| `UNKNOWN` 被滥用成「重试」 | 高 | 代码层禁止从 UNKNOWN 直接重派；审计 UNKNOWN 比例 |
| DO 单对象写入/吞吐上限 | 中 | 大对象外置；事件批量提交 |
| 平台 beta 特性变更（Containers） | 中 | 保持 `POST /run` 契约与「无状态容器」假设，必要时换执行器 |
| 一次 Run 超过平台请求时长 | 中 | 安全点落 WAITING + 续 Run |
| 恢复边界被超卖 | 中 | 文档与 API 明确 `TOOL_BOUNDARY` |
| 本地与边缘发展成两套循环（Driver 内长业务） | 高 | 单一 `tick` + 契约测试；Code Review 以「Driver 有无业务分支」为硬标准 |

---

## 11. 里程碑外 backlog

- 多租户配额与计费（DO/Worker 侧）
- Workflows 承载「审批后自动多步」固定流程
- D1 会话目录与跨会话检索
- R2 生命周期策略（旧 checkpoint 归档）
- 客户端 SDK（WS + `Last-Event-ID`）
- 移动端离线队列（本地暂存 → 联网补投）
- 数据主权（jurisdiction）与企业合规

---

## 12. 完成定义（Definition of Done）

一个里程碑完成，必须同时满足：

1. 代码合并，`cargo fmt --all -- --check` 通过；
2. 定向测试 + 全量 `cargo test --workspace -j 2 --no-fail-fast` 0 failed；
3. 关键项完成反向验证并记录在 PR 描述里；
4. 对应能力登记到 gap tracker；
5. 文档更新（架构文档的契约或本文的任务状态）。
6. 若动到端口或 Driver：同一组契约测试在本地实现与边缘实现上均全绿，且无「仅出现在单个 Driver」的业务分支。
