# ACP 智能体稳定性调查与修复

> 调查基线：`f1674b1`（2026-10-27）。修复状态截至 **2026-10-28（第四轮，含 P2 全部推进）**。
> 本文只保留**最新状态**：历轮回执已合并进 §0 状态总表与 §2 逐项说明，不再单列。

---

## 0. 结论与当前状态

### 0.1 结论

1. **`Session writer ownership expired or fenced` 的直接成因已消除。** 原链条是：`acquire` 先拿 120s 租约 → 全量重放 journal（可能数分钟）→ 才启动心跳 → 第一次 `flush` 续租时命中“租约已过期” → 被当成永久 fencing。现在：心跳前移到 `acquire` 之后、同 owner/epoch 过期可原地续租、transient 失败退避重试、`Stale committed prefix` 会重基而不是判死。
2. **`The previous turn is still open` 的错误路径已移除。** dispatcher 对孤儿 open turn 自动关闭，`submit` 前按状态路由（可恢复 → 先 resume；suspended/有不确定工具 → 关闭），不再 `inbox.reject("turn_unfinished")` 丢掉用户消息。
3. **“错误不再以那种文案出现”≠“任意中断后可继续”。** 仍然做不到的三件事：冷启动重放仍然慢（journal 无 segment/compaction，P0-2）、journal 仍落在工程目录 `agents/`（P0-11）、模型流的半截输出重启后不可恢复（P1-2）。
4. **剩余最大风险是 journal 的体量与投影成本**（P0-2 / P1-11）：一 batch 一 commit 文件、无快照/回收，长会话冷启动是 O(全部 commit)，也是磁盘与重放开销的主源。

### 0.2 状态总表

| 编号 | 问题（一句话） | 状态 | 落地方式 / 原因 |
|---|---|---|---|
| P0-1 | 租约在重放期间过期：心跳启动时机错误 | ✅ 已修 | `acquire` 后立即启动心跳；replay 失败先停心跳再 release |
| P0-2 | 每 chunk 一 commit，journal 爆炸 | ◐ 部分 | `model/chunk` 已合并（4KiB / 64 delta / 1s）；**无 segment / compaction / retention** |
| P0-3 | 租约过期 = 永久 fence | ✅ 已修 | `ensure_owned` 只在 owner/epoch 变化或 sealed 时失败；同 owner 过期可续租 |
| P0-4 | 心跳一次失败就永久停止 | ✅ 已修 | transient 失败指数退避重试；只有 fenced/sealed 才停止 |
| P0-5 | `flush` 的 clone-then-clear 丢并发 append | ✅ 已修 | `flush_lock` 串行化 + `InflightBatch` 预留 seq，成功后只确认该 batch |
| P0-6 | 任何 commit 错误都变 sticky | ✅ 已修 | `is_retryable_session_error` 分 transient/fatal；transient 重试同 batch（6 次退避）；stale 走重基（P1-15） |
| P0-7 | 工具执行前无 checkpoint、无 `tool/dispatch` | ✅ 已修 | acting 前 `before_acting` checkpoint + 每个 call 一条 `tool/dispatch`；工具结果落 context 后 `after_acting` checkpoint；投影把 `action/start` 计为 uncertain |
| P0-8 | 工具执行 detached，取消不停工具 | ✅ 已修 | `ActingTaskGuard` + `ActingStream`：stream drop 时置 cancel 并 abort；`run_acting_calls` 每轮检查 cancel / `check_interrupted` |
| P0-9 | 关闭/退出没有 drain | ✅ 已修 | `run_stdio` 返回后（含错误）`drain_sessions("transport closed")`；`run_stream_with_extensions` 也持有 `ExecutionGuard` |
| P0-10 | 文件存储 CAS 非跨进程原子、无 fsync | ✅ 已修 | `Filesystem::sync` + `LocalFilesystem::sync_all`；可变值 CAS 加 `{path}.lock` 文件锁（30s 陈旧可破）；immutable commit/blob 不写 `.version` |
| P0-11 | journal 默认写在工程目录 `agents/` | ⬜ 未做 | 需路径迁移 + 旧数据 fallback（否则老会话“消失”），属数据面变更 |
| P0-12 | 大 checkpoint 撞 `MAX_PENDING_BYTES` | ✅ 已修 | `checkpoint_now` 先 flush 再独占提交；`admits_bytes` 允许空缓冲下的单个大 payload（journal 已把 >64KiB 溢写 blob） |
| P0-13 | `turn_unfinished` 直接拒绝新 prompt | ✅ 已修 | dispatcher 自动关闭孤儿 turn；`submit` 前按状态路由，不再 reject 用户消息 |
| P1-1 | observation 失败被降级成可重试 tool result | ✅ 已修 | 新增 `Error::ActionDispatch`（start 失败 = 工具未执行，可重试）与 `Error::ActionObservation`（end 失败 = run 级错误，journal 保持 uncertain） |
| P1-2 | 模型流部分输出崩溃后不可恢复 | ⬜ 未做 | 需先定策略（丢弃 partial 还是继续），且要在 projection 里跟踪 model call 状态机 |
| P1-3 | 每次 prompt 先 abandon，重启后无法继续 | ✅ 已修 | `prepare_open_turn_before_submit`：可恢复的 interrupted turn 先排一个 `resume`；ACP 侧改 `stop_active_run`（只打断自己拥有的 run） |
| P1-4 | prompt 没有稳定幂等键 | ✅ 已修 | 键 = `prompt-<文本 SHA-256 前 16 hex>-<受理时间戳ms>`；**跨进程去重仍受协议限制**（见 §0.3） |
| P1-5 | cancel 是 session 级共享 flag | ✅ 已修 | `cancel: AtomicBool` → `cancel_epoch: AtomicU64`，每个 prompt 只认自己的 epoch |
| P1-6 | 权限/elicitation/存储等待无超时 | ✅ 已修 | `INTERACTION_TIMEOUT = 600s` 包裹 permission / elicitation，超时映射为拒绝；prompt lock 获取可取消 |
| P1-7 | 会话注册表 load/resume/delete/evict 语义不完整 | ✅ 已修 | `session/list` 合并内存注册表 + `list_session_keys`（durable store）；load/resume 校验存在性；close/delete 先 `cancel_and_wait` |
| P1-8 | panic / mutex poisoning 卡死 dispatcher | ✅ 已修 | dispatcher 循环与 ACP prompt 任务 `catch_unwind`；poisoned mutex 用 `lock_state` 恢复 |
| P1-9 | inbox 租约过长 + 重试过短 | ✅ 已修 | `INBOX_LEASE` 120s → 10s，`ACCEPT_ATTEMPTS` 100 → 400（80ms 退避）；`launch` 失败保留 queued |
| P1-10 | suspended / external execution 不可恢复 | ✅ 已修 | 私有扩展方法 `_agentscope/session/respond`（ACP 扩展方法须 `_` 前缀）→ `AcpAgentSession::respond` |
| P1-11 | 投影/缓存无限增长 | ⬜ 未做 | 正解是 P0-2 的快照；直接裁剪会丢 `started`/`inputs` 的去重能力 |
| P1-12 | 事件 channel 无界 | ✅ 已修 | 新增 `event_queue::EventQueue`（容量 4096，`Mutex` + `Notify`）：满则丢弃新事件并计数，`AGENT_RESULT` 永不丢；丢弃过 delta 时在 turn 结束前告知客户端 |
| P1-13 | `model/chunk` 与模型流强耦合 | ✅ 已修 | `MergedChunk` 合并（4KiB / 64 delta / 1s），单次 commit 失败不再逐 token 截断 |
| P1-14 | 损坏/未知事件让 session 永久打不开 | ✅ 已修 | `visit` 改为「重放可读后缀」；`required=false` 的坏事件跳过；inbox 未知事件忽略；只容忍数据类错误，IO 仍上报 |
| P1-15 | `Stale committed prefix` 无重基策略 | ✅ 已修 | `flush` 判定 stale 后先 `renew`，再按新 head 重编号未提交事件 + 换新 batch id（最多 3 次） |
| P2-1…P2-20 | 可观测性/次级风险（20 项） | ◐ 大部分已修 | 15 项已修、5 项部分（P2-3/P2-10/P2-13/P2-16/P2-20），见 §2.3 |

