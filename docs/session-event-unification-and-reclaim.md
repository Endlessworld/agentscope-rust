# 会话事件模型统一 + 存储回收优化（设计与执行手册）

> 状态：待执行（2026-10-30 立项）
> 适用仓库：`agentscope-rust`（分支 `dev`）
> 阅读顺序：§0 速览 → §1 现状证据 → §4 任务分解（实际动手）→ §8 交接约定
>
> 本文档自包含：接手会话无需阅读历史对话，所有结论都标注了 `文件:行号` 证据；所有待核实项已按附录 B 的命令提前核实完毕，可直接动手。

---

## 0. 一页速览

### 0.1 要解决的三件事

| 编号 | 问题 | 量化证据 |
|---|---|---|
| **A** | 一个流式 chunk 在内存里有 3 份表示、在代码里有 5 套映射器 | §1.2、§1.3 |
| **B** | journal 落盘粒度是「每 ~4 KB 一个 commit 文件」 | §1.3，494,181 commit / 36 会话 |
| **C** | 回收机制的 `complete` 死锁 + 只读会话永不触发 → blob 永远删不掉 | §1.4，33.3 GB blob vs 485 MB commit |
| **D** | 峰值内存浪费：snapshot 全量构造 + `to_vec`、transcript 二次克隆、blob 三份拷贝 | 附录 C（由 T3.6 / T10 / T11 修） |

### 0.2 里程碑（按可独立交付排序）

| 里程碑 | 任务 | 风险 | 预期收益 |
|---|---|---|---|
| **M1** | T1 落盘粒度、T2 reclaim 死锁、T3 session_search 读放大（含 `LocalFilesystem::read` 零拷贝） | 低 | 磁盘/IO 降 1–2 个数量级，`session_search` 从分钟级降到秒级，搜索峰值 ↓ ~95% |
| **M2** | T4 `EventKind` 表、T5 `EventMeta` 复用 | 低 | 57 类 × 32 变体的换算从 5 处收敛到 1 处 |
| **M3** | T6 `EventPayload` 类型化、T7 投影器统一 | 中 | 消灭 `fields.get(...)` 50 处，UI 可从 journal 重放 |
| **M4** | T8 `Arc` 共享 + `AgentEvent` 降格、T9 GC 入口 | 中高 | 三套表示合成一套；历史会话可回收 |
| **M5** | T10 snapshot 流式序列化、T11 transcript 流式/去克隆（可与 M1 并行） | 中 | 再降一个数量级的**峰值**内存（详见附录 C） |

### 0.3 非目标（已评估并否决，见 §7）

- ❌ 用 ACP `SessionUpdate` 统一框架事件
- ❌ 让 `SessionEvent` **借用** `&AgentEvent`
- ❌ 用 `EventData.fields`（`JsonMap`）当 payload 类型
- ❌ 把流式 delta 逐条写入 journal
- ❌ 把「降稳态 RSS」当成本方案目标（稳态由 `AgentState.context` 决定，属 compaction/pruning 课题，见附录 C）

### 0.4 验收总命令（每个任务完成后必须全绿）

```powershell
cargo build -p agentscope-core -p agentscope-harness
cargo test -p agentscope-core --lib
cargo test -p agentscope-harness --lib
cargo test -p agentscope-extensions-acp
cargo test -p agentscope-extensions-agui
cargo test --workspace -j 2 --no-fail-fast > $env:TEMP\ws.log 2>&1; Select-String '^test result:' $env:TEMP\ws.log
```

判定标准：**无 `failures:` 段 + `EXITCODE=0`**。`Select-String 'FAILED'` 会误命中测试名，不可作为判据。

---

## 1. 现状与证据

### 1.1 四层事件模型（不是三套）

| 层 | 类型 | 定义位置 | 形状 | 每 chunk 一份？ |
|---|---|---|---|---|
| ① provider chunk | `ChatResponse` | `core/src/model/` | SSE 增量 | 是（源头） |
| ② 运行时流式 API | `AgentEvent`（**32 变体**） | `core/src/event/event.rs:19` | `SCREAMING_SNAKE_CASE` tag + `EventData` | **是**（`react_agent.rs:1849`、`2591/2601`） |
| ③ 持久化事实 | `SessionEvent`（**57 类型**） | `core/src/session/event.rs:16` | 信封 `{seq,eventId,occurredAt,type,runId,turnId,required,payloadJson}` | **是，合并后**（`models.rs:303` `record_merged_chunk`） |
| ④ 线协议 | ACP `SessionUpdate` | 外部 crate `agent_client_protocol` schema v1 | `AgentMessageChunk` / `ToolCallUpdate` / `Plan` … | 是（`extensions-acp/src/event.rs:714` `map_delta_event`） |

关键结论：**`SessionEvent` 不是 `AgentEvent` 的包装**，两者是平行模型，唯一接触点是 §1.2 的 `observe()`。

### 1.2 两块关联代码（唯一的桥，且只覆盖 5/32）

```rust
// crates/agentscope-core/src/session/recorder.rs:1755
pub fn observe(&self, event: &crate::event::AgentEvent) -> crate::error::Result<()> {
    match event {
        AgentEvent::RequireUserConfirm(data)      => { /* → interaction/requested */ }
        AgentEvent::RequireExternalExecution(data) => { /* → interaction/requested */ }
        AgentEvent::ExternalExecutionResult(data)  => { /* → interaction/resolved  */ }
        AgentEvent::UserConfirmResult(data)        => { /* → interaction/resolved  */ }
        AgentEvent::RequestStop(_)                 => { /* → run/stop_requested   */ }
        _ => {}   // 其余 27 个变体被丢弃
    }
}
```

`observe()` 的调用点：`core/src/session/state_access.rs:397`、`recovery.rs:446`、`migration.rs:270`。它是**手工拆字段 + 重命名**，不是包装。

### 1.3 落盘粒度：每 ~4 KB 一个 commit（问题 B 的根因）

```rust
// crates/agentscope-core/src/session/recorder.rs:1349
pub async fn record<T: serde::Serialize>(&self, event_type: &str, payload: &T) -> Result<u64> {
    let seq = self.append(event_type, payload)?;
    self.flush().await?;      // ← 立即落盘：写 commit 文件 + .version + checksum（+ sync）
    Ok(seq)
}

// crates/agentscope-core/src/session/models.rs:257 —— 合并阈值
self.chunks >= 64 || self.bytes >= 4 * 1024 || self.since.elapsed() >= FLUSH_INTERVAL
// crates/agentscope-core/src/session/models.rs:303 —— 合并后调用 record("model/chunk", ...)
```

即：**每约 4 KB 流式文本 → 一个 `model/chunk` 事件 → 一次 `flush()` → 一个 commit 文件 + 一个 `.version` 文件**。

实测（本次调查，2026-10-30）：

| 指标 | 数值 |
|---|---|
| legacy `agents/` 树（**已由用户删除**） | 36 会话 / 494,181 个 commit JSON / 970,214 个 commits 目录文件 / 985,079 文件 / 33,800 MB |
| 其中 blob | 14,608 个 / **33,315 MB** |
| 其中 commit | 485 MB |
| 每会话平均 commit | 494,181 / 36 ≈ **13.7k** |
| 当前树 `.agentscope/sessions` | 5 会话 / 505 commit / 3.07 MB |
| 本开发会话（`…a0df`） | 806 commit / **125 个 `state/checkpoint` 事件** / 3 turns / 5 blob / 5 snapshot 文件 |
| 单 commit 文件读（含元数据） | **0.388 ms/文件**（3000 文件采样） |
| blob 顺序读 | 920 MB/s（74.6 MB 采样） |

`record_merged_chunk` 的 payload 结构（`models.rs:303-325`）：`{modelCallId, purpose, chunk: ChatResponse, mergedChunks}`。
`model/chunk` 是**必需的**：`models.rs:118-140` 用它重建被中断的流式输出（`InterruptedOutput`），不能简单删掉。

### 1.4 回收机制与六个盲区（问题 C）

现有能力（已实现、有测试，不是缺失）：

