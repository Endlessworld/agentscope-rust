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
| D4 | Stop：取消全部，下次提交重投 | 客户端仍只发 `session/cancel`；后端取消当前 Turn **并清空队列**（`Queued` Turn 逐个 cancel）。被取消的消息由客户端本地留存，下次提交前用 `_acp_extends/session/queue` 按原顺序重投（新 `clientRequestId`）。 |
| D5 | Pause 由用户显式 Resume | `pause` 后 Turn 停在 `Paused`，不自动恢复；`resume` 由客户端调用。 |
| D6 | 事件归属 | 每个 `SessionNotification` 的 `_meta` 带 `{ "turnId": "..." }`；`queue`/`steer` 的响应也返回 `turnId`，前端据此把流式内容绑到正确的消息气泡。 |

### D4 的理由与备选

- **取消全部（采用，2026-11-21 用户确认）**：Stop 是「都停下」。保留队列会让用户已经放弃的内容在下一次 prompt 时悄悄跑起来，而且 `Queued` 混在 `queue/list` 里，客户端分不清「还在排队」和「已被取消」。代价是 durable 里这些 Turn 变 `Cancelled`、载荷不可恢复，所以文本必须在客户端留存并重投：前端把 `state='queued'` 的气泡改判 `parked`（「待重发」），下次 `sendMessage` 前逐条 `queue` 回来，再发自己的 prompt —— 顺序仍是 FIFO。
- **保留队列（原方案，已废弃）**：`session/cancel` 只取消当前 Turn。优点是不用重投载荷；缺点是队列在用户按下停止之后依然存在（durable 可见），下一次 prompt 会把它们跑起来，与「停止」的直觉冲突。

## 1. 协议面

全部为私有请求（客户端 → agent），`params` 均为 camelCase：

| 方法 | params | result | 作用 |
|---|---|---|---|
| `_acp_extends/session/steer` | `{sessionId, text, clientRequestId}` | `{turnId}` | 把一条消息折进当前活跃 Turn，下一推理边界生效 |
| `_acp_extends/session/queue` | `{sessionId, content, clientRequestId}` | `{turnId, position}` | 立即 durable 受理为排队 Turn；`content` 与 `session/prompt` 同形 |
| `_acp_extends/session/queue/remove` | `{sessionId, turnId, clientRequestId}` | `{turnId, removed}` | 取消指定的排队 Turn（与 durable 同步）；已开始/已结束返回 `removed: false` |
| `_acp_extends/session/queue/list` | `{sessionId}` | `{items: [{turnId, position, status}]}` | 重连后对账；不回显输入文本（客户端自己持有） |
| `_acp_extends/session/pause` | `{sessionId, clientRequestId}` | `{turnId, status}` | 暂停当前 Turn（`Pausing`→`Paused`，位置与工具保留） |
| `_acp_extends/session/resume` | `{sessionId, turnId, clientRequestId}` | `{turnId, stopReason}` | 从 `Paused` 继续（用户显式触发）；请求保持到该 Turn 结束并流式输出 |
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
- [x] **P4 Pause/Resume**：`pause` 先置 `TurnState.pause_requested` 再 `suspend`；驱动循环把 `Paused` 当作「本 prompt 的结算点」，命中 `interrupted` 且 `pause_requested` 时**不再自动 resume**，以 `Cancelled` 回答原 prompt；`resume` 在请求内（spawn 任务 + 同一 `prompt_lock`）`resume` 后驱动该 Turn，流式输出并返回 `stopReason`。
  - 顺带修掉一个既有缺口：轮询循环原先只把**终态**当结算点，`Paused` 会让 `session/prompt` 永远挂着；现在 `Paused` 也会离开循环，`interrupted` 分支因此第一次真正可达。
  - `Pausing`（暂停请求已发出、执行体尚未到边界）并入 `Running` 的静默看门狗，避免挂死的 provider 让暂停永远观察不到。
  - 验证：`tests/acp_pause_test.rs` 2 条（排队 Turn 暂停后不跑、resume 只跑一次且带 `_meta.turnId`；运行中的 Turn 被暂停后 prompt 以 `Cancelled` 返回且 Turn 停在 `paused`）；ACP 全量 22 个 target 全绿。
