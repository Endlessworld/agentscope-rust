# Session journal 分段/快照/回收（P0-2）设计方案

> 目标：内存与磁盘占用小、智能体可长期稳定运行、异常中断恢复时损失最小。
> 现状基线：`f1674b1` + 2026-10-28 的 P0/P1/P2 修复。

## 1. 现状与瓶颈

```text
session.json              SessionHeader
head.json                 Head { seq, epoch, owner, lease, commitId }   ← 唯一 CAS 指针
commits/<hash>.json       Commit { batchId, parent, firstSeq, lastSeq, hash, events, payloadBlobs }
blobs/<hash>              >64KiB payload 溢出
writer-seals/<hash>.json
exports/<hash>.json
```

- 写入：每次 flush 一个 commit 文件（不可变、内容寻址、校验和自证），`head.json` 用 CAS 推进。
  **崩溃损失 = 0 个已提交事件**，这是当前设计的核心优点，必须保留。
- 读取：`segments_through` 从 head 沿 `parent` 回溯到目标 seq，然后逐个 commit 反序列化 + fold。
  冷启动是 **O(全部 commit 数)**，长会话是几千个文件、几千次读 + 解析。
- 内存：fold 出的 `SessionTurns`/`Transcript`/`SessionProjection` 随会话线性增长。
- 磁盘：所有历史永久保留；`model/chunk` 已合并，但 tool result / checkpoint 大对象仍在。

结论：瓶颈不在写入粒度（已经是最小损失），而在**读取时的重放长度**和**永不回收的历史**。

## 2. 已否决的方案

| 方案 | 否决理由 |
|---|---|
| 写入侧把 N 个 batch 合并成一个 commit 文件 | 需要内存缓冲，崩溃时丢掉最多 N 个 batch —— 直接违背「恢复损失最小」 |
| 单一 append-only 日志文件 | `AtomicSessionStorage` 是整文件 CAS（`compare_and_set(path, version, bytes)`），没有 append 原语；要新增追加语义就得放弃「内容寻址 + 校验和」 |
| 直接裁剪/丢弃老事件 | 丢 `started`/`inputs` 的去重能力，`SessionTurns` 无法再判重放；`transcript` 直接缺内容 |
| 只做 retention 不做快照 | 历史被删后无法重建 projection，下次冷启动只能从 seq 0 开始 —— 比现状更糟 |

## 3. 选定方案：不可变快照 + 尾段保留

核心思想：**把「已经折叠好的视图」当作一等持久对象，读取从快照起步，写入仍是一事件一提交。**

### 3.1 新增对象

```text
snapshot.json                 SnapshotPointer { seq, commitId, hash, version }   ← CAS 发布点
snapshots/<hash>.json         Snapshot（不可变、内容寻址）
```

```rust
struct Snapshot {
    format: u32,              // 演进用
    as_of_seq: u64,
    as_of_commit_id: String,  // 必须是链上真实存在的 commit
    projection: SessionProjection,   // stateJson / uncertainToolCalls / activeRuns / verifications
    turns: SessionTurns,             // 含 inputs（去重所需）
    transcript: Transcript,          // 会话可见内容
    pending: BTreeMap<String, SessionEvent>, // 未决交互（准入/恢复热路径都要）
    runs: Vec<String>,               // 见过的执行 run id（上限 4096，去重守卫用）
}
```

`Snapshot` 自身用 `JournalSessionLog::hash(bytes)` 命名，读时校验，损坏就忽略并回退全量重放。

### 3.2 写入时机（由 recorder 拥有 writer 租约时执行）

- 触发：`durable_seq - last_snapshot_seq >= SNAPSHOT_EVERY_EVENTS`（建议 512）
  或 `now - last_snapshot_at >= SNAPSHOT_EVERY`（建议 60s），且当前无 in-flight batch。