| 动作 | 位置 |
|---|---|
| 删 commit + 收集 blob | `journal.rs:556` `reclaim`，候选条件 `last_seq < floor && batch_newest <= now - min_age`（`:604`） |
| 删 blob | `journal.rs:653` `reclaim_blobs` |
| 扫孤儿 blob | `journal.rs:672` `reclaim_orphan_blobs` |
| 记 floor | `journal.rs:720` `mark_reclaimed` → `reclaimed.json` |
| 读侧容忍 | `journal.rs:304-310`、`:1107-1113` |
| 触发链 | `recorder.flush()` → `publish_snapshot_if_due`（`recorder.rs:1287`）→ `publish_snapshot_locked` → `reclaim(previous.seq, retention)`（`journal.rs:545`） |

常量与默认值：

| 常量 / 默认 | 值 | 位置 |
|---|---|---|
| `SNAPSHOT_EVERY_EVENTS` | 512 | `snapshot.rs:30` |
| `SNAPSHOT_EVERY` | 60 s | `snapshot.rs:33` |
| `SessionRetention::default()` | `prune: true`, `min_age: 30 天` | `snapshot.rs:151-158` |
| `PRUNE_MIN_EVENTS` | 1024 | `journal.rs:45` |
| `PRUNE_MAX_SCAN` | 4096 | `journal.rs:49` |
| `PRUNE_BATCH` | 512 | `journal.rs:52` |
| `SUPERSEDED_MAX_COMMITS` | 512 | `journal.rs:56` |
| `SUPERSEDED_MAX_EVENTS` | 4096 | `journal.rs:60` |

**六个盲区**：

1. **只读会话永不回收**：回收寄生在写路径（`recorder.flush`），历史会话不再写入 → 永不触发。
2. **没有 snapshot 就没有 floor**：`reclaim` 在 `floor == 0` 直接 return（`journal.rs:557`）。
3. **`complete` 死锁**：`journal.rs:582` `let complete = enumerated.len() <= PRUNE_MAX_SCAN;` —— 超过 4096 个 commit 时**只删 commit、不删 blob**；孤儿清扫也只在 `complete` 时跑（`:634-640`）。这是「33.3 GB blob 一条都没删」的结构性原因。
4. **删除速率封顶**：`PRUNE_BATCH=512` + `PRUNE_MAX_SCAN=4096` 每次 flush 一轮，13.7k commit 的会话需要 100+ 轮写路径触发。
5. **checkpoint payload 回收窗口**：`reclaim_superseded_checkpoints`（`journal.rs:1281`）用 `stop_at = keep_seq - SUPERSEDED_MAX_EVENTS(4096)` 且最多扫 512 个 commit，超窗即静默 `break` → 长 turn 的早期 checkpoint payload 永久残留（本会话 125 个 checkpoint 事件只剩 5 个 blob，其中含此类残留）。
6. **快照对象永不回收**：`publish_snapshot_locked` 每 512 events / 60 s 生成一个新对象（含完整 transcript + 完整 `state_json`，MB 级，`journal.rs:470-490`），`reclaim` 只扫 `commits/` 与 `blobs/`，旧快照对象无人删除。

附带事实：`WorkspaceSessionStorage::read` 在每个 value 上做 5–6 次系统调用（`storage.rs:230`：`exists` → `exists(.version)` → 读 `.version` → `is_file` → `fs::read`），且 value 缺失但 `.version` 存在时会**忙等 250 ms**（`storage.rs:243-245`）。

### 1.5 `session_search` 的读放大（问题 C 的用户可见症状）

```
SessionSearchTool::call                 tool/session_search.rs:251  ('outer: for key in keys)
└─ store.list()                          agent/session/storage.rs:420
   ├─ 当前树 .agentscope/sessions/...    5 会话
   └─ legacy 树 agents/...               storage.rs:445-462（36 会话 / 33.8 GB；2026-10-30 已删）
└─ transcript() → replay_views           core/session/views.rs:238 / 283
   ├─ head() / load_verified() / commit_at() / reclaimed_floor()
   └─ visit()                            journal.rs:1145
      ├─ segments_through()              journal.rs:238  ← 第 1 遍：整篇读每个 commit
      └─ read_commit()                   journal.rs:161  ← 第 2 遍：再读一遍 + 水合全部 blob
```

三重放大：**① 每个 commit 读两遍；② `read_commit` 无条件水合 payload blob（`journal.rs:168-200`，含 2.28 MB 均值 checkpoint），并重新序列化 + SHA-256 校验；③ `LocalFilesystem::read` 对整个文件做 `from_utf8_lossy().into_owned()` + `split('\n').collect::<Vec<_>>()`（`local.rs:396-397`）**。

`SessionSearchTool` 还有一个独立缺陷：`register_all`（`session_search.rs:188`）不给工具绑 `with_context`，因此 `RuntimeContext::default()` 的 `user_id` 恒为 `None`，搜索范围不受调用者身份约束。

---

## 2. 目标架构

### 2.1 分层原则

```
                  ┌──── SessionEvent（唯一模型）────┐
provider chunk → 归一化 →  信封: seq/kind/turn/run/required/meta
                  │      payload: Arc<EventPayload>（类型化）
                  └──────────┬─────────────────────┘
                     ┌───────┴────────┐
             事实通道（required=true）      增量通道（required=false）
             seq 单调、落盘、可 replay      可丢、可合并、默认不落盘
                     │                          │
      ┌────────┌─────┴───┬─────────┐      ┌──────┴────┬─────────┐
  journal(磁盘)  ACP     AGUI  ControlPlane  ACP chunk  AGUI delta
```

四条硬约束：

1. **`SessionEvent` 是唯一真相源**；`AgentEvent` / ACP `SessionUpdate` / AGUI 事件 / subagent 帧全部是**投影**（单向 `From<&SessionEvent>`），不得反向依赖。
2. **payload 类型化**（`EventPayload` 枚举），不再用 `fields: JsonMap` 字符串键访问。
3. **一份 payload 一份内存**：`Arc<EventPayload>` 在 journal / 投影 / 缓冲之间共享，不 clone 字符串。
4. **协议不进入 core**：`agentscope-core` 不依赖 `agent_client_protocol` / AGUI 任何类型（保持现状）。

### 2.2 `EventMeta`（复用 `EventData` 的信封半截）

```rust
// 现状：core/src/event/event.rs:80
pub struct EventData {
    pub id: String,                 // → EventMeta.id
    pub created_at: String,         // RFC3339 字符串 → 改为 i64 ms
    pub source: Option<String>,
    pub metadata: JsonMap,
    #[serde(flatten)]
    pub fields: JsonMap,            // ← 这一半被 EventPayload 取代，必须删
}
```

目标：

```rust
pub struct EventMeta {
    pub id: String,                 // 与 SessionEvent.event_id 合并为一份
    pub created_at: i64,            // ms，与 SessionEvent.occurred_at 合并为一份
    pub source: Option<String>,
    pub metadata: JsonMap,
}
```

为何 `created_at` 必须改为 `i64`：`occurred_at` 被回收逻辑用作年龄比较与排序（`journal.rs:601` 用 `batch_newest = events.max(occurred_at)` 对比 `now_ms() - min_age`；`journal.rs:781/815` 的 `CommitLiteEvent`）。字符串日期会让每次比较都要 parse。

兼容要求：`created_at` 改 `i64` 会改变 `AgentEvent` 的 JSON 形状（`EventData` 被 20 个文件引用，AGUI / subagent 出口也在用）→ **T5 必须同时保留 `createdAt`（RFC3339）作为只读兼容字段**，下个大版本再删。

### 2.3 `EventPayload`（取代 `fields`）

变体数量目标：**57 个 wire 类型 → 30 个左右变体**（多条 wire 名共用变体，靠 `EventKind` 区分）。完整映射见 §3。

### 2.4 `EventKind` 单一类型表（最关键的一步）

```rust
pub struct EventKindSpec {
    pub kind: EventKind,
    pub wire: &'static str,        // 保持不变："model/chunk" / "TEXT_BLOCK_DELTA"
    pub required: bool,            // 事实通道 = true
    pub persisted: bool,           // 是否允许写入 journal
    pub streamed: bool,            // 是否参与流式（AgentEvent 侧）
    pub payload: PayloadTag,       // 期望的 EventPayload 变体
}
```

