# Durable 存储信息论模型 — 落地跟踪

> 最后更新：2026-11-20（`feature/sqlite`）
> 范围：`crates/agentscope-durable*`、`crates/agentscope-core/src/journal`、`crates/agentscope-extensions-acp` 的持久化路径。
> 关联：`docs/MEMORY_GAP_TRACKER.md`（内存/磁盘优化台账，L1–L35）。
> 状态：✅ 已落地 / 🟡 部分 / ❌ 未落地 / ⏸ 暂缓 / 🧪 需先测量。
> 数据来源：20 轮（8KB 用户 + 8KB 回复，`DIAG_TURNS=20`，`--features alloc-stats,durable-sqlite`；SQLite `durable.db` / FileStore `AGENTSCOPE_DURABLE_STORE=file`）。最近提交：`6bc452a`（L34）、`22c12e5`（L35）、`7be409e`（第 1 项归因）。

## 0. 结论（先回答）

- **五原则没有全部落地**：P1 🟡、P2 🟡、P3 🟡（固定 15MB，非自适应）、P4 🟡、P5 ✅（L31）。
- **具体存储模型（`frames/`、`snapshots/`、`index/`）❌ 未落地**；但 `store_model_min_test` 已在独立临时库中验证该模型的三项主张：DB/唯一内容 **1.09**、写入量对比「每轮重写整份 state」**1122×**、快照 + 后续帧可逐字节重建状态。
- 四项指标：冗余率 🧪（模型验证 1.09，生产未测）；更新成本 ❌（SQLite `djournal:facts` ≈8.55MB/轮，按 40KB 新信息 ≈219×，含读/压实）；恢复成本 🧪（S=15MB）；提交次数 **15/轮**（11 次 recorder 提交 + 4 个 backend 站点）。
- **提交合并已归因并否证**（X20）：15 次提交各自对应恢复语义边界（dispatch、usage、输入+checkpoint、迭代、turn 收尾、finalization），合并到 1–2 次会破坏恢复语义；真正的成本在**单次事务**（≈0.66MB/事务，读整份文档 + 序列化 delta）。
- **下一步：B3-phase2（已获批准）** —— 内存键索引 + 快照，降低单次事务的文档读取成本。
- 已落地：L24–L35（含 L30 checkpoint 间隔、L31 prune 保留 `event-id`、L32/L33 P1 去重、L34 settle 轻量 fold、L35 SQLite 空间回收）、SQLite 后端 + ACP 接线。

## 1. 形式化（信息源 / 信道 / 失真）

历史问题：三者没有分开建模——把「状态」当成「信息」，于是每轮重写整份状态（L30 后已改为间隔）。

| 要素 | 方案定义 | 当前实现 | 状态 |
|---|---|---|---|
| 信息源 | 每轮真正新增：新消息 + 工具结果 + 决策，约几 KB–40KB/轮 | 事件日志记录；`state/checkpoint` 间隔写（L30）；`request/prepared` 已去全量（L32）；`message/appended` / `context/replaced` 仍内联 | 🟡 |
| 信道 | 擦除 + 截断 + 并发交错（kill -9、半行写入、双写者） | 锁 + 原子写 + 单调 seq + head CAS + WAL；帧边界与 digest 分散在 `types/commit.rs`、`backend_thread_journal.rs`、`store/file.rs`、`durable-sqlite` | 🟡 |
| 失真 | 可丢「人读历史」，绝不可丢「状态机语义」 | `compact.rs` 允许清单 + `EVENT_RETENTION=1024` + `reclaim_superseded_checkpoints`；L31 保留 `event-id`；marker 无界增长 | ✅/⚠️ |

## 2. 五条设计原则

### P1 信息唯一性（每个信息单元只写一次）🟡

**定义**：状态的每个 bit 都应能由事件序列确定性推导；同一份对话不应在四条路径各存一份。