- 顺序：**先写不可变 `snapshots/<hash>.json`（幂等，可重试）→ 再 CAS 发布 `snapshot.json`**。
  指针是唯一提交点：写对象时崩溃只是留下孤儿文件，下次覆盖或忽略，**不丢任何事件**。
- 内容来源：优先用 recorder 已持有的 fold 状态；没有就从上一个快照 + 尾段重放一次。
- 只有持租约的 writer 可以生成快照/回收（天然单写者，无需额外锁）。

### 3.3 读取路径

- `SessionProjection::read` / `SessionTurns::read` / `transcript`：
  1. 读 `snapshot.json`；校验 `as_of_seq <= head.seq` 且 `as_of_commit_id` 在链上可达；
  2. 命中 → 以快照为初值，`visit(as_of_seq, head.seq)` 只折叠尾段；
  3. 未命中/校验失败 → 现状全量重放（安全降级，行为不变）。
- 收益：冷启动重放从 O(全部 commit) 降到 O(尾段 commit)（≤ 512 事件），内存只需折叠尾段。

### 3.4 历史回收（retention）

- 保留最新 `SNAPSHOT_KEEP = 2` 份快照（当前 + 上一份，用于指针损坏时回退）。
- 删除**被较旧的那份快照完整覆盖**的 commit 文件与其独占 blob：
  即 `commit.last_seq <= snapshot_prev.as_of_seq` 且 `commit.id != snapshot_prev.as_of_commit_id`。
- `segments_through` 需要知道「截断点」：从 head 回溯到 `snapshot_prev.as_of_commit_id` 即停止，
  不再要求链一直连到 seq 0。`scan(0, n)` 语义随之变为「快照 + 尾段」。
- 崩溃安全：删除发生在指针发布之后；删一半崩溃只是残留文件，下一轮再删。
- 保守默认：`retention` 默认开启但保留「最近 N 天」（建议 30 天）内的全部 commit，只有更老的才删；
  `AGENTSCOPE_SESSION_RETENTION=off` 可关闭（导出/审计场景）。

### 3.5 为什么不破坏崩溃恢复

| 崩溃点 | 结果 |
|---|---|
| 写快照对象中途 | 孤儿不可变文件；指针未动；下次重写 |
| 指针 CAS 前 | 旧指针仍有效；尾段照常重放 |
| 指针 CAS 后、删除前 | 只是磁盘多一点；下轮再删 |
| 删除中途 | 残留文件；快照已覆盖这些 commit，读路径不再需要它们 |

所有情况下**已提交事件一个不丢**，与现状一致。

## 4. 分阶段实施

| 阶段 | 内容 | 规模 | 风险 |
|---|---|---|---|
| S1 | `Snapshot` 对象 + `snapshot.json` 指针 + 三个读路径的快照起步 + 等价性测试（快照读 vs 全量读结果必须逐字段相同） | ~450 行 | 低（只加速，不改语义） |
| S2 | recorder 侧定时/定量生成快照 + `SessionMetrics` 增加 `snapshot_seq`/`snapshot_age_ms` | ~200 行 | 低 |
| S3 | retention：`segments_through` 截断点 + 删除逻辑 + 崩溃点测试（删一半、指针损坏） | ~300 行 | 中（改 `scan`/`visit` 边界语义） |
| S4 | （可选）transcript 压缩（tool result 摘要化）、`export`/`fork` 对快照边界的适配 | ~200 行 | 中 |

**建议先做 S1+S2**：立刻拿到「冷启动 O(尾段) + 内存只折叠尾段」，磁盘不变但可观测；
S3 单独一个提交，带 `AGENTSCOPE_SESSION_RETENTION` 开关，便于回退。

### 4.1 实施结果（S1–S4 已完成）

落地位置：