两条通道的判定全部读这张表，不再在 5 个投影器里各自 match 常量。

---

## 3. 收敛表：57 个 wire 类型 × 32 个 `AgentEvent` 变体 → `EventPayload`

### 3.1 57 类的真实分布（已按写入点核实）

两个「静态 grep 找不到」的真相：

- `turn/*` 的终止类（`completed|failed|suspended|cancelled|interrupted`）由 `format!("turn/{terminal}")` **动态拼接**（`recorder.rs:1717`），`terminal` 来自 `finish(status)` 的参数。
- `message/system|user|assistant` 与 `tool/result` 共 4 类共用**同一条写入路径**（`recorder.rs:1540` → `message_event_type()` `recorder.rs:1819-1831`）。

| 分组 | wire 类型 | 写入点 |
|---|---|---|
| run | `run/start` | `recorder.rs:1404` |
| run | `run/end` | `recorder.rs:1720` |
| run | `run/stop_requested` | `recorder.rs:1807`（经 `observe`） |
| turn | `turn/start` / `turn/resumed` | `recorder.rs:1408-1410` |
| turn | `turn/output` | `recorder.rs:1697` |
| turn | `turn/{completed,failed,suspended,cancelled,interrupted}` | `recorder.rs:1717`（动态拼接） |
| turn | `turn/end` | **无生产写入** |
| step | `step/start` / `step/end` | `recorder.rs:1439` / `:1433`、`:1714` |
| input | `input/received` / `input/applied` | `recorder.rs:1422` / `:1535` |
| input | `input/discarded` | **零写入** |
| inbox | `inbox/{accepted,opened,closed,rejected,handled}` | `session/inbox.rs:235/254/271/288/305` |
| inbox | `inbox/started` / `inbox/applied` | `session/execution.rs:183` / `:260` |
| message | `message/{system,user,assistant}` | `recorder.rs:1540` |
| message | `tool/result` | 同上（`recorder.rs:1824`） |
| model | `request/prepared` | `recorder.rs:1635` |
| model | `model/dispatch` / `model/end` | `recorder.rs:1647` / `models.rs:488` |
| model | `model/chunk` | `models.rs:303`（合并后） |
| model | `model/retry` / `context/build` | **零写入** |
| tool | `tool/requested` | `recorder.rs:1545` |
| tool | `tool/decision` / `action/start` / `action/end` | `observation/mod.rs:176,184` / `:201` / `:203` |
| tool | `tool/dispatch` | `react_agent.rs:2897` |
| tool | `tool/chunk` | **零写入** |
| interaction | `interaction/requested` / `interaction/resolved` | `recorder.rs:1760,1772` / `:1784,1796` |
| context | `context/replaced` | `recorder.rs:1523` |
| compaction | `compaction/start` / `compaction/end` | `harness/memory/compaction.rs:715` / `:744` |
| state | `task/changed` / `plan/changed` / `permission/changed` | `recorder.rs:1496-1516` |
| state | `verification/result` | `harness/verification/mod.rs:185` |
| state | `state/checkpoint` | `recorder.rs:1560` |
| state | `state/restored` | **零生产写入**（`views.rs:85` 消费） |
| subagent | `subagent/spawned` / `subagent/completed` / `subagent/linked` | **零写入**（预留） |
| recovery | `recovery/applied` / `migration/baseline` | `recorder.rs:1396` / `:1390` |
| hint | `presentation/hint` | 仅测试 `recorder.rs:2488` |

### 3.2 `EventPayload` 变体表（30 个，取代 `fields`）

字段定义约定：**每个变体的字段名与现有写入点的 `json!` 字面量一字不差**（照抄写入点，不要重新命名）。

| # | 变体 | 覆盖 wire 类型 | 字段来源（写入点） |
|---|---|---|---|
| 1 | `RunStart { execution_run_id, coverage }` | `run/start` | `recorder.rs:1404-1407` |
| 2 | `RunClosed { status, reason?, error? }` | `run/end`、`turn/*` 终止 | `recorder.rs:1717-1720` |
| 3 | `RunStopRequested {}` | `run/stop_requested` | `recorder.rs:1807-1810` |
| 4 | `TurnOpened { resumed: bool }` | `turn/start`、`turn/resumed` | `recorder.rs:1408-1412` |
| 5 | `TurnOutput { message: Msg, status }` | `turn/output` | `recorder.rs:1697-1703` |
| 6 | `Step { step_id, iteration?, status? }` | `step/start`、`step/end` | `recorder.rs:1433/1439/1714` |
| 7 | `InputReceived { messages: Vec<Msg> }` | `input/received` | `recorder.rs:1422` |
| 8 | `InputAck { input_id, applied: bool }` | `input/applied`、`input/discarded` | `recorder.rs:1535-1538` |
| 9 | `InboxEvent { phase, command_id?, payload? }` | 7 个 `inbox/*` | `inbox.rs:235-310`、`execution.rs:183/260` |
| 10 | `Message { message: Msg }` | `message/{system,user,assistant}` | `recorder.rs:1540-1543` |
| 11 | `ToolResult { result: ToolResultBlock }` | `tool/result` | `recorder.rs:1824` |
| 12 | `RequestPrepared(PreparedRequest)` | `request/prepared` | `recorder.rs:1635-1640`（结构体已存在） |
| 13 | `ModelDispatch { model_call_id, attempt_id }` | `model/dispatch` | `recorder.rs:1647-1650` |
| 14 | `ModelChunk { model_call_id, purpose, chunk, merged_chunks }` | `model/chunk` | `models.rs:303-325` |
| 15 | `ModelEnd { model_call_id, .. }` | `model/end` | `models.rs:488` |
| 16 | `ToolRequested { message_id, tool_call_id, call }` | `tool/requested` | `recorder.rs:1545-1553` |
| 17 | `ToolDecision { .. }` | `tool/decision` | `observation/mod.rs:176,184` |
| 18 | `ActionPhase { phase }` | `action/start`、`action/end` | `observation/mod.rs:201,203` |
| 19 | `ToolDispatch { .. }` | `tool/dispatch` | `react_agent.rs:2897` |
| 20 | `ToolChunk { .. }` | `tool/chunk`（零写入，保留） | — |
| 21 | `InteractionRequested { request_id, kind, call }` | `interaction/requested` | `recorder.rs:1760-1780` |
| 22 | `InteractionResolved { request_id, kind, result }` | `interaction/resolved` | `recorder.rs:1784-1802` |
| 23 | `Compaction { phase, metrics? }` | `compaction/start`、`compaction/end` | `harness/memory/compaction.rs:715,744` |
| 24 | `ContextReplaced { reason, as_of_seq, .. }` | `context/replaced` | `recorder.rs:1523-1530` |
| 25 | `SectionChanged { section, state }` | `task/changed`、`plan/changed`、`permission/changed` | `recorder.rs:1496-1516` |
| 26 | `Verification { .. }` | `verification/result` | `harness/verification/mod.rs:185` |
| 27 | `RecoveryApplied { .. }` | `recovery/applied` | `recorder.rs:1396-1401` |
| 28 | `MigrationBaseline { state_json, coverage }` | `migration/baseline` | `recorder.rs:1390-1393` |
| 29 | `StateCheckpoint { state_json, reason }` | `state/checkpoint` | `recorder.rs:1560` + `checkpoint_payload()` `:1862` |
| 30 | `Hint { .. }` | `presentation/hint` | 仅测试，生产未接线 |

**零写入的 8 类**（`context/build`、`model/retry`、`input/discarded`、`state/restored`、`subagent/*`×3、`tool/chunk`）：在变体表里**保留为 `reserved`**，继续用 `serde_json::Value` payload（零迁移成本），不强行造变体。

### 3.3 `AgentEvent` 32 个变体的去向