**统计：** P0 12/13 已修（P0-2 部分、P0-11 未做）；P1 12/15 已修（P1-2 / P1-11 未做）；P2 15 已修 / 5 部分 / 0 未做。

### 0.3 仍未完成（及原因）

| 项 | 为什么还没做 |
|---|---|
| **P0-2** journal segment / compaction / retention | 一 batch 一 commit 文件且无段合并，冷启动仍要扫全部 commit。真正修法是「分段落盘 + 写入折叠后的 projection 快照 + 删除已被快照覆盖的 commit」，涉及 `SessionLog::commit/visit/read_after` 全链路与所有存储后端，缺少崩溃点设计时改不动 |
| **P0-11** journal 默认路径在 `agents/` | 改成用户目录需要路径迁移 + 旧数据 fallback（否则老用户的会话全部“消失”），是数据面变更，需单独决策与迁移脚本 |
| **P1-2** 模型流 partial 恢复 | 需要先定语义：客户端已看到半截文本时，重启是丢弃 partial（回到 checkpoint）还是继续（保留 partial 让模型续写）。两种都要改 projection 与 model call 状态机 |
| **P1-11** 投影缓存上限 | 正解是 P0-2 的快照，否则裁剪会丢 `started`/`inputs` 的去重能力 |
| **P1-4** 跨进程幂等 | ACP 的 `PromptRequest` 不携带客户端请求 id，进程重启后的重试仍会产生新 turn。要真正去重需要协议层 request id 或自定义 metadata + journal 持久化 `requestId -> turnId` |
| **P1-14** repair / quarantine 工具 | 现在坏段只是不再阻断打开，没有显式的「截断到指定 seq / 隔离坏 commit」命令；隔离需要写侧 API 与目录清单（`list_paths` 目前约定 recovery 不使用） |
| **P2** 其余项 | P2-3（`Drop` 不能 await）、P2-13（blob 懒加载要改 `visit` 签名）、P2-16（chunk 采样）为需先定设计的部分项；P2-10 仅剩其它 crate 的 `lock().unwrap()`；P2-20 的 core `delta_tx` 未改 |
| **未补的测试** | 两进程 SQLite / 文件 CAS（需独立进程）、`session/list` 重启可见的端到端、`_agentscope/session/respond` 端到端（需挂起工具的 run） |

---

## 1. 现场证据

### 1.1 错误链路

```text
ACP PromptRequest
  -> AcpAgentSession::run_prompt_inner
  -> abandon_unfinished / stop_active_run        (全量扫 journal)
  -> session.submit_with_key_msg                 (harness dispatcher)
  -> launch -> SessionRecorder::open
        acquire 120s lease
        SessionProjection::read + log.scan + transcript   << 旧实现：此处可能已超 120s，心跳尚未 start
  -> recorder.start -> flush -> log.renew -> ensure_owned
  -> Err("session log: Session writer ownership expired or fenced")
```

模型流中的第二条路径：`SessionModels::Recorded::stream` → `recorder.record("model/chunk")` → `flush` → `ensure_owned`，同样是这个错误；`run_stream` 再包成 `Error::Agent`，所以用户看到 `Agent error: session log: ...`。工具回写时失败则显示 `Unable to persist action observation; do not replay the action automatically: ...`。

### 1.2 数据证据（本机 `agents/`）

| 项目 | 数值 |
|---|---:|
| `agents/` 总文件数 | 934,123 |
| `agents/` 总大小 | 约 5,774 MB |
| 最大会话 `commits/` 文件数 | 184,530 |
| 最大会话 head `seq` | 93,234 |
| 抽样解析成功的事件类型 | 932 条中 920 条是 `model/chunk` |

`head.json` 示例（已脱敏）：

```json
{"seq":93234,"epoch":1,"owner":"<run-id>","leaseUntil":1793033778682,"commitId":"<hash>"}
```

该 `leaseUntil` 对应 `2026-10-26 16:56:18Z`，日志在 `16:53:04` 左右中断在半轮工具调用，说明进程没走正常 `finish/close`，owner 留在 head 上。

### 1.3 日志证据