```text
crates/agentscope-core/src/session/snapshot.rs    SessionSnapshot / SnapshotPointer / SessionRetention
crates/agentscope-core/src/session/journal.rs     发布、校验、回收、reclaimed 标记、floor 感知的链游走
crates/agentscope-core/src/session/views.rs       replay_views 从快照起步；四个单视图读接口都走同一入口
crates/agentscope-core/src/session/recorder.rs    定量/定时发布 + snapshot_seq/snapshot_age_ms 指标 + 跨快照的去重守卫
crates/agentscope-core/src/session/export.rs      已回收前缀不再卡死导出
crates/agentscope-core/src/session/storage.rs     AtomicSessionStorage::remove（默认不支持 = 不回收）
```

与设计稿的差异（都是实现时发现必须这么做）：

1. **快照额外存 `pending`**：`admission_views`/`recovery_views` 都要 pending 交互表；只存 projection/turns/transcript 会让这两条热路径继续全量重放，快照等于白做。
2. **快照额外存 `runs`（最近的执行 id，上限 4096）**：recorder 的「Run ID already used」守卫原本看全量事件；从快照起步后就看不到旧事件了，必须把这份名单随快照带走，否则重放旧 run id 会被放过。
3. **存储需要 `remove` 能力**：`AtomicSessionStorage` 原本只有 read/CAS/list_paths。默认实现返回不支持 → 该后端 `reclaim` 直接跳过（快照仍生效，只不删历史）。已实现：内存、workspace 文件、SQLite；`BaseStore` 后端无删除 API，保留不回收。
4. **`reclaimed.json` 标记**：删除后写一个单调递增的 floor，链游走据此停住，不再要求链回溯到 seq 0。
5. **阈值**：`PRUNE_MIN_EVENTS = 1024`（floor 与 head 差不够就不扫）、`PRUNE_MAX_SCAN = 4096`（一轮最多看这么多 commit）、`PRUNE_BATCH = 512`（一轮最多删这么多）；`SNAPSHOT_EVERY_EVENTS = 512` / `SNAPSHOT_EVERY = 60s`。

语义约定（必须知道的行为）：

| 场景 | 行为 |
|---|---|
| 快照与链不匹配 / 快照损坏 | 忽略快照，退回全量重放（日志 warn） |
| 历史已回收且快照不可用 | **报错**`Session history before seq N was reclaimed and its snapshot is unusable`——宁可失败，不能返回缺头部的视图 |
| `scan(after, through)` 且 `after < floor` | 报错`Session events at or before seq N were reclaimed and cannot be read`（export/fork/迁移靠它说真话） |
| `visit(0, head)`（重放） | 返回仍可读的后缀（含跨越 floor 的锚点 batch） |
| exporter 的 cursor 落在 floor 之下 | 告警一次、cursor 跳到 floor 后继续导出（已删事件无法再导出） |
| `AGENTSCOPE_SESSION_RETENTION` | `off`/`0`/`false`/`no` 关闭删除；数字 = 最小保留天数；默认 30 天 |

**S4 只做了一半**：transcript 的 tool result 摘要化**未实现**——它会丢内容，属于产品决策，不是性能开关；已做的 S4 部分是 export/fork 对快照边界的适配（fork/export_state 走投影，天然兼容；exporter 跳 floor）。

**已验证**：core 360 passed（新增 10 个用例：快照等价性、跨链指针、损坏对象、回收只删旧段、无快照时响亮失败、recorder 发布与指标、跨快照 run 去重、exporter 跳 floor；harness 侧 2 个：快照不写 `.version`/删除、快照跨进程重开）；harness lib 487 passed。

## 5. 需要留意的兼容点

- `SessionLog::scan(after, through)`：`after < snapshot.seq` 时必须仍能返回数据（用快照展开），否则 export/迁移会静默少数据。
- `recovery`/`fork`/`migration` 目前假设「从 0 重放」，要改为「快照 + 尾段」。
- `blob` 回收要按「被删 commit 引用且无其它引用」判定；blob 名是内容哈希，天然可共享。
- 双进程：快照/回收只能在持租约期间做；`snapshot.json` 用 CAS，读到旧指针只是慢一点。