| AgentEvent 变体 | 去向 | 理由 |
|---|---|---|
| `TextBlock{Start,Delta,End}` | **仅增量通道**，不落盘 | 终态由 `message/assistant` 表达 |
| `ThinkingBlock{Start,Delta,End}` | 仅增量 | 同上 |
| `DataBlock{Start,Delta,End}` | 仅增量 | 同上 |
| `ToolCall{Start,Delta,End}` | 增量 + `ToolRequested` 投影 | 工具调用事实已由 `tool/requested` 落盘 |
| `ToolResultTextDelta` / `ToolResultDataDelta` | 仅增量 | 终态由 `tool/result` 表达 |
| `ToolProgress` | 仅增量 | `event_type.rs:25-28` 注释明确：display-only |
| `ModelCallStart` / `ModelCallEnd` | 投影自 `model/dispatch` / `model/end` | 已有对应事实 |
| `ToolResultStart` | 投影自 `tool/dispatch` / `action/start` | — |
| `ToolResultEnd` | 投影自 `tool/result` | — |
| `RequireUserConfirm` / `RequireExternalExecution` / `UserConfirmResult` / `ExternalExecutionResult` | 投影自 `InteractionRequested` / `InteractionResolved` | 已有桥 `recorder.rs:1755`，改为反向 |
| `RequestStop` | 投影自 `RunStopRequested` | 同上 |
| `AgentStart` / `AgentEnd` | 仅增量 | 会话层用 `run/start` 与 `run/end` 表达，不重复落盘 |
| `AgentResult` | 仅增量 + 终态比对 | 终态由 `turn/output` 落盘 |
| `ExceedMaxIters` / `AllToolsDenied` | 仅增量 | 审计价值待定（T6 决策） |
| `SubagentExposed` | 投影自 `subagent/*` | 双未接线，T7 一并接 |
| `HintBlock` | 投影自 `Hint` | 双未接线 |
| `Custom` | 仅增量 | ACP compaction 特例已用 `kind="compaction"`（`harness/acp/session.rs`） |

结论：**真正需要在 journal 里的 AgentEvent 只有 8 类**（已与 `SessionEvent` 重叠或可投影），其余 24 类均为流式增量。这是可以删 `fields: JsonMap`、把 `AgentEvent` 降为视图的直接依据。

---

## 4. 任务分解

每个任务的格式：**目标 / 改动点 / 步骤 / 验收 / 反向验证 / 风险与回滚**。

> 通用约束：不得删既有测试断言凑绿；不得 `#[ignore]`；每个任务单独提交；提交前 `git status` 必须可见改动。

---

### T1（P0）journal 落盘粒度：`model/chunk` 不再逐次 flush

**目标**：把「每 ~4 KB / 64 chunk 一个 commit 文件」降到「每 turn 或每 512 KB 一个」。

**根因**：`recorder.record()` = `append()` + **立即 `flush()`**（`recorder.rs:1349-1357`），而 `record_merged_chunk` 每次达阈值就调它（`models.rs:303`），阈值 = `chunks>=64 || bytes>=4KB || elapsed>=FLUSH_INTERVAL`（`models.rs:257`）。

**改动点**：
1. `core/src/session/models.rs`：`record_merged_chunk` 改用 `recorder.append("model/chunk", ..)`（不 flush）。
2. `core/src/session/recorder.rs`：确保以下时点必定 flush：`checkpoint_now`（已有）、`finish`（已有，`:1721`）、`close`。
3. 阈值调整：`chunks >= 64 → 256`、`bytes >= 4KB → 32KB`；`FLUSH_INTERVAL` 保持 1s（`models.rs:189`）或调至 200ms。

**验收**：
- 新增测试 `merged_chunks_do_not_commit_per_batch`：模拟 500 个 chunk 推入 `Recorded::stream`，断言 `log.list_paths("commits")` 数量 ≤ 5（而非 ≥ 8）。
- 既有回归：`cargo test -p agentscope-core --lib session::recorder`、`session::models`。

**反向验证**：把 `append` 改回 `record` → `merged_chunks_do_not_commit_per_batch` 必须 FAILED。

**风险**：中断恢复粒度变粗——`recovered_interrupted_output`（`models.rs:118-140`）只能恢复到最近一次 flush。缓解：`checkpoint_now` 本就每 iteration 至少一次（`before_acting` `react_agent.rs:2891`、`after_acting` `:2996`），恢复精度由 checkpoint 决定，不依赖 chunk flush。
**回滚**：改回 `record()` 一行。

---

### T2（P0）reclaim 的 `complete` 死锁：让 blob 真的能被删

**目标**：解决「commit 能删、blob 永远删不掉」（`journal.rs:582`）。

**改动点**（`core/src/session/journal.rs`）：
1. 删掉 `complete` 对 blob 删除的门控，改为**存活区间重建 live set**：
   - 先删候选 commit（`last_seq < floor`），删失败的记入 `still_alive`；
   - 从 `head` 反向走到 floor，收集 `commit.payload_blobs` → `live`；
   - `still_alive` 的 commit 引用也并入 `live`（宁漏删不误删）；
   - `list_paths("blobs")` 一次，`live` 之外全删（受 `PRUNE_BATCH` 限速）。
2. `reclaim_orphan_blobs` 不再只在 `deleted == 0 && complete` 时跑，改为每次 reclaim 末尾无条件跑。
3. 新增 `PRUNE_BLOB_SWEEP_LIMIT`（建议 8192，避免一次删太久）。

**验收**：
- 新增测试 `blobs_are_reclaimed_when_commits_exceed_prune_max_scan`：造 5000 个 commit（> `PRUNE_MAX_SCAN=4096`）、每个带一个 1 MB blob，publish 两次快照 + reclaim，断言 `blobs` 目录剩余数量 == 存活区间引用数。
- 新增测试 `reclaim_keeps_history_replayable`：reclaim 之后 `transcript()` 不报错（防止误删仍在引用的 blob）。

**反向验证**：恢复 `if complete { .. }` 门控 → 第一个测试 FAILED（blob 数不降）。

**风险**：误删仍被引用的 blob → `read_commit` 遇到缺失 blob 会报 `Missing or corrupt committed payload blob`（`journal.rs:196`）。缓解：`still_alive` 并入 live + `reclaim_keeps_history_replayable` 必须存在。
**回滚**：恢复 `complete` 分支（单文件单函数）。

---

### T3（P0）`session_search` 读放大

**目标**：一次搜索不再「每个 commit 读两遍 + 把所有 checkpoint blob 读进内存」。

**改动点**：
1. `core/src/session/journal.rs`：新增 `read_commit_selective(id, kinds: &[&str])`（或给 `read_commit` 加 `HydratePolicy`）——只需 `message/*`、`turn/output`、`tool/result` 时，**不读 `payload_blobs`**；校验和无法验证时记 `warn` 而不是 `Err`。
2. `core/src/session/journal.rs`：`visit()`（`:1145`）不再二次读取——将 `segments_through` 与 `read_commit` 合并为单遍反向遍历（复用 `read_commit_lite` 拿到 `payload_blobs`，再按需水合）。
3. `core/src/session/views.rs`：`replay_views` 接受 `ViewSelection` 时传递所需 event kind 集合到 `visit`。
4. `harness/src/tool/session_search.rs:188` `register_all` 补 `with_context`（修身份丢失）；`SessionSearchTool/ListTool/HistoryTool` 的 `with_context` 已存在（`:180/303/369`）。
5. `harness/src/agent/session/storage.rs:446`：legacy 分支加 `exists("agents")` 短路（目录已删，避免每次失败 `ls`）。
6. `harness/src/filesystem/local.rs:379-400`：`read(offset, limit)` 改为流式切片——`offset=0 && limit=usize::MAX`（journal 读路径的唯一用法）时**不做** `from_utf8_lossy().into_owned()` 整文件复制 + `split('\n').collect::<Vec<_>>()` 行指针表；非全文读取时用 seek 定位后再取需要的行。这一条把单个 2.28 MB blob 的瞬时占用从约 5 MB 降到 2.3 MB。

**验收**：
- 新增测试（用 `CountingStorage`）：一个会话含 3 个 1 MB checkpoint blob + 20 个 message 事件，`transcript()` 断言 **未读取任何 blob**（reads 计数 == commit 数）。
- 新增测试：`session_search` 在 2 个会话上搜不到时，单会话 commit 读取次数 == commit 数（不再是 2 倍）。

**反向验证**：恢复全量水合 → 第一个测试 FAILED（blob 被读）。

