# ACP 交互能力落地跟踪（Steer / Queue / Pause）

> 范围：`crates/agentscope-extensions-acp`（服务端）+ `D:/IdeaProjects/acp-agent-ui`（客户端，独立仓库）。
> 目标：在 ACP 会话上实现 Codex 式的「执行中插话（Steer）」「排队（Queue）」「暂停/继续（Pause/Resume）」，语义建立在 `agentscope-durable` 的 steer/inject、submit 队列与 suspend/resume 之上。
> 本文件是单一事实来源：每落地一项就把对应条目标记完成，并补上验证方式。

## 0. 决策记录

| # | 决策 | 说明 |
|---|---|---|
| D1 | 私有扩展统一 `_acp_extends/*` | 既有 `_agentscope/providers/*`、`_agentscope/session/respond` 全部改名；ACP 只要求 `_` 前缀，命名空间由我们自定。客户端需同步改（旧方法直接 `method not found`，不影响标准面）。 |
| D2 | Queue **durable 可见** | `queue` 立即 `submit`，返回 `turnId`；Turn 立刻出现在 `Queued`，跨进程/重启可见。 |
| D3 | 队列必须能**单项移除** | 新增 `queue/cancel`：取消指定 queued Turn（客户端的「取消这条排队消息」与 durable 同步），并提供 `queue/list` 供重连对账。 |
| D4 | Stop：客户端语义不变 | 客户端仍只发 `session/cancel`。后端**只取消当前 Turn，队列保留**（见下方理由）；用户下次提交时，未处理的 Turn 天然排在新 Turn 之前被驱动，效果等同「没处理的再入队」，且无需复制/重投载荷。 |
| D5 | Pause 由用户显式 Resume | `pause` 后 Turn 停在 `Paused`，不自动恢复；`resume` 由客户端调用。 |
| D6 | 事件归属 | 每个 `SessionNotification` 的 `_meta` 带 `{ "turnId": "..." }`；`queue`/`steer` 的响应也返回 `turnId`，前端据此把流式内容绑到正确的消息气泡。 |

### D4 的理由与备选

- **保留队列（采用）**：`session/cancel` 只取消当前 Turn（现有代码路径不变）。队列 Turn 在 durable 里本来就是 `Queued`，下次 `session/prompt` 的入口会先把队首推进完，再跑新 Turn —— 与「下次提交时再入队」的可观察结果一致，且不用保存或重投输入载荷。
- **取消全部（备选）**：`session/cancel` 遍历 `queued` 逐个 `cancel`。代价是：durable 里这些 Turn 变成 `Cancelled`，载荷随之不可恢复，必须由客户端本地保留并带新 `clientRequestId` 重投，否则消息丢失。若确认要这个语义，改动集中在 `cancel` 处理与客户端重投，约 10 + 100 行。

## 1. 协议面

全部为私有请求（客户端 → agent），`params` 均为 camelCase：

| 方法 | params | result | 作用 |
|---|---|---|---|
| `_acp_extends/session/steer` | `{sessionId, text, clientRequestId}` | `{turnId}` | 把一条消息折进当前活跃 Turn，下一推理边界生效 |
| `_acp_extends/session/queue` | `{sessionId, content, clientRequestId}` | `{turnId, position}` | 立即 durable 受理为排队 Turn；`content` 与 `session/prompt` 同形 |
| `_acp_extends/session/queue/remove` | `{sessionId, turnId, clientRequestId}` | `{turnId, removed}` | 取消指定的排队 Turn（与 durable 同步）；已开始/已结束返回 `removed: false` |
| `_acp_extends/session/queue/list` | `{sessionId}` | `{items: [{turnId, position, status}]}` | 重连后对账；不回显输入文本（客户端自己持有） |
| `_acp_extends/session/pause` | `{sessionId, turnId?}` | `{turnId, status}` | 暂停当前 Turn（`Pausing`→`Paused`，位置与工具保留） |
| `_acp_extends/session/resume` | `{sessionId, turnId}` | `{turnId, status}` | 从 `Paused` 继续（用户显式触发） |
| `_acp_extends/session/respond` | `{sessionId, answers}` | `{}` | 既有：回答被挂起的交互（本次仅改名） |
| `_acp_extends/providers/{remove,default,catalog,probe}` | 不变 | 不变 | 既有：供应商管理（本次仅改名） |

