# ACP 智能体稳定性深度调查报告

> 调查对象：`D:\IdeaProjects\agentscope-rust`  
> 基线：`f1674b1`（当前工作区另有 5 个未提交修改，主要是 `execution_baseline` / `RecorderBaseline` 性能优化；本报告按当前工作区实际代码分析）  
> 日期：2026-10-27  
> 约束：本次只做只读调查，未修改任何代码。  
> 目标：解释 `Agent error: session log: Session writer ownership expired or fenced`，并穷举 ACP 执行链路中会导致“无法长期稳定运行 / 中断后不能恢复”的问题，给出可直接在修复会话中执行的方案。

---

## 0. 结论摘要（先看这里）

这个报错不是一个孤立的“心跳没加”问题。当前代码已经有 `LeaseHeartbeat`，但它仍然会在下面几类情况下必然出现：

1. **租约在“重放 journal”期间就已经过期，而心跳要等重放完成才开始。**  
   `SessionRecorder::open_with_lease_and_baseline` 先 `acquire` 租约，然后执行 `SessionProjection::read`、`log.scan`、`transcript` 等全量重放，最后才构造 `SessionRecorder` 并启动心跳（`recorder.rs:355`、`357-463`、`466`）。  
   只要一次冷启动/重启后的重放超过 `LEASE=120s`，租约已经过期；随后 `recorder.start()` 的第一次 `flush()` 续租时命中 `ensure_owned` 的 `lease_until <= now`，直接返回 `Session writer ownership expired or fenced`。  
   **这是最符合现场数据、也最容易复现的根因。**

2. **journal 被“每个模型 chunk 一个 commit”放大到不可重放。**  
   `SessionModels::Recorded` 对每个流式 chunk 调 `recorder.record("model/chunk", ...)`（`models.rs:172`），而 `record` 立即 `flush`（`recorder.rs:618-626`）。在本机 `agents/` 下实测：  
   - 934,123 个文件、约 5.77 GB；  
   - 单个会话 184,530 个文件（`seq=93,234`，`commits` 目录同时存在 `.version` 文件，约 2 文件/事件）；  
   - 抽样解析成功的 932 条事件中，920 条是 `model/chunk`，基本一条事件一个 commit。  
   这会让冷启动重放从“秒级”变成“分钟级”，直接触发第 1 条。它也是长期运行的磁盘/内存/CPU 风险。

3. **租约过期被当作“永久 fencing”，即使没有任何其他 writer 接管。**  
   `ensure_owned` 只要 `lease_until <= now` 就报错（`journal.rs:115-126`），不会区分“自己只是暂停/睡眠/时钟跳变”与“已被别人接管”。`release` 也走同一个检查，因此过期后连释放都释放不了。操作系统休眠、NTP 校时、VM 时钟跳变、长时间 GC/阻塞、调试器暂停，都会触发同一错误。

4. **心跳第一次失败就永久退出，且没有监督/重启。**  
   `LeaseHeartbeat` 在 `renew` 失败时直接 `return`（`recorder.rs:189-197`）。一次瞬时存储错误、一次 CAS 争用、一次时钟跳变，就让后台续租永久消失；之后只要出现一段没有 `flush` 的静默期（长模型流、慢工具），租约到期，下一次写就报同样的错。

5. **`flush` 存在“并发 append 丢事件”的窗口。**  
   `flush` 在锁内 clone `pending`，释放锁后 commit，成功后再锁内 `pending.clear()`（`recorder.rs:570-596`）。如果这期间有 `append`，新事件会被一起清掉，但从未提交。后续 seq 继续复用，journal/内存状态分叉。这是长期运行中“偶发丢消息/状态倒退”的来源之一。

6. **任何一次 commit 失败都会变成 sticky failure。**  
   `flush` 对任何 `Err` 都写入 `guard.failure`（`recorder.rs:604-613`），之后所有 `append` 都报 `Session persistence failed`。临时 IO、CAS 争用、存储抖动都会把整个 run 判死，而不是重试。

7. **工具执行在 ACP 默认路径上被 `tokio::spawn` 分离，取消 run 不会停止工具。**  
   `HarnessAgent` 默认注册 `AgentTraceMiddleware`（`harness_agent.rs:1409`），它实现了 `on_acting`，因此 `acting_phase` 永远走 `acting_stream_core` 的 `tokio::spawn` 分支（`react_agent.rs:2625`）。取消/丢弃 run 时，工具任务继续跑、继续产生副作用；`run_acting_calls` 在工具之间也不检查中断（`react_agent.rs:2649-2831`）。

8. **崩溃发生在工具执行中时，恢复链路缺少“工具已派发”的事实。**  
   代码只在 reasoning 之前发布/ checkpoint 状态（`react_agent.rs:1764`），assistant 消息和 tool_use 在 acting 前没有 checkpoint；`tool/dispatch` 只有读取方和测试写入，没有生产写入方（`views.rs:372`，全仓无生产 `append("tool/dispatch")`）。因此工具可能已经执行，但恢复时投影的 `uncertain_tool_calls` 是空的，`abandon_turn` 不会为它补 “interrupted” 结果，最终可能丢结果，甚至让模型再次调用工具。

9. **关闭/退出路径没有真正等待 ACP run。**  
   `GracefulShutdownManager` 的 `ExecutionGuard` 只在 `call_with_format_and_extensions` 里注册（`react_agent.rs:534`），ACP 走的是 `stream_events_with_extensions`（`react_agent.rs:845`），计数永远为 0；`acp_harness_agent` 的 shutdown handler 等不到任何活跃请求，10s 内没有可等的就直接 `std::process::exit(130)`（`acp_harness_agent.rs:1227-1246`）。stdin 关闭/IDE 重启时更没有任何 drain 逻辑，`main` 直接返回。结果是：run 被硬杀、租约不释放、下一次启动等 120s 并再次触发重放/租约错误。

10. **文件存储的 compare-and-set 不是跨进程原子，也没有 fsync。**  
    `WorkspaceSessionStorage` 把 value 和 `.version` 分成两个文件写（`storage.rs:62-92`、`152-183`），进程内锁只能保护同进程；跨进程 stale writer 可以互相覆盖，崩溃在 value/version 之间会留下不一致。对于“多个 IDE 窗口/两个 ACP 进程”或“重启后旧进程未退”，这是 `Stale committed prefix`、租约争用和 head 回退的根源之一。  
   另外，journal 默认落在 `<cwd>/agents/`（`WorkspaceSessionLogStore` + `SessionKey::storage_path`），模型的文件工具可以直接看到/修改/删除它，且会污染工程目录。

11. **大 checkpoint 会撞 `MAX_PENDING_BYTES=16MB`。**  
    `capture_state` 在 finish 时把完整 `AgentState` 作为 `state/checkpoint` 追加（`recorder.rs:816-823`、`901-943`），`append` 超过 16MB 直接报 `Session pending buffer exhausted`（`recorder.rs:49`、`549-550`）。现场日志已经出现过：  
    `recording the run's terminal facts failed ... Session pending buffer exhausted`。这会让长会话的终态写入失败，turn 被报告为 interrupted。

结论：**当前 ACP 不是“长期稳定、任意中断可恢复”的架构**。要满足你的要求，需要同时修租约、journal 体积、恢复事实、工具取消、退出 drain、存储原子性和 ACP 会话语义；只加/调心跳间隔不够。

---

## 1. 现场证据

### 1.1 实际错误链路（直接命中用户报错）

最可能的完整链路：

```text
ACP PromptRequest
  -> AcpAgentSession::run_prompt_inner            (extensions-acp/session.rs:1275)
  -> session.abandon_unfinished()                 (session.rs:1392)
        -> open_turn / SessionTurns::read         全量扫描 journal
        -> abandon_turn / SessionProjection::read 全量扫描 + 可能写 cancel
  -> session.submit_with_key_msg(...)             (session.rs:1403)
  -> AgentSession dispatcher                      (harness/session/session.rs:983)
  -> launch                                       (harness/session/session.rs:1093)
  -> SessionRecorder::open_with_baseline / open   (core/session/recorder.rs:355)
        acquire 120s lease
        SessionProjection::read                    全量重放
        log.scan(0, durable_seq)                   再全量扫描
        transcript()                               再全量扫描
        << 此处可能已超过 120s；心跳尚未 start >>
  -> recorder.start(...)                          (recorder.rs:657)
  -> flush -> log.renew                           (recorder.rs:584)
  -> JournalSessionLog::update_owner
  -> ensure_owned: head.lease_until <= now
  -> Err("session log: Session writer ownership expired or fenced")  (journal.rs:115-126)
```

如果失败发生在模型流中，则另一条路径：

```text
SessionModels::Recorded::stream
  -> recorder.record("model/chunk")               (core/session/models.rs:172)
  -> flush -> renew/commit -> ensure_owned
  -> 同一个 Session 错误
  -> run_stream 把非 Interrupted/Cancelled 错误转成 Error::Agent
  -> launch 把 detail 写入 run/end（若 finish 本身也失败则写不进去）
  -> ACP 输出 "Agent error: session log: ..."
```

`Error::Session` 的 Display 就是原消息（`session_error` 增加 `session log: ` 前缀），`run_stream` 又会包成 `Error::Agent`，所以最终用户看到：

```text
Agent error: session log: Session writer ownership expired or fenced
```

如果是在工具 action observation 回写时失败，则会看到另一种文案：

```text
Unable to persist action observation; do not replay the action automatically: session log: Session writer ownership expired or fenced
```

两者根因相同：**租约已经失效，但错误被当成不可恢复的持久化失败传播。**

### 1.2 数据证据（本机 `agents/` 目录）

在当前工程根目录下实测：

| 项目 | 数值 |
|---|---:|
| `agents/` 总文件数 | 934,123 |
| `agents/` 总大小 | 约 5,774 MB |
| 含 `commits/` 的会话数 | 39 |
| 最大会话 `commits/` 文件数 | 184,530 |
| 最大会话 head `seq` | 93,234 |
| 最大会话 `head.json` owner | 仍存在，lease 已过期 |
| 抽样解析成功的事件类型 | 932 条中 920 条是 `model/chunk`，其余才是 step/action/compaction 等 |

`head.json` 示例（已脱敏）：

```json
{"seq":93234,"epoch":1,"owner":"<run-id>","leaseUntil":1793033778682,
 "commitId":"<hash>"}
```

该 `leaseUntil` 对应 `2026-10-26 16:56:18Z`；日志在 `2026-10-26 16:53:04` 左右中断在半轮工具调用中，说明进程没有走正常 `finish/close`，owner 留在 head 上。  
下一次启动时，`acquire` 虽然可以因 lease 过期而接管，但在接管前会先经历：

- `abandon_unfinished` 的 `SessionTurns::read`；
- `abandon_turn` 的 `SessionTurns::read` + `SessionProjection::read`；
- `SessionRecorder::open` 的 `SessionProjection::read` + `log.scan` + `transcript`。

对 93,234 个事件、184,530 个 commit 文件来说，这些全量扫描在 Windows 冷缓存下极容易超过 120s。**这正是“发送消息时立刻报租约错误”的典型场景。**

### 1.3 日志证据

本机 `.agentscope/log/acp-agent.log` 中已出现：