**风险**：跳过校验和验证 → 损坏的 commit 不再被检出。缓解：只在 `kinds` 不含 `state/checkpoint` 时跳过，且记 `warn`；`recovery` 路径仍走严格校验。
**回滚**：按函数回滚，不影响存储格式。

---

### T4（P1）`EventKind` 单一类型表

**目标**：把 `AgentEventType`（32，`event/event_type.rs:6`）与 `SessionEventTypes::BUILTIN`（57，`session/event.rs:111`）合并为一张表。

**改动点**：
1. 新增 `core/src/event/kind.rs`：`EventKindSpec { kind, wire, required, persisted, streamed, payload: PayloadTag }` + `const SPECS: &[EventKindSpec]`（89 个 wire 名去重后约 84 条）。
2. `AgentEventType::wire_name()`（`event_type.rs:45`）改为查表；`SessionEventTypes::BUILTIN` 改为从表派生（`const` 无法运行时构造 → 保留字面量数组 + 新增测试断言两者一致）。
3. `StreamOptions::should_stream`（`agent/stream_options.rs:36`）、`EventCategory::of`（`:122`）改为读 `streamed` 列。

**验收**：
- 新增测试 `every_wire_name_has_exactly_one_spec`、`builtin_list_matches_specs`、`agent_wire_names_are_unique`。
- 现有 `stream_options` 测试全绿（行为不变）。

**反向验证**：故意把 `model/chunk` 改成 `required=false` → `builtin_list_matches_specs` FAILED。
**风险**：低（纯数据 + 查表）。**回滚**：删文件，恢复两处 match。

---

### T5（P1）`EventMeta`：复用 `EventData` 的信封半截

**目标**：消除 `id` / `created_at` 两份真相；为 T6 删 `fields` 铺路。

**改动点**：
1. `core/src/event/event.rs:80`：`EventData` **保留** `id` / `created_at` / `source` / `metadata`，**不删 `fields`**（T6 再删），新增 `pub created_at_ms: i64`（`#[serde(default = now_ms)]`，同时保留 `created_at` 字符串供 20 个引用点兼容），新增 `fn meta(&self) -> EventMeta`。
2. 新增 `EventMeta { id, created_at_ms, source, metadata }`（可放同文件）。
3. `SessionEvent`（`session/event.rs:16`）**不改字段**（它已有 `event_id`/`occurred_at`），只新增 `fn meta(&self) -> EventMeta` 与 `From` 互转。

**验收**：
- JSON 快照测试：`EventData` 序列化包含 `createdAt`（旧）与 `createdAtMs`（新），旧字段值不变。
- `cargo test --workspace` 全绿（20 个引用点不动）。

**反向验证**：把 `created_at_ms` serde 名改为 `createdAt` → 快照测试 FAILED。
**风险**：中（序列化形状）→ 靠「只增不改」规避。**回滚**：删新增字段。

---

### T6（P1）`EventPayload` 类型化（写入侧）+ 读取侧集中解析

**目标**：消灭 `fields.get(...)` 50 处 / `data.get(...)` 分散解析；**不改存储格式**（`payloadJson` 不变）。

**改动点**：
1. 新增 `core/src/session/payload.rs`：按 §3.2 定义 30 个变体 + `PayloadTag` + `fn decode(event: &SessionEvent) -> Result<EventPayload>`； 零写入的 8 类走 `EventPayload::Reserved(Value)`。
2. `recorder.append<T: Serialize>` 保持不变（写侧已是 `&T`）。新增 `append_payload(&self, kind: EventKind, payload: &EventPayload)` 供强类型写入点迁移（先迁 `model/chunk`、`state/checkpoint`、`interaction/*` 三类大对象）。
3. `core/src/session/views.rs`：`apply_transcript_event` / `apply_projection_event` / `apply_pending_event` / `SessionTurns::apply_event` 改为 `match payload::decode(event)?`。
4. 保留 `SessionEvent::payload_json` 与 `EventPayload` 的**单向往反测试**：`decode(synthesize(p)) == p`。

**验收**：
- 新增 30 个变体的 serde 往返测试（每个变体一条）。
- 新增 `payload_json_is_byte_stable`：对同一业务事件，旧 `json!` 字面量与 `EventPayload` 序列化结果的 `to_vec` 相等。
- `views.rs` 里 `rg "data\\.get\(" core/src/session/views.rs` 命中数归零（或仅剩 Reserved 分支）。

**反向验证**：改 `ModelChunk.merged_chunks` 为 `merged` → 往返测试 FAILED。
**风险**：中高（改动面广）→ 按 `recorder.rs` / `views.rs` 分两笔提交；先做只读侧（decode）再做写入侧。

---

### T7（P1）投影器统一：`SessionEvent → 目标协议`

**目标**：ACP / AGUI 不再各自从 `AgentEvent.fields` 取字段。

**改动点**：
1. `extensions-acp/src/event.rs`：新增 `project(event: &SessionEvent) -> Vec<SessionNotification>`；`map_delta_event` 保留用于增量通道。
2. `extensions-acp/src/session.rs:1892-2000` 的巨型 match：事实类分支改为 `project(&session_event)`；增量类分支保持。
3. `extensions-agui/src/adapter/strategy/converters.rs` 同理（第二步）。
4. `harness/src/agent/subagent/protocol/event.rs` 改为 `SessionEvent → 帧`。

**验收**：
- 现有 ACP 通知序列快照测试全绿（`extensions-acp`）。
- 新增测试：同一 `SessionEvent` 经 `project` 与经「先转 AgentEvent 再 map」两条路径得到的通知集合相等（迁移期双路径对账）。

**反向验证**：去掉 `project` 中的 `ToolKind` 映射 → 快照测试 FAILED。
**风险**：中高（外部协议行为）→ 双路径对账测试先行；AGUI 放第二步。

---

### T8（P2）`Arc` 共享 + `AgentEvent` 降格

**目标**：一个 chunk 只分配一份 payload；`AgentEvent` 从「独立枚举模型」变为「`SessionEvent` 的流式视图」。

**前置条件**：T4/T5/T6/T7 全部完成且全绿。

**改动点**：
1. `SessionEvent` 新增 `payload: Arc<EventPayload>`（`#[serde(skip)]` 运行时缓存）+ `payload_json`（磁盘格式不变）；读入时惰性填 `Arc`。
2. 新增 `AgentEventView { kind: EventKind, meta: EventMeta, payload: Arc<EventPayload> }` 与 `impl From<&SessionEvent>`；新代码用 View。
3. `AgentEvent` enum 标记 `#[deprecated]`，保留 68 个引用点编译；逐 crate 迁移（先 core → harness → acp/agui → 其余）。
4. `recorder.observe(&AgentEvent)`（`recorder.rs:1755`）删除，改为在产生点直接 `append_payload`。

**验收**：
- `stream_events` 签名不变（`BoxStream<AgentEvent>`），但每个 delta 的 payload 与对应 `SessionEvent` 共享同一 `Arc`（用 `Arc::ptr_eq` 断言）。
- `rg -c "observe\("` 在 `core/src/session` 中为 0。

**反向验证**：保留 `observe` 旧路径 → `ptr_eq` 测试 FAILED。
**风险**：高（影响 10 个 crate / 68 文件）→ **建议拆成 4 个单独 PR**；允许长期共存，不阻塞 M1-M3。

---

### T9（P2）会话 GC 入口 + 快照对象回收 + legacy 分支清理

**目标**：解决回收盲区 1/2/6（见 §1.4），让历史会话也能被回收。

**改动点**：
1. `core/src/session/log.rs`：`SessionLog` trait 新增 `async fn compact(&self, retention: &SessionRetention) -> Result<CompactReport>`（默认实现返回 `Ok(Default)`）。
2. `journal.rs` 实现 `compact`（不需要 writer 的部分）：
   - 若有 `snapshot.previous.json` → `floor = previous.seq`；
   - 否则：从 head 反向走到 `batch_newest <= now - cutoff` 的边界，取 `floor = last_seq`（**按年龄切尾**）；
   - 调 `reclaim(floor, retention)`（循环直到删不动或达到上限）；
   - `mark_reclaimed(floor)`。