```text
WARN session writer lease renewal failed: session log: Session writer ownership expired or fenced
WARN recording the run's terminal facts failed ... error=session log: Failed to commit session prefix: session log: Stale committed prefix
WARN recording the run's terminal facts failed ... error=session log: Session pending buffer exhausted
WARN failed to close the cancelled turn; the next prompt may be rejected ... Session writer is still busy
```

分别对应：心跳失败、`flush` 的 stale 前缀、pending 上限、取消后抢不到 writer。这四条对应的问题均已处理（P0-4 / P1-15 / P0-12 / P1-9）。

---

## 2. 问题与修复（逐项）

> 每项格式：**位置** → **根因/问题** → **影响** → **落地 / 修复方案**。已修项只写落地方式，未修项保留原方案。

### 2.1 P0（阻断长期稳定运行）

#### P0-1 租约在重放期间过期 ✅

- **位置：** `core/src/session/recorder.rs`（`open_with_lease_and_baseline`）、`journal.rs`（`ensure_owned`）
- **根因：** `acquire` 先拿租约，再做 `SessionProjection::read` + `log.scan` + `transcript` 全量重放，最后才构造 recorder 并启动心跳。重放超过租约 → `start()` 的第一次 `flush` 必然失败。
- **影响：** 冷启动/重启后第一条 prompt 直接 `Agent error: session log: ...`，用户消息丢失。
- **落地：** `acquire` 成功后立即启动心跳（replay 全程受租约保护）；replay 失败时先停心跳再 `release`，不再泄漏租约。

#### P0-2 每个模型 chunk 一个 commit，journal 爆炸 ◐

- **位置：** `core/src/session/models.rs`（chunk 记录）、`recorder.rs`（`record = append + flush`）、`journal.rs`（一 batch 一 commit）、`harness/src/agent/session/storage.rs`（每个 commit 一个 `.version` 文件）
- **根因：** 每个流式 chunk 调 `recorder.record("model/chunk")`，每次都 renew + commit，于是「一个 token ≈ 2 个文件 + 一次 head CAS」。
- **影响：** 冷启动重放从秒级变成分钟级（直接触发 P0-1）；磁盘/NTFS 元数据/备份被拖垮。
- **已做：** `MergedChunk` 合并（4KiB / 64 delta / 1s），`model/chunk` 数量从“一 token 一条”降到“每批一条”；immutable commit/blob 不再写 `.version`。
- **未做：** segment 文件 / journal compaction / 快照 + 旧 commit 回收 / 单会话硬上限。目标：10 万事件 session 冷启动 < 10s、文件数 < 1,000。

#### P0-3 租约过期 = 永久 fence ✅

- **位置：** `journal.rs`（`ensure_owned` / `release` / `update_owner`）
- **根因：** 旧 `ensure_owned` 把 `head.lease_until <= now` 当成失去所有权，`release` 也走同一个检查，过期后连自己的 owner 都清不掉。
- **影响：** 机器休眠、NTP 校时、VM 时钟跳变、调试器暂停、长 GC 都会报错，即使没有第二个 writer。
- **落地：** 只有 `epoch/owner` 不匹配或 writer 被 sealed 才算失去所有权；同 owner/epoch 过期可原地 `renew`；`release` 只看 owner/epoch。

#### P0-4 心跳一次失败就永久停止 ✅

- **位置：** `recorder.rs`（`LeaseHeartbeat::start`）
- **根因：** `sleep -> renew -> Err => warn + return`，没有重试、没有分类、没有重启。
- **影响：** 一次瞬时存储错误/时钟跳变后后台续租永久消失，下一次长模型流就撞 P0-3。
- **落地：** transient 失败指数退避重试至租约截止；只有 fenced/sealed（不可重试）才停止；日志带 run/attempt。

#### P0-5 `flush` 的 clone-then-clear 丢并发 append ✅

- **位置：** `recorder.rs`（`flush`）
- **根因：** 锁内 clone `pending` → 释放锁 commit → 成功后再锁内 `pending.clear()`，期间 `append` 的新事件被一起清掉，seq 随后被复用。
- **影响：** 静默丢消息/工具结果，journal 与内存分叉，恢复时出现缺口。
- **落地：** `flush_lock` 串行化 flush；`InflightBatch` 把本次要提交的事件与 seq 预留出来，`append` 的 seq 叠加 inflight 长度；成功后只确认该 batch。

#### P0-6 任何 commit 错误都变成 sticky failure ✅

- **位置：** `recorder.rs`（`flush` 的 `Err` 分支、`check`）
- **根因：** 无差别写 `guard.failure`，之后所有 `append` 都报 `Session persistence failed`。
- **影响：** 一次 IO/CAS 抖动就让整轮失败，用户重发。
- **落地：** `is_retryable_session_error` 分 transient / fatal；transient 重试同一 batch（6 次，100ms 起指数退避）；`Stale committed prefix` 单独走重基（P1-15）。

#### P0-7 工具执行前没有 checkpoint，也没有 `tool/dispatch` 生产写入 ✅

- **位置：** `core/src/agent/react_agent.rs`（acting 前/后）、`core/src/session/views.rs`（`uncertain_tool_calls`）
- **根因：** checkpoint 只发生在下一次 reasoning 的 `prepare_model`，而工具在那之前已执行；`SessionProjection` 也不把 `action/start` 当 uncertain。
- **影响：** 工具执行中崩溃 → assistant tool_use 丢失、`uncertain_tool_calls` 为空 → 不补 interrupted 结果，模型可能重复调用工具。
- **落地：** acting 前 `checkpoint_now(state, "before_acting")` + 每个 actionable call 一条 `tool/dispatch`；工具结果写入 context 后立即 `checkpoint_now(state, "after_acting")`；投影把 `action/start` 计为 uncertain，`action/end` / `tool/result` 清除。

#### P0-8 工具执行 detached，取消不停工具 ✅

- **位置：** `core/src/agent/react_agent.rs`（`acting_phase` / `run_acting_calls`）
- **根因：** 有任意 `on_acting` 中间件时（ACP 默认有 `AgentTraceMiddleware`）工具走 `tokio::spawn` 分支，`JoinHandle` 被丢弃；`run_acting_calls` 在工具之间也不检查中断。
- **影响：** 按 Stop 后工具继续写文件/跑 shell；recorder 已关闭后 `action/end` 写不进去。
- **落地：** `ActingTaskGuard` + `ActingStream`，stream drop 时置 cancel 并 abort 任务；工具循环每轮检查 cancel 与 `check_interrupted`。