```
request/prepared  →  只携带 messages_digest + 新增消息 hash 列表（L32 已做一半：messageIds + 摘要）
state/checkpoint  →  首轮 I 帧 + 后续 P 帧（L30 已做间隔；前缀帧能力就绪、默认关闭）
message/appended  →  内容寻址引用（未做；SQLite 无 blob 表）
context/replaced  →  由消息投影重建或只存引用（未做）
```

**现状**
- L32（P1-1）：`request/prepared` 只存 `messageIds` + 摘要，存储 4,709,502 B → 58,171 B；读取端从未消费该字段，模型请求字节不变。
- L33（P1-3）：`checkpoint-history` 只存 metadata + `journalSeq`，`state/checkpoint` 是状态的唯一副本（stored 再 −34%，`checkpoint-history` 500KB → ~1KB）。
- 仍内联：`message/assistant` 175KB/20 轮、`context/replaced`、`state/checkpoint` 的 `stateJson`（272,553 B/20 轮）、`turn/output` 176KB。
- 大 payload：FileStore 对 >16KB 的 block 走 `blobs/<digest>`；SQLite 无 blob 表（X17 实测 +33MB，已回退）。
- `event-id` 标记（L31）：幂等元数据，不是内容副本。

**证据**：`crates/agentscope-core/src/journal/recorder.rs`（`prepare_request` / `checkpoint_payload`）；`crates/agentscope-durable/src/store/file.rs`（`BLOB_THRESHOLD` / `blobs`）；`payload_report`（`durable-sqlite/examples`）。

**缺口**：`message` / `context` 去重未做；SQLite 无内容寻址层；没有冗余率探针。

**动作**：1) 加冗余率探针（§4.4）；2) `message/appended`、`context/replaced` 评估引用化（先确认 `events()` 回放消费方）；3) 不要重复 X17/X19（event 级内容寻址在 SQLite 上净变差）。

**验收**：同一消息只落一份（冗余率 <1.5）；模型请求字节不变；20 轮 `djournal:facts` 下降；跨语言契约测试通过。

### P2 分层编码（I 帧 / P 帧）🟡

**定义**：I 帧（完整快照，每 S 字节或 K 个事件一次）+ P 帧（增量，引用父 commit）；恢复 = 最近 I 帧 + 尾部 P 帧，代价 O(S + 尾部)。

**现状**
- L30 的 `state/checkpoint` 是 I 帧（间隔 full）；事件日志本身就是 P 帧（message/tool/context/state/…）。
- 没有单独的 state diff、父 hash、快照索引；恢复是「最近 checkpoint + 尾部事件」。
- 前缀帧（`contextFromEvents` + `stateDigest`）写入端与 `JournalProjection` 折叠端都已实现，`CHECKPOINT_PREFIX_FRAMES = false` 默认关闭。

**证据**：`recorder.rs` 的 `checkpoint_payload` / `state/checkpoint`；`JournalProjection`；`reclaim_superseded_checkpoints`；`checkpoint-history`。

**缺口**：state diff 与事件日志是否重复未证明；没有 `parent_hash`；没有 `snapshots/<seq>.snap.idx`；恢复成本未量化。

**动作**：1) 先量化「事件日志已包含全部 delta」（若成立则不引入 state diff，只保留 I 帧间隔）；2) 前缀帧开启前必须先把 `AgentScopeExecutionHost` 的恢复路径从 `commit.checkpoint` 改成 `JournalProjection`（否则恢复后上下文为空）；3) 加恢复成本探针。

**验收**：恢复成本 O(S)；没有额外的 state 写入；事件日志能独立重建状态。

### P3 快照间隔由 MDL 决定 🟡

**定义**：设快照字节 `B`、每轮新增事件字节 `R`、快照间隔 `S`（字节）。总成本 = 快照写入 `(R/S)·B` + 恢复读取 `≈ S/2`。对 S 求导：