3. 入口：`crates/agentscope-harness/examples/session_gc.rs`（`--dry-run` / `--older-than 30d` / `--batch N`）+ 可选 CLI 子命令。
4. 旧快照对象回收：`list_paths("snapshots")` → 保留 `snapshot.json` 与 `snapshot.previous.json` 指向的两个 hash 对象，其余删（受 `PRUNE_BATCH` 限制）。
5. `harness/src/agent/session/storage.rs:446` legacy 分支：若 `agents` 目录不存在直接返回（省一次失败 `ls`）；若存在且为空同。

**验收**：
- 新增测试 `compact_reclaims_a_read_only_session`：构造 2000 个 commit（无 snapshot）→ `compact(cutoff=0)` → commit 数归零、`reclaimed.json` 写入、`transcript()` 返回「已被回收」错误而非 panic。
- 新增测试 `compact_keeps_only_two_snapshot_objects`。
- example 在真实工作区 `--dry-run` 跑一次，输出「将删除 N 个 commit / M 个 blob / K 个快照对象」。

**反向验证**：把 `compact` 改成直接 `return Ok(Default)` → 两个测试 FAILED。
**风险**：中（按年龄切尾会丢历史文本）→ 必须 `--older-than` 显式 opt-in，`dry-run` 为默认。
**回滚**：删除 trait 方法与 example；无存储格式变更。

---

### T10（P2，内存专项）`snapshot` 不再全量构造 + 全量 `to_vec`

**目标**：把每次 `publish_snapshot` 的峰值内存从「结构 + 序列化字节」（约 2×，长会话数十 MB）降到可忽略。

**现状**（`core/src/session/journal.rs:466-490`）：`SessionSnapshot` 含完整 `transcript.messages` + `projection.state_json` + `turns` + `pending` + `runs`；每 512 events / 60 s（`snapshot.rs:30/33`）就完整构造一次并 `serde_json::to_vec` 成一块 `Vec<u8>` 写盘。

**改动点**（两步，可分开交付）：
1. **步 A（低风险）**：`to_vec` 改为 `serde_json::to_writer` 写入可复用的 `Vec<u8>` 缓冲（复用 `SessionRecorder` 上的 scratch buffer），写完立即 `drop(snapshot)`，缩短结构存活时间。
2. **步 B（需扩接口）**：`AtomicSessionStorage`（`core/src/session/storage.rs`）新增 `compare_and_set_stream(path, expected, write: impl FnOnce(&mut dyn io::Write) -> Result<()>, len: u64)`，`WorkspaceSessionStorage` 用 `BufWriter` + 增量 SHA-256 实现；这样 `session.json` 对象无需整块 bytes。注意：`compare_and_set` 仍保留，流式路径只用于 snapshot 对象（不可变、内容寻址）。
3. **state 分片**：`projection.state_json` 超过阈值（建议 256 KB）时移到 `blobs/{hash}`（复用现有 blob 目录与回收逻辑），snapshot 只存 hash。对象从 MB 级降到 KB 级。

**验收**：
- 新增测试 `snapshot_state_is_chunked_when_large`：构造 5 MB `AgentState` → publish → `snapshot.json` 对象大小 < 512 KB，且 `load_verified` 能还原。
- 新增测试 `snapshot_falls_back_when_blob_missing`：删掉分片 blob → `replay_views` 降级为全链重放而不 panic（现有降级路径 `snapshot.rs:182`）。
- 新增测试 `snapshot_serialization_uses_scratch_buffer`（步 A）：同一 recorder 发两次快照，第二次不再新分配大块缓冲（用自定义 `Write` sink 统计）。

**反向验证**：删掉 state 分片 → `snapshot_state_is_chunked_when_large` FAILED。
**风险**：snapshot 不再自包含（读时多一次 blob 读）；旧快照（无分片）必须继续可读（向后兼容，靠 `Option<hash>` 字段）。
**回滚**：分片开关置 `None` 阈值即可（不删代码路径）。

---

### T11（P2，内存专项）`transcript` 流式化 + 去掉二次克隆

**目标**：消除 transcript 折叠的双份（`HashMap<String, Msg>` + `ordered` + 最终 `.cloned()`），并让 `session_search` 不物化完整 transcript。

**现状**（`core/src/session/views.rs`）：

```rust
// :341  TranscriptFold { ordered: Vec<String>, messages: HashMap<String, Msg> }
// :236  finish_transcript
transcript.messages = fold.ordered.into_iter()
    .filter_map(|id| fold.messages.get(&id).cloned())   // ← 每个 Msg 再克隆一遍
    .collect();
```

即：峰值 = 1× 折叠结构 + 1× 最终 `Vec<Msg>`。

**改动点**：
1. `finish_transcript` 改用 `remove()` 的 move 语义，去掉 `.cloned()`；返回 `(Transcript, remaining: usize)`（`remaining` 必须为 0，供测试断言）。
2. `TranscriptFold` 改为 `Vec<Msg>` + `HashMap<String, usize>`（id 索引），去掉重复 id 字符串与大 map 双重存管。
3. 新增 `views::visit_transcript(log, kinds, &mut FnMut(&Msg) -> Result<bool>)`：流式产出，`false` 提前中断；`SessionSearchTool::call`（`session_search.rs:251`）改用它，**不再构造完整 transcript**，命中 `limit` 条即 `return false`。
4. `session_history` 仍可物化（它本身就要输出完整 N 条），但只物化最后 `lastN` 条（现在 `:419` 先全量后截断——已核实无提前截断，`transcript.messages.len().saturating_sub(limit)` 之后再切片）。

**验收**：
- 新增测试 `finish_transcript_moves_messages`：断言 `remaining == 0`（move 语义的直接证据）。
- 新增测试 `search_stops_reading_after_limit`：用 `CountingStorage`，命中 1 条后断言只读了 1 个 commit。
- 新增测试 `transcript_order_preserved_with_duplicate_ids`（防 move 改造打乱顺序）。

**反向验证**：把 `remove` 改回 `get().cloned()` → `finish_transcript_moves_messages` FAILED（`remaining > 0`）。
**风险**：`visit_transcript` 与 `replay_views` 逻辑重复 → 共享一个内部 fold 遍历器（`for_each_event`），不复制语义。
**回滚**：`visit_transcript` 可保留但不使用；`finish_transcript` 回退一行。

---

## 5. 兼容与迁移

### 5.1 磁盘格式会不会变？

| 任务 | 是否改磁盘格式 | 说明 |
|---|---|---|
| T1 | 否 | 只改 flush 节奏；`model/chunk` payload 内容不变 |
| T2 | 否 | 只删文件，不改 schema |
| T3 | 否 | 只改读路径 |
| T4 | 否 | 纯内存查表 |
| T5 | **是（只增不改）** | `EventData` 新增 `createdAtMs`，保留 `createdAt` |
| T6 | 否 | `payloadJson` 字节不变（有 `payload_json_is_byte_stable` 保证） |
| T7 | 否 | 只改投影器 |
| T8 | 否 | `payload: Arc` 是 `#[serde(skip)]` |
| T9 | 否 | 只删文件 |
| T10 | **是（只增不改，向后兼容）** | snapshot 新增 `stateBlobHash` 可选字段；旧快照无该字段仍可读 |
| T11 | 否 | 只改内存折叠与搜索路径 |

`schema_version` 无条件保持 `1`（`session/event.rs:44`）。`SessionEventCodecRegistry::validate`（`event.rs:170`）不得因新增键失败。

### 5.2 数据窗口

- legacy `agents/` 树已于 2026-10-30 由用户删除（实测目录已空），**不需兼容**。
- 当前树只有 5 个会话 / 505 commit / 3.07 MB → **现在是做 T5/T6 的最佳窗口**；越往后活跃会话变多，迁移成本越高。
- 若其他机器上仍有 legacy 树：`list()` 的 legacy 分支保留可读（T3 只加 `exists` 短路），不强制迁移。

---

## 6. 测试与验收矩阵