#### P0-9 关闭/退出没有 drain ✅

- **位置：** `extensions-acp/src/acp_harness_agent.rs`、`core/src/agent/react_agent.rs`（`ExecutionGuard`）
- **根因：** `ExecutionGuard` 只注册在 `call_with_format_and_extensions`，ACP 走的是 `stream_events_with_extensions`，计数恒为 0；`run_stdio` 返回后 `main` 直接结束，`Drop` 里的 `spawn(release)` 来不及跑。
- **影响：** run 被硬杀、租约不释放、head 留 stale owner，下一次启动等 30s 并再次重放。
- **落地：** `run_stdio` 返回后（含错误）先 `drain_sessions("transport closed")`：interrupt → 等 `is_running` / `active_request_count` 归零（≤20s）→ `cancel` + `close`；信号路径复用同一函数，仅它负责 `exit(130)`。流式路径也持有 `ExecutionGuard`。

#### P0-10 文件存储 CAS 不是跨进程原子，也没有 fsync ✅

- **位置：** `harness/src/agent/session/storage.rs`、`harness/src/filesystem/local.rs`
- **根因：** 互斥只有进程内 `Mutex<HashMap>`；value 与 `.version` 是两次写；rename 前后无 fsync。
- **影响：** 两个 ACP 进程/两个 IDE 窗口同 session 时丢 commit、head 回退、`Stale committed prefix`；断电后已确认的 turn 消失。
- **落地：** `Filesystem::sync`（默认 no-op）+ `LocalFilesystem::sync_all`；可变值（head/元数据/cursor）的 CAS 额外持 `{path}.lock` 文件锁（create-if-absent，30s 后视为陈旧可破，`list_paths` 过滤 `.lock`）；`replace` 先 sync 临时文件再 rename；immutable commit/blob 不写 `.version`。

#### P0-11 journal 默认写在工程目录 `agents/` ⬜

- **位置：** `harness/src/agent/session/storage.rs`（`WorkspaceSessionLogStore::open`）、`core/src/session/key.rs`（`storage_path`）
- **影响：** 模型的文件工具可以直接读/改/删 journal；工程目录被百万文件/GB 级数据污染；会话明文内容落在工作区。
- **修复方案：** journal 改到独立数据目录（`~/.agentscope/sessions/<workspace-hash>/` 或 `%LOCALAPPDATA%`）；若必须放 cwd，至少放 `.agentscope/sessions` 并在文件工具/索引中排除；提供 retention/GC 与启动时输出 journal 根路径。**未做原因：需路径迁移 + 旧数据 fallback。**

#### P0-12 大 checkpoint 撞 `MAX_PENDING_BYTES` ✅

- **位置：** `core/src/session/recorder.rs`（`MAX_PENDING_BYTES`、`append`、`capture_state`）
- **根因：** `capture_state` 把完整 `AgentState` 当一条 `state/checkpoint` 追加，长会话很容易超限；旧上限 16MiB。
- **影响：** `finish` 失败 → turn 被报 interrupted；日志出现 `Session pending buffer exhausted`。
- **落地：** `checkpoint_now` 先 `flush` 再 `capture_state`，让大 checkpoint 独占一个 commit（journal 已把 >64KiB payload 溢写 blob）；`admits_bytes` 在缓冲区为空时接受单个超大 payload（代价是一次大分配，而不是整轮丢失）。

#### P0-13 `turn_unfinished`：上一轮未终结时 ACP 直接拒绝新 prompt ✅

- **位置：** `harness/src/agent/session/session.rs`（`submit` / `abandon_unfinished`）、`extensions-acp/src/session.rs`
- **根因：** 取消/中断后 turn 仍是 `interrupted`/`suspended`（按设计非 terminal），ACP 前的 `abandon_unfinished` 只是 best-effort，失败后 dispatcher 看到 open turn 就 `inbox.reject("turn_unfinished")`。
- **影响：** 用户下一条消息永久丢失（command 被 rejected），必须手重发。
- **落地：** dispatcher 遇到孤儿 open turn 自动 `abandon_turn` 后继续；`submit` 前按状态路由（无 open turn 跳过 / 有活跃 run 让 dispatcher 串行 / suspended 或有 uncertain 工具则关闭，可恢复 turn 的 `resume` 见 P1-3）；**不再 `inbox.reject("turn_unfinished")`**，用户消息不会因此丢失。

---

### 2.2 P1（长期运行 / 恢复语义）

#### P1-1 observation 失败被降级成可重试 tool result ✅

- **位置：** `core/src/observation/mod.rs`、`core/src/agent/react_agent.rs`、`core/src/error.rs`
- **根因：** `run_acting_calls` 对 `Err(e)` 统一 `results.record(error_result(..))`，把“工具已执行但结果没落盘”变成一条模型可重试的错误结果。
- **影响：** 模型重试导致重复副作用；真实 `ToolResultBlock` 被丢弃。
- **落地：** `Error::ActionDispatch`（`action/start` 确认失败 = 工具未执行，可重试）vs `Error::ActionObservation`（`action/end` 失败 = run 级错误，journal 保持 uncertain，不降级）。

#### P1-2 模型流部分输出崩溃后不可恢复 ⬜

- **位置：** `core/src/session/models.rs`（chunk + `model/end`）、`core/src/session/views.rs`（投影不消费 chunk）、`core/src/agent/react_agent.rs`（`partial_assistant` 仅在内存）
- **影响：** 用户已看到的半截回答重启后丢失；没有 `model/end` 就无法判断模型调用是否已发出/结果未知。
- **修复方案：** 在 `model/end`/step 边界 checkpoint assistant 消息，或从 `model/chunk` 重建 partial；投影跟踪 model call 状态（`request/prepared` 无 `model/end` 即 incomplete）；明确“丢弃 partial”还是“继续”的恢复策略，两者必须一致。

#### P1-3 重启后无法真正继续原 turn ✅