- [x] **P5 客户端**（`acp-agent-ui`，独立仓库，改动在工作树未提交）：Enter → steer、Tab → queue、排队气泡 + 移除、暂停/继续按钮、按 `turnId` 归属流式内容。
  - `acpClient.ts`：4 处 `_agentscope/providers/*` 跟改名成 `_acp_extends/providers/*`；`session/update` 处理器把信封的 `_meta.turnId` 透传给 `onSessionUpdate`；新增 `steerSession` / `queueMessage` / `listQueue` / `removeQueued` / `pauseSession` / `resumeTurn`。
  - `useChat.ts`：`assistantForTurn(state, turnId)` 按 Turn 归位（占位壳绑定、排队气泡转生成中、上一轮收尾）；`sendMessage(content, files, mode?)` 在执行中改为 steer/queue 而不再静默丢弃；`pauseTurn` / `resumeTurn` / `removeQueued` / `refreshQueue`；`finishPrompt` 按 `pausedTurnId` 把输出结算成 `paused` 而不是 `done`。
  - `ChatContainer.vue`：document 捕获阶段拦截输入框的 Enter/Tab（输入框自己的 Enter 提交在冒泡阶段，晚一步就会把消息丢掉）；Stop 旁新增暂停按钮；暂停后显示「继续」条；提交链路带一次性的 `pendingSubmitMode`。
  - `MessageList.vue` / `MessageItem.vue`：`onRemoveQueued` 下发；用户气泡显示「排队中 #n / 撤回」「已插话」「已暂停」；`v-memo` 带上 `state`/`queuePosition`。
  - `types/chat.ts`：`ChatMessage` 增加 `turnId` / `queuePosition` / `state`；`prompt-input/types.ts` 的 `submitForm` 返回类型改成 `Promise<void>`（实现本来就是 async）。

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
| 2026-11-21 | P4 完成：`pause`/`resume` 两种扩展、`TurnState.pause_requested`、`DrivePlan.pause_requested`、驱动循环把 `Paused` 当结算点、`resume_turn`（同一 `prompt_lock` 内驱动）、`tests/acp_pause_test.rs` | `--test acp_pause_test` 2 passed；全量 22 target 全绿；fmt 干净 |
| 2026-11-21 | P5 完成（`acp-agent-ui`，未提交）：providers 改名对齐、`_meta.turnId` 透传、6 个扩展方法、`assistantForTurn` 按 Turn 归位、执行中 Enter=steer / Tab=queue、排队气泡与撤回、暂停/继续 | `pnpm typecheck` 干净；`pnpm build`（typecheck + vite build）EXIT=0 |
| 2026-11-21 | 排队消息不再平铺进对话流：输入框上方「队列」入口 chip + 右侧面板 `queue` 分区（`QueueSection.vue`）展示详情与撤回；Turn 开始时 `promoteQueued` 把消息搬到对话流末尾 | `pnpm typecheck` / `pnpm build` EXIT=0 |
| 2026-11-21 | D4 落地：Stop 取消当前 Turn + 整个队列（`cancel_queued`），客户端把消息标 `parked` 并在下次提交前重投；顺带修掉 durable `cancel_effects` 误把下一条排队 Turn claim 成 running 的缺陷 | durable / ACP 全套绿；前端 `pnpm typecheck` / `pnpm build` EXIT=0 |

### 待办提醒

- P2 抽取 `drive_one_turn` 前，先记录一次 20 轮 ACP 诊断基线（`acp:settle` / `djournal:facts` / `djournal:read`），用于比对「行为零变化」。
- `acp-agent-ui` 是独立仓库：P5 的改动已落在那边的工作树里但**未提交**（该仓库还有其他人未提交的改动），提交与端到端手测由用户决定。
- `queue/list` 只回 `{turnId, position, status}`，不回消息正文：重连时客户端只能用本地留存的气泡对齐位置/状态，正文不会从后端重建。

## 8. 语义修正（2026-11-21，暂停 / 插话 / 队列对齐）

用户手测发现的三处偏差与修正：

| 现象 | 根因 | 修正 |
|---|---|---|
| 先回 `{"status":"paused"}` 又回 `{"stopReason":"cancelled"}`，看着像被取消 | ACP 没有 `paused` 停止原因，暂停只能借 `cancelled` 结束 prompt，客户端只能靠本地标记猜 | 暂停结束时 `PromptResponse._meta.paused = true`；客户端以它为准，不再把暂停当取消 |
| 暂停期间模型消息丢掉 | 适配器一看到 `Pausing` 就切断模型流，半条 assistant 消息没进 checkpoint，继续后模型重写一遍 | 暂停改为**让当前模型消息收尾**再在下一个边界停：流还在出内容就不切，只有连续 `PAUSE_GRACE = 3s` 无输出（provider 卡死）才切断 |
| 暂停中点继续 / 回车，界面停在暂停态 | 暂停走的是取消路径，暂停期间再发 `session/prompt` 会另起一个 Turn | 暂停中回车 = **插话 + 继续**：`steer` 折进暂停的 Turn，再 `resume` 把它跑起来，界面同步退出暂停态 |

语义边界（写清楚，避免再次混淆）：