```text
WARN session writer lease renewal failed: session log: Session writer ownership expired or fenced
WARN recording the run's terminal facts failed ... error=session log: Failed to commit session prefix: session log: Stale committed prefix
WARN recording the run's terminal facts failed ... error=session log: Session pending buffer exhausted
WARN failed to close the cancelled turn; the next prompt may be rejected ... Session writer is still busy; the interrupted turn could not be closed
```

这些分别对应：

- 心跳在无上下文的情况下失败（`recorder.rs:189-197`）；
- `flush` 的 `Stale committed prefix`（`journal.rs:411-412`）；
- `MAX_PENDING_BYTES` 撞上限（`recorder.rs:49`、`549`）；
- 取消后 `abandon_turn` 5s 内抢不到 writer（`recovery.rs:27-30`、`83-92`）。

---

## 2. 问题清单（按严重度）

> 编号用于修复会话逐项对照。P0 是“必须修，否则仍会复现用户报错/不可恢复”；P1 是“长期运行或恢复语义会坏”；P2 是“稳定性/可观测性/性能风险”。

### 2.1 P0（阻断长期稳定运行）

#### P0-1 租约在重放期间过期：心跳启动时机错误

**位置：**
- `crates/agentscope-core/src/session/recorder.rs:355`（`acquire`）
- `crates/agentscope-core/src/session/recorder.rs:357-463`（`prepared`：`SessionProjection::read`、`log.scan`、`transcript`）
- `crates/agentscope-core/src/session/recorder.rs:466`（`LeaseHeartbeat::start`）
- `crates/agentscope-core/src/session/journal.rs:115-126`（`ensure_owned`）

**根因：**  
`log.acquire` 先拿 120s 租约，然后才做全量重放；重放期间没有任何续租。重放完成才构造 `SessionRecorder`，此时才启动心跳。  
`SessionRecorder::open` 的冷路径还会额外做：

- `SessionProjection::read`（全部 commit，hydrate 所有 blob）；
- `log.scan(0, durable_seq)`（检查 run/turn 是否已用）；
- `transcript`（再扫一遍）。

即使心跳启动及时，`SessionExecution::prepare` 的 admission 扫描在未命中 `execution_baseline` 时也会全量读；当前未提交优化只覆盖了部分场景。

**影响：**  
重放 > 120s 时，`recorder.start` 的第一次 `flush` 必然失败；`launch` 把命令 `inbox.reject`，ACP 直接打印 `Agent error: session log: ...`，用户消息丢失，必须重发。  
即使重放没有超过 120s，只要接近上限，后续任何一次慢 commit 也会触发。

**修复：**
1. `acquire` 成功后立即启动心跳（或用一个 RAII `LeaseGuard` 持有 heartbeat），再做任何 replay；replay 失败/取消时先停心跳再释放。
2. 重放过程中每隔 `lease/4` 或每 N 个 commit 主动 `renew`；不要依赖后台任务在单线程 runtime 上一定被调度。
3. 持久化“最新 checkpoint 指针/索引”，从 head 反向找最后一个 `state/checkpoint`，不要把整个 journal 全量重放；至少把 `SessionProjection::read` 改成“反向扫描到最近 checkpoint”。
4. `SessionRecorder::open` 的 `log.head` 失败时也要释放 writer（现在 `let head = log.head().await?` 会泄漏租约，见 P2-4）。
5. 对重放设置可观测耗时；超过阈值告警并拒绝使用旧 lease 直接启动。

**回归测试：**
- 用一个 delay/controlled storage 让 `SessionProjection::read` 超过 `lease`，断言 heartbeat 在 replay 期间保持租约，`start()` 不失败。
- 冷启动 10k/100k commit，断言 open 总耗时 < lease 或至少租约不丢。

---

#### P0-2 每个模型 chunk 一个 commit，journal 爆炸

**位置：**
- `crates/agentscope-core/src/session/models.rs:172`：每个 `Ok(chunk)` 调 `recorder.record("model/chunk", ...)`。
- `crates/agentscope-core/src/session/recorder.rs:618-626`：`record = append + flush`，每次都 renew + commit。
- `crates/agentscope-core/src/session/journal.rs:383-458`：一个 batch 一个 commit 文件；`commit_id = hash(batch_id)`。
- `crates/agentscope-harness/src/agent/session/storage.rs:152-183`：每个 commit 还写一个 `.version` 文件（`compare_and_set` 对 immutable commit 也走 value + version 两步）。

**证据：**  
最大会话 184,530 个 `commits/` 文件对应 `seq=93,234`；抽样解析成功的 932 条事件中 920 条是 `model/chunk`。  
`agents/` 总计 93 万文件、5.7GB。

**影响：**
- 冷启动/重启重放需要打开/解析 10 万级文件，直接触发 P0-1。
- Windows NTFS 目录元数据、杀毒/索引、备份会被拖垮；长期运行磁盘耗尽。
- 每次 chunk 都做 2-3 次文件写/rename 和一次 head CAS，模型流越流畅，journal 越慢，形成正反馈。
- 一个 chunk 写失败就中断整个模型流（见 P0-6/P1-13）。

**修复：**
1. `model/chunk` 不要逐条 `record`。改成 `append` 缓冲，按时间窗口（如 100-500ms）或字节阈值（如 64KB）批量 flush；只在 `model/end`、工具边界、step 边界强制 flush。
2. 如果 chunk 不是恢复必需（推荐）：不要持久化每个 token；只保留最终 assistant message / 关键摘要。恢复时以 checkpoint + `model/end` 为准；部分输出按策略丢弃或重建。
3. commit 文件与 version 文件分离对 immutable 对象没有意义：immutable commit/blob 直接写一个内容寻址文件，不写 `.version`；或整体改用 SQLite（一行一个对象，CAS 单语句）。
4. 引入 segment 文件/日志追加（例如每 1-4MB 一个 segment），而不是一个事件一个文件。
5. 增加 journal compaction / 快照 + 旧 commit 回收；给每会话设硬上限（磁盘/事件数），超限归档。

**验收：**
- 一次 10 万 token 的流式回答，事件数不应线性等于 token 数；commit 数控制在百级。
- 10 万事件 session 冷启动重放 < 10s（目标），且文件数 < 1,000。

---

#### P0-3 租约过期 = 永久 fence，缺少“自己续租/接管”的语义

**位置：**
- `crates/agentscope-core/src/session/journal.rs:115-126`（`ensure_owned`）
- `crates/agentscope-core/src/session/journal.rs:357-363`（`release` → `update_owner`）
- `crates/agentscope-core/src/session/journal.rs:365-493`（`commit`）
- `crates/agentscope-core/src/session/journal.rs:616-650`（`update_owner`）

**根因：**  
`ensure_owned` 的判断是：

```text
head.epoch != writer.epoch
|| head.owner != writer.owner
|| head.lease_until <= now
```

前两个是真正的 fencing；第三个只是“租约到期，允许别人接管”，并不等于“已经被别人接管”。当前实现把“到期”直接当“失去所有权”。  
`release` 也调用 `ensure_owned`，所以租约过期后连“把自己的 owner 清掉”都做不到，head 会长期保留 stale owner。

**影响：**  
- 机器休眠 >120s、VM 时钟跳变、NTP 校时、调试器暂停、长时间 GC/阻塞：没有任何其他 writer，也会报错。
- 重启后旧 writer 的 head 仍显示 owner，直到下一次 acquire；期间 `steal`/诊断混淆。
- 多进程时钟偏差会提前 fence 健康 writer。

**修复：**
1. 把“lease 到期”与“ownership 失效”拆开：
   - `ensure_owned` 只在 `epoch/owner` 不匹配或 sealed 时失败；
   - 如果 owner/epoch 仍匹配但 lease 过期，允许 `renew` 原地续租（或 CAS 一次延长）。
2. `release` 只检查 owner/epoch，不检查 lease；自己的 stale head 必须能清掉。
3. 增加 grace/skew（例如 `lease_until + clock_skew_tolerance` 才允许 takeover），或使用存储侧时间/单调时钟；至少检测系统时钟跳变并重新计算。
4. `commit` 在写大 blob/大 commit 前主动延长租约；head CAS 时把 `lease_until` 一并推进，而不是保持旧值。
5. 对“lease 过期但 owner 仍是自己”的情况记录 warning + 指标，而不是直接抛给用户。

**回归测试：**
- lease 过期但无 takeover → `renew`/`commit` 成功。
- 另一个 writer 已 takeover（epoch 变化）→ 旧 writer 被 fence。
- `release` 在过期后仍能清 owner。
- 模拟时钟前进/后退。

---

#### P0-4 心跳一次失败就永久停止，且没有监督/重启

**位置：**
- `crates/agentscope-core/src/session/recorder.rs:180-201`（`start`，无 runtime 时返回 `None`，无告警）
- `crates/agentscope-core/src/session/recorder.rs:189-197`（`renew` 失败直接 `return`）

**根因：**  
后台任务只做：

```text
sleep(interval) -> renew -> 如果 Err 就 warn + return
```

没有重试、没有区分 transient/fenced、没有重启、没有向 recorder 报告“心跳已死”。  
`LeaseHeartbeat::start` 在 `Handle::try_current()` 失败时返回 `None`，也不会告诉调用方“本 run 没有租约保护”。

**影响：**  
一次瞬时存储错误/争用/时钟跳变后，后台续租永久消失；下一次长模型流/慢工具期间 lease 到期，`flush` 报同一个错。现场日志已出现一次 `session writer lease renewal failed`。

**修复：**
1. 失败分类：
   - `ownership expired/fenced/sealed`：真正失去所有权，停止并通知 recorder；
   - `IO/CAS contention/timeout`：指数退避重试，直到 lease 截止；记录连续失败次数。
2. 心跳任务加 supervisor：意外 panic/退出后自动重启（在 lease 有效期内），或所有 `renew` 由 recorder 内的 `LeaseGuard` 统一管理。
3. `start` 无法获得 runtime 时应返回错误/降级为 inline renew，而不是静默 `None`。
4. 日志带 `session_id/run_id/epoch/owner/lease_until/attempt`，便于定位；暴露心跳 age 指标。
5. 心跳间隔不要固定 30s：lease/4 是上限，建议 lease/6~lease/8，并加 jitter，避免多会话同拍。

---

#### P0-5 `flush` 的 clone-then-clear 会丢并发 append

**位置：**
- `crates/agentscope-core/src/session/recorder.rs:570-596`

**根因：**

```text
lock -> clone pending (N) -> unlock
  ... await renew/commit ...
lock -> durable_seq = head.seq
     -> pending.clear()      // 清掉 N + 期间 append 的新事件
```

如果 `append` 在 unlock 和第二次 lock 之间执行，新事件会获得 `seq = durable_seq + N + 1`，但随后被 `clear()` 丢弃；下一次 append 又复用同一 seq。  
`SessionRecorder` 是 `Send+Sync`，ACP 中有多个任务可能写它（模型流、action observer、finish、detached tool、middleware），这个窗口是真实存在的。

**影响：**  
- 静默丢消息/工具结果；journal 与内存状态分叉。
- seq 复用导致后续 commit 内容与已提交事实冲突。
- 恢复时看到缺口，可能触发 `Stale committed prefix` / `Non-contiguous`。