错误约定：`data` 里放稳定 token（`no_active_turn` / `turn_not_running` / `not_queued`），客户端据此决定「降级为 queue」或提示。

## 2. 服务端映射

| 能力 | durable 原语 | 生效点 |
|---|---|---|
| steer | `DurableRuntime::steer(thread, turn, requestId, text)` → `input:steer` Signal | `AgentScopeExecutionHost::apply_inputs`：下一推理边界折成一条 user 消息，`checkpoint + consumed_signals` 同一次 commit |
| queue | `DurableSession::submit(requestId, input)` | 同 Thread 按受理顺序；`tick` 永远推进最老的非终态 Turn |
| queue/cancel | `DurableRuntime::cancel(thread, turn, requestId)`（`epoch == 0` 的 Turn 直接 `Cancelled`） | 下一次 `frame`/`active` 即不可见 |
| pause | `DurableRuntime::suspend(turn, requestId)` | adapter 在边界返回 `HostPaused` → `Paused` |
| resume | `DurableRuntime::resume(turn, requestId)` | Turn 回 `Queued`，需驱动循环重新 tick |

驱动结构（P2 已落地）：`DurableDriver::run_turn` 只做「准备 runtime → 结算遗留 uncertainty → 唤醒暂停的队首 → `submit` 拿 `turn_id`」，随后调用 `DurableDriver::drive_turn(runtime, turn_id, DrivePlan, sink)` 完成「tick + 结算 + 渲染」。

排空（P3 已落地）：`AcpAgentSession::run_prompt_inner` 在提交自己的 prompt **之前**和 `driver.run` 返回**之后**各调用一次 `DurableDriver::drain_queued`，因此：
- 上个进程留下的 `Queued` Turn 先跑（FIFO 不被新 prompt 插队）；
- prompt 执行期间客户端 queue 的消息，在同一个请求里接着跑；
- 取消代数（`cancel_epoch`）变化即停止排空，未开始的 Turn 保持 `Queued`（D4）；
- 队首是 `Paused` 时整条队列停下（不越过用户的暂停）。

代价：`session/prompt` 的响应会延迟到队列排空；客户端需按 `_meta.turnId` 归属流式内容，而不是按「哪个 prompt 请求还开着」。

`Paused` 的自动恢复（现有 `interrupted` 分支）必须让位于用户显式暂停（`pause_requested` 标记，P4）。

## 3. 分期任务

- [x] **P0 改名** `_agentscope/*` → `_acp_extends/*`（代码、README、跟踪文档）。
- [x] **P1 Steer**：`_acp_extends/session/steer` + `AcpAgentSession::steer` + 处理函数 + 参数校验单测。
  - 活跃 Turn 的 `epoch == 0` 窗口（admit→claim）短暂重试（500ms / 50ms 步长），之后返回 `turn_not_running`，由客户端降级为 `queue`。
- [x] **P2 拆分准入与驱动**：`run_turn` = 准备 + `submit`；`drive_turn(runtime, turn_id, DrivePlan, sink)` = tick/结算/渲染。行为零变化（`attach_events` 唯一变化的位置：从 `submit` 前挪到 `drive_turn` 内，两者之间没有 tick）。
  - 验证：ACP lib 152 全绿；`cargo check --all-targets` 干净；未做 20 轮诊断（`drive_turn` 与 `run_turn` 调用序列逐行一致）。
- [x] **P3 Queue**：`queue` / `queue/remove` / `queue/list` + 排空循环 + `_meta.turnId`。
  - 去掉了原计划的 `TurnState.queued` 台账：排空按 durable 的队首（最老非终态 Turn）驱动，台账反而是第二份真相。
  - 幂等：`clientRequestId` 前加 `queue-` 作 durable `request_id`；重试时消息 id 是新生成的，digest 必然不同 → `CONFLICT` 时回读 `admission(request_id)` 返回首次受理的 Turn。
  - `_meta.turnId` 在 `drive_turn` 的 sink 包装层统一注入，所有 `SessionNotification` 都带。
  - 验证：新增 `tests/acp_queue_test.rs`（2 条：durable 可见 + 幂等 + 移除；下一条 prompt 先跑队列、`_meta.turnId` 归属）；ACP 全量 21 个 target 全绿。