- **插话（steer）**：不新建 Turn、不排队；文本作为 Signal 折进当前活跃 Turn，在**下一个推理边界**进上下文。它不会打断正在跑的模型调用或工具执行，那是「暂停」的事：暂停 → 插话 → 继续。
- **队列（queue）**：新建 Turn，`Queued` 等当前 Turn 结束后自动跑。
- 暂停落在 finalization（答复已生成完）时 `resume` 只补 finalization，不再调模型；这期间插的话不会进上下文（当前实现的已知边界）。

前端配套：

- 插话不再挂在小气泡下面，而是像上下文压缩卡片一样在对话流里留一张卡片（`ChatMessage.kind = 'steer'`，正文即插话内容，随消息持久化）；卡片之后的增量为同一 Turn 的新气泡，读起来是「答一半 → 已插话 → 接着答」。
- 暂停先进入「正在暂停…」（`pausing`），`session/prompt` 真正带 `_meta.paused` 回来才显示「已暂停 · 继续」，按钮与实际状态不再打架。
- 验证：`cargo test -p agentscope-durable-adapter-agentscope`、`cargo test -p agentscope-extensions-acp` 全绿；`cargo fmt --all -- --check` 干净；前端 `pnpm typecheck` / `pnpm build` 均 EXIT=0。

## 9. 排队消息的呈现（2026-11-21）

现象：排队气泡留在对话流里，出现「用户消息 1、用户消息 2、助手消息 1、助手消息 2」——第二个用户消息在前一轮答复之前就落了位，按 `turnId` 归属也救不回来。

结论：**排队消息不属于对话流**，它是「待执行的输入」，和子代理 / 计划一样属于状态面板。

| 位置 | 形态 |
|---|---|
| 输入框上方 | 「队列 N」入口 chip（复用 `statusChips`，点击 `panel.openTab('queue')`），与子代理 / 计划 / 上下文同一套观感 |
| 右侧面板 | 新增 `queue` 分区（`components/chat/panel/QueueSection.vue`）：按 `queuePosition` 列出正文与附件，每条带「撤回」（`removeQueued(turnId)`） |
| 对话流 | 不渲染 `state === 'queued'` 的消息（`MessageList.visibleMessages`），原来的「排队中 #n / 撤回」小气泡与 `onRemoveQueued` 通道一并删除 |

时序：排队消息创建时仍写进 `state.messages`（`state='queued'`，随会话持久化，刷新后队列不丢），只是不渲染；这一轮真正开始时（首个带 `turnId` 的 `session/update`）`promoteQueued` 把它从原位置搬到 `messages` 末尾并改判 `streaming`，紧接着的助手气泡就排在它后面，顺序自然是「用户 → 助手」。`queue/remove` 返回 `removed: false`（已开始执行）时同样搬到末尾，不再堆在队列里。

后端契约未变：`_acp_extends/session/queue`、`.../queue/list`、`.../queue/remove` 仍是唯一事实来源；`queue/list` 只回 `{turnId, position, status}`，正文靠本地留存（与 `user_message_chunk` 回放的既有约定一致）。

## 10. Stop = 取消整个队列（2026-11-21 落地）

| 侧 | 改动 |
|---|---|
| durable | `DurableRuntime::cancel` 只有在 Turn 已经开跑（`status != Queued`）时才走 `cancel_effects`。此前 `cancel_effects` 会先 `claim` 线程，而 claim 落在**下一个**未完成 Turn 上：删除/取消一条排队消息会把排在它后面的那条点亮成 `running`（无 owner、无人驱动），再取消它就成了永远不落地的 `cancelling`。回归测试：`cancel_waiting_test::cancelling_a_queued_turn_leaves_the_next_one_queued`。 |
| ACP | `AcpAgentSession::cancel_queued()`（durable）分页遍历 `turns`，把每个 `Queued` 逐个 `cancel`（请求 id `stop-<turnId>`，可重放）；`session/cancel` 处理器在 `session.cancel()`（挪 epoch 停当前 Turn）之后 **await** 它，保证紧接着的下一次 `queue` 重投不会和还在飞的取消打架。非 durable 构建返回 0。 |
| 客户端 | `cancel()` 把本地 `queued` 气泡改判 `parked` 并清掉 `turnId`/`queuePosition`；`state.messages` 仍持久化，刷新后依旧可见。`sendMessage` 开头调 `requeueParked()`：按原顺序 `acp.queueMessage(...)` 重投（附件句柄放在内存 Map 里，重开页面后只能重投正文），成功后恢复 `queued`。`removeQueued` 改为按**消息 id**：`parked` 的只删本地，`queued` 的才走 `queue/remove`。队列面板把 `parked` 标成「待重发」。 |

验收：`cargo test -p agentscope-durable`（82 lib + 6 组集成，含新回归）、`cargo test -p agentscope-extensions-acp`（lib 161 + 22 targets，含 `stop_cancels_the_whole_queue_without_running_it`）全绿；`pnpm typecheck` / `pnpm build` EXIT=0。