| 任务 | 新增测试（建议名） | 命令 | 反向验证 |
|---|---|---|---|
| T1 | `merged_chunks_do_not_commit_per_batch` | `cargo test -p agentscope-core --lib session::` | 改回 `record()` → FAILED |
| T2 | `blobs_are_reclaimed_when_commits_exceed_prune_max_scan`、`reclaim_keeps_history_replayable` | `cargo test -p agentscope-core --lib session::journal` | 恢复 `complete` 门控 → FAILED |
| T3 | `transcript_never_reads_checkpoint_blobs`、`search_reads_each_commit_once` | `cargo test -p agentscope-core --lib session::`；`cargo test -p agentscope-harness --test harness_session_web_tools_test` | 恢复全量水合 → FAILED |
| T4 | `every_wire_name_has_exactly_one_spec`、`builtin_list_matches_specs` | `cargo test -p agentscope-core --lib event::` | 改 `required` 列 → FAILED |
| T5 | `event_meta_serialization_snapshot` | `cargo test -p agentscope-core --lib event::` | 改 serde 名 → FAILED |
| T6 | `payload_roundtrip_<variant>` ×30、`payload_json_is_byte_stable` | `cargo test -p agentscope-core --lib session::payload`；`cargo test -p agentscope-core --lib session::views` | 改字段名 → FAILED |
| T7 | `acp_projection_matches_legacy_path` | `cargo test -p agentscope-extensions-acp` | 去掉 `ToolKind` 映射 → FAILED |
| T8 | `delta_shares_payload_with_session_event` | `cargo test --workspace -j 2 --no-fail-fast` | 保留 `observe` 旧路径 → FAILED |
| T9 | `compact_reclaims_a_read_only_session`、`compact_keeps_only_two_snapshot_objects` | `cargo run -p agentscope-harness --example session_gc -- --dry-run` | `compact` 返回空 → FAILED |
| T10 | `snapshot_state_is_chunked_when_large`、`snapshot_falls_back_when_blob_missing`、`snapshot_serialization_uses_scratch_buffer` | `cargo test -p agentscope-core --lib session::snapshot` | 删掉分片 → FAILED |
| T11 | `finish_transcript_moves_messages`、`search_stops_reading_after_limit`、`transcript_order_preserved_with_duplicate_ids` | `cargo test -p agentscope-core --lib session::views`；`cargo test -p agentscope-harness --test harness_session_web_tools_test` | 改回 `get().cloned()` → FAILED |

### 6.1 全量回归口径

```powershell
cargo test --workspace -j 2 --no-fail-fast > $env:TEMP\ws.log 2>&1
'EXIT=' + $LASTEXITCODE
Select-String '^failures:' $env:TEMP\ws.log
Select-String '^test result:' $env:TEMP\ws.log | Measure-Object
```

判定：**无 `^failures:` 段 + `EXIT=0`**。

### 6.2 已知噪音（不是回归）

- `agentscope-extensions-training::runner`：进程级 `OnceLock` 静态注册表，并行跑时测试间竞争。
- `acp_stdio_thought_test`：依赖真实模型 reasoning，flaky。
- `Select-String 'FAILED'` 会误命中测试名（如 `..._failed_...`），不可作判据。
- Windows 磁盘坑：`target/debug/deps/*.pdb` 可能膨胀到数十 GB → 报 `os error 1455`；真凶是 `.pdb`，删即释放（可再生）。

---

## 7. 风险登记与已否决方案

### 7.1 已否决（附理由，避免后人重走）

| 方案 | 结论 | 理由 |
|---|---|---|
| 用 ACP `SessionUpdate` 统一框架事件 | ❌ 否决 | ① 外部 crate 版本主权（2.2.0 / schema 1.9，已开 6 个 `unstable_*`）；② 无 `seq`/`required`/`turn`/`run`/`lease`/snapshot 概念，57 类中大部分无对应物；③ 大量 UI-only 字段（`ToolKind`/`title`/`locations`/`Plan`）；④ 会让 `agentscope-core` 依赖 `agent_client_protocol`，破坏分层；⑤ 它是双向会话协议，不只是事件模型 |
| `SessionEvent` 借用 `&AgentEvent` | ❌ 否决 | ① 生命周期会穿透 `Commit`/`visit`/`SessionLog` trait，`Arc<dyn SessionLog>` 变 dyn-unsafe；② replay 时无源可借（磁盘上只有 `payload_json`），字段读回恒 `None`；③ 双真相源 |
| 用 `EventData.fields`（`JsonMap`）当 payload | ❌ 否决 | `fields` 就是当前病根（`fields.get(...)` 50 处）；且急切解析比 `payload_json` 惰性串更贵（写入多一次往返） |
| 把流式 delta 逐条写入 journal | ❌ 否决 | 直接重现 33 GB / 494k commit 问题；`model/chunk` 只需合并后的块（供 `InterruptedOutput` 恢复） |
| 用 `AgentEvent` 替代 `SessionEvent` | ❌ 否决 | `AgentEvent` 无 `seq`/`required`，无法表达 `state/checkpoint`/`inbox`/`turn` 生命周期；且 68 个消费点被迫理解存储概念 |

### 7.2 主要风险

| 风险 | 影响任务 | 缓解 |
|---|---|---|
| 误删仍被引用的 blob → replay 报错 | T2/T9 | 删失败的 commit 并入 live set；`reclaim_keeps_history_replayable` 自检测试 |
| 落盘粒度变粗 → 中断恢复精度下降 | T1 | 恢复精度由 `checkpoint_now`（每 iteration）保证，不依赖 chunk flush |
| `created_at` 改类型破坏 20 个引用点 | T5 | 只增不改（保留 `createdAt` 字符串），双字段共存 |
| 投影器重构改变 ACP 输出 | T7 | 双路径对账测试（`acp_projection_matches_legacy_path`） |
| `AgentEvent` 降格影响 10 个 crate / 68 文件 | T8 | 拆 4 个 PR，允许长期共存，不阻塞 M1-M3 |
| 按年龄切尾丢历史文本 | T9 | `--older-than` 显式 opt-in，`--dry-run` 默认 |
| snapshot 分片后不再自包含 | T10 | `stateBlobHash` 为 `Option`，旧快照兼容；blob 缺失时降级全链重放 |
| `to_writer` 流式写与现有 `compare_and_set(&[u8])` 接口冲突 | T10 | 先做步 A（scratch buffer），步 B 新增 `compare_and_set_stream`，不动旧方法 |
| `finish_transcript` 改 move 语义打乱顺序 | T11 | `transcript_order_preserved_with_duplicate_ids` 先行 |

---

## 8. 交接约定（给接手会话）

### 8.1 可改范围

| 任务 | 允许修改 |
|---|---|
| T1 / T2 / T3 / T4 / T5 / T6 | `crates/agentscope-core/src/{event,session}/**`，`crates/agentscope-harness/src/{agent/session,tool}/**` |
| T7 | 上述 + `crates/agentscope-extensions-{acp,agui}/src/**` |
| T8 | 全仓（逐 crate 迁移） |
| T9 | `core/src/session/**` + `harness/examples/**` |
| T10 | `core/src/session/**`（`journal.rs`/`snapshot.rs`/`storage.rs`） |
| T11 | `core/src/session/views.rs` + `harness/src/tool/session_search.rs` |

不得修改：`Cargo.toml` 的 `[workspace.dependencies]`（除非新增依赖必需且已在其他 crate 出现）；不得改 ACP `Cargo.toml` 的 `unstable_*` 列表。

### 8.2 提交纪律

- **一个任务一笔提交**，提交信息前缀 `perf(session):` / `refactor(session):` / `feat(session):`。
- 每笔提交前：`git status` 可见改动；`cargo test` 对应 crate 全绿；反向验证已做过并记录在提交消息尾部（`Reverse-check: <测试名> FAILED when <回退点> reverted`）。
- 不删既有断言、不 `#[ignore]`、不降级测试。

### 8.3 回执格式（交回主会话时）

```
任务：T<N>
改动：<文件列表，行数范围>
测试：<新增测试名 ×N>；命令 + EXIT + passed/failed
反向验证：<回退点> → <测试名> FAILED → 已恢复
指标：<量化变化，如 commit 数 13.7k → <1k>
未完成/阻塞：<原因，无则写无>
```