```
S* ≈ sqrt(2 · B · R)        （Young 公式的离散版本）
B ≈ 2.9MB（整份 state）、R ≈ 40KB/轮 → S* ≈ 15MB ≈ 数百轮
```

**现状**：`CHECKPOINT_INTERVAL_BYTES = 15MB`（`recorder.rs`）；20 轮低于间隔，只有首次/基线 full checkpoint。

**证据**：`recorder.rs` 常量/注释；L30 20 轮实测（`MEMORY_GAP_TRACKER.md` §1.6）：「跳过全部 checkpoint」110.6MB / peak 6.78MB，**不如间隔版** 82.8MB / 4.37MB（注：82.8MB 是终态回退的错误基线，修正后见 §4.2）。

**缺口**：固定常量，非运行时自适应；没有恢复成本测量；长会话跨过 15MB 后需验证。

**动作**：加 B/R 统计与恢复读取统计；按 `S*` 动态调整（可选）；保持 15MB 作为默认。

**验收**：写入 + 恢复总成本在 S* 附近；20/100 轮 A/B 可复现。

### P4 原子帧 + 单调序号 = 擦除信道上的 ARQ 🟡

**定义**：每次提交是一个自描述帧 `seq | commit_id | length | payload | digest(prev_seq, payload)`；读端从 head 反向验证 digest 链，遇到缺口/损坏停在最后一个完整帧。幂等重放 = 重传，`event-id` = 去重窗口。

**现状**
- 已有：`Commit`/`JournalEvent` 的 seq/event_id/commit_id；`head` 的 seq/commit_id/lease/epoch CAS；`journal_position`；`replay_head` 与 `event-id` 幂等；FileStore append-only tail（B3 phase1，`f7d9783`）；SQLite 行 + WAL。ACP 不走 core `AtomicThreadJournal`。
- **提交次数 = 15/轮**（第 1 项归因，`7be409e`）：11 次 recorder 提交 + 4 个 backend 站点，且各自是恢复语义边界（见 §5「第 1 项」）。

**证据**：`types/commit.rs`、`backend_thread_journal.rs`、`store/file.rs`、`durable-sqlite/src/lib.rs`、`backend/compact.rs`。

**缺口**：没有统一的帧格式；边界/digest 分散在 `commits/`、head、scope 文档；没有 `frames/<seq>.frame`；没有从 head 反向验证 digest 链的读端；崩溃矩阵不完整。

**动作**：1) 定义统一帧（formatVersion、digest 链、读端停在最后完整帧）；2) FileStore 与 SQLite 各做一层适配（迁移/双写/回滚）；3) 跨语言版本。**注意**：不要试图把 15 次提交合并成 1–2 次（X20 已否证），统一帧的收益在「单次读/写的开销」，不在「次数」。

**验收**：单一帧格式；kill -9/半行/双写者测试；幂等重放；旧数据迁移。

### P5 有损压缩边界（只对人读历史有损）✅（L31 后）

| 数据 | 可否有损 | 理由 |
|---|---|---|
| 旧事件的业务 payload | 可折叠为 digest 或 `{"reclaimed":true}` | 客户端只读窗口内事件；恢复不依赖 |
| 旧快照 | 可删除 | 有更新的快照 |
| turn/tool 未结算状态 | **不可** | 丢失会导致重复执行副作用 |
| admission / receipt / 幂等标记 | **不可** | 丢失会导致重复提交 |
| 当前 state 的字段级语义 | **不可** | 状态机正确性 |

**现状**：`compact.rs` 明确 never dropped：tool outcome、message、baseline/restore、未被替换的 event；`EVENT_RETENTION=1024` 只丢旧 public events；`reclaim_superseded_checkpoints` 只丢旧 checkpoint payload；L31 后 `event-id` 保留。