**修复：**
1. `flush` 持有一个 `tokio::sync::Mutex` 串行化所有 flush，或在 recorder 内用单写队列。
2. 用 `split_off`/`drain(..n)` 在锁内把“本次要提交的 N 条”移出 pending，保留新 append；commit 成功后只确认这 N 条；失败时把它们放回队首（保持顺序）。
3. `batch_id` 与“被移出的 batch”绑定，而不是与整个 pending 绑定。
4. 加并发测试：flush 进行中 append，断言新事件不被丢、seq 连续。

---

#### P0-6 任何 commit 错误都变成 sticky failure，没有重试

**位置：**
- `crates/agentscope-core/src/session/recorder.rs:604-613`
- `crates/agentscope-core/src/session/recorder.rs:519-527`（`check`）
- `crates/agentscope-core/src/session/journal.rs:387-492`（commit 有 CAS 循环，但最终仍返回错误）

**根因：**  
`flush` 的 `Err` 分支无差别设置 `failure`，之后 `append`/`record` 全部直接失败。`JournalSessionLog::commit` 的 `Stale committed prefix`、`Commit contention`、`Writer acquisition/renewal contention`、瞬时 IO、远程存储抖动，全都被当成不可恢复。

**影响：**  
一次存储抖动 → 整个 run 失败 → ACP 报错；如果 finish 也失败，turn 被报为 interrupted；用户必须重发。长期运行下这是必然事件。

**修复：**
1. 错误分类：`retryable`（IO、CAS 争用、busy、timeout、network）vs `fatal`（数据损坏、batch id 冲突、非连续、权限）。
2. 对 retryable 做指数退避 + jitter，保留同一 `batch_id`，利用 `JournalSessionLog::commit` 的幂等回放；最多到 deadline（如 30s）再上报。
3. 不要把 transient 写入 `failure`；只有 fatal 才 sticky。
4. 对 `Stale committed prefix` 先重新 `head` + 检查 batch 是否已落盘；如果 head 被其他 writer 推进，重新打开 recorder/重新 base，而不是把整轮判死。
5. `record` 的模型 chunk 路径不应因为一次 flush 失败就中断模型流；至少应保留 chunk、继续输出，后台重试持久化（恢复时以 checkpoint 为准）。

---

#### P0-7 工具执行前没有 checkpoint，也没有 `tool/dispatch` 生产写入

**位置：**
- `crates/agentscope-core/src/agent/react_agent.rs:1764`：只在 reasoning 前发布 state。
- `crates/agentscope-core/src/agent/react_agent.rs:1851`：assistant 消息只 push 到私有 state。
- `crates/agentscope-core/src/agent/react_agent.rs:2508-2598`：acting 前没有 `recorder.checkpoint`。
- `crates/agentscope-core/src/session/views.rs:372-394`：`tool/dispatch` 才写入 `uncertain`；`action/start` 不写。
- 全仓搜索：`tool/dispatch` 只有读取方/测试写入，没有生产 `append("tool/dispatch")`。

**根因：**  
工具一旦执行，恢复需要知道两件事：

1. assistant 消息（含 tool_use）已经进入 committed state；
2. 该 tool call 已派发、但结果未知。

当前实现两件都缺：

- 状态 checkpoint 只发生在下一个 reasoning 的 `prepare_model`（`models.rs:152` → `recorder.prepare_model` → `checkpoint_now`），而工具在那之前已经执行；
- `SessionActionObserver` 只写 `action/start`/`action/end`，`SessionProjection` 不把 `action/start` 当作 uncertain；`tool/dispatch` 没有写入方。

**影响：**  
进程在工具执行中崩溃时：

- 最新 checkpoint 可能还是模型调用前的状态，assistant tool_use 丢失；
- `action/start` 存在，但 `uncertain_tool_calls` 为空；
- `abandon_turn` 不会为该工具补 synthetic interrupted result；
- 工具的真实副作用没有被记录，模型可能再次调用；或者恢复后的上下文出现 orphan tool result。

这是“任意中断后继续执行”要求的直接违背。

**修复：**
1. reasoning 返回含 tool_calls 的 assistant 消息后、**执行工具前**：
   - `recorder.checkpoint_now(state, "before_acting")`；
   - 为每个 actionable tool call `recorder.record("tool/dispatch", {toolCallId,...})`，并且与 checkpoint 同一 batch 或同一确认边界。
2. `SessionProjection::read` 把 `action/start` 也加入 `uncertain_tool_calls`，`action/end` 移除；这样即使没有 `tool/dispatch` 也能检测。
3. `abandon_turn` 根据 uncertain + 已提交的 tool_use 生成 interrupted result；如果 state 没有 tool_use，则至少把 action 记入 `recovery/applied`，不要让下一轮模型重试。
4. 工具结果写入后立即 checkpoint，再进入下一次 model call，避免“工具成功但结果丢失”。

**回归测试：**
- 模拟工具执行中进程退出（只提交 `action/start`，不提交 checkpoint/end），重新打开必须报 `Explicit reconciliation required` 或自动补 interrupted，且绝不重新执行。
- 断言恢复后 context 中每个 tool_use 都有对应 tool_result。

---

#### P0-8 ACP 默认路径的工具执行是 detached，取消不停止工具

**位置：**
- `crates/agentscope-harness/src/agent/harness_agent.rs:1409`：默认注册 `AgentTraceMiddleware`。
- `crates/agentscope-harness/src/agent/middleware/agent_trace.rs:244`：实现 `on_acting`。
- `crates/agentscope-core/src/agent/react_agent.rs:2531`：`participants(OnActing).is_empty()` 为 false。
- `crates/agentscope-core/src/agent/react_agent.rs:2625`：`tokio::spawn(async move { run_acting_calls(...) })`。
- `crates/agentscope-core/src/agent/react_agent.rs:2649-2831`：工具循环中没有 `check_interrupted`。

**根因：**  
当有任意 `on_acting` 中间件时（ACP 默认有 `AgentTraceMiddleware`），工具执行被 `tokio::spawn` 到独立任务，返回一个 `rx` 流。`acting_phase` 只消费 `rx`；一旦 agent future 被丢弃/取消，`rx` 被 drop，但 spawned task 没有 `JoinHandle` 保存、没有 abort，工具继续运行。  
此外，`run_acting_calls` 在一个 tool 完成后不会检查中断，所以一次 batch 中的所有工具都会跑完，即使第一下之后用户已经取消。

**影响：**
- 用户按 Stop 后工具仍在写文件/执行 shell；下一轮可能同时操作同一资源。
- 取消后 run 被关闭（recorder close），工具稍后 `observe(action/end)` 写已关闭 recorder，结果丢失并报错。
- 工具超时 `tokio::time::timeout` 只 drop tool future，不代表外部进程/线程停止；shell 工具可能留下子进程。

**修复：**
1. 用结构化并发：`JoinSet` + `CancellationToken`，由 `acting_phase` 持有；流被 drop/取消时 `abort_all()` 并等待（有上限），或至少标记为 uncertain 并持久化。
2. `run_acting_calls` 在每次工具前和工具后检查 `check_interrupted`；取消后不再启动后续工具。
3. 不要 `tokio::spawn` 后丢掉 handle；把 handle 放进一个 guard，在 drop 时取消。
4. 对 shell/外部进程增加 kill-on-drop（进程组 kill），确保超时/取消真正停止副作用。
5. 如果工具不可取消，明确标记 `interrupted/uncertain`，禁止自动重试。

---

#### P0-9 关闭/退出没有 drain ACP run，租约和 turn 留在 journal

**位置：**
- `crates/agentscope-core/src/agent/react_agent.rs:534`：`ExecutionGuard::acquire()` 只在 `call_with_format_and_extensions`。
- `crates/agentscope-core/src/agent/react_agent.rs:723-834`、`845-861`：ACP 路径走 `run_stream_with_extensions`/`stream_events_with_extensions`，没有 `ExecutionGuard`。
- `crates/agentscope-extensions-acp/src/acp_harness_agent.rs:1167`、`1171`：`run_stdio` 返回后 `main` 直接结束。
- `crates/agentscope-extensions-acp/src/acp_harness_agent.rs:1227-1246`：shutdown handler 轮询 `GracefulShutdownManager`（计数恒为 0）后 `process::exit(130)`。
- `crates/agentscope-core/src/session/recorder.rs:130-166`：`Drop` 里 `spawn(release)`，不等待。
- `crates/agentscope-core/src/agent/react_agent.rs:145-184`：`RunGuard` 同样 `spawn(close)`，不等待。

**影响：**
- IDE 重启/关闭 stdin/连接断开时，`main` 返回，tokio runtime 关闭，进行中的 `SessionRecorder`/工具任务被 drop；`Drop` 里 spawn 的 release 来不及执行。
- head 保留 owner 120s；下一进程需要等待/重放，且极易触发 P0-1。
- Ctrl-C/SIGTERM 的 shutdown handler 也等不到 stream run（计数 0），10s 后强退；不会写 `turn/cancelled`，也不会释放 lease。
- 现场日志“进程在 16:53 半轮工具调用中结束”就是例证。

**修复：**
1. 把 `ExecutionGuard` 也接入 `run_stream_with_extensions`/`stream_events_with_extensions`（或用独立的 `RunRegistry` 记录所有活跃 run）。
2. ACP shutdown：
   - 监听 stdin EOF / transport close，不只是 Ctrl-C/SIGTERM；
   - 停止接收新 prompt；
   - 对每个活跃 session 调用 cancel + `abandon_unfinished`；
   - 等待 `finish/close` 完成（有上限，如 30s）；
   - 最后再退出；不要直接 `process::exit`。
3. `SessionRecorder::Drop` / `RunGuard::Drop` 不能只 spawn；至少：
   - 在 runtime 还活着时用 `spawn` 并在 shutdown 阶段等待；
   - 或写一个“release tombstone”让下一进程快速接管；
   - 或把租约缩短到 5-10s，并允许同 owner 过期后自愈（P0-3）。
4. 增加 SIGKILL/断电级测试：即使无法执行 Drop，下一进程应在短时间接管，且 launch 不因旧 lease 报错。

---

#### P0-10 文件存储 CAS 不是跨进程原子，且没有 fsync

**位置：**
- `crates/agentscope-harness/src/agent/session/storage.rs:62-92`（value/version 两文件）
- `crates/agentscope-harness/src/agent/session/storage.rs:152-183`（`compare_and_set`）
- `crates/agentscope-harness/src/agent/session/storage.rs:84-92`（`replace` rename）
- `crates/agentscope-harness/src/filesystem/local.rs:487-498`（`move_path` 使用 `tokio::fs::rename`）
- `crates/agentscope-harness/src/agent/session/sqlite.rs:42-65`（SQLite 每次连接、同步调用）

**根因：**
- `compare_and_set` 的互斥只有进程内 `locks: Mutex<HashMap<...>>`；跨进程两个 writer 可以同时读到相同 version，然后各自写 value/version。
- value 与 `.version` 是两次独立写；崩溃可能落在中间，留下“新 value + 旧 version”或反之。
- 没有 `sync_all`/目录 fsync；`Flush` 返回成功不代表断电后仍在。
- `replace` 先写临时文件再 rename，但没有 fsync 文件/目录。

**影响：**
- 两个 ACP 进程/两个 IDE 窗口同一 session 时可能丢 commit、head 回退、`Stale committed prefix`。
- 断电/蓝屏后已确认的 turn 可能消失，恢复状态与客户端看到的不一致。
- 与 P0-1/P0-6 叠加，产生用户可见错误。

