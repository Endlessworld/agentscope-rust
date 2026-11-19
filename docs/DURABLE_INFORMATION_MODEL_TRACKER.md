# Durable 存储信息论模型 — 落地跟踪

> 最后更新：2026-11-20（`feature/sqlite`）
> 范围：`crates/agentscope-durable*`、`crates/agentscope-core/src/journal`、`crates/agentscope-extensions-acp` 的持久化路径。
> 关联：`docs/MEMORY_GAP_TRACKER.md`（内存/磁盘优化台账，L1–L31）。
> 状态：✅ 已落地 / 🟡 部分 / ❌ 未落地 / ⏸ 暂缓 / 🧪 需先测量。
> 数据来源：20 轮（8KB 用户 + 8KB 回复，`DIAG_TURNS=20`，`--features alloc-stats,durable-sqlite`；SQLite `durable.db` / FileStore `AGENTSCOPE_DURABLE_STORE=file`）；L30/L31 提交 `b143a3c` / `70302b2`。

## 0. 结论（先回答）

- **没有全部实现，也没有完全遵守五条原则。**
- 五原则：**P1 🟡、P2 🟡、P3 🟡（固定 15MB，非自适应）、P4 🟡、P5 ✅（L31 后；marker 无界增长是代价）**。
- 具体存储模型（`frames/`、`snapshots/`、`index/`）**❌ 未落地**：当前是 FileStore scope 文档 + append-only tail + 大 blob，SQLite 一行一条记录。
- 指标：冗余率 **🧪 未测**；更新成本 **❌**（FileStore `sbytes≈1.3MB/轮` ≈32×；SQLite `djournal:facts≈4.14MB/轮` ≈100×，含读/压实）；恢复成本 **🧪 未测**（S=15MB）；提交次数 **❌** 16/轮（目标 1–2）。
- 已落地：L30（checkpoint 间隔）、L31（prune 保留 `event-id`）、SQLite 后端 + ACP 接线、L24–L29 的存储/内存优化。
- 最大收益点：**P1**（`request/prepared` 全量 messages、`message/appended` / `context/replaced` 内联；checkpoint 间隔已做）+ **提交合并**（16→1–2）。P2 的 state diff 需先证明不重复事件日志；P1 的 event 级内容寻址需先复测（X17 曾中性回退）。

## 1. 形式化对照

| 要素 | 方案定义 | 当前实现 | 状态 |
|---|---|---|---|
| 信息源 | 每轮真正新增：新消息 + 工具结果 + 决策，约几 KB/轮 | 事件日志记录；`state/checkpoint` 已改为间隔（L30）；但 `request/prepared` 仍带全量 messages，`message/appended` / `context/replaced` 仍内联 | 🟡 |
| 信道 | 擦除 + 截断 + 并发交错（kill -9、半行写入、双写者） | 锁 + 原子写 + 单调 seq + head CAS + WAL；帧边界与 digest 分散在 `types/commit.rs`、`backend_thread_journal.rs`、`store/file.rs`、`durable-sqlite` | 🟡 |
| 失真 | 可丢「人读历史」，绝不可丢「状态机语义」 | `compact.rs` 允许清单 + `EVENT_RETENTION=1024` + `reclaim_superseded_checkpoints`；L31 保留 `event-id`；marker 无界增长 | ✅/⚠️ |

## 2. 五条设计原则

### P1 信息唯一性（每个信息单元只写一次）🟡

**现状**
- `state/checkpoint`：L30 后只在首次 capture 和每 15MB 写 full，其余 `checkpoint:deferred`；20 轮内最多 1 次 full（基线/首次）。
- `request/prepared`：仍携带全量 messages；`message/appended` / `context/replaced` 仍内联内容；`state/checkpoint` 也保存 `stateJson`。同一段对话在四条路径出现 3–4 次。
- 大 payload：FileStore 对 >16KB 的 block 走 `blobs/<digest>`；SQLite 没有 blob 表（X17 内容寻址层实测中性 +33MB，已回退）。
- `event-id` 标记：L31 保留；它是幂等元数据，不是内容副本。

**证据**
- `crates/agentscope-core/src/journal/recorder.rs`：`CHECKPOINT_INTERVAL_BYTES`、`bytes_since_checkpoint`、`checkpoint:deferred`；`request/prepared` / `message/appended` / `context/replaced` 的 payload 构造。
- `crates/agentscope-durable/src/store/file.rs`：`BLOB_THRESHOLD` / `blobs`；`crates/agentscope-durable-sqlite/src/lib.rs`：无 blob 表。
- X17：`docs/MEMORY_GAP_TRACKER.md` §4。