**证据**：`compact.rs` 模块文档/`prune`；`70302b2`；`prune_tests::a_pruned_event_keeps_its_id_marker_so_a_replay_cannot_resurrect_it`；20 轮 `jentries=39 < 1024`（当前基线远未触发裁剪）。

**缺口**：marker 每个已提交事件一个 digest，无界增长；没有一张「不可有损字段」的测试清单。

**动作**：给 marker 设计保留/归档策略（不能随事件删）；补「不可有损」字段的恢复测试；在 `compact` 文档中维护边界表。

**验收**：任何可丢弃项都有测试证明不影响状态机；marker 增长有上限/归档。

## 3. 具体存储模型

```
session/
  meta.json                  formatVersion, session key, 创建时间（一次性）
  head.json                  seq, commit_id, lease, epoch（CAS 原子换）

  frames/<seq>.frame         seq | commit_id | len | payload | digest(prev)
                             ← 唯一的写入路径：追加一个帧
  blobs/<sha256>             内容寻址；同内容跨轮/跨会话只存一份
  snapshots/<seq>.snap       I 帧：整份状态 + 覆盖到的 seq
  snapshots/<seq>.snap.idx   稀疏索引：part → offset（只对快照建）
  index/<prefix>.idx         二级索引（turn/tool/execution id → frame seq）
```

关键点：

- **写入只追加帧**，永不重写整份文档 → 删掉 tail 压实、文档字节索引、trim 的一半；
- **读按索引**：`get(part)` 走最新快照的 idx，再叠加其后帧的 delta；没有 idx 时才回退线性扫描；
- **GC**：blob 引用计数 + 宽限期（`BLOB_GC_GRACE`），帧按 `EVENT_RETENTION` 与「最近快照之前的帧」回收；
- **提交批量化**：同批事件进一个帧（`flush` 已能把 pending 合成一个 commit，收尾批 5 个事件就是 1 次提交）；**不要**跨恢复边界合并（X20）。

与当前实现对照：

| 方案 | 当前 FileStore | 当前 SQLite | 结论 |
|---|---|---|---|
| `meta.json` | scope 的 `session`/`format` 键 | `agentscope_record` 行 | 🟡 等价但非独立 |
| `head.json` | scope 的 head/thread 记录 | 行 + 事务 | 🟡 |
| `frames/<seq>.frame` | `scopes/<digest>.json` + `scopes/<digest>.log`（B3 phase1，L27） | `agentscope_record` 键值行 | ❌ 统一帧 |
| `blobs/<sha256>` | `blobs/<digest>`（>16KB） | 无（X17 已回退） | 🟡/❌ |
| `snapshots/<seq>.snap` + `.idx` | `state/checkpoint`/`checkpoint-history` + 内存 `DocumentIndex`（L28） | `state/checkpoint` 行 | ❌ 独立快照/索引 |
| `index/<prefix>.idx` | 前缀扫描；`turn-order`/`queue` | `agentscope_work` | 🟡 二级索引存在但非独立 |

结论：**具体存储模型未落地**。当前 FileStore 是「scope 文档 + append-only tail + 大 blob」；SQLite 是「一行一条记录 + WAL + 就绪索引」。`frames/`、`snapshots/`、`index/` 的统一布局需要独立立项（格式版本、迁移、崩溃矩阵、跨语言）。

## 4. 四项指标

### 4.1 定义

| 指标 | 定义 | 当前 | 目标 | 状态 |
|---|---|---|---|---|
| 冗余率 | 磁盘字节 / 去重后的唯一字节 | 未测；模型验证 1.09（§4.3）；生产 stored 1,946,332 B，唯一内容未知 | <1.5 | 🧪 |
| 更新成本 | 每次提交写入字节 / 新增信息字节 | SQLite `djournal:facts` 170,996KB/20 ≈ 8.55MB/轮，按 40KB 新信息 ≈219×（含读/压实，非纯写）；FileStore `sbytes≈1.3MB/轮` ≈32× | 1–2 | ❌ |
| 恢复成本 | 恢复读取字节 / 状态字节 | 未测；S=15MB；20 轮 `jreads=3`、`parses=0` | O(1) | 🧪 |
| 提交次数/轮 | 写事务数 / 轮 | **15**（11 次 recorder 提交 + 4 个 backend 站点）；归因完成，**不可合并到 1–2**（X20） | 语义边界内最小化（当前 15 已接近下限） | 🟡 |