- **位置：** `harness/src/agent/session/session.rs`、`extensions-acp/src/session.rs`
- **根因：** 旧实现每次 prompt 前无条件 `abandon_unfinished()`，把 interrupted turn 标成 cancelled；`resume` 只在 prompt loop 存活期间可用。
- **落地：** `prepare_open_turn_before_submit` 把可恢复的 interrupted turn 用 `resume` 排在前面（`turn/resumed` 而不是 `turn/cancelled`）；ACP 侧不再无条件 abandon，改 `stop_active_run`（只打断自己拥有的 run）。

#### P1-4 ACP prompt 没有稳定幂等键 ✅（跨进程受协议限制）

- **位置：** `extensions-acp/src/session.rs`（`prompt_idempotency_key`）、`harness/src/agent/session/session.rs`（`submit_with_key_msg`）
- **根因：** 每次 `run_prompt_inner` 生成新 `Uuid`；ACP 的 `PromptRequest` 不把 JSON-RPC request id / 客户端 turn id 传进来。
- **落地：** 键 = `prompt-<用户文本 SHA-256 前 16 hex>-<受理时间戳ms>`。同一文本同一时刻 = 同一轮（本进程重试复用）；同一文本稍后 = 新一轮（真实重复提问不被合并）；键长 37 字节，journal 里能看出这一轮对应哪条 prompt。
- **限制：** 进程重启后客户端重试仍会产生新 turn。真正去重需要协议层 request id / 自定义 metadata + journal 持久化 `requestId -> turnId`。

#### P1-5 cancel 是 session 级共享 flag ✅

- **位置：** `extensions-acp/src/session.rs`、`coding_agent.rs`（`CancelNotification` 只带 session id）
- **根因：** 单个 `AtomicBool`，且每个 prompt 开始时 `store(false)`，所以取消会串轮次或被清掉。
- **落地：** `cancel_epoch: AtomicU64`；每个 prompt 捕获自己的 epoch，`cancel()` 只递增 epoch → 只停“取消时正在跑的那一轮”。`prompt_lock` 获取可被取消/`closed` 打断。

#### P1-6 权限 / elicitation / 存储等待没有超时 ✅

- **位置：** `extensions-acp/src/session.rs`（`request_permission` / `elicit` / confirm handler）
- **影响：** 用户不回答或客户端卡住时工具永久等待，cancel 也无法打断。
- **落地：** `INTERACTION_TIMEOUT = 600s` 包裹 permission / elicitation，超时映射为 `Cancelled` 结果（→ 拒绝，绝不落入“通道失败即自动批准”分支）。journal 操作仍无整体 deadline（见 P2）。

#### P1-7 会话注册表 / load / resume / delete / evict 语义不完整 ✅

- **位置：** `extensions-acp/src/coding_agent.rs`
- **根因：** `session/list` 只读内存注册表（重启后为空）；load/resume 对未知 id 静默创建空会话；delete/evict 不 cancel 运行中的 prompt、不删 journal，旧 runtime 可重新注册。
- **落地：** `session_infos(cwd, cursor)` 合并注册表 + `HarnessAgent::list_session_keys`（durable store），按 cwd 缓存 catalog agent；load/resume 用 `session_exists` 校验，未知 id 返回 `invalid_params`；`CloseSession`/`DeleteSession` 先 `cancel_and_wait` 再 close/remove。

#### P1-8 panic / mutex poisoning 卡死 dispatcher ✅

- **位置：** `harness/src/agent/session/session.rs`（dispatch loop、大量 `lock().unwrap()`）、`extensions-acp/src/{coding_agent,acp_harness_agent}.rs`
- **影响：** dispatch task panic 后 `state.stop` 仍为 `Some`，`start()` 不重启，后续 prompt 一直 queued；poisoned mutex 连锁 panic。
- **落地：** dispatcher 循环 `catch_unwind`；poisoned mutex 统一用 `lock_state` 恢复；ACP prompt 任务 `catch_unwind`，panic 时回 `internal_error` 而不是让 client 永久等待。

#### P1-9 inbox 租约过长 + 重试过短 + transient reject 永久化 ✅

- **位置：** `core/src/session/inbox.rs`、`harness/src/agent/session/session.rs`（`launch`）
- **根因：** `INBOX_LEASE = 120s` 而 `acquire` 只重试 1s；`launch` 打开 recorder 只 3×100ms；`prepare` 任何错误都 `inbox.reject`。
- **落地：** `INBOX_LEASE` 120s → 10s；`ACCEPT_ATTEMPTS` 100 → 400（80ms 退避）；`launch` 的 open/admission/start/checkpoint 失败保留 queued 重试，不再永久 reject。

#### P1-10 suspended / external execution 在 ACP 中不可恢复 ✅

- **位置：** `extensions-acp/src/session.rs`、`extensions-acp/src/ext.rs`
- **根因：** ACP 把 `status != "completed"` 一律 `abandon_turn` + Refusal，且没有 respond 入口。
- **落地：** 私有扩展方法 `_agentscope/session/respond`（ACP 扩展方法必须以 `_` 开头），参数 `{sessionId, answers:{<requestId>:{output|approved[,reason]}}}` → `AcpAgentSession::respond` → 继续原 turn。

#### P1-11 全量投影/缓存无限增长 ⬜

- **位置：** `core/src/session/views.rs`（`SessionTurns::read` / `SessionProjection::read` / `pending_interactions`）、`harness/src/agent/session/session.rs`（`execution_view` 缓存）、`core/src/session/execution.rs`（`before_step` 每步 new inbox）、`core/src/session/inbox.rs`（全量 fold 缓存）
- **影响：** 长会话越来越慢、内存越来越大，最终重放超时回到 P0-1。
- **修复方案：** 持久化最新 checkpoint 索引做增量投影（P0-2 的快照）；`execution_view` 只缓存 `turn_id -> status/run_id`；inbox 缓存只留未处理/最近 N 条；`before_step` 复用同一 inbox；checkpoint 保留最近 N 个 + 摘要。

#### P1-12 unbounded event channel + 同步 emit ⬜

- **位置：** `extensions-acp/src/session.rs`（`unbounded_channel`）、`core/src/agent/react_agent.rs`（delta channel）
- **影响：** 客户端读得慢时事件堆积；`emit` 阻塞时 prompt loop 无法检查 cancel/心跳。
- **修复方案：** 有界 channel + 按事件类型分级丢弃（终态/工具事件不能丢，delta 可合并）；独立 writer task 负责 `send_notification`；监控队列长度。**未做原因：`set_event_sink` 是同步闭包，`try_send` 会丢文本 delta，需先定丢弃策略。**