**修复：**
1. 本地 ACP 默认改用 SQLite（单行 CAS，天然跨连接/进程原子；`synchronous=FULL` 已有）。
2. 若保留文件后端：
   - 用文件锁（advisory lock）+ 单文件 envelope（version 与 payload 同一文件，rename 原子替换）；
   - `fsync` 文件 + 父目录；
   - 或使用 `O_EXCL` lock file + owner/epoch/PID/start-time，进程死亡可检测。
3. 明确文档：`WorkspaceSessionStorage` 只保证单进程；多进程必须用 `StoreSessionStorage`/SQLite。ACP 默认路径要选对。
4. 增加两个进程并发 CAS 的集成测试（不是两个线程）。

---

#### P0-11 journal 默认写在工程目录 `agents/`，模型工具可见

**位置：**
- `crates/agentscope-harness/src/agent/session/storage.rs:224-233`（`WorkspaceSessionLogStore::open`）
- `crates/agentscope-harness/src/agent/harness_agent.rs:1315-1318`（默认 `LocalFilesystem::new(cwd)`）
- `crates/agentscope-core/src/session/key.rs:35-44`（`agents/...`）
- `.gitignore` 仅把 `agents` 忽略，不阻止工具读写。

**影响：**
- 模型的文件工具可以 `ls/read/edit/delete` journal；一次误操作就会破坏会话。
- 工程目录被 93 万文件/5.7GB 污染；文件搜索、workspace index、备份、杀毒都会变慢。
- 会话内容（包括密钥/隐私）以明文落在工作区，可被模型/其他进程读取。
- 多个项目/会话共享 cwd 时，路径和清理策略不清晰。

**修复：**
1. journal 放到独立数据目录（如 `~/.agentscope/sessions/<workspace-hash>/...` 或 `%LOCALAPPDATA%`），不要放在 cwd。
2. 如果必须放 cwd，放到 `.agentscope/sessions` 且在文件系统工具/workspace 索引中显式排除；设置权限。
3. 提供 retention/GC：按时间/大小清理旧 session；至少不要无限增长。
4. ACP 启动时输出 journal 根路径，便于用户清理。

---

#### P0-12 `MAX_PENDING_BYTES=16MB` 会让大 checkpoint 的终态写入失败

**位置：**
- `crates/agentscope-core/src/session/recorder.rs:49`
- `crates/agentscope-core/src/session/recorder.rs:549-550`
- `crates/agentscope-core/src/session/recorder.rs:728-830`（`capture_state`）
- `crates/agentscope-core/src/session/recorder.rs:901-943`（`finish_inner`）

**根因：**  
`capture_state` 把完整 `AgentState` JSON 作为 `state/checkpoint`，还可能先追加 `context/replaced` + 所有 fresh message；这些都在 pending 中累积，`append` 对单次 payload 与累计 bytes 做 16MB 检查。长会话/大工具结果很容易超过。

**影响：**  
`finish` 失败 → `turn/*`/`run/end` 写不进去 → receipt 报 interrupted；日志已出现 `Session pending buffer exhausted`。  
运行中 `before_step`/`prepare_model` 的 checkpoint 也可能撞上限，直接中断 run。

**修复：**
1. 大 checkpoint 不要走 pending buffer：直接写 blob（内容寻址）并在 commit 中引用；commit 本身只带引用。
2. `capture_state` 拆分为多个 commit（消息、状态、task/plan 分片），或在 checkpoint 前先 flush 旧 pending，再单独提交 checkpoint。
3. `MAX_PENDING_BYTES` 只限制“可重放的小事件”，对单个必需的大事件给出明确上限/流式写入，而不是直接失败。
4. 对 `context/replaced` 做引用/摘要，不要复制整个 context 到 pending。

---

#### P0-13 `turn_unfinished`：上一轮 turn 未终结时，ACP 直接拒绝新 prompt

**用户看到的文案：**

```text
Agent error: The previous turn is still open, so this prompt could not start.
Wait for it to finish or cancel it, then send the prompt again.
```

**位置：**
- `crates/agentscope-harness/src/agent/session/session.rs:1015-1017`：`view.open_turn.is_some() && command.kind == "submit"` 时 `inbox.reject(command, "turn_unfinished")`。
- `crates/agentscope-extensions-acp/src/session.rs:1788-1805`：ACP 把 `turn_unfinished` 翻译成用户文案并返回 `StopReason::Refusal`。
- `crates/agentscope-extensions-acp/src/session.rs:1392-1398`：新 prompt 前只做 best-effort `session.abandon_unfinished()`，失败仅 warn，然后仍然 `submit`。
- `crates/agentscope-harness/src/agent/session/session.rs:446-484`：`abandon_unfinished` 只等 `active_run` 15s；`abandon_turn` 抢 writer 约 5s（`recovery.rs:27-58`）。
- `crates/agentscope-core/src/session/views.rs:44-46`：`Turn::ended` 只认 completed/cancelled/failed；`interrupted`/`suspended` 故意不是 terminal。
- `crates/agentscope-core/src/session/inbox.rs:268-282`：`inbox.reject` 写终态，之后该 command 永久 rejected。

**根因：**  
这条错误本身不是“租约错误”，而是 **turn 生命周期没有收敛**：

1. 上一轮 run 被取消/中断/挂起后，turn 仍是 `interrupted` 或 `suspended`，按设计不是 terminal；
2. ACP 新 prompt 前调用 `abandon_unfinished()`，但它可能因为以下原因失败：
   - 上一轮 run 的 `active_run` 还在（工具/权限/elicitation/存储卡住）；
   - 旧 recorder/heartbeat 还持有 writer lease；
   - `abandon_turn` 5s 内抢不到 writer；
   - `finish` 失败导致根本没有 `turn/*` 终态事件；
   - 旧 runtime/重复 `AgentSession` 在 abandon 之后又写入了 `run/start`；
3. `abandon_unfinished` 失败只 warn，随后仍然 `submit`；
4. dispatcher 看到 `open_turn` 仍存在，直接 `inbox.reject("turn_unfinished")`；
5. ACP 把这个拒绝对用户显示成 “previous turn is still open”，并返回 Refusal。

**影响：**  
- 用户下一条消息永久丢失（该 command 被 rejected），必须手动重发；
- 如果 `finish` 失败/旧 writer 未释放，turn 会一直 open，后续每条 prompt 都报同一错误；
- 与 P0-3/P0-9/P0-12/P1-3/P1-9 相互放大：租约/storage 问题会让 abandon 失败，abandon 失败又让这条错误变成粘性。

**这条错误能否被“以上优化”修掉？**

- **只做 Phase 0 的租约/心跳/journal 优化：只能降低概率，不能保证消除。**  
  因为即使租约永远不丢，只要上一轮 run 真的还在跑、`abandon_unfinished` 因为工具/权限卡住而超时、或者 `finish` 没写终态，dispatcher 仍然会 reject。
- **做完报告里的全部 P0 + P1（尤其是 P0-7/8/9/13 + P1-3/5/6/7/9）：可以消除这个用户可见错误。**  
  但必须把它当成独立的“turn 状态机”问题修，不能只靠“abandon 再试一次”。

**修复（必须同时做）：**

1. **ACP 在 submit 前建立确定性状态机**，不要 best-effort：
   ```text
   loop {
     open_turn = session.open_turn()
     if none: break
     latest = status/run_id
     match latest:
       running/starting:
         // 上一轮仍活着：等待，或按用户明确取消后 abort 并等待
         wait_or_cancel(run_id, deadline)
       interrupted:
         // 可恢复：resume（或用户明确选择关闭）
         resume(turn_id)
       suspended:
         // 等待外部结果/respond，不要 submit
         wait_for_respond(turn_id)
       orphaned (无 active_run、无 writer):
         abandon_unfinished() 重试到终态
     if deadline exceeded:
       // 不要把用户消息 reject；保持 queued，并返回可重试/忙碌状态
       return retryable_busy
   }
   submit()
   ```
2. **`abandon_unfinished` 必须保证收敛：**
   - 先按 run id 取消并等待 run 结束（有上限）；
   - 抢 writer 用指数退避，覆盖一个 lease 周期，不要 5s/15s 就放弃；
   - 同时关闭 main turn 和 inbox turn；
   - 写 `turn/cancelled|interrupted` 终态；
   - 返回前再次读 `open_turn`，直到 None；否则保留“close requested”持久标记，下一 tick 继续。
3. **dispatcher 不要用 `inbox.reject("turn_unfinished")` 终结用户消息：**
   - 如果 open turn 属于 dead/orphaned run：自动关闭后继续；
   - 如果 run 仍 active：保持 `queued`，让 ACP 等待/取消，而不是永久 reject；
   - 如果必须让调用方知道，返回可重试的 `busy/needs_resume` 状态，不要 `Refusal`。
4. **`finish` 绝不能静默失败：**
   - 所有路径（正常、取消、stall、panic、shutdown）都必须尝试写 terminal fact；
   - 写失败时留下一个可恢复的 tombstone/recovery marker，下一次 open 自动把 turn 标为 interrupted/cancelled；
   - 不能出现“客户端已收到结束，但 journal 仍 open”。
5. **prompt lock 生命周期与 turn 对齐：**
   - 只有 `open_turn == None`（或已进入明确的 queued 状态）才释放 prompt lock；
   - 取消路径 10s 超时后不要只是 `abandon_turn` 一次然后返回；保留 per-turn close 请求，阻止下一条 prompt 抢跑。
6. **防止旧 runtime 复活：**
   - evict/close/delete 不能留下仍能写 journal 的旧 `AgentSession`；
   - 每个 session key 进程内唯一 runtime；close 等待旧 run 退出或标记 generation；
   - 旧 runtime 的 `run/start` 不得在 abandon 之后落盘（否则 open turn 会重新出现）。

**验收测试：**
- 长工具/权限等待中 cancel，**立刻**发送下一条 prompt：应 EndTurn，不出现 `turn_unfinished`。
- `finish` 注入存储失败，重启后发送 prompt：应自动修复/abandon，不出现该错误。
- 两次 prompt 并发：第二条排队，第一条完成后正常执行；取消第二条不得影响第一条。
- 旧 runtime 仍在写 journal 时新 session 打开：不得出现 open turn 复活/重复执行。
- kill -9 在 turn 中途，重启后发“继续”：应 resume 或明确关闭，不返回 Refusal。

---

### 2.2 P1（长期运行/恢复语义问题）

#### P1-1 action observation 失败被降级成可重试的 tool result，且丢失真实结果

**位置：**
- `crates/agentscope-core/src/observation/mod.rs:238-274`
- `crates/agentscope-core/src/agent/react_agent.rs:2703-2748`、`2771-2811`
- `crates/agentscope-core/src/error.rs:64-67`

**问题：**  
`observe` 文档说 `ActionObservation` 不可重试，但 `run_acting_calls` 在 `Err(e)` 时：

```text
results.record(error_result(call, &err_msg), false);
```

把 observation 失败变成一条错误 tool result，模型会看到它并可能重试；如果失败发生在 action/end（工具已执行），真实 `rb` 被丢弃，模型重试会造成重复副作用。  
这与“工具已执行、不可重放”的语义相反。