### 4.2 20 轮基线（8KB + 8KB，L34/L35 后，无第 2 项）

| 指标 | SQLite（L30 基线） | SQLite（P1-1 + P1-3） | SQLite（当前：+L34/L35） | FileStore（L30，待复测） |
|---|---:|---:|---:|---:|
| `djournal:facts` | 388.6MB | 167.0MB | **167.0MB** | 322.1MB |
| `djournal:read` | 122.7MB | 94.0MB | **94.0MB** | 182.7MB |
| `flush:done` | 176.0MB | 218.9MB | **217.1MB** | 135.0MB |
| `acp:settle` | 84.3MB | 57.9MB | **43.1MB**（L34） | 34.9MB |
| `checkpoint:payload` | 24.1MB | 23.6MB | **23.6MB** | — |
| `flush:batch` | — | 7.1MB | **7.1MB** | — |
| DB/磁盘 | 11.4MB | 4.12MB | **2.52MB**（L35） | 4.23MB |
| `peak_live` | 3.93MB | 3.80MB | **3.57MB** | 6.58MB |
| 耗时 | 6.46s | 5.33s | **≈5.3s** | 9.99s |
| `jentries` | 357 | 39 | **39** | 357 |
| `jreads` | 3 | 3 | **3** | 3 |
| `parses` / `picks` | 0 / 0 | 0 / 0 | **0 / 0** | 0 / 8 |
| `txns` / `writes` | 0 / 0（未接线） | 15 / 0 | **15 / 0** | 16 / 1 |
| 20 轮总分配 | ≈870MB | 640.9MB | **≈640MB（32.0MB/轮）** | — |
| stored（`payload_report`） | 7,106,083 B | 1,946,332 B | **1,946,332 B** | — |

要点：
- `flush:done` 比 L30 基线高约 41MB，是 `frame` 每 tick 多读一次 `checkpoint-history` + journal 条目（P1-3 的代价），总量仍是净降。
- stored 1,946,332 B 的构成：`state/checkpoint` 40 条（20 全量 + 20 deferred）= 272,553 B；`turn:<hash>` 20 × 18,941 B ≈ 378KB；`model/chunk` 356KB；`durable/finalization_started` 179KB；`turn/output` 176KB；`message/assistant` 175KB；`event:` 143KB；`message/user` 93KB；`request/prepared` 58KB。
- **第 2 项（frame 单事务 + `ThreadData.checkpoint_journal_seq`）已回退**：实测 `flush:done`/`djournal` 合计 +6.3MB，且跨运行噪声 ±4MB（压实节奏不同会让 `sbytes` 从 227,794 变到 716,018）。

### 4.3 模型验证（`store_model_min_test`）

独立临时 SQLite 库、20 轮、每轮 3×8KB + 每 5 轮 1×64KB，验证 §3 模型：

| 项 | 值 |
|---|---:|
| 唯一内容 `unique` | 754,264 B |
| 帧写入 `frames` | 2,671 B |
| 快照写入 `snapshots`（只存 content id） | 3,922 B |
| 「每轮重写整份 state」`legacy_rewrites` | 7,397,246 B |
| legacy / new | **1122×** |
| DB / unique | **1.09** |
| 保留快照数 | 2 |
| 快照 + 后续帧重建 | 逐字节相等 ✅ |

结论：内容寻址 + 增量帧 + 索引式快照把磁盘压到唯一内容的 1.09 倍；**这是 B3-phase2 的设计依据**（生产实现不必复制该布局，但应达到同一量级）。