#### P1-13 `model/chunk` 写入与模型流强耦合 ✅

- **位置：** `core/src/session/models.rs`、`core/src/agent/react_agent.rs`
- **根因：** 每个 chunk 先 `record` 再 yield，持久化失败就 `break`，而“已有可见文本”又会抑制重试。
- **落地：** `MergedChunk` 合并（4KiB / 64 delta / 1s），把 commit 次数从“一 token 一次”降到“每批一次”。

#### P1-14 损坏/未知事件让 session 永久不可打开 ✅

- **位置：** `core/src/session/journal.rs`（`visit` / `segments_through`）、`core/src/session/views.rs`（`fold_tolerantly`）、`core/src/session/inbox.rs`、`core/src/session/recorder.rs`（`open`）
- **根因：** 任一 `read_commit` 校验失败、未知 inbox 事件、payload 解码失败都会中止整个 projection/recovery，用户只能手删 session。
- **落地：** ① `visit` 改为「重放可读后缀」：链回溯遇到不可读 commit（校验和不符 / 缺失 / 坏 JSON / 断链 / 环）即停并 `warn`；严格读 `scan`/`read_after` 语义不变，按游标循环的调用方不会空转。② `SessionProjection::as_of_seq` 取 `head.seq`（新事实接在 head 之后，不回填空洞）。③ `fold_tolerantly`：`required=false` 的坏事件跳过，`required=true` 仍中止。④ `transcript` / `SessionTurns` / `pending_interactions` / `recorder.open` 改走 `visit`。⑤ inbox 未知事件忽略 + `warn`。⑥ **只容忍数据类错误**（`Error::Session` / `Error::Json`），IO 错误仍上报，避免网络抖动被当成历史丢失。

#### P1-15 `Stale committed prefix` / CAS 争用没有重基策略 ✅

- **位置：** `core/src/session/recorder.rs`（`flush` / `rebase_onto_head` / `is_stale_prefix`）、`journal.rs`（`commit`）
- **根因：** `commit` 发现 `head.seq != expected_seq` 直接 stale；`flush` 把它变成 sticky，而“另一 writer 推进了 head”没有重新基于最新 head 继续的策略。
- **落地：** 三分：同 batch 已提交 → 成功（原有）；**head 只前进且无本 batch → 重基**（先 `renew` 确认仍持有所有权，再按新 head 重编号 in-flight + pending 事件并换新 batch id，最多 3 次）；数据不连续/损坏 → fatal。重基与 P1-14 配套：重放被截断后 `durable_seq < head.seq` 不再变成 sticky 失败。

### 2.3 P2（可观测性 / 次级风险）

> 2026-10-28 第五轮：P2-15/P2-20（ACP 侧）已修，P1-12 落地。

| 编号 | 问题 | 状态 | 位置 / 修复 |
|---|---|---|---|
| P2-1 | `SessionRecorder::open` 在 `log.head()` 失败时不释放已 acquire 的 writer | ✅ | `recorder.rs`：`head()` 失败与 replay 失败两条路径都 `release`（+ 回归测试 `an_open_that_fails_to_read_the_head_releases_the_writer`） |
| P2-2 | `LeaseHeartbeat::start` 无 runtime 时静默 `None` | ✅ | `recorder.rs`：无 runtime 时 `warn` 并说明回落到「每次 flush 内联续租」；有 runtime 时记录 `last_renew_ms` 供 P2-18 观测 |
| P2-3 | `Drop` 里只 `spawn(release)`，不等待 | ◐ | `recorder.rs`、`react_agent.rs`：正常退出已由 P0-9 drain 覆盖；硬杀仍靠租约到期（`Drop` 无法 await） |
| P2-4 | `SessionLogExporter::drain` 每次全量 scan | ✅ | `session/export.rs`：有界批量（256）+ 每批只推进一次游标；批内失败回退到最后一个已确认事件 |
| P2-5 | `save_session_config` 非原子写 | ✅ | `acp/session.rs`：临时文件 → fsync → rename → 目录 fsync；失败清理临时文件 |
| P2-6 | MCP server 先 `bridged.push(name)` 再 connect，失败后永不重试 | ✅ | `acp/session.rs`：连接成功后才记录名字（stdio/sse/http 与 MCP-over-ACP 两条路径） |
| P2-7 | MCP connect / elicitation / permission 无总超时 | ✅ | `acp/session.rs`：permission / elicitation 600s；新增 `MCP_CONNECT_TIMEOUT = 60s` 覆盖 create+connect 与 MCP-over-ACP |
| P2-8 | `session_infos` 按 UUID 排序（注释“id 含时间戳”是错的） | ✅ | `coding_agent.rs`：按 `updated_at` 倒序、id 兜底；先去重再排序 |
| P2-9 | `Usage`/`last_input_tokens`/`updated_at` 在 clone 间不共享 | ✅ | `acp/session.rs`：`last_input_tokens` 改 `Arc<AtomicU64>`、`updated_at` 改 `Arc<Mutex<_>>`；`usage`/`turn_usage` 无跨 clone 读者，保持原样 |
| P2-10 | 大量 `lock().unwrap()`，poison 后连锁 panic | ◐ | `core/src/session/**`、`harness/src/agent/session/**`、`extensions-acp/src/**` 全部改 poison 容忍；其他 crate 仍有 |
| P2-11 | `agent.session()` 每次 `SessionInbox::new`；`before_step` 每步重建 | ✅ | `core/src/session/execution.rs`：`SessionExecution` 持 `OnceLock<SessionInbox>`，每步复用 fold 缓存 |
| P2-12 | `SessionInbox::fold` 未知事件 fatal | ✅ | 随 P1-14 改为忽略 + `warn` |
| P2-13 | `SessionProjection::read` 每个 commit 读两次、所有 blob 都 hydrate | ◐ | `journal.rs`：单 commit 单次读已落地；blob 仍随 commit 全量 hydrate（要懒加载需改 `visit` 的 `&SessionEvent` 签名） |
| P2-14 | `SessionTurns::read`/`pending_interactions` 每次 cold turn 全量且各扫一遍 | ✅ | `views.rs`：`replay_views` 单次 visit 同时折叠多个视图；`open_views`/`admission_views`/`recovery_views` 各自只选所需视图（+ 等价性测试 `one_replay_matches_the_individual_views`） |
| P2-15 | `MAX_INLINE_PAYLOAD_BYTES=64KB` 导致大量 blob + version 文件 | ✅ | immutable commit/blob 不写 `.version`（+ 回归测试 `immutable_payload_blobs_do_not_get_a_version_file`）；>64KiB 的 payload 按事件各写一个 blob，commit JSON 只留 `{}` + 引用 |
| P2-16 | `model/chunk` 含完整 chunk/usage，每 token 都落盘 | ◐ | 随 P1-13 合并降量；进一步可采样/不落 token |
| P2-17 | 错误日志缺少 session/run/epoch/owner/lease | ✅ | `recorder.rs` 致命提交失败带 run/turn/epoch/owner/lease/attempt；`journal.rs` 四条重放告警带 `session` 前缀；重基路径带 run/head_seq |
| P2-18 | 没有 journal 大小/事件数/重放耗时/心跳年龄指标 | ✅ | `recorder.rs` 新增 `SessionMetrics` + `SessionRecorder::metrics()`：`durable_seq`/`pending_events`/`pending_bytes`/`replay_ms`/`heartbeat_age_ms`/`lease_ms`（告警与自动 compaction 仍待接入） |
| P2-19 | `AgentSession::start` 每 50ms 轮询，即使 idle | ✅ | `harness/src/agent/session/session.rs`：accept 后 `Notify` 唤醒；idle 指数退避至 `IDLE_POLL_MAX = 500ms`，有活跃 run 保持 50ms |
| P2-20 | ACP `events_tx`/core `delta_tx` unbounded | ◐ | ACP 侧已改有界 `EventQueue`（见 P1-12）；core `delta_tx`（`react_agent.rs:845`）仍是 per-call 无界，消费者是本次 run 的事件流，随 run 结束释放，未改 |