**修复：**
1. action/start 失败（工具未执行）：可以安全重试，重试耗尽后再上报。
2. action/end 失败（工具已执行）：**保留真实 `ToolResultBlock`**，把 observation 写成 uncertain/outbox；绝不让模型看到“可重试错误”。
3. 引入持久化 outbox/重试队列；恢复时对未确认 observation 做 reconcile，而不是重跑工具。
4. 如果无法持久化，至少把工具结果写入 state/checkpoint，并把 action 标为 unverified，禁止自动重试。

---

#### P1-2 模型流部分输出在崩溃后不可恢复

**位置：**
- `crates/agentscope-core/src/session/models.rs:159-244`（chunk 记录 + model/end）
- `crates/agentscope-core/src/session/views.rs:346-428`（投影不消费 chunk）
- `crates/agentscope-core/src/agent/react_agent.rs:1827-1832`（只有内存 `partial_assistant`）

**问题：**  
模型流中途崩溃时，journal 里有 `model/chunk`，但 `SessionProjection` 只取 `state/checkpoint`；`partial_assistant` 只在当前进程内存。重启后用户已经看到的半截回答不会被恢复，下一轮从旧 checkpoint 开始，可能重复/断层。  
`model/dispatch`/`model/end` 也没有状态机（只有 `request/prepared`），无法判断“模型调用是否已发出、是否结果未知”。

**修复：**
1. 在 `model/end`/step 边界 checkpoint assistant 消息；或从 `model/chunk` 重建 partial assistant（如果保留 chunk）。
2. 在 projection 中跟踪 model call 状态；对 `request/prepared` 无 `model/end` 的调用标记 incomplete。
3. 明确恢复策略：丢弃 partial（状态回到 checkpoint）或继续（保留 partial + 让模型继续）。二者必须一致，不能客户端已看到文本但 journal 没有。
4. 对 partial 输出失败/重试设置可见性规则，避免重复输出。

---

#### P1-3 ACP 每次新 prompt 都先 abandon 未完成 turn；重启后无法真正继续

**位置：**
- `crates/agentscope-extensions-acp/src/session.rs:1392`：`session.abandon_unfinished()`
- `crates/agentscope-extensions-acp/src/session.rs:1812-1842`：只有“本次 prompt 等待期间被 host interrupt”才 `session.resume`
- `crates/agentscope-harness/src/agent/session/session.rs:446-484`：`abandon_unfinished`

**问题：**  
进程崩溃/IDE 重启后，`AcpAgentSession` 是新的；用户发“继续”时，`run_prompt_inner` 无条件 `abandon_unfinished()`，把上一轮的 interrupted turn 标成 cancelled，再加一个新 turn。  
`session.resume` 只在 prompt loop 还活着、看见 `status=interrupted` 时使用；重启后没有这条路径。因此“恢复后继续执行原任务”实际上做不到，只能“重新发一个新 prompt”。

**修复：**
1. 启动/首个 prompt 时检查 `open_turn`：
   - 若是 interrupted 且用户意图是继续，调用 `resume`；
   - 或给客户端一个明确的 `session/continue`/`resume turn` 能力；
   - 不要默认 abandon。
2. 持久化 prompt/turn 的 idempotency key，让客户端重试/重启能关联到同一 turn。
3. `abandon_unfinished` 应是显式用户操作或超时策略，不是每次 prompt 的前置动作。

---

#### P1-4 ACP prompt 没有稳定幂等键，客户端重试会重复用户消息

**位置：**
- `crates/agentscope-extensions-acp/src/session.rs:1399`：每次 `run_prompt_inner` 生成新的 `Uuid`。
- `crates/agentscope-harness/src/agent/session/session.rs:345-377`：幂等键只在本进程内用于 `submit` 重试。
- ACP `PromptRequest` 没有把 JSON-RPC request id / 客户端 turn id 传入。

**问题：**  
进程崩溃、响应丢失、IDE 重试时，客户端会重新发 prompt；ACP 每次都生成新 key → 新 turn → 重复用户消息，甚至重复工具副作用。  
`submit_with_key` 的幂等只在同一次 `run_prompt_inner` 内部有效。

**修复：**
1. 用 ACP JSON-RPC request id（或客户端提供的 idempotency key）派生稳定 turn id。
2. 在 journal 中持久化 `promptRequestId -> turnId`；重复请求直接返回同一 receipt。
3. 对“同内容 + 同 session + 短时间”做去重（谨慎，不能误合并用户真实重复消息）。
4. ACP 协议层若无法提供 id，增加自定义 `session/prompt` metadata 或本地持久化映射。

---

#### P1-5 cancel 是 session 级共享 flag，取消会串轮次

**位置：**
- `crates/agentscope-extensions-acp/src/session.rs:500`（`AtomicBool`）
- `crates/agentscope-extensions-acp/src/session.rs:1076-1078`（`cancel`）
- `crates/agentscope-extensions-acp/src/session.rs:1294`（每个 prompt 开始 `store(false)`）
- `crates/agentscope-extensions-acp/src/coding_agent.rs:476-483`（CancelNotification 只带 session_id）

**问题：**  
- ACP 的 `session/cancel` 只有 session id，没有 prompt/request id；`cancel` 是共享 `AtomicBool`。
- 如果 prompt B 在 `prompt_lock` 上等待，客户端取消 B，`cancel=true` 会让正在跑的 A 停止；B 开始后又 `store(false)`，取消丢失。
- 一个挂起的 prompt 会一直持有 `prompt_lock`，后续 prompt 永久等待。

**修复：**
1. 每个 prompt 一个 `CancelToken`，按 JSON-RPC request id / turn id 存入 map；`session/cancel` 若无法区分，至少取消“当前运行 + 最新排队”并明确协议语义。
2. 不要把 cancel 存在 session 级并每轮 reset。
3. `prompt_lock` 获取用 `select!` 监听取消/超时；不要无限等待。
4. 对排队 prompt 提供 reject/cancel 回执。

---

#### P1-6 权限/elicitation/存储等待没有超时，cancel 也无法打断

**位置：**
- `crates/agentscope-extensions-acp/src/session.rs:90-114`：`request_permission` / `elicit` 使用 `block_task().await`，无 timeout。
- `crates/agentscope-extensions-acp/src/session.rs:204`、`2757`：confirm handler 内联 await。
- `crates/agentscope-extensions-acp/src/session.rs:1496-1545`：task 轮询无 timeout 的 `session.task()`。
- `crates/agentscope-harness/src/agent/session/session.rs`：大量 journal 操作无 timeout。

**问题：**  
用户不回答权限表单/elicitation，或客户端连接卡住，工具会永久等待；`agent.interrupt` 不会中断这个 await。ACP cancel 循环 10s 后尝试 `abandon_turn`，writer 仍被持有，报 `Session writer is still busy`（现场日志已出现）。  
存储挂起（远程 store、网络盘、SQLite busy）同样可能无限等待。

**修复：**
1. 给所有 client 请求（permission/elicitation/MCP）加 timeout + 与 cancel token 的 `select!`；取消时返回 denied/cancelled，并记录。
2. 给所有 journal 操作加整体 deadline（例如 30s），区分 transient。
3. `prompt_lock`/`session.task`/`abandon` 全部可取消。
4. 超时后不要把 turn 留在 open；写 `turn/cancelled`，或进入明确的 suspended 状态。

---

#### P1-7 ACP 会话注册表/load/resume/delete/evict 语义不完整

**位置：**
- `crates/agentscope-extensions-acp/src/coding_agent.rs:257-303`（evict/remove）
- `crates/agentscope-extensions-acp/src/coding_agent.rs:602-659`（load/resume/delete）
- `crates/agentscope-extensions-acp/src/coding_agent.rs:319-343`（list 只查内存）
- `crates/agentscope-extensions-acp/src/session.rs:1081-1089`（close 用 `try_lock` 非阻塞）

**问题：**
- `session/list` 只返回内存注册表；进程重启后为空（尽管 journal 存在）。客户端无法发现历史会话。
- `LoadSession`/`ResumeSession` 对未知 id **静默创建空会话并返回成功**，拼错 id 会得到“空历史”。
- `DeleteSession` 只从内存移除，不 cancel 运行中的 prompt、不删 journal；prompt 完成后 `register_session` 可能把它重新加回（`closed` 未置位）。
- `max_sessions` 淘汰时优先 idle，但所有会话都在运行时仍会淘汰一个 running 会话；其 prompt 仍持有副本，完成后可能重新注册/与新建的同 id runtime 并存。
- `close` 的 `try_lock` 失败时 runtime 不释放，旧 `AgentSession` 可能继续 dispatch；新 load 可能创建第二个 runtime。

**修复：**
1. 持久化 session index（可复用 journal `SessionLogStore::list`），`session/list` 从 store 读。
2. `load/resume` 校验 journal 是否存在；不存在返回明确错误，不创建空会话。
3. `delete` 先 `close`/cancel，再删 journal；设置 `closed`，禁止重新注册。
4. 容量满且全部 running：拒绝新会话，或排队，不要淘汰 running。
5. 每个 session key 保证只有一个 `AgentSession`（全局 registry + 锁）；close 要等待旧 runtime 退出或标记 generation。

---

#### P1-8 panic / mutex poisoning / 未捕获异常会永久卡死 dispatcher 或客户端

**位置：**
- `crates/agentscope-harness/src/agent/session/session.rs:728-746`（`start` spawn 的 dispatch loop 没有 catch/restart）
- 同文件大量 `state.lock().unwrap()`（316、325、471、729、745、787、794、871、935、951、978、990、1046、1066、1083、1122、1126、1214、1266、1280、1298、1362、1380）
- `crates/agentscope-extensions-acp/src/coding_agent.rs:61`（`expect`）、`acp_harness_agent.rs:814`（`panic!` build model）
- ACP handlers / prompt task 没有 `catch_unwind`，panic 后 responder 可能永不响应。

**问题：**
- dispatch task panic 后 `state.stop` 仍是 `Some`，`start()` 不会重启；后续 prompt 一直 queued → 300s 后 Refusal。
- 任何持锁 panic 会 poison `std::sync::Mutex`；之后所有 `.lock().unwrap()` 连锁 panic。
- 动态切换模型构建失败会 `panic!`，触发 prompt task panic，客户端 hanging。
- `register_session`/`create_session` 在 handler 中 panic 可能破坏连接。

**修复：**
1. `state`/`config`/`client` 等所有 mutex 使用 `unwrap_or_else(|p| p.into_inner())` 或封装 `lock_recover()`。
2. dispatch loop 用 `catch_unwind` 包裹每次 tick；panic 后记录、清理 active state、继续下一 tick（或重启任务）。
3. ACP prompt task 用 `catch_unwind`/`AssertUnwindSafe`，保证 `responder` 一定收到 error response；清理 `running`。
4. `build_model` 改为返回 `Result`；`create_session` 失败返回 ACP internal error，不 panic。
5. 对 `process::exit` 之外的所有任务加 panic hook/metrics。

---

#### P1-9 Inbox 租约过长 + 重试过短 + transient reject 永久化

**位置：**
- `crates/agentscope-core/src/session/inbox.rs:17`（`INBOX_LEASE=120s`）
- `crates/agentscope-core/src/session/inbox.rs:100`、`302-320`（100 次 × 10ms = 1s）
- `crates/agentscope-core/src/session/inbox.rs:268-282`（`reject` 写终态）
- `crates/agentscope-harness/src/agent/session/session.rs:1164-1196`（open 3 次 × 100ms）
- `crates/agentscope-harness/src/agent/session/session.rs:1225-1232`（prepare 失败直接 reject）