### 4.4 待补探针

1. **冗余率**：按 event payload 统计总字节与去重后唯一字节（分别看 `message/appended`、`context/replaced`、`state/checkpoint`）。
2. **更新成本**：区分纯写（`sbytes` / SQLite 写事务字节）与含读/压实的 `djournal:facts`；按轮归一化。
3. **恢复成本**：冷启动 `recorder::open` 从最近 checkpoint + tail 重放的读取字节/时间；`DIAG_TURNS=1/20` 对比。
4. **提交次数**：SQLite `Txn` 计数已接线（`write_stats()`）。

## 5. 已落地 / 已否定（不要重复踩坑）

### 已落地

- L24 settle 复用 journal projection；L25 只读事务文档缓存；L26 幂等读回后置；L27 append-only tail；L28 文档字节索引；L29 压实策略；**L30 checkpoint 间隔 + turn 边界强制**（修正 deferred 期间 `previous_state` 抢先更新导致终态回退）；**L31 prune 保留 `event-id`**。
- **L32（P1-1）**：`request/prepared` 只存 `messageIds` + 摘要（4,709,502 B → 58,171 B）。
- **L33（P1-3）**：`checkpoint-history` 只存 metadata + `journalSeq`，`state/checkpoint` 是唯一副本。**坑**：`apply_journal` 必须返回它分配的**存储槽位**（不能用事件 JSON 的 `seq`，两者起点差一格）；`write_checkpoint` 的 `unchanged` 分支也要把 `journalSeq` 跟到新条目。
- **L34**：`UncertaintyTracker`（`core/journal/views.rs`）只折叠不确定工具调用，跳过 `state/checkpoint` 与 state JSON；ACP settle 缓存改用它，`acp:settle` 57.9 → **44.2MB**（`6bc452a`）。
- **L35**：SQLite 新库 `auto_vacuum=INCREMENTAL`（pragma 必须早于 WAL header，且值不能绑定参数 → 只能 `execute_batch` 字面量）、trim 后 `VACUUM`（`incremental_vacuum` 在 WAL 下实测一页不回收）；DB 4.12 → **2.52MB**（−39%），机制单测 `reclaim_space_frees_the_pages_a_trim_released` 覆盖（`22c12e5`）。
- **P 帧（前缀 checkpoint）**：写入端（`contextFromEvents` + `stateDigest`）与折叠端就绪，默认关闭；阻塞点见 P2。
- **第 1 项归因（`7be409e`）**：15 次写事务 = 11 次 recorder 提交 + 4 个站点（`turn.rs:42` admit、`turn.rs:402`/`499`、`tools.rs:13`；首轮另有 `identity.rs:12`）。11 次提交的 shape（每轮）：`model/chunk`×2、`model/end`×2、`request/prepared+model/dispatch`×2、`state/checkpoint+turn/output+step/end+turn/completed+run/end`×1、`message/user+state/checkpoint`×1、`step/start`×1、`durable/finalization_started+message/assistant`×1、`durable/finalization_completed`×1。结论：这些是恢复语义边界，**不可合并**；成本在单次事务（≈0.66MB/事务）。
- SQLite `RecordStore` + ACP `durable-sqlite`（`durable.db`，单机预览）。

### 已否定 / 中性（不要再直接做）