---

## 3. 验收标准与测试矩阵

### 3.1 必须通过的验收

1. **100 轮以上长会话**（含大文件读写、长模型流、多工具）连续运行 24h，不出现 `Session writer ownership expired or fenced`、`pending buffer exhausted`、`Stale committed prefix`。
2. **任意阶段 kill -9 后重启**：5s 内可接管/释放旧 lease；不重复执行已产生副作用的工具；原 turn 可 resume 或明确 reconcile，不丢用户输入。
3. **冷启动重放**：10 万事件 session 在 10s 内 open（或至少租约不丢、用户不报错）。⬜ 依赖 P0-2
4. **取消**：长工具执行中 cancel，工具在 1s 内停止/被标记 uncertain；session 可立即接受下一条 prompt。
5. **慢客户端**：事件队列有界，内存不增长；连接断开后 run 被取消/收尾。✅ ACP 侧已改有界队列（超限丢弃 + 计数 + turn 结束告知）；core `delta_tx` 仍是 per-call 无界（P2-20 部分）
6. **双进程**：两个 ACP 进程同 session 并发，不丢 commit、不 head 回退；只有一个 writer。
7. **磁盘/网络抖动**：注入 10 次 transient 失败，run 继续或明确恢复，不能一次就 sticky。

### 3.2 测试矩阵

已落地（关键回归）：

| 测试 | 覆盖 |
|---|---|
| `an_expired_lease_can_be_renewed_by_the_same_owner` / `a_lease_held_by_writer_a_still_fences_writer_b` | P0-3（正反两向） |
| `a_transient_commit_failure_is_retried_and_does_not_poison_the_recorder` | P0-6 |
| `an_event_appended_during_a_flush_is_not_dropped` | P0-5 |
| `an_action_start_alone_marks_the_tool_uncertain` / `a_persisted_tool_result_clears_dispatch_uncertainty` | P0-7 |
| `settled_tool_results_are_checkpointed_before_the_next_model_step` | P0-7（`after_acting` 早于下一次 `before_model`） |
| `dropping_the_acting_stream_aborts_the_tool_task` | P0-8 |
| `a_lock_file_from_another_process_serializes_compare_and_set` | P0-10（他进程持锁时阻塞 / 陈旧锁可破 / 释放） |
| `an_oversized_payload_is_admitted_when_nothing_else_is_buffered` | P0-12 |
| `an_interrupted_resumable_turn_is_continued_before_a_new_submission_runs` / `an_interrupted_turn_is_closed_before_a_new_submission_runs` | P0-13 / P1-3 |
| `a_failed_start_acknowledgement_never_runs_and_is_retryable` / `a_failed_end_acknowledgement_propagates_instead_of_returning_a_result` | P1-1（正反两向） |
| `the_prompt_key_hashes_the_text_and_the_acceptance_time` / `a_prompt_without_text_still_gets_a_key` | P1-4 |
| `ext::tests::*`（5 个） | P1-10（方法名 `_` 前缀、参数往返、空答案拒绝） |
| `a_corrupt_segment_still_opens_the_session_from_the_last_checkpoint` | P1-14（坏段 + 严格 `scan` 仍报错） |
| `an_unreadable_optional_event_is_skipped_but_a_required_one_is_not` | P1-14（required 分级） |
| `an_unknown_inbox_event_is_ignored` | P1-14 |
| `a_batch_is_rebased_when_the_committed_prefix_moved` | P1-15 |
| `immutable_commits_do_not_get_a_version_file`、`tool::tool::tests::strict_is_opt_in_and_injects_additional_properties_false` | 存储 / 工具 schema 附带硬化 |

待补：

| 测试 | 目的 | 阻塞原因 |
|---|---|---|
| 两进程 SQLite / 文件 CAS 并发 | P0-10 真正的跨进程 | 需独立进程 |
| `session/list` 重启可见端到端 | P1-7 | 需真实 cwd + 持久化 store |
| `_agentscope/session/respond` 端到端 | P1-10 | 需一个挂起工具的 run |
| stdin EOF drain | P0-9 | 需真实 stdio 子进程 |
| `lease_survives_replay_longer_than_lease` | P0-1（controlled storage 使 replay 超过 lease） | 需可注入延迟的 storage |
| `large_checkpoint_spills_without_pending_limit` | P0-12（超大 state 溢写 blob） | 可补 |