**问题：**
- inbox 操作很短，却用 120s lease；进程在 accept 中崩溃后，其他进程最多等 120s，但 `acquire` 只重试 1s → 用户 prompt 直接被拒。
- `launch` 打开 recorder 只重试 3×100ms；SQLite busy_timeout 是 10s，release 也可能慢，远不够。
- `execution.prepare` 的任何错误都 `inbox.reject` 永久拒绝；transient 存储错误也会让用户消息丢失。
- `reject` 本身失败时命令仍 queued，可能无限重试/超时。

**修复：**
1. inbox lease 降到 5-10s；`acquire` 等待直到 lease 过期 + 少量 margin，而不是固定 1s。
2. 所有 accept/reject 使用幂等 batch id；transient 错误重试。
3. `launch` 打开 recorder 用指数退避（至少覆盖一个 lease 周期），或等待 active writer 释放；不要 3 次就 reject。
4. `prepare` 的 transient 错误保留 queued，交由 admission deadline；只有业务校验错误才 reject。
5. 对 reject 加幂等/补偿，确保客户端拿到明确错误而不是 queued 卡死。

---

#### P1-10 suspended / external execution 在 ACP 中不可恢复

**位置：**
- `crates/agentscope-extensions-acp/src/session.rs:1860-1875`：`status != "completed"` 直接 `abandon_turn` + Refusal。
- `crates/agentscope-core/src/session/execution.rs` / `SessionActionObserver` 已记录 `interaction/requested`。
- ACP 没有 `session/respond` 处理器。

**问题：**  
工具 `suspend`（外部执行）时，run 结束为 `suspended`，turn 不是 terminal；ACP 把它当错误，`abandon_turn` 把 pending interaction 全部取消，用户无法再回答。  
即使 journal 有 pending request，下一次 prompt 也会因为 pending interactions 被 `prepare` 拒绝或 abandon 掉。

**修复：**
1. ACP 增加 `respond` / `session/continue` 路径，保持 turn open，等待外部结果。
2. `PromptResponse` 用 `StopReason` 的 suspended 语义（若协议支持）或明确的自定义通知；不要 Refusal + abandon。
3. 重启后从 `pending_interactions` 恢复并让客户端重新回答。
4. 如果暂不支持，启动时明确拒绝注册会 suspend 的工具，而不是运行时丢 turn。

---

#### P1-11 全量投影/缓存无限增长，长会话 CPU/内存持续恶化

**位置：**
- `crates/agentscope-core/src/session/views.rs:61-161`（`SessionTurns::read` 全量）
- `crates/agentscope-core/src/session/views.rs:336-472`（`SessionProjection::read` 全量）
- `crates/agentscope-core/src/session/views.rs:523-531`（`pending_interactions` 全量）
- `crates/agentscope-harness/src/agent/session/session.rs:1040-1089`（`execution_view` 缓存）
- `crates/agentscope-core/src/session/execution.rs:203-246`（`before_step` 每次 new inbox + full fold）
- `crates/agentscope-core/src/session/inbox.rs:142-203`（inbox 全量 fold/缓存）
- `crates/agentscope-core/src/session/recorder.rs:728-830`（每次 checkpoint 全量 state）

**问题：**
- 每次 cold turn 仍要做 `SessionTurns::read` + `pending_interactions` + `SessionProjection::read` + `log.scan` + `transcript`。
- `execution_view` 缓存整个 `SessionTurns`（含所有 turn inputs，可能含图片 base64）和 pending；无上限。
- `SessionInbox.cache` 保存所有 accepted commands（含完整消息），无上限。
- `SessionExecution::before_step` 每个 reasoning step 都新建 `SessionInbox` 并全量 fold；max_iters=9999 时可放大到不可接受。
- `capture_state` 每个 iteration 序列化整个 state；journal 中保留每个 checkpoint。

**影响：** 长会话越来越慢、内存越来越大，最终重放超过 lease，回到 P0-1。

**修复：**
1. 所有 projection 改为增量（已有 `apply_event`，但 cold 仍全量；需要持久化索引/最新 checkpoint）。
2. `execution_view` 不缓存所有 inputs，只缓存 `turn_id -> status/run_id` 和必要的最新输入；历史输入从 journal 按需读。
3. `SessionInbox` 缓存只保留未处理/最近 N 条；已处理命令落盘。
4. `before_step` 复用同一个 `SessionInbox`/cache，不要每个 step 重建。
5. journal checkpoint 保留最近 N 个 + 摘要；旧 checkpoint 压缩。
6. 给每个 session 设置内存/事件上限，超限归档/提示。

---

#### P1-12 unbounded event channel + 同步 emit 可能拖垮/阻塞会话

**位置：**
- `crates/agentscope-extensions-acp/src/session.rs:1378-1383`：`tokio::sync::mpsc::unbounded_channel`。
- `crates/agentscope-core/src/agent/react_agent.rs:773`：`futures::channel::mpsc::unbounded` delta channel。
- ACP `emit` 直接调用 `send_notification`，无 try/backpressure。

**问题：**  
客户端读得慢/网络阻塞时，事件在无界队列堆积，内存无限增长；`emit` 阻塞时 prompt loop 无法检查 cancel/心跳 ticker，取消失效。

**修复：**
1. 用有界 channel（例如 256-1024 事件），对 delta 做合并/丢弃策略；终态/工具事件不能丢。
2. 独立的 writer task 负责 `send_notification`，prompt loop 只入队；队列满时阻塞读取或合并。
3. 监控队列长度/丢弃数；连接关闭时取消 run。

---

#### P1-13 `model/chunk` 写入与模型流强耦合，存储抖动会截断回答

**位置：**
- `crates/agentscope-core/src/session/models.rs:159-244`
- `crates/agentscope-core/src/agent/react_agent.rs:2328-2355`（partial output retry 抑制）

**问题：**  
每个 chunk 在 yield 给客户端前先 `record`；一旦持久化失败，流立即 `break`，客户端可能已经看到前半段文本，run 失败。  
重试策略又因为“已经输出可见文本”而抑制重试（`retry_suppressed_error`），所以这个失败没有安全恢复路径。

**修复：**
1. 解耦：先把 chunk 交给客户端/内存累积，持久化异步批量；失败只影响 journal ack，不截断模型流。
2. 或者持久化失败时保留当前 partial，标记 incomplete，让下一轮从 partial 继续/重试。
3. 明确“可见输出”与“已持久化”两个状态；不要用可见输出来阻止所有恢复。

---

#### P1-14 损坏/未知事件会让整个 session 永久不可打开

**位置：**
- `crates/agentscope-core/src/session/views.rs:346-428`：`SessionEventTypes::validate` / `event.data()?` 任一失败中止整个 projection。
- `crates/agentscope-core/src/session/journal.rs:128-174`：`read_commit` 校验失败直接返回。
- `crates/agentscope-core/src/session/inbox.rs:191-193`：未知 inbox 事件直接错误。
- `crates/agentscope-core/src/session/recovery.rs:118-123`：投影失败则无法 abandon。

**影响：** 一个损坏 commit / 新版本写入的未知事件 / 一条旧格式脏数据，会让所有 resume/abandon 失败；用户只能手动删 session。升级/回滚时尤其危险。

**修复：**
1. 读取时隔离坏 segment：记录 `corrupt_seq`，从最近 good checkpoint 恢复；把损坏段移到 quarantine。
2. 未知事件按 `required` 字段决定：非 required 忽略并记录；required 才中止，并提供修复工具。
3. 提供显式 `repair`/`inspect` 命令（截断到 seq、忽略坏 commit、重建索引）。
4. 写路径做 schema 版本兼容测试（旧读新、新读旧）。

---

#### P1-15 `Stale committed prefix` / CAS 争用没有重基（rebase）策略

**位置：**
- `crates/agentscope-core/src/session/journal.rs:387-492`
- `crates/agentscope-core/src/session/recorder.rs:604-613`

**问题：**  
`commit` 发现 `head.seq != expected_seq` 直接 `Stale committed prefix`；`flush` 把它变成 sticky failure。  
对于同一 writer 的 lost ack，代码有 batch id 回放；但对于“另一个 writer/陈旧 runtime 已推进 head”的情况，没有“重新读取、重新基于最新 head 继续”的策略。  
在 ACP 多进程/evict/旧 runtime 未退出的场景中，这是用户可见错误。

**修复：**
1. 区分：
   - 同一 batch 已提交 → 返回成功（已有）；
   - head 只前进且没有本 batch → 本 recorder 已失去所有权：停止并重新 open/replay（transient 可重试）；
   - 数据不连续/损坏 → fatal。
2. 不要把所有 stale 都变成用户终态错误；让上层 `launch` 重新打开 recorder（带指数退避），或在 turn 结束前对齐。
3. 加单 writer 约束：每个 session key 在进程内一个 `SessionWriter` 令牌；多进程用真正的 CAS/lease。

---

### 2.3 P2（其他稳定性/可观测性风险）

| 编号 | 问题 | 位置 | 修复 |
|---|---|---|---|
| P2-1 | `SessionRecorder::open` 在 `log.head()` 失败时不释放已 acquire 的 writer，lease 泄漏 120s | `recorder.rs:355-356` | 用 guard/`scopeguard` 确保 release |
| P2-2 | `LeaseHeartbeat::start` 无 runtime 时静默 `None`，用户以为有保护 | `recorder.rs:180-201` | 返回 `Result`/fallback inline renew/告警 |
| P2-3 | `SessionRecorder::Drop` / `RunGuard::Drop` 只 spawn release，进程退出时不执行 | `recorder.rs:130-166`、`react_agent.rs:145-184` | shutdown 阶段显式 await；或缩短 lease + tombstone |
| P2-4 | `SessionLogExporter::drain` 每次 flush 全量 scan；ACP 未启用但也应修 | `export.rs:114-131` | 增量游标 + 批量 |
| P2-5 | `save_session_config` 非原子写，崩溃可能留下空 config | `acp/session.rs:428-445` | 临时文件 + rename + fsync |
| P2-6 | MCP server 先 `bridged.push(name)` 再 connect；失败后本 session 永不重试 | `acp/session.rs:891-899`、`937-943` | 成功后再标记；失败可重试/超时 |
| P2-7 | MCP connect/elicitation/permission 无总超时，NewSession 可挂起 | `acp/session.rs:831-978` | 每步 timeout + cancel |
| P2-8 | `session_infos` 按 UUID 排序并注释“id 含时间戳”，错误但非致命 | `coding_agent.rs:319-343` | 按 updated_at/created_at 排序 |
| P2-9 | `Usage`/`last_input_tokens`/`updated_at` 在 clone 间不共享，配置 handler 可能覆盖旧值 | `acp/session.rs` 字段 | 改为 Arc/共享状态 |
| P2-10 | 大量 `std::sync::Mutex::lock().unwrap()`；panic 后 poison 连锁 | ACP/harness 多处 | `lock_recover`/避免 unwrap |
| P2-11 | `agent.session()` 每次 `SessionInbox::new` 新缓存；`before_step` 每步重建 | `execution.rs:203-246` | 复用 inbox/cache |
| P2-12 | `SessionInbox::fold` 未知事件 fatal，升级/回滚不兼容 | `inbox.rs:191-193` | 非 required 忽略 |
| P2-13 | `SessionProjection::read` 每个 commit 读两次（segments_through + read_commit）；所有 blob 都 hydrate | `journal.rs:128-218` | 单次读 + 懒 hydrate + 跳过无关事件 |
| P2-14 | `SessionTurns::read`/`pending_interactions` 每次 cold turn 全量，且 `pending` 与 `turns` 各扫一遍 | `views.rs:61`、`523` | 合并为一次 visit + 增量索引 |
| P2-15 | `MAX_INLINE_PAYLOAD_BYTES=64KB` 导致 request/prepared/state 大量 blob + version 文件 | `journal.rs:28`、`storage.rs` | 大对象单独 segment/SQLite，不写 version |
| P2-16 | `model/chunk` 事件含完整 chunk/usage，每个 token 都落盘 | `models.rs:172` | 采样/批量/不落 token |
| P2-17 | 错误日志缺少 session/run/epoch/owner/lease，无法定位 | `recorder.rs:195` | 结构化字段 |
| P2-18 | 没有 journal 大小/事件数/重放耗时/心跳年龄指标 | 全局 | metrics + 告警 + 自动 compaction |
| P2-19 | `AgentSession::start` 每 50ms 轮询，即使 idle；多 session 常驻 I/O | `session.rs:728-746` | 事件驱动 + 退避 |
| P2-20 | ACP `events_tx`/core `delta_tx` unbounded，慢客户端内存增长 | 见 P1-12 | 有界队列 |