- X15：`JournalAppend.entries → RawValue`，+1MB/轮，完全回退。
- X16：全量文档索引（含只读事务），负收益；L28 限定写事务。
- X17：SQLite 内容寻址 blob 层，**+33MB**，回退。
- X18：增量 `scope_bytes` 列，中性，回退。
- X19：`TxnBackend` 级 event 内容寻址（`content.rs`，16KB 阈值），全绿但净变差（`djournal:facts` +68.6MB、`acp:settle` +73.1MB、总分配 +140MB），完全回退。
- **X20（第 1 项）**：把 `model/chunk` 改为 `record_if_open` 合并进 `model/end`（txns 15→13）后 `a_stalled_stream_is_journaled_within_the_flush_window` 失败——「停滞流窗口必须落盘」是恢复边界，回退。
- R7/B2b：去 `payloadJson` 转义，仅 1–2MB/轮，不推进。
- 「跳过全部 checkpoint」：不如间隔版。
- 完全禁用压实：磁盘膨胀。
- **`scope_parts` 接只读文档缓存**：`djournal:read` 96,252.1KB **逐位不变**——`pick_from_cache`（按 part 集合的答案缓存）已覆盖全部重复读，`fs::read` 未被触达；回退。
- **第 2 项（frame 单事务）**：无净收益，回退（见 §4.2）。

## 6. 路线图与验收

| 阶段 | 内容 | 状态 | 依赖/风险 | 验收 |
|---|---|---|---|---|
| 0 | 本文档 | ✅ | — | 本文 |
| 1 | 指标探针：冗余率/更新/恢复/提交 | 🟡 基线已取；冗余率与恢复成本待测 | DIAG 计数器；不改格式 | 四项可复现 |
| 2 | P1：`request/prepared` 去全量（L32）；`checkpoint-history` 引用化（L33）；`message`/`context` 去重 | 🟡 前两项已落地（stored 7.11MB → 1.95MB）；event 级内容寻址已否证（X17/X19） | `message`/`context` 需先确认 `events()` 回放消费方 | 同一消息一份；冗余 <1.5 |
| 3 | 提交合并 15→1–2 | ❌ **已否证（X20）**：15 次是恢复语义边界 | — | 不再追求；改为优化单次事务 |
| 3b | **B3-phase2：内存键索引 + 快照**（降单次事务 0.66MB/事务） | 🟡 **已批准，待实施** | `store/file.rs` 布局、崩溃恢复矩阵、`formatVersion` | `djournal:facts` 明显下降；DB/unique 接近 §4.3 |
| 4 | P2：state P 帧/I 帧决策；父 hash；快照索引 | 🟡 前缀帧能力就绪、默认关闭 | 先把 `ExecutionHost` 恢复改成 journal 投影 | 恢复 O(S)；无额外 state 写入 |
| 5 | P4/具体模型：统一 frame + snapshots/index + GC | ❌ | formatVersion、跨语言、崩溃矩阵 | 单一帧；崩溃停在最后完整帧 |
| 6 | P3：自适应 S*（运行时 B/R） | 🟡 固定 15MB | B/R 统计；恢复探针 | 总成本在 S* 附近 |
| 7 | 生产化：SQLite CI 契约、schema 版本/迁移、`synchronous`、备份/WAL、ACP load/resume/crash/并发 | ❌ | 独立清单 | 生产门槛 |

**建议顺序**：1（指标）→ **3b（B3-phase2，已批准）** → 2（`message`/`context` 去重）→ 4（host 恢复改投影后开 P 帧）→ 5（统一帧）→ 6 → 7。

## 7. 风险与待决策

1. **B3-phase2 的格式边界**：只改 Rust `FileStore` 私有磁盘布局，不动 `durable.v1` wire；若将来 Java 与 Rust 共用一个 FileStore root，快照/日志头必须带 `formatVersion` 显式拒绝不兼容读端（提前加，成本低）。
2. **P2 阻塞点（已验证）**：前缀帧没有会话可拼接，必须先把 `AgentScopeExecutionHost` 恢复改成走 `JournalProjection`，再开 `CHECKPOINT_PREFIX_FRAMES`。
3. **P5 marker 增长**：`event-id` 不能随事件删；需要归档/上限策略。
4. **统一帧（阶段 5）**是否值得取代 FileStore scope/SQLite 行：大工程，先做崩溃/迁移设计。
5. **第 5–7 项（`turn.output`/`model/chunk`/`finalization_started` 去重）**：收益仅磁盘 −555KB、内存 0，且需读侧 `read_parts`（会引入整份文档读）；建议合并为一次批量并在 B3-phase2 之后做（B3-phase2 的索引/快照会让读侧补读变便宜）。