### 3.3 最小复现脚本

1. **构造大 journal**：跑一个 5,000+ chunk 的流式回答，检查 `agents/.../commits` 文件数。
2. **冷启动计时**：杀掉进程，记录 `SessionRecorder::open` 从 `acquire` 到 `start` 的耗时；> lease 即命中。
3. **注入短 lease**：用 `open_with_lease`，在 replay 前 sleep 3×lease；旧实现应失败，现实现应成功。
4. **工具中 kill**：让 `execute` 跑 `Start-Sleep 60`，执行中 kill -9；重启后检查 journal 是否出现 `tool/dispatch` / uncertain，是否重跑。
5. **stdin EOF**：关闭 ACP 客户端 stdin，观察进程是否等 run 收尾、是否释放 lease。

---

## 4. 验证记录（截至 2026-10-28，第四轮 P2 完成后）

| 命令 | 结果 |
|---|---|
| `cargo test -p agentscope-core --lib` | 346 passed / 0 failed / 3 ignored |
| `cargo test -p agentscope-core --tests` | 全部 target 通过 |
| `cargo test -p agentscope-harness --lib` | 484 passed / 0 failed |
| `cargo test -p agentscope-harness --test harness_session_test --test harness_session_journal_wiring_test --test harness_session_web_tools_test` | 4 / 19 / 8 passed |
| `cargo test -p agentscope-extensions-acp` | 65 lib（含 5 项 `event_queue` 单测）+ 全部集成 test 通过 |
| `cargo fmt --check`（改动文件） | 干净（`shutdown/mod.rs`、`e2b.rs`、`harness_shutdown_test.rs` 为仓库既有未格式化文件，本次未动） |

反向验证（逐个回退实现 → 确认 FAILED → 恢复）：

| 回退点 | 期望失败的测试 | 结果 |
|---|---|---|
| `if is_stale_prefix(..) && rebases < FLUSH_REBASES` → `if false` | `a_batch_is_rebased_when_the_committed_prefix_moved` | FAILED |
| `segments_through(.., true)` → `false` | `a_corrupt_segment_still_opens_the_session_from_the_last_checkpoint` | FAILED |
| `Err(error) if !event.required` → `if false` | `an_unreadable_optional_event_is_skipped_but_a_required_one_is_not` | FAILED |
| inbox 未知事件忽略 → `return Err(Unknown inbox event)` | `an_unknown_inbox_event_is_ignored` | FAILED |
| `after_acting` → `before_model` | `settled_tool_results_are_checkpointed_before_the_next_model_step` | FAILED |
| submit 前的 `resumable` 强制 `false` | `an_interrupted_resumable_turn_is_continued_before_a_new_submission_runs` | FAILED |
| 关闭跨进程文件锁 | `a_lock_file_from_another_process_serializes_compare_and_set` | FAILED |
| `open` 的 head 失败路径去掉 `release` | `an_open_that_fails_to_read_the_head_releases_the_writer` | FAILED |
| `self.inbox()` 回退为 `SessionInbox::new` | `a_step_reuses_the_folded_inbox_instead_of_rebuilding_it` | FAILED |
| 批内失败时不回退游标 | `cursor_only_advances_after_a_successful_accept` | FAILED |
| `next_idle_backoff` 恒返回当前值 | `an_idle_dispatcher_backs_off_and_a_busy_one_does_not` | FAILED |
| `sort_session_infos` 回退为按 id 排序 | `the_session_list_orders_by_last_activity_not_by_id` | FAILED |
| 不再写 `updated_at` | `a_clone_sees_the_activity_of_a_turn_it_did_not_run` | FAILED |
| `metrics()` 的 `heartbeat_age_ms` 恒 `None` | `metrics_report_replay_pending_and_heartbeat_age` | FAILED |
| `EventQueue::push` 的容量判定 → `if false` | `a_full_queue_drops_the_newest_and_counts_deltas`、`a_full_queue_still_accepts_the_agent_result` | FAILED |
| `/blobs/` 从不可变列表移除 | `immutable_payload_blobs_do_not_get_a_version_file` | FAILED |

---

## 5. 关键代码索引

> 行号取自 **2026-10-27 基线**，已随三轮修复漂移，按文件名 + 符号名定位更可靠。

| 主题 | 文件（基线行号） |
|---|---|
| 租约检查 / acquire / release / commit | `core/src/session/journal.rs:115-126`、`:253-363`、`:365-493` |
| `update_owner` / 心跳常量 | `journal.rs:616-650`、`core/src/session/recorder.rs:38-46` |
| recorder open / replay / 心跳 | `recorder.rs:336-483`、`:174-227` |
| flush / sticky / record / append | `recorder.rs:570-615`、`:530-626` |
| `MAX_PENDING_BYTES` / finish / Drop | `recorder.rs:49`、`:876-969`、`:130-166` |
| 每 chunk 记录 | `core/src/session/models.rs:152-244` |
| 投影 / action 不确定 | `core/src/session/views.rs:336-472`、`:372-394` |
| `abandon_turn` / recovery | `core/src/session/recovery.rs:79-92`、`:94-277` |
| acting spawn / tool 执行 / observe | `core/src/agent/react_agent.rs:2508-2642`、`:2649-2831` |
| state 发布 / checkpoint | `react_agent.rs:1764`、`:1851` |
| ACP prompt 主循环 / cancel / events | `extensions-acp/src/session.rs:1275-1878`、`:1075-1078`、`:1378-1383` |
| ACP 会话注册表 / load-resume-delete | `extensions-acp/src/coding_agent.rs:223-343`、`:602-659` |
| ACP shutdown | `extensions-acp/src/acp_harness_agent.rs:1159-1247` |
| `ExecutionGuard` | `core/src/agent/react_agent.rs:534`、`:723-861` |
| 文件 CAS / SQLite / 默认 journal 路径 | `harness/src/agent/session/storage.rs:62-92`、`:152-183`、`:224-233`；`sqlite.rs:42-65`；`core/src/session/key.rs:35-44` |
| inbox lease / harness launch / dispatch loop | `core/src/session/inbox.rs:14-17`、`:302-320`；`harness/src/agent/session/session.rs:1093-1406`、`:983-1033`、`:728-746` |
