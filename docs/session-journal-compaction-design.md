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

## 5. 需要留意的兼容点

- `SessionLog::scan(after, through)`：`after < snapshot.seq` 时必须仍能返回数据（用快照展开），否则 export/迁移会静默少数据。
- `recovery`/`fork`/`migration` 目前假设「从 0 重放」，要改为「快照 + 尾段」。
- `blob` 回收要按「被删 commit 引用且无其它引用」判定；blob 名是内容哈希，天然可共享。
- 双进程：快照/回收只能在持租约期间做；`snapshot.json` 用 CAS，读到旧指针只是慢一点。