**缺口**
- 没有 `messages_digest` + 新增消息 hash 列表；没有 event 级 `$blob` 引用；没有把 `context` 表示为消息投影。
- 没有 redundancy 指标，无法证明「同一内容只存一份」。

**动作**
1. 先加探针：统计 `request/prepared`、`message/appended`、`context/replaced`、`state/checkpoint` 的字节与去重后的唯一字节（§4）。
2. 评估 `request/prepared` 去全量 messages：改为 `messages_digest` + 新增消息 hash 列表；需要 `PROTOCOL_VERSION` + Java 同步；模型请求字节必须逐字节不变（prompt cache）。
3. 评估 event 级内容寻址：先复测 X17（SQLite blob 层曾中性）；不要直接假定有收益。
4. `context/replaced` 只在压缩/替换时写，评估能否由消息投影重建或只存引用。

**验收**：同一段消息只落一份（冗余率 <1.5）；模型请求字节不变；20 轮 `djournal:facts` 下降；跨语言契约测试通过。

### P2 分层编码（I 帧 / P 帧）🟡

**现状**
- L30 的 `state/checkpoint` 是 I 帧（间隔 full）；事件日志本身就是 P 帧（message/tool/context/state/…）；没有单独的 state diff、父 hash、快照索引；恢复是「最近 checkpoint + 尾部事件」。

**证据**：`recorder.rs` 的 `checkpoint_payload` / `state/checkpoint`；`JournalProjection`；`reclaim_superseded_checkpoints`；`checkpoint-history`。

**缺口**
- state diff 与事件日志是否重复未证明；没有 `parent_hash`；没有 `snapshots/<seq>.snap.idx`；恢复成本未量化。

**动作**
1. 先量化「事件日志已包含全部 delta」这一事实；若成立，不引入 state diff，只保留 I 帧间隔。
2. 若不成立，再设计 P 帧并升级 wire。
3. 加恢复成本探针（§4）。

**验收**：恢复成本 O(S)；没有额外的 state 写入；事件日志能独立重建状态（已有 `JournalProjection` 单测/恢复用例）。

### P3 快照间隔由 MDL 决定 🟡

**现状**
- `CHECKPOINT_INTERVAL_BYTES=15MB`；注释给出 `S* ≈ sqrt(2·B·R)`；B≈2.9MB、R≈40KB/轮 → 15MB；20 轮低于间隔，只有首次/基线 full checkpoint。
- 探针「跳过全部 checkpoint」为 110.6MB / peak 6.78MB；间隔版 82.8MB / peak 4.37MB，间隔版更优。

**证据**：`recorder.rs` 常量/注释；L30 20 轮实测（`MEMORY_GAP_TRACKER.md` §1.6）。

**缺口**
- 固定常量，非运行时自适应；没有恢复成本测量；长会话跨过 15MB 后需验证。

**动作**：加 B/R 统计与恢复读取统计；按 `S*` 动态调整（可选）；保持 15MB 作为默认。

**验收**：写入 + 恢复总成本在 S* 附近；20/100 轮 A/B 可复现。

### P4 原子帧 + 单调序号 = 擦除信道上的 ARQ 🟡

**现状**
- 已有：`Commit`/`JournalEvent` 的 seq/event_id/commit_id；`head` 的 seq/commit_id/lease/epoch CAS；`journal_position`；`replay_head` 与 `event-id` 幂等；FileStore append-only tail（B3 phase1）；SQLite 行 + WAL。ACP 不走 core `AtomicThreadJournal`。

**证据**：`crates/agentscope-durable/src/types/commit.rs`、`backend_thread_journal.rs`、`store/file.rs`、`durable-sqlite/src/lib.rs`、`backend/compact.rs`。

**缺口**
- 没有统一的 `seq | commit_id | len | payload | digest(prev)` 帧格式；边界/digest 分散在 `commits/`、head、scope 文档；没有 `frames/<seq>.frame`；没有从 head 反向验证 digest 链的读端；崩溃矩阵不完整。

**动作**
1. 先定义统一帧（formatVersion、digest 链、读端停在最后完整帧）。
2. 在 FileStore 和 SQLite 各做一层适配；迁移/双写/回滚。
3. 跨语言版本。

**验收**：单一帧格式；kill -9/半行/双写者测试；幂等重放；旧数据迁移。

### P5 有损压缩边界（只对人读历史有损）✅（L31 后）