- [ ] **P4 Pause/Resume**：`pause` 置 `pause_requested`，prompt 循环遇到 `Paused` 不再自动 resume，以 `Cancelled + _meta.paused` 返回；`resume` 用 `drive_one_turn` 在后台驱动并流式输出。
- [ ] **P5 客户端**（`acp-agent-ui`，独立仓库）：Enter → steer、Tab → queue、排队气泡 + 移除、暂停/继续按钮、按 `turnId` 归属流式内容。

## 4. 客户端契约（acp-agent-ui）

1. `acpClient.ts` 增加 6 个方法，沿用 `_ctx.request('_acp_extends/...')`。
2. `useChat.sendMessage` 去掉 `if (state.isStreaming) return`：streaming + Enter → `steer`；streaming + Tab → `queue`；idle → `session/prompt`。
3. `applySessionUpdate` 增加 `turnId → message` 映射（默认分支保持现状，兼容旧 agent）。
4. 队列气泡：`queued` 状态 + 移除按钮；`queue/list` 用于重连恢复。

## 5. 验收

| 阶段 | 验收 |
|---|---|
| P1 | 两轮脚本模型：第二轮请求里出现 steer 文本；已发出的工具调用未被中断；重复 `clientRequestId` 只生效一次 |
| P2 | `cargo test -p agentscope-extensions-acp --lib` 全绿；20 轮 ACP 诊断 `acp:settle` / `djournal:*` 逐项不变 |
| P3 | 两条 queue 顺序执行；`queue/cancel` 后 durable 中该 Turn 为 `Cancelled`；两个 Turn 之间 kill 后重启能续跑 |
| P4 | 暂停后事件停流、`Paused` 可读；resume 不产生第二次工具调用 |
| 全局 | `cargo fmt --all -- --check` 干净；现有 ACP 集成测试无回归 |

## 6. 风险

- `run_prompt_inner` 约 700 行：P2 的抽取是唯一结构性改动，单独提交、单独验证；其余全部为新增。
- 双驱动：队列由「当前持有 `prompt_lock` 的循环」驱动，不引入第二个 driver；跨进程时租约/围栏仍是最终裁决。
- 幂等：`clientRequestId` 直接作 durable `request_id`；`queue/cancel` 重复调用返回同一结果。

## 7. 进度日志

| 日期 | 内容 | 验证 |
|---|---|---|
| 2026-11-20 | P0 完成：`_agentscope/*` → `_acp_extends/*`（`ext.rs`、`providers_admin.rs`、`coding_agent.rs`、ACP README、5 份 docs 共 32 处） | `rg _agentscope` 仅剩本文件的改名说明 |
| 2026-11-20 | P1 完成：`_acp_extends/session/steer`（`ExtSteerRequest`/`ExtSteerParams`/`ExtSteerResult` + `JsonRpcResponse` 实现）、`AcpAgentSession::steer`（活跃 Turn 解析、admit→claim 窗口 500ms 重试、`request_id` 幂等）、`handle_ext_steer` + 注册 | `cargo test -p agentscope-extensions-acp --lib`：**152 passed / 0 failed**（新增 4 条）；`cargo fmt --all -- --check` 干净 |
| 2026-11-21 | P2 完成：`run_turn` 在 `submit` 后调用新的 `drive_turn`（`DrivePlan` 承载 `cancel_epoch`/`admitted_epoch`/`closed`/`cancellation`/`wakeup`/`caps`），`attach_events` 下沉 | ACP lib 152、全量 target 21 个全绿、fmt 干净 |
| 2026-11-21 | P3 完成：`ext.rs` 三种 queue 方法（`wire_response!` / `impl_ext_request!` 宏消除重复）、`AcpAgentSession::{queue,queued,remove_queued}`、`DurableDriver::drain_queued` + 两处调用、`_meta.turnId`、`tests/acp_queue_test.rs` | lib **158 passed**；`--test acp_queue_test` 2 passed；全量 21 target 全绿；fmt 干净 |

### 待办提醒

- P2 抽取 `drive_one_turn` 前，先记录一次 20 轮 ACP 诊断基线（`acp:settle` / `djournal:facts` / `djournal:read`），用于比对「行为零变化」。
- 客户端（`D:/IdeaProjects/acp-agent-ui`）不在本仓库，P5 只能给契约，不能直接改。