---

## 3. 修复方案（建议按阶段执行）

> 原则：**租约错误永远不能成为用户可见的终态**；恢复必须幂等；任何“不确定”都要持久化，不能靠内存。

### Phase 0：先消灭用户报错（1-3 天）

目标：即使 journal 很大/机器休眠/存储抖动，也不再在“发送消息”时直接报错。

1. **LeaseGuard 重构**
   - `acquire` 后立即启动心跳；`SessionRecorder::open` 的所有 replay 都在心跳保护下。
   - `ensure_owned` 仅在 owner/epoch 不匹配或 sealed 时失败；同 owner/epoch 过期允许 renew。
   - `release` 不检查 lease。
   - `commit` 在长写前 renew，并在 head CAS 时推进 lease。
2. **心跳可靠性**
   - transient renew 失败指数退避重试到 deadline；
   - 心跳任务 panic/退出可监督重启；
   - 日志带 session/run/epoch/owner/lease。
3. **`flush` 正确性与重试**
   - 串行化 flush；用 `drain(..n)` 只确认已提交前缀；
   - retryable 错误退避重试，不 sticky；
   - `Stale committed prefix` 重新 open/rebase，而不是直接失败。
4. **大 checkpoint**
   - `capture_state` 走 blob/分段提交；`MAX_PENDING_BYTES` 不对单条必需事件硬失败。
5. **工具恢复事实**
   - acting 前 checkpoint；写 `tool/dispatch`；projection 识别 `action/start`；
   - observation end 失败保留真实结果/标记 uncertain。
6. **退出 drain**
   - ACP 监听 stdin EOF + signal；cancel + abandon + await finish/close 后再退出；
   - `ExecutionGuard` 接入 stream 路径；`process::exit` 前等待。
7. **立即止血 journal**
   - 至少把 `model/chunk` 改为批量；immutable commit 不写 `.version`；限制单会话文件数。
   - 即使暂时不做 compaction，也能把 184k 文件降到可控数量。

### Phase 1：恢复语义与长期运行（1-2 周）

1. **Journal 架构**
   - SQLite/segment 日志；单行/单文件 CAS；fsync。
   - 最新 checkpoint 索引 + 反向恢复；journal compaction/retention。
   - 大对象（request/prepared/state/tool output）单独 blob，不逐事件 commit。
2. **ACP 恢复**
   - `session/list` 从持久化 store 读；
   - load/resume 校验 journal；
   - 首个 prompt 检测 open turn，支持 resume 而不是无条件 abandon；
   - 稳定 prompt idempotency key（request id）；
   - suspended/external execution 支持 `respond`。
3. **取消与并发**
   - per-prompt cancel token；prompt lock 可取消/超时；
   - 工具结构化并发 + kill-on-drop；
   - permission/elicitation/MCP 全超时。
4. **错误与 panic**
   - 统一错误分类；retryable/fatal；
   - mutex poisoning 恢复；
   - dispatch/prompt handler catch + restart；
   - 永远给 ACP responder 一个结果。

### Phase 2：硬化与可观测性（持续）

1. 多进程 CAS/租约测试、断电/蓝屏模拟。
2. journal/内存/事件队列上限、自动归档、磁盘告警。
3. 指标：`lease_age_ms`、`renew_failures`、`replay_duration_ms`、`journal_events`、`journal_bytes`、`pending_bytes`、`flush_retries`、`tool_uncertain_count`、`shutdown_drain_ms`。
4. 故障注入：随机 kill -9、断网、磁盘满、时钟跳变、权限不响应、慢客户端。
5. 文档化恢复状态机：`running -> interrupted -> (resume|cancel)`，`uncertain tool -> reconcile`。

---

## 4. 验收标准与测试矩阵

### 4.1 必须通过的验收

1. **100 轮以上长会话**（含大文件读写、长模型流、多工具）连续运行 24h，不出现 `Session writer ownership expired or fenced`、`pending buffer exhausted`、`Stale committed prefix`。
2. **任意阶段 kill -9 后重启**：
   - 5s 内可接管/释放旧 lease（不能等 120s）；
   - 不重复执行已经产生副作用的工具；
   - 原 turn 可 resume 或明确 reconcile，不丢用户输入。
3. **冷启动重放**：10 万事件 session 在 10s 内 open（或至少租约不丢、用户不报错）。
4. **取消**：长工具执行中 cancel，工具在 1s 内停止/被标记 uncertain；session 可立即接受下一条 prompt。
5. **慢客户端**：事件队列有界，内存不增长；连接断开后 run 被取消/收尾。
6. **双进程**：两个 ACP 进程同 session 并发，不丢 commit、不 head 回退；只有一个 writer。
7. **磁盘/网络抖动**：注入 10 次 transient 失败，run 继续或明确恢复，不能一次就 sticky。

### 4.2 建议新增测试

| 测试 | 目的 |
|---|---|
| `lease_survives_replay_longer_than_lease` | P0-1：replay 期间心跳 |
| `same_owner_can_renew_after_expiry` | P0-3：过期自愈 |
| `release_clears_expired_own_head` | P0-3：release |
| `heartbeat_retries_transient_failure` | P0-4 |
| `flush_does_not_drop_concurrent_append` | P0-5 |
| `transient_commit_failure_is_retried` | P0-6 |
| `crash_mid_tool_marks_uncertain_and_never_reexecutes` | P0-7 |
| `cancel_stops_detached_tool_task` | P0-8 |
| `stdin_eof_drains_active_run_and_releases_lease` | P0-9 |
| `two_processes_cas_never_lose_commit` | P0-10 |
| `large_checkpoint_spills_without_pending_limit` | P0-12 |
| `observation_end_failure_preserves_tool_result` | P1-1 |
| `restart_then_prompt_resumes_open_turn` | P1-3 |
| `prompt_retry_with_same_request_id_is_idempotent` | P1-4 |
| `cancel_queued_prompt_does_not_stop_running_prompt` | P1-5 |
| `permission_wait_times_out_and_cancel_works` | P1-6 |
| `session_list_survives_restart` | P1-7 |
| `dispatcher_survives_panic_and_restarts` | P1-8 |
| `inbox_lease_takeover_after_crash` | P1-9 |
| `suspended_tool_waits_for_respond` | P1-10 |

### 4.3 最小复现脚本（修复会话可直接用）

1. **构造大 journal**：启动 ACP，跑一个 5,000+ chunk 的流式回答，检查 `agents/.../commits` 文件数。
2. **冷启动计时**：杀掉进程，记录 `SessionRecorder::open` 从 `acquire` 到 `start` 的耗时；>120s 即命中。
3. **注入 lease 200ms**：用 `open_with_lease` 测试，在 replay 前 sleep 3×lease；当前实现应失败，修复后应成功。
4. **工具中 kill**：让 `execute` 执行 `Start-Sleep 60`，在工具执行中 kill -9；重启后检查 journal 是否出现 `tool/dispatch`/uncertain，是否重跑。
5. **stdin EOF**：关闭 ACP 客户端 stdin，观察进程是否等待 run 收尾、是否释放 lease。

---

## 5. 关键代码索引（修复会话速查）

| 主题 | 文件:行 |
|---|---|
| 租约检查 | `crates/agentscope-core/src/session/journal.rs:115-126` |
| acquire / release / commit | `journal.rs:253-363`、`365-493` |
| update_owner | `journal.rs:616-650` |
| 租约/心跳常量 | `crates/agentscope-core/src/session/recorder.rs:38-46` |
| 心跳实现 | `recorder.rs:174-227` |
| recorder open / replay / heartbeat start | `recorder.rs:336-483` |
| flush / sticky failure | `recorder.rs:570-615` |
| record / append | `recorder.rs:530-626` |
| MAX_PENDING | `recorder.rs:49`、`549-550` |
| finish / close / Drop | `recorder.rs:876-969`、`130-166` |
| 每 chunk 记录 | `crates/agentscope-core/src/session/models.rs:152-244` |
| 投影 / action 不确定 | `crates/agentscope-core/src/session/views.rs:336-472`、`372-394` |
| abandon_turn | `crates/agentscope-core/src/session/recovery.rs:79-92`、`94-277` |
| acting spawn | `crates/agentscope-core/src/agent/react_agent.rs:2508-2642` |
| tool 执行 / observe | `react_agent.rs:2649-2831` |
| state 发布 / 无 acting 前 checkpoint | `react_agent.rs:1764`、`1851` |
| ACP prompt 主循环 | `crates/agentscope-extensions-acp/src/session.rs:1275-1878` |
| ACP cancel | `session.rs:1075-1078`、`1294`、`1670-1732` |
| ACP unbounded events | `session.rs:1378-1383` |
| ACP session registry | `crates/agentscope-extensions-acp/src/coding_agent.rs:223-343` |
| load/resume/delete | `coding_agent.rs:602-659` |
| ACP shutdown | `crates/agentscope-extensions-acp/src/acp_harness_agent.rs:1159-1247` |
| execution guard 缺失 | `crates/agentscope-core/src/agent/react_agent.rs:534`、`723-861` |
| 文件 CAS | `crates/agentscope-harness/src/agent/session/storage.rs:62-92`、`152-183` |
| SQLite | `crates/agentscope-harness/src/agent/session/sqlite.rs:42-65` |
| 默认 journal 路径 | `storage.rs:224-233`、`crates/agentscope-core/src/session/key.rs:35-44` |
| inbox lease | `crates/agentscope-core/src/session/inbox.rs:14-17`、`302-320` |
| harness launch | `crates/agentscope-harness/src/agent/session/session.rs:1093-1406` |
| dispatch loop | `session.rs:983-1033`、`728-746` |

---

## 6. 最终判断