**现状**
- `compact.rs` 明确 never dropped：tool outcome、message、baseline/restore、未被替换的 event；`EVENT_RETENTION=1024` 只丢旧 public events；`reclaim_superseded_checkpoints` 只丢旧 checkpoint payload；L31 后 `event-id` 保留。

**证据**：`compact.rs` 模块文档/`prune`；`70302b2`；`prune_tests::a_pruned_event_keeps_its_id_marker_so_a_replay_cannot_resurrect_it`；20 轮 `jentries=357 < 1024`。

**缺口**
- marker 每个已提交事件一个 digest，无界增长；没有一张「不可有损字段」的测试清单；`prune` 的 marker 保留与存储效率有张力。

**动作**：给 marker 设计保留/归档策略（不能随事件删）；补「不可有损」字段的恢复测试；在 `compact` 文档中维护边界表。

**验收**：任何可丢弃项都有测试证明不影响状态机；marker 增长有上限/归档。

## 3. 具体存储模型对照

| 方案 | 当前 FileStore | 当前 SQLite | 结论 |
|---|---|---|---|
| `meta.json` | scope 的 `session`/`format` 键 | `agentscope_record` 行 | 🟡 等价但非独立 |
| `head.json` | scope 的 head/thread 记录 | 行 + 事务 | 🟡 |
| `frames/<seq>.frame` | `scopes/<digest>.json` + `scopes/<digest>.log`（B3 phase1） | `agentscope_record` 键值行 | ❌ 统一帧 |
| `blobs/<sha256>` | `blobs/<digest>`（>16KB） | 无（X17 已回退） | 🟡/❌ |
| `snapshots/<seq>.snap` + `.idx` | `state/checkpoint`/`checkpoint-history` + 内存 `DocumentIndex`（L28） | `state/checkpoint` 行 | ❌ 独立快照/索引 |
| `index/<prefix>.idx` | 前缀扫描；`turn-order`/`queue` | `agentscope_work` | 🟡 二级索引存在但非独立 |

结论：**具体存储模型未落地**。当前 FileStore 是「scope 文档 + append-only tail + 大 blob」；SQLite 是「一行一条记录 + WAL + 就绪索引」。`frames/`、`snapshots/`、`index/` 的统一布局需要独立立项（格式版本、迁移、崩溃矩阵、跨语言）。

## 4. 指标与当前基线

### 4.1 定义

| 指标 | 定义 | 当前 | 目标 | 状态 |
|---|---|---|---|---|
| 冗余率 | 磁盘字节 / 去重后的唯一字节 | 未测；同一消息在 `request/prepared`/`message/appended`/`context/replaced`/`state/checkpoint` 出现 3–4 次；L30 已去掉 19/20 次 full checkpoint | <1.5 | 🧪 |
| 更新成本 | 每次提交写入字节 / 新增信息字节 | FileStore `sbytes=1,299,520 B/轮`，按 40KB 新信息 ≈32×；SQLite `djournal:facts=82.8MB/20≈4.14MB/轮` ≈104×（含读/压实，非纯写） | 1–2 | ❌ |
| 恢复成本 | 恢复读取字节 / 状态字节 | 未测；S=15MB；20 轮 `jreads=3`、`parses=0` | O(1) | 🧪 |
| 提交次数/轮 | 写事务数 / 轮 | 16（FileStore `txns=16`；SQLite 计数器未接线，实际同） | 1–2 | ❌ |

### 4.2 20 轮基线（8KB + 8KB，L30 后）

| 指标 | SQLite | FileStore |
|---|---:|---:|
| `djournal:facts` | **82.8MB** | **322.1MB** |
| `djournal:read` | 47.0MB | 182.7MB |
| `flush:done` | 41.0MB | 135.0MB |
| `acp:settle` | 27.8MB | 34.9MB |
| DB/磁盘 | **4.76MB** | **4.23MB** |
| `peak_live` | 4.37MB | 6.58MB |
| 耗时 | **2.69s** | **9.99s** |
| `jentries` | 357 | 357 |
| `jreads` | 3 | 3 |
| `parses` / `picks` | 0 / 0 | 0 / 8 |
| `txns` / `writes` | 0 / 0（计数器未接线） | 16 / 1 |
| `sbytes` | 0（不适用） | 1,299,520 B |

### 4.3 需要补的探针