## 8. 验证命令

```powershell
# 模型验证（内容寻址 + 增量帧 + 索引式快照）
cargo test -p agentscope-durable --test store_model_min_test -- --nocapture

# SQLite 20 轮
$env:DIAG_TURNS='20'; $env:DIAG_USER_BYTES='8192'; $env:DIAG_REPLY_BYTES='8192'
Remove-Item Env:AGENTSCOPE_SKIP_CHECKPOINT, Env:AGENTSCOPE_DURABLE_STORE -ErrorAction SilentlyContinue
cargo test -p agentscope-extensions-acp --features alloc-stats,durable-sqlite --test acp_live_memory_test -- --nocapture --test-threads=1 --ignored

# FileStore 20 轮
$env:AGENTSCOPE_DURABLE_STORE='file'
cargo test -p agentscope-extensions-acp --features alloc-stats,durable-sqlite --test acp_live_memory_test -- --nocapture --test-threads=1 --ignored

# 回归
cargo test -p agentscope-durable -j 2 --no-fail-fast
cargo test -p agentscope-durable-sqlite -j 2
$env:DURABLE_CONTRACT_STORE='sqlite'; cargo test -p agentscope-durable --test backend_contract -j 2
cargo test -p agentscope-extensions-acp --lib --features durable-sqlite -j 2
cargo fmt --all -- --check
```

## 9. 变更记录

| 日期 | 变更 |
|---|---|
| 2026-11-20 | 初版：形式化（源/信道/失真）、五原则现状、具体模型对照、四项指标、路线图与验收；基于 L30/L31 与 SQLite 20 轮基线。 |
| 2026-11-20 | X19（event 级内容寻址）净变差（+140MB）完全回退；同时修正 L30 的 turn 边界终态回退（`checkpoint_due`/`unchanged`），SQLite 修正基线 388.6MB/11.4MB/3.93MB/6.46s。 |
| 2026-11-20 | **P1-1（L32）** + **P1-3（L33）**：stored 7.11MB → 1.95MB；20 轮总分配 ≈870 → 640.9MB、DB 11.4 → 4.12MB、`peak_live` 3.80MB、5.33s。P 帧实现完成但默认关闭。 |
| 2026-11-20 | **L34**：settle 轻量 fold，`acp:settle` 57.9 → 44.2MB（`6bc452a`）。 |
| 2026-11-20 | **L35**：SQLite 空间回收（auto_vacuum 顺序坑 + trim 后 VACUUM），DB 4.12 → 2.52MB（−39%）（`22c12e5`）。 |
| 2026-11-20 | **第 2 项回退**：frame 单事务实测无净收益，回退。 |
| 2026-11-20 | **`scope_parts` 文档缓存实测零收益，回退**（`djournal:read` 逐位不变）。 |
| 2026-11-20 | **第 1 项归因**：15 次写事务 = 11 次 recorder 提交 + 4 个站点；X20（`model/chunk` 合并实验）被 `a_stalled_stream_is_journaled_within_the_flush_window` 否决并回退；结论：合并空间受恢复语义限制，成本在单次事务（`7be409e`）。 |
| 2026-11-20 | **文档重构**：合并重复的「五项原则」「四项指标」两套章节为一套（删除概述副本），纳入 L34/L35、X20、`scope_parts` 零收益、第 1/2 项结论与当前 20 轮基线；`store_model_min_probe.rs` 转正为 `store_model_min_test.rs`（断言 DB/unique 与 legacy/new，作为 B3-phase2 的设计依据）；B3-phase2 已获批准。 |