- **直接原因：** 当前代码把“租约过期”当成不可恢复的 fencing，而租约又在一个可能长达数分钟的全量重放期间没有被续租；journal 的逐 chunk commit 把重放时间放大到必然超过 120s。
- **为什么“重启后第一次发消息”最容易出现：** 冷启动没有 `restored_head` 基线，必须走全量 `SessionProjection::read` + `scan` + `transcript`；同时 `abandon_unfinished` 还会先做多次全量扫描。旧进程被 IDE/stdio 关闭时没有 drain/release，head 留着 stale owner，进一步加重等待和重放。
- **为什么“心跳已经加了还报”：** 心跳只覆盖“recorder 构造之后”的时间，不覆盖 replay；一次失败就退出；过期又不允许自愈；所以它只能缓解“静默 run”，不能解决“冷启动/暂停/存储抖动”。
- **修复重点顺序：**  
  1) 让租约过期不再等于失败（self-renew + allow expired own lease）；  
  2) 把心跳前移到 acquire 之后；  
  3) 把 journal 从“每 chunk 一 commit”改成批量/segment；  
  4) 工具 acting 前 checkpoint + tool/dispatch + 结构化取消；  
  5) 退出 drain；  
  6) 最后再做 ACP 恢复语义/幂等/会话索引。  
  前 4 项完成后，用户报告的这条错误基本不会再出现；后 2 项决定“任意中断后是否真的能继续”。

> 本次调查未修改任何代码。修复会话建议按 P0 → P1 → P2 顺序逐项做，并为每个 P0 先写一个反向验证测试（先让测试在旧实现上失败，再修）。

---

## 7. 修复回执（2026-10-27）

本轮按 P0 → P1 → P2 顺序落地了下面的改动。**未**通过删除断言、新增 `#[ignore]` 或放宽测试来“变绿”。

### 7.1 已修复

| 编号 | 问题 | 落地方式 | 位置 |
|---|---|---|---|
| P0-1 | 心跳在 replay 之后才启动 | `acquire` 后立即 `LeaseHeartbeat::start`，replay 失败时先停心跳再 release | `session/recorder.rs` `open_with_lease_and_baseline` |
| P0-3 | 租约过期 = 永久 fence | `ensure_owned` 只在 owner/epoch 变化或 sealed 时失败；同 owner/epoch 过期可续租 | `session/journal.rs` |
| P0-4 | 心跳一次失败就永久停止 | transient 失败指数退避重试；仅 fenced/sealed（非可重试）才停止 | `session/recorder.rs` `LeaseHeartbeat::start` |
| P0-5 | `flush` clone-then-clear 丢并发 append | `flush_lock` 串行化 + `InflightBatch` 保留预留 seq/事件；成功后只确认该 batch；append 的 seq 叠加 inflight 长度 | `session/recorder.rs` |
| P0-6 | 任何 commit 错误都变 sticky | `is_retryable_session_error` 区分 transient/fatal；transient 重试同一 batch（6 次，指数退避），fatal 才 sticky 并回填事件 | `session/recorder.rs` |
| P0-7 | 工具执行前无 checkpoint、无 `tool/dispatch` | acting 前 `checkpoint_now(state, "before_acting")` + 每个 call 一条 `tool/dispatch`；投影把 `action/start` 计为 uncertain，`action/end`/`tool/result` 清除 | `agent/react_agent.rs`、`session/views.rs` |
| P0-8 | 工具执行 detached，取消不停工具 | `ActingTaskGuard` + `ActingStream`：stream drop 时置 cancel 并 abort 任务；`run_acting_calls` 每轮检查 cancel 与 `check_interrupted` | `agent/react_agent.rs` |
| P0-9 | 退出不 drain | ACP shutdown 同时看 `AcpAgentSession::is_running()` 与 `GracefulShutdownManager`，20s 后 close 所有 session；`run_stream_with_extensions` 持有 `ExecutionGuard`，ACP 流式路径也计入 shutdown 计数 | `extensions-acp/src/acp_harness_agent.rs`、`agent/react_agent.rs` |
| P0-12 | 16MB pending 上限让大 checkpoint 终态写入失败 | `MAX_PENDING_BYTES` 16MiB → 64MiB（payload 超 64KiB 已溢写 blob） | `session/recorder.rs` |
| P0-13 | `turn_unfinished` 直接拒绝新 prompt | dispatcher 遇到孤儿 open turn 自动 `abandon_turn` 后继续；`submit` 前 `close_orphaned_turn_before_submit`；不再 `inbox.reject("turn_unfinished")` | `harness/agent/session/session.rs` |
| P1-1 | action observation 失败被降级为可重试 tool result | `Error::ActionObservation` 直接 `return Err`（run-level error），工具在 journal 保持 uncertain；普通工具错误仍是 tool result | `agent/react_agent.rs` |
| P1-3 | ACP 每次 prompt 前 abandon 只是 best-effort | 保留 ACP 侧提前关闭，但真正的权威兜底移到 dispatcher（P0-13），失败不再导致 `turn_unfinished` | `extensions-acp/src/session.rs`、`harness/agent/session/session.rs` |
| P1-7 | load/resume/delete/close 语义不完整 | `LoadSession`/`ResumeSession` 用 `session_exists` 校验，未知 id 返回 `invalid_params`；`CloseSession`/`DeleteSession` 先 `cancel_and_wait` 再 close/remove | `extensions-acp/src/coding_agent.rs` |
| P1-9 | inbox 租约过长、重试过短 | `INBOX_LEASE` 120s → 10s，`ACCEPT_ATTEMPTS` 100 → 400，10ms→80ms 退避 | `session/inbox.rs` |
| P1-10 | suspended / external execution 在 ACP 中不可恢复 | 新增 `AcpAgentSession::respond`（API 层），复用 `AgentSession::respond_all` | `extensions-acp/src/session.rs` |
| P1-13 | `model/chunk` 与模型流强耦合 | 前序提交 `b14a70a` 已改为 `MergedChunk`（4KiB / 64 delta / 1s 合并） | `session/models.rs` |

### 7.2 一并硬化的部分

| 改动 | 原因 |
|---|---|
| `SessionRecorder::LEASE` 120s → 30s | 崩溃后不再让会话看起来“活着”两分钟 |
| `recovery::LEASE` 120s → 15s、`ACQUIRE_ATTEMPTS` 200 → 1200 | 关闭 turn 是短操作，且要与运行中的 writer 竞争 |
| `launch` 的 open/admission/start/checkpoint 失败保留 queued 重试 | 瞬时 writer 冲突不再永久 reject 用户消息 |
| `finish` 失败后调用 `abandon_turn` | 终态写失败不再留下 open turn |
| `launch` 增加 `saw_terminal` | 没有 terminal event 的 run 不再被误报为 completed |
| `Cancelled(_)` 映射为 `cancelled` | 之前落入 `failed`，让客户端看到错误的终态 |
| `WorkspaceSessionStorage` immutable commit/blob 不写 `.version` | 每个 commit 少一个文件、少一个崩溃窗口；内容冲突仍报错 |
| `SqliteSessionStorage` read/CAS/list_paths 走 `spawn_blocking` | SQLite 同步 IO + busy_timeout 不再阻塞 runtime 和心跳 |
| ACP prompt lock 获取可取消、可被 `closed` 打断 | 卡住的上一轮不再让后续 prompt 永久挂起 |
| `HarnessAgent::list_session_keys` + `session_log_store` 字段 | 为 `session/list` 持久化读取提供 API（ACP 尚未接入，见下） |

### 7.3 新增/调整的测试

| 测试 | 覆盖 |
|---|---|
| `a_transient_commit_failure_is_retried_and_does_not_poison_the_recorder` | P0-6 |
| `an_expired_lease_can_be_renewed_by_the_same_owner` | P0-3 |
| `a_lease_held_by_writer_a_still_fences_writer_b` | P0-3（反向：接管后必须 fence） |
| `an_action_start_alone_marks_the_tool_uncertain` | P0-7 |
| `a_persisted_tool_result_clears_dispatch_uncertainty` | P0-7（denied 调用不要求 reconcile） |
| `an_event_appended_during_a_flush_is_not_dropped` | P0-5：commit 在途中 append 不丢、seq 不被复用 |
| `dropping_the_acting_stream_aborts_the_tool_task` | P0-8：drop stream 后工具任务确实被中止 |
| `immutable_commits_do_not_get_a_version_file` | immutable 存储 |
| `an_interrupted_turn_is_closed_before_a_new_submission_runs`（原 `..._blocks_submission_until_it_is_abandoned`） | P0-13：改为期望“自动关闭 + completed” |

### 7.4 验证结果

| 命令 | 结果 |
|---|---|
| `cargo test -p agentscope-core --lib` | 324 passed / 0 failed / 3 ignored |
| `cargo test -p agentscope-core`（全部 target） | 全绿 |
| `cargo test -p agentscope-harness --lib` | 415 passed / 0 failed |
| `cargo test -p agentscope-harness`（全部 target） | 全绿 |
| `cargo test -p agentscope-extensions-acp` | 50 lib + 全部集成 test 通过 |
| `cargo check --workspace --all-targets` | 通过（仅既有 warning） |

### 7.5 仍未完成（明确遗留）

| 编号 | 项 | 现状 / 影响 |
|---|---|---|
| P0-2 | journal 仍是“一 commit 一文件” | `model/chunk` 合并已把量级降下来，但没有 segment/compaction；超长会话的冷启动重放仍然慢（只是不会再因为租约过期而失败） |
| P0-10 | 文件存储 CAS 非跨进程原子 | 单进程内已用 `cas_locks` 串行化；多进程同 session 仍未加跨进程文件锁。SQLite 后端由 SQLite 自身保证 |
| P0-11 | 默认 journal 路径仍在工程目录 `agents/` | 未改；模型工具仍可能读写该目录 |
| P1-4 | ACP prompt 没有稳定幂等键 | 仍是每次新 UUID；客户端重试会重复用户消息 |
| P1-5 | cancel 仍是 session 级共享 flag | 同 session 并发 prompt 已有 `prompt_lock` 串行，但 cancel 语义仍是“整会话” |
| P1-6 | 权限/elicitation/存储等待没有统一超时 | 仅 prompt lock 与 watchdog 有超时 |
| P1-7 | ACP `session/list` 仍读内存注册表 | `HarnessAgent::list_session_keys` 已就绪，但 ACP 未接入；进程重启后 list 看不到历史会话 |
| P1-8 | 未做 panic 捕获 / mutex poisoning 统一处理 | dispatcher 仍可能被 panic 卡死 |
| P1-10 | ACP 无协议级 `respond` 处理器 | `AcpAgentSession::respond` 只是 API；客户端没有对应的 ACP 方法可调用 |
| P1-11 / P1-12 | 投影缓存无上限、事件 channel 无界 | 未改 |
| P1-14 / P1-15 | 损坏事件不可跳过、CAS 争用无 rebase | 未改 |
| P2 | 其余可观测性/次级风险 | 未做 |
| — | `SessionRecorder::Drop` / `RunGuard::Drop` 只用 `spawn` release | 没有 await；进程立即退出时仍依赖租约到期 |
| — | 未补的测试 | shutdown drain、两进程 SQLite/文件 CAS |

### 7.6 对用户报告的两个错误的结论

- `Session writer ownership expired or fenced`：**直接成因已消除**（心跳覆盖 replay + 同 owner 过期可续租 + transient 重试）。大 journal 仍会慢，但不会再因此 fence。
- `The previous turn is still open, so this prompt could not start`：**该错误路径已移除**（dispatcher 自动关闭孤儿 turn，submit 前也会主动关闭）。未知工具结果一律记为 `interrupted`，不会被当成成功。