1. **冗余率**：按 event payload 统计总字节与去重后唯一字节；分别看 `request/prepared`、`message/appended`、`context/replaced`、`state/checkpoint`。
2. **更新成本**：区分纯写（`sbytes` / SQLite 写事务字节）与含读/压实的 `djournal:facts`；按轮归一化。
3. **恢复成本**：冷启动 `recorder::open` 从最近 checkpoint + tail 重放的读取字节/时间；`DIAG_TURNS=1/20` 对比。
4. **提交次数**：给 SQLite `Txn` 计数（`BEGIN IMMEDIATE` 次数），与 FileStore `txns` 对齐。

## 5. 已落地 / 已否定（不要重复踩坑）

### 已落地

- L24 settle 复用 journal projection；L25 只读事务文档缓存；L26 幂等读回后置；L27 append-only tail；L28 文档字节索引；L29 压实策略；**L30 checkpoint 间隔**；**L31 prune 保留 `event-id`**。
- SQLite `RecordStore` + ACP `durable-sqlite`（`durable.db`，单机预览）。
- 20 轮：SQLite 总分配 1180.8MB → 82.8MB（`djournal:facts`）等；详见 `MEMORY_GAP_TRACKER.md` §1.6。

### 已否定 / 中性（不要再直接做）

- X15：`JournalAppend.entries → RawValue`（R3/B2a），实测 +1MB/轮，完全回退。
- X16：全量文档索引（含只读事务），负收益；L28 限定写事务。
- X17：SQLite 内容寻址 blob 层，实测 **+33MB**，已回退；event 级内容寻址须先复测。
- X18：增量 `scope_bytes` 列，实测中性，已回退。
- R7/B2b：去 `payloadJson` 字符串转义，仅 1–2MB/轮，不推进。
- 「跳过全部 checkpoint」：110.6MB / peak 6.78MB，**不如间隔版** 82.8MB / 4.37MB；不要用 skip-all。
- 完全禁用压实：`djournal:facts` 329MB / DB 18.15MB，磁盘膨胀；不要禁用。

## 6. 路线图与验收

| 阶段 | 内容 | 状态 | 依赖/风险 | 验收 |
|---|---|---|---|---|
| 0 | 本文档 | ✅ | — | 本文 |
| 1 | 指标探针：冗余率/更新/恢复/提交 | ⏳ 下一步 | DIAG 计数器；不改格式 | 20 轮可复现；四项基线 |
| 2 | P1：`request/prepared` 去全量；`message`/`context` 去重/引用；event 级内容寻址 | ❌ | Java wire `PROTOCOL_VERSION`；prompt cache；X17 复测 | 同一消息一份；模型请求字节不变；冗余 <1.5 |
| 3 | 提交合并 16→1–2 | ❌ | 事务边界/tick 组提交；ACP 语义 | `txns` 1–2/轮；`djournal:facts`/`flush:done` 降 |
| 4 | P2：state P 帧/I 帧决策；父 hash；快照索引 | ❌ | 先证明不重复事件日志；格式/迁移 | 恢复 O(S)；无额外 state 写入 |
| 5 | P4/具体模型：统一 frame + snapshots/index + GC | ❌ | formatVersion、跨语言、崩溃矩阵；独立立项 | 单一帧；崩溃停在最后完整帧；幂等 |
| 6 | P3：自适应 S*（运行时 B/R） | 🟡 固定 15MB | B/R 统计；恢复探针 | 写入 + 恢复总成本在 S* 附近 |
| 7 | 生产化：SQLite CI 契约、schema 版本/迁移、`synchronous`、备份/WAL、ACP load/resume/crash/并发 | ❌ | 独立清单 | 生产门槛 |

**建议顺序**：1 → 3（提交合并，低风险）→ 2（P1 去重，高收益）→ 4（state diff 先测量）→ 5（统一帧，大工程）→ 6 → 7。

## 7. 风险与待决策

1. **P2：事件日志是否就是 P 帧？** 建议先测量；若事件已覆盖全部 delta，不做 state diff（避免多一层重复）。
2. **P1：`request/prepared` 去全量 messages** 需要新的 wire 版本与 Java 同步；先做 probe，确认收益。
3. **P1：event 级内容寻址 vs X17 的存储级 blob**：先复测，不假定。
4. **P4/具体模型：统一帧** 是否值得取代 FileStore scope/SQLite 行；大工程，先做崩溃/迁移设计。
5. **P5：`event-id` marker 的增长策略**（不能随事件删；按 commit 窗口归档？）。
6. **下一步**：先 §6.1（指标）还是 §6.3（提交合并）？建议先指标，再提交合并，再 P1 去重。

## 8. 验证命令

```powershell
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