### 8.4 环境提醒（Windows / PowerShell）

- 不要用管道直连 `cargo test --workspace`（巨量输出触发 .NET Stack overflow）→ 先 `Out-File` 再 `Select-String`。
- 命令护栏是朴素子串匹配：`git add` 可能被拦 → 用 `git stage`；`rm` 子串被拒 → 拆词。
- `LocalFilesystem::write` **拒绝覆盖已存在文件** → 改文件要先删；追加走 `append_utf8_workspace_relative`。
- 单条命令超时约 300s → 长任务写 `.bat` 后台跑 + 轮询日志。

---

## 附录 A：现状事实速查（接手前先读）

| 事实 | 位置 |
|---|---|
| `AgentEvent` 32 变体 | `core/src/event/event.rs:19` |
| `EventData { id, created_at, source, metadata, fields }` | `core/src/event/event.rs:80` |
| `AgentEventType` + `wire_name()` | `core/src/event/event_type.rs:6` / `:45` |
| `SessionEvent` 信封 | `core/src/session/event.rs:16` |
| `SessionEventTypes::BUILTIN`（57） | `core/src/session/event.rs:111` |
| `recorder.observe()`（5/32 桥） | `core/src/session/recorder.rs:1755` |
| `record()` = append + flush | `core/src/session/recorder.rs:1349` |
| `capture_state()` / `checkpoint_now()` | `recorder.rs:1459` / `:1573` |
| 合并阈值（64/4KB/interval） | `core/src/session/models.rs:257` |
| `record_merged_chunk()` | `core/src/session/models.rs:303` |
| `read_commit()`（水合全部 blob） | `core/src/session/journal.rs:161` |
| `segments_through()`（全链反向读） | `journal.rs:238` |
| `reclaim()` / `complete` 门控 | `journal.rs:556` / `:582` |
| `reclaim_orphan_blobs()` | `journal.rs:672` |
| `visit()` / `commit_at()` | `journal.rs:1145` / `:379` |
| `reclaim_superseded_checkpoints()` | `journal.rs:1281` |
| `transcript()` / `replay_views()` | `core/src/session/views.rs:238` / `:283` |
| `SNAPSHOT_EVERY_EVENTS` / `SNAPSHOT_EVERY` | `core/src/session/snapshot.rs:30` / `:33` |
| `SessionRetention::default()`（prune, 30d） | `snapshot.rs:151` |
| `session_search` 外层循环 | `harness/src/tool/session_search.rs:251` |
| `register_all`（丢 context） | `session_search.rs:188` |
| `list()` legacy 分支 | `harness/src/agent/session/storage.rs:445` |
| `WorkspaceSessionStorage::read`（250ms 忙等） | `storage.rs:230` / `:243` |
| `LocalFilesystem::read`（全文件 copy + split） | `harness/src/filesystem/local.rs:396` |
| ACP `map_delta_event` | `extensions-acp/src/event.rs:714` |
| ACP 巨型 match | `extensions-acp/src/session.rs:1892` |
| `stream_events` 出口 | `harness/src/agent/harness_agent.rs:375` / `:395` |

## 附录 B：自校验命令集

```powershell
# 1) 枚举两侧类型
rg -n 'pub enum AgentEvent' -A 40 crates/agentscope-core/src/event/event.rs
rg -n 'BUILTIN: \[&.static str; 57\]' -A 60 crates/agentscope-core/src/session/event.rs

# 2) 找所有业务事件写入点（注意 turn/* 是 format! 动态生成）
rg -n '\.append\(' crates/agentscope-core/src/session crates/agentscope-harness/src/agent/session
rg -n 'format!\("turn/' crates/agentscope-core/src/session/recorder.rs

# 3) 找 fields 字符串键访问（T6 的工作量）
rg -c 'fields\.get\(' -g '*.rs' crates
rg -n 'data\.get\(' crates/agentscope-core/src/session/views.rs

# 4) 验证零写入类型
rg -n '"subagent/spawned"|"tool/chunk"|"model/retry"|"context/build"' -g '*.rs' crates

# 5) 目录体积（Windows）
Get-ChildItem .agentscope/sessions -Recurse -File | Measure-Object Length -Sum
```

---

## 附录 C：内存专项（峰值 vs 稳态）

### C.1 结论

本方案（T1–T11）**降低峰值内存，不降稳态 RSS**。稳态由 `AgentState.context` 决定，属 compaction/pruning 课题，不在本方案范围。

### C.2 逐任务内存影响

| 任务 | 影响 | 原因 |
|---|---|---|
| T1 落盘粒度 | **略增** ↑ | 缓冲更久，未提交 batch / `MergedChunk` 停留更久 |
| T2 reclaim 死锁 | 无 → | 只删文件 |
| T3 读放大（含 `local.rs` 零拷贝） | **大幅降** ↓↓ | 不读 checkpoint blob；去掉整文件复制 + 行指针表 |
| T4 `EventKind` 表 | 无 → | 几十条静态常量 |
| T5 `EventMeta` | **略增** ↑ | 新增 `created_at_ms`，且与 `createdAt` 双字段共存 |
| T6 `EventPayload` 类型化 | 取决于实现 | 禁止双存（见 C.4）；解码后不缓存中间 `Value` 才是降 |
| T7 投影器统一 | 小幅降 ↓ | 少一层 `fields` 的 `Value` 中间树 |
| T8 `Arc` 共享 | **明确降** ↓ | 每 chunk 从 2–3 份 String 降到 1 份；大对象（`Msg`/`ChatResponse`）共享 |
| T9 GC 入口 | 不直接降 → | 间接：commit 变少 → 每次 replay 折叠的事件变少 |
| T10 snapshot 流式 | **明显降** ↓↓ | 不再全量构造 + 全量 `to_vec`；state 分片后对象从 MB 降到 KB |
| T11 transcript 流式 | **明显降** ↓↓ | 去掉 `.cloned()` 双份；搜索不物化完整 transcript |

### C.3 量化估算（基于本次实测）

| 场景 | 现状峰值（估） | 全量实施后 | 变化 |
|---|---|---|---|
| 稳态（一轮对话中） | `AgentState.context` 1–3 MB + ACP 队列（有界 4096，`event_queue.rs:20`） | 同左 + T1 缓冲略增 | ≈ 持平 |
| `session_search` 搜 800 commit / 5 blob 的会话 | ~15–25 MB | ~0.5–1 MB | **↓ 约 95%** |
| 每次 `publish_snapshot`（512 events / 60 s） | 全量结构 + `to_vec` ≈ 2–5 MB（长会话数十 MB） | 分片后 < 512 KB + 流式写 | **↓ 1–2 个数量级** |
| 冷启动 open 长会话 | snapshot + 全量 transcript + 每 commit 水合 | commit 变少 + 流式折叠 | ↓ 依赖 T9/T11 |

### C.4 硬约束：禁止双存

```rust
// ❌ 反模式：磁盘串与枚举并存 → 内存反而增加
struct SessionEvent { payload_json: String, payload: EventPayload }

// ✅ 正确：磁盘侧 String、内存侧枚举，互斥；解码结果只交给消费者，不缓存
impl SessionEvent { fn payload(&self) -> Result<EventPayload> }
struct AgentEventView { payload: Arc<EventPayload> }
```

### C.5 如何验证“内存真的降了”

1. **集成层（Windows）**：
   ```powershell
   # 采样峰值工作集：跑 session_search 前后对比
   Get-Process -Name agentscope* | Select-Object Name, @{n='WS_MB';e={[math]::Round($_.WorkingSet64/1MB,1)}}
   ```
2. **单元层（更可靠）**：在测试 crate 里装一个统计 allocator（`#[global_allocator]`，只统计 `alloc/dealloc` 字节数与峰值），断言：
   - `transcript_never_reads_checkpoint_blobs`：峰值 < 2× 消息总字节；
   - `finish_transcript_moves_messages`：峰值 ≈ 1× 消息总字节（无二次克隆）；
   - `snapshot_state_is_chunked_when_large`：峰值 < 1 MB。
3. **反向验证**：回退到 `.cloned()` / 全量水合 / `to_vec` 任一实现，上述峰值断言必须 FAILED。

