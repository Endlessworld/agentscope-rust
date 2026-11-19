# 内存占用优化 GAP_TRACKER

> 跟踪 AgentScope durable 运行时在 ACP harness 下的堆分配与峰值内存（生产 RSS 曾从 <25MB 涨到约 184MB）。
> 本文档是**单一事实来源**：测量口径、已落地项、剩余优化点、已否决项。
> 最后更新：2026-11-19（dev 工作树；L1–L22 已改未提交）。

## 0. 阅读约定

- **编号**：`L*` = 已落地改动（附验证基线）；`R*` = 剩余优化点（按性价比排序）；`X*` = 已否决 / 已证伪。
- **两个口径不可混用**：
  - `MB/轮` = 单轮累计堆分配（`alloc-stats` 计数分配器）。只有**同一轮数**下的数字才可比。
  - `peak_live` = 存活堆峰值。多数改动只降前者；峰值只在特定窗口下降。
- **两条基准命令**：
  - **40 轮**（`DIAG_TURNS=40`）：报 `peak_live` / 稳态 `live` / 阶段峰值。
  - **20 轮**（`DIAG_TURNS=20`）：报每轮累计分配与各 `diag` 窗口；**每轮分配的对比必须同为 20 轮**。
- 每条都要有 `file:line` 证据；未实测的收益一律标「推断」。

---

## 1. 基线与测量

### 1.1 基准负载

真实 `AcpAgentSession::run_prompt`，离线 `EchoModel`，每轮 user 8KB + assistant reply 8KB。

### 1.2 演进（40 轮口径）

| 指标 | 改造前 | 当前 | 备注 |
|---|---:|---:|---|
| `peak_live` | 19.68MB | **16.35MB** | −17%；仍受「同一份会话多种表示并存」限制（见 §1.3） |
| 稳态 `live` | 3.65MB | 4.26MB | 单份最新 state |
| 整份 scope 解析/轮 | ~148 | **16** | = commit 次数（L1） |
| 写字节 / 40 轮 | 42.87MB | 42.87MB | 未变，磁盘格式未改 |
| `commit` / `peak_commit` | 31.21 / 39.54MB | 23.8–28 / 30–37MB | CRT retained pages，波动大 |
| 累计分配/轮（**20 轮口径**） | ≈110MB | **≈84MB** | 基线修正（X14）：L22 时真实值 ≈152MB/轮（旧记 98MB 是漏计）；L24 后 108MB/轮；L25 后 1727.9MB/20 轮 ≈ 86MB/轮；**L26 后 1671.5MB/20 轮 ≈ 83.6MB/轮** |

健康信号：`drop(session)` 后 live → 0.07–0.09MB，无全局泄漏。

### 1.3 分阶段归因（20 轮口径，L26 后，累计分配）

| `diag` 窗口 | 累计 | 每轮 | 内容 |
|---|---:|---:|---|
| `djournal:facts` | 895.4MB | 44.8 | `backend.commit` → `FileStore::commit`；16 次写事务/轮，每次读+解析+重写整份 1.36MB scope |
| `djournal:read` | 336.6MB | 16.8 | **L26 后**：数值包含原 `djournal:encode` 窗口（两标签相邻，读回消失后被并入）。L25 时为 207.9MB：只读事务命中文档缓存，`fs::read` 从 ≈20 次/轮 → ≈7.6 次/轮（写后失效必重读）；L24 前 544.5MB / 27.2 |
| `djournal:encode` | **0** | **0** | **L26 消除**：该窗口原是提交前的幂等读回（`read_after` 分页比对），改为提交被拒后才读；entries 编码成本本来落在前一个窗口（R3 实测证实 `to_value`→`to_string` 对该窗口零影响） |
| `flush:done` | 202.9MB | 10.1 | **不是 flush 的成本**（`flush:committed → done` 恒 0.0KB）：落在 flush 之后的 tick 收尾 + turn ending 写事务，随轮线性增长 ⇒ 归 B3（§3.3 R9） |
| `checkpoint:payload` | 24.1MB | 1.2 | 被测 payload 本体 |
| `djournal:stored` | **0** | 0 | L22 前为 12.7MB/轮（提交后二次 `head()`） |
| `acp:settle` | **102.9MB** | **5.1** | L24 后：`catch_up` 增量折叠 + 每轮克隆 `state_json`（≈2MB/轮）；L25 后内部 `JournalProjection::read` 命中文档缓存，再降 40MB；此前片段 505.8MB |
| `acp:reconcile` | **0** | **0** | L24 后不再触发（正常轮无悬挂）；此前片段 527.9MB |

> **A/B（2026-11-19，同一 20 轮）**：把 `settle_finished_turn_uncertainties` 临时短路（`if false && …`）后，全部窗口之和 **3043.8MB → 2021.9MB（−1021.9MB = −51.1MB/轮）**，说明 settle 路径真实成本 ≈51MB/轮（约总量三分之一）。**L24 落地后实测 2160.9MB（−882.9MB = −44.1MB/轮，−29%）**：比短路版多 139MB，即增量检查成本（`acp:settle` 142.8MB/20 轮，其中 ≈40MB 是每轮 `catch_up` 克隆整份 `state_json`）。`peak_live` 8.96 → 9.90MB（缓存常驻 +0.94MB）。
>
> **L25（R2）后实测 1727.9MB（再 −433MB = −21.7MB/轮，−20%）**，全部来自只读事务不再读整份文档：`djournal:read` 544.5 → 207.9MB、`acp:settle` 142.8 → 102.9MB、`flush:done` 216.9 → 202.9MB。`peak_live` 9.90 → **11.21MB**（文档缓存常驻 1.36MB）。`txns` 计数 76–86 → **16–17/轮**：只读事务不再计入 `TRANSACTIONS`（它们走 `read_transaction`）。
>
> **L26（R4）后实测 1671.5MB（再 −56.4MB = −2.82MB/轮，−3.3%）**：正常路径不再做提交前的幂等读回，`djournal:encode` 窗口归零、`jreads` 15 → **3**/轮；`djournal:facts` 895.4MB、`sbytes` 21,287,416、`peak_live` 11.21MB 均不变（磁盘格式未动）。反向验证：短路 `replay_head` 后 `a_stale_journal_position_is_rejected`（`idempotent replay` 断言）与 `tool_facts_test` 的 replay 用例失败。

`peak_live` 的阶段峰值（40 轮口径）：`checkpoint:skipped` 16.36 / `checkpoint:done` 15.36 / `step:request_built` 13.67 / `checkpoint:capture` 10.58 / `commit:finish` 10.26MB。
⇒ **峰值出现在 checkpoint 时刻**：state 与其转义 payload / 待提交批次同时在内存里。

### 1.4 存储侧计数（turn 9，20 轮口径）

`parses=16 picks=8 jreads=3 writes=16 sbytes`（L26 后；L25 时 `jreads=15`）；**`txns` 16–17/轮**（L25 前 76–86：只读事务现在走 `read_transaction`，不计入 `TRANSACTIONS`）。scope 文档 **1.36MB**；`sbytes` 21.3MB/轮 ÷ 1.36MB ≈ **16 次全量重写/轮**；blobs 192 个 / 23.4MB（135 个 64–256KB，最大 270KB）。

结构性结论：**每个事务都读整份文档**（86 事务/轮），**每个写事务都重写整份**。这是 44–152MB/轮分配的主干。

### 1.5 测量工具

```powershell
$env:DIAG_TURNS='40'
cargo test -p agentscope-extensions-acp --features alloc-stats --test acp_live_memory_test -- --ignored --nocapture --test-threads=1
```

- 探针：`crates/agentscope-core/src/diag.rs`，**默认零成本**（未 `enable()` 时每次 mark 仅一次 relaxed 原子读）。
- 装配：`diag::register_live(mem::live_bytes)` + `diag::enable()`；打印 `diag::peak_by_label()`。
- 标签是**线程局部**：`--test-threads=1` + 单线程 runtime 下可信，多线程会串味（L15/L20 的教训）。
- 环境变量：`DIAG_TURNS`、`DIAG_USER_BYTES`、`DIAG_REPLY_BYTES`。
- 临时 `eprintln!("DIAG …")` 探针在用完后**必须还原**（收尾前 `rg 'DIAG "'` 应为空）。

---

## 2. 已落地（L1–L22）

| # | 改动 | 位置 | 效果 | 验证 |
|---|---|---|---|---|
| L1 | `StagedTxn` 惰性化：`get(k)` 按需 `pick_parts`、`keys()` 走 `key_map`、`commit` 用 `RawDocument`（`BTreeMap<String, Box<RawValue>>`）、`roll_out`/`collect_blobs` 走 RawValue | `durable/src/store/file.rs` | 整份解析 148→16/轮 | durable 105 |
| L2 | `Txn::get_all`（默认逐个 `get`，FileStore 整页一次取）；journal 读取改 `keys`+`get_all` | `store.rs`、`backend/query.rs`、`backend/commit.rs` | 消整页 O(n²)（compaction 61.9s→14s） | durable 105 |
| L3 | `turn::frame()` 只读 thread/turn/checkpoint 三键 | `backend/turn.rs` | 减少整份解析 | `frame_read_traffic_test` |
| L4 | `publish_context` 先比较后复制 | `core/agent/react_agent.rs` | `commit` −1.5MB | core 572 |
| L5 | `AgentState.context: Vec<Msg>` → `Arc<Vec<Msg>>`（`context_mut()` = `Arc::make_mut`） | `core/state/agent_state.rs` + 全部调用点 | `peak_live` 17.71→16.74MB | core/harness/durable/acp |
| L6 | 请求构建 `with_capacity` + `extend_from_slice` | `react_agent.rs` | 少一次临时分配（峰值无变化） | 同上 |
| L7 | `core::diag` + 计数分配器 `observe` + 阶段 marks | `core/src/diag.rs`、`acp/src/mem.rs` | 观测能力（生产零成本） | — |
| L8 | context 编译不触发省略时 move 请求消息；`at_path` 用 `mem::take` | `harness/agent/context/builder.rs`、`middleware/at_path.rs` | `step:published` 9.68→9.21MB | harness 734 |
| L9 | `tasks/permission/plan_mode/tool_context` 改 `Arc`（`Arc::make_mut`） | `core/state/agent_state.rs` + 4 处迁移 | 每 tick 不再深拷贝这四块 | core 572 / harness 734 |
| L10 | `Msg.content: Arc<Vec<ContentBlock>>`（`content_mut()`），全 workspace 61 处迁移 | `core/message/msg.rs:78` | `peak_live` 16.74→16.35MB；`request_built` 14.59→13.65MB | 全绿（含 fmt） |
| L11 | capture 的 5 个追加点改借用结构体直序列化（不再 `json!` 建树） | `core/journal/recorder.rs` | 40 轮负载未触发（`replaced=false`），保留给 compaction 重写整份 context 时 | core 572 |
| L12 | `HashingSink` 顺带累加转义长度；`checkpoint_payload` 一次预分配 | `recorder.rs` | `checkpoint:appended` 16.28→15.95MB | core 572 |
| L13 | **M4**：`write_json` 流式写盘（`to_writer` → `CountingSink<BufWriter<File>>` → rename） | `durable/src/store/file.rs` | 整份 scope 不再进 `Vec<u8>` | durable 67 + 170 |
| L14 | **M1**：`payload_json: String → Arc<str>` | `core/journal/event.rs`、`recorder.rs`、`atomic.rs` | 克隆事件不复制 payload 文本（L19 改类型） | core/harness/durable |
| L15 | 线程局部探针（窗口状态按线程）+ `checkpoint:skipped/done` 出口边界 | `core/src/diag.rs`、`recorder.rs` | 证明 capture 内部几乎不分配（`fingerprint` 64KB/40 轮累计） | 40 轮实测 |
| L16 | 发现：`StoreJournalStorage::compare_and_set` 把 journal 字节 base64 内联（1.33×）；`FileStore` 本有 `$blob` 外置 | `harness/agent/session/store.rs` | ACP 走文件后端、无 base64 ⇒ 不改，记录备查（X6） | — |
| L17 | **M2a 第一步**：`externalize` 用 `CountingSink` 判定 + `store_blob_streamed`（边写边哈希 → 临时文件 → 按 digest rename） | `durable/src/store/file.rs` | 去掉 turn 39 约 3.8MB/轮的临时缓冲；`sbytes` 字节不变 | durable 67 |
| L18 | `hash_events()`：`Sha256Sink` 直接吃 `to_writer` 字节；替换 commit / `read_commit` / 无 blob 复核三处 `hash(&to_vec(...))` | `core/journal/atomic.rs` | commit 不再为算校验和物化整批（probe：`finish` 6.5→2.2MB/轮） | 新增字节等价用例 |
| L19 | `payload_json: Arc<str>` → `Arc<String>`（`Arc::new(String)` 不拷贝） | `event.rs`、`recorder.rs`、`atomic.rs` + 22 处测试字面量 | checkpoint 窗口 −2.0MB/轮 | core 573 / harness 734 |
| L20 | `diag` 标签改 task-local + 6 个思考阶段标记 | `core/src/diag.rs`、`react_agent.rs`、`recorder.rs` | 消除跨任务误记（`checkpoint:skipped` 2.78GB→1.21GB） | 20 轮实测 |
| L21 | `flush` / `AtomicThreadJournal::commit` / 适配器 `commit` 加密标记 | `recorder.rs`、`atomic.rs`、`backend_thread_journal.rs` | 定位到 durable commit 路径占 ≈95% | 20 轮实测 |
| L22 | **方案 A**：`BackendThreadJournal::commit` 用本地推导的 `Head` 取代提交后二次 `head()` | `durable-adapter/backend_thread_journal.rs` | `djournal:stored` 12.7MB/轮 → **0** | 新用例 `the_head_a_commit_returns_is_the_one_a_reader_sees`；41 targets / 351 passed |
| L23 | 诊断标记：ACP `acp:prompt_enter/prompt_done/run_enter/before_settle/settle/settle_read/reconcile/after_settle`；harness `context:enter/collect/materials/evict/project/layout`；测试新增 `DIAG_EVENTS_ALL=1`（打印全部窗口） | `acp/src/session.rs`、`harness/agent/context/builder.rs`、`acp/tests/acp_live_memory_test.rs` | 把 settle（≈51MB/轮）与 context 编译各自归因；证明 `flush:committed` 恒 0.0KB | harness lib 734 / acp 22 targets 181 passed / fmt 干净 |
| L24 | **R1**：`settle_finished_turn_uncertainties_cached` — `SessionRuntime` 持跨轮 `JournalProjection` 缓存，每轮 `catch_up` 只 fold 新增事件；无悬挂直接返回，有悬挂才 `reconcile` 并清缓存 | `acp/src/session.rs`、`durable` 不改 | 20 轮总分配 **3043.8 → 2160.9MB（−44.1MB/轮，−29%）**；每轮全量折叠读消失；`peak_live` 8.96 → 9.90MB（缓存常驻，+0.94） | 新用例覆盖全量/命中/失效三条路径；acp 22 targets / 181 passed / 4 ignored / fmt 干净 |
| L25 | **R2**：`RecordStore::read_transaction` + `store::txn_read` + `FileStore` 文档缓存（`stamp(len,mtime,created)` → `Arc<Vec<u8>>`，双 stamp 夹读）；**写事务绝不读缓存**，只读事务的 `put`/`schedule` 直接报错；`backend/query.rs` 8 个只读入口改 `with_session_read`（`prune_events` 保持写） | `durable/src/store.rs`、`store/file.rs`、`backend/{mod,query}.rs` | 20 轮总分配 **2160.9 → 1727.9MB（−21.7MB/轮，−20%）**；`djournal:read` 544.5 → 207.9MB；`peak_live` 9.90 → 11.21MB（缓存常驻 1.36MB） | 新用例：只读事务只读一次盘 / 写后失效 / 另一 host 写后失效 / 只读拒写；durable 108 passed（含 `three_process_test`）/ adapter 全绿 / acp 181 / harness 734 / fmt 干净 |
| L26 | **R4**：幂等读回从提交前移到提交被拒后（新 `BackendThreadJournal::replay_head`，固有方法）；正常路径直接提交，只在 `state.journal_position` 不一致或 commit 报错时读回比对（同 id 同内容 → 返回 head；同 id 不同内容 → 报错） | `durable-adapter-agentscope/src/backend_thread_journal.rs` | 20 轮总分配 **1727.9 → 1671.5MB（−2.82MB/轮，−3.3%）**；`djournal:encode` 9.3MB/轮 → **0**；`jreads` 15 → **3**；`sbytes` 21,287,416 不变 | 反向验证：短路 `replay_head` 后 `a_stale_journal_position_is_rejected`（`idempotent replay` 断言）与 `tool_facts_test` replay 用例失败；adapter 66 / acp 181 / harness 734 / fmt 干净 |

> L22 的三条前提（改动前已核实，**别改坏**）：`backend/commit.rs` 的 commit 不续租、不改 `thread.epoch`、不清 `turn.owner`（普通 Running 批次）；全仓库只有 `recorder::flush` 消费 `commit()` 返回值且只取 `head.seq`。

---

## 3. 剩余优化点

### 3.1 可直接做（不碰磁盘 wire、不碰跨语言契约）

> R3–R6 已于 2026-11-19 全部处理：**R3 已实现并回退**（实测净收益为负 → X15）；**R4 落地为 L26**；**R5 归 R8**（CPU 侧，无独立内存收益）；**R6 关闭**（`scope_bytes` 只是 `fs::metadata`，零堆分配）。

### 3.2 需批准（动 wire / 跨语言 / 大工程）

#### R7 —— B2b：journal `payload` 去掉一层 `payloadJson` 字符串转义

- **收益**：+2–3MB/轮。
- **代价**：改 core `JournalEvent` 契约 + Java wire `PROTOCOL_VERSION`，约 30 处调用点。

#### R8 —— B3：单文档全量重写 → append-only log + 周期快照

- **设计**：`scopes/<digest>.snapshot.json` + `scopes/<digest>.log`；commit 只追加 `{generation, ops:[{part, bytes|null}], crc}` + fsync（O(staged)）；`keys` 用内存 `BTreeSet`；log > `max(4MB, snapshot/4)` 时重写快照 → rename → truncate；崩溃靠 `generation` 识别旧记录；`roll_out`/`collect_blobs` 挪到快照轮转。
- **收益**：**60–70MB/轮（−60%~70%）**，唯一数量级方案。
- **代价**：`base_raw`/`scope_parts`/`pick_from_cache`/`roll_out`/`collect_blobs`/`scope_bytes`/`read_parts` 全重写；崩溃一致性测试矩阵扩张（log 半写 / 快照替换中途 / 两代 log 共存）；`three_process_test` 跨进程 tail 语义。工程量为 L18+L19+L13 总和再加码。
- **前置**：独立工作树，先写崩溃恢复测试。

### 3.3 已量化并关闭（原 R9–R15，结论留档）

| 原编号 | 结论 | 实测 |
|---|---|---|
| R9 `flush:done` | **不是 flush 的成本**：`flush:committed → flush:done` 恒 **0.0KB**（exporter 为 `None`、`publish_snapshot_if_due` 是 no-op）。10.6MB/轮落在 flush **之后**：tick 收尾 + `driver.finish()` + durable turn ending 写盘；随轮增长（3 轮时每轮末尾 2.4–3.8MB，20 轮 10.6MB/轮），与 `djournal:facts` 同源（写事务重写整份 scope）⇒ **归入 B3（R8）**，无独立优化点 | 20 轮 216.9MB；3 轮逐窗口 260KB–3.8MB |
| R10 snapshot 发布 | **ACP 不发布 snapshot**：`BackendThreadJournal` 未覆写 `ThreadJournal::snapshot/publish_snapshot`，走默认 `Ok(None)`；`publish_snapshot_if_due` 即使 due 也不做事。副作用：`replay_views` 的 seed 永远为空 ⇒ **每次折叠都从 seq 0 开始**，这是 R1 的根因之一（L24 的缓存对此免疫：命中时只 fold 新增） | `journal/log.rs:152-182` 默认实现 |
| R11 `checkpoint_fields()` | **0.35KB/轮**（20 轮 7.0KB）⇒ 零成本 | 20 轮 `allocated_by_label` |
| R12 `Model::stream(Vec<Msg>)` | **147KB/轮**（`reasoning:provider` 2.9MB/20 轮），上限 ~0.25MB/轮 ⇒ 不做 | 同上 |
| R13 models.json catalog | **不存在 15MB catalog**：`~/.agentscope` 下只有 **2.9KB** 的 `models.json`；catalog 是代码内嵌的 `ModelDescriptor` 表，`catalog_providers()` 只做内存分组，无文件重解析 | 目录实测 + `setup.rs:77` |
| R14 eviction 整份 `to_vec` | **生产路径无 `to_vec`**：`tool_result_eviction.rs:447`、`context_middleware.rs:1027/1298/1403`、`conversation_compaction.rs:520+` 全在 `#[cfg(test)]` 之后（分别在 316 / 741 / 461 行起）；`builder.rs:328` 传的是 move 的 owned 列表。`context:collect/materials/evict` 合计 **0.01MB/轮** ⇒ 关闭 | 三个 `cfg(test)` 行号 + 20 轮标记 |
| R15 provider 流式合并 | **无逐 delta 序列化**：`responses/mod.rs:769/775` 的 `json!` 在 `into_final`（每响应一次），`relay.rs` 全在请求构建；逐 delta 唯一热点（core `MergedChunk::push`）已修 | 静态审计 |
| R18 task-local 标签 | **已由 L20 完成**（`diag::join_task` + `OWNER` + 任务切换关窗），无需再改 | `core/src/diag.rs` |

### 3.4 观测类（成本极低，仅为定位）

- R16：`step` 内细分 acting 阶段（工具执行前后各一 mark），确认 `checkpoint:capture` 里 acting 占比。
- R17：durable `commit` 内部细分（thread / turn / facts / checkpoint / turn-ending 各阶段），把 44.6MB/轮与 R9 的 10.6MB/轮 拆到单项。

---

## 4. 已否决 / 已证伪（不要重复踩）

| # | 结论 | 依据 |
|---|---|---|
| X1 | **ACP 每轮全量「重放」journal（O(n²) replay）不成立** | 20 轮实测：19/20 轮 `recorder=baseline`，仅首轮 `baseline=none:no-state` + `recorder=replay seq=0 elapsed_ms=0`（journal 为空）。免重放已生效；`jentries` 的真凶是 settle 的全量折叠（已由 L24 消除） |
| X2 | provider 侧不是成本点 | task-local 后 `reasoning:provider` ≈**0.15MB/轮**（此前 11MB/次是跨任务误记） |
| X3 | `checkpoint:fresh` 的 `message.clone()` 不分配 | 累计 **0.0KB**（L10 后 `Msg::clone` 已是 Arc 级） |
| X4 | capture 前段「整份 state 编码两趟 6–7MB」不成立 | `HashingSink` 只有 `len` + `DefaultHasher`，不缓存字节；`MessageFingerprint` 只有 `len`+`hash` |
| X5 | 「ACP 提交 33MB 在 core `atomic.rs`」不成立 | ACP 用 durable adapter 的 `BackendThreadJournal`；`commit:*` 标记在诊断中 0 次触发 |
| X6 | M2a-2（去 journal base64）不做 | base64 只在 `StoreJournalStorage`（mysql/redis/mongodb/oss/jdbc/pg/cos）；ACP 走 `WorkspaceJournalStorage`，直接写文件字节 |
| X7 | M2b（payload 暂存文件不驻留）不做 | `Commit.hash` 覆盖**水合后** events 的 JSON 文本，`serialize_str` 必须拿完整 `&str`；后端只收整块字节；暂存后 commit 侧反多一次「文件读 → String」拷贝 |
| X8 | B1（journal 不内联 `stateJson`）不做 | ≤1MB/轮，且与 `compact::RECLAIMED_PAYLOAD` 折叠、checkpoint history 回收冲突 |
| X9 | P0-1 `RuntimeContext.memory` 克隆 | 来自本轮输入（`AgentInput`），量小；整份克隆只在结构化输出路径 `react_agent.rs:1346` |
| X10 | `conversation_compaction` 生产路径整份 `to_vec` | 生产部分无该调用，461 行起全是 `#[cfg(test)]` |
| X11 | P1-1（`Msg.content` Arc）预估 −35% 未达 | 实际 −2.3%；保留理由是降低 O(上下文) 线性成本，且是 R12 的前置 |
| X12 | B3 的替代方案：per-part 单文件 + manifest / redb / sled | 多文件不原子；新依赖 + 跨 host 语义不明 |
| X13 | L6、L11 未带来可测收益 | 保留仅为消除一次临时分配 / 覆盖 compaction 路径；别把它们当收益来源引用 |
| X14 | 「20 轮累计分配 ≈98MB/轮」是**漏计** | settle 区间无标记时其 ≈51MB/轮 落入无标签被丢弃；加 `acp:settle` 并做 A/B 后真实值 ≈**152MB/轮**（3043.8MB/20 轮）。今后引用「每轮分配」必须说明是否包含 settle |
| X15 | **R3（B2a：`entries: Vec<Box<RawValue>>` + `put_raw` + digest 分离）已实现并回退** | 20 轮实测 **1727.9 → 1748.7MB（+1MB/轮，两次测量一致）**：`apply_journal` 省 `Value` 树（`djournal:facts` −0.46MB/轮）被适配器 `to_string` + `RawValue::from_string` 的整份文本复制（`flush:batch` 11.7→42.1MB）吃光；`digest` 快路径收益 ≈0。`sbytes` 21,287,416 逐字节不变。结论：journal 的内存成本在「一次编码文本」本身，不在中间 `Value` 树；要再降只能动 wire 文本（R7）或批量异步化 |

---

## 5. 落地检查清单（每项改动都要满足）

1. `cargo test -p agentscope-core --lib`、`-p agentscope-harness --lib`、`-p agentscope-durable -p agentscope-durable-adapter-agentscope -p agentscope-extensions-acp` 全绿。
2. `cargo check --workspace` EXIT=0；`cargo fmt --all -- --check` 干净。
3. 持久化改动必须**字节兼容**：journal/checkpoint 往返测试 + `sbytes` 与旧实现逐字节对比。
4. 20 轮与 40 轮各跑一次，记录 `peak_live`、窗口表、`parses/picks/jreads/txns/writes/sbytes` 计数器。
5. **反向验证**：把关键改动短路，对应用例必须失败（证明用例真的覆盖）。
6. 临时探针必须还原：`rg 'DIAG "' crates/` 为空，且 `git diff` 不含调试打印。
7. 临时 A/B 短路（如 `if false && …`）必须还原，并在 tracker 里保留 A/B 数字与前后对照。
8. 工作树里的历史改动（L1–L25）提交前单独 review：L22 依赖的三条前提（§2 备注）需在 diff 里仍然成立；L25 的前提是「写事务绝不读缓存、只读事务拒写」。
9. 序列化/磁盘改动必须提交**字节等价对拍用例**（结构体 `to_string` vs 旧 `json!` 的 `Value::to_string()`）；`serde_json` 的 `preserve_order` 已显式固定在 workspace 依赖里 —— 缺了它，`cargo test -p <crate>` 单独构建时 `Value::Object` 退回 `BTreeMap`（字母序），journal payload 字节会随构建配置漂移，`sbytes` 基线随构建失效（2026-11-19 已实测）。

---

## 6. 变更记录（倒序：最新在上）

| 日期 | 内容 |
|---|---|
| 2026-11-19 | **序列化字节一致性加固**：新增 `recorder.rs::the_structured_payloads_serialize_exactly_as_the_json_macro_did`，对 `context/replaced`、`message/<role>`、`tool/requested`、`input/applied`、`state/changed` 五个结构化 payload 与旧 `json!` 的 `Value::to_string()` 逐字节对拍（反向验证：调换 `ToolRequested` 字段声明顺序 → 用例立即失败）。同时发现并修复**构建态字节漂移**：`cargo test -p agentscope-core` 的依赖图未启用 `preserve_order`，`Value::Object` 退回 `BTreeMap`（字母序），而全量构建经 `rag`/`bson` 联启用了它（插入序）——同一份代码会因构建配置写出不同 journal 字节；已在 workspace `serde_json` 显式加 `preserve_order`（与已测 ACP 基线一致，`sbytes` 21,287,416 不变）。验证：core 574 passed / 3 ignored、harness 734、durable+adapter 20 targets 174、acp 21 targets 181 + 4 ignored、`cargo fmt` 干净 |
| 2026-11-19 | **R4 落地（L26）**：幂等读回从提交前移到提交被拒后（`BackendThreadJournal::replay_head`，固有方法）；正常路径直接提交，只在 `state.journal_position` 不一致或 commit 报错时读回比对（同 id 同内容 → 返回 head，同 id 不同内容 → 报错）。20 轮实测 **1727.9 → 1671.5MB（−2.82MB/轮，−3.3%）**，`djournal:encode` 9.3MB/轮 → **0**、`jreads` 15→**3**；`djournal:facts`、`sbytes` 21,287,416、`peak_live` 11.21MB 均不变。反向验证：短路 `replay_head` → `a_stale_journal_position_is_rejected`（`idempotent replay` 断言）与 `tool_facts_test` replay 用例失败。验证：adapter 66 / acp 181 / harness 734 / `cargo check --workspace` EXIT=0 / fmt 干净 |
| 2026-11-19 | **R3 实测后回退（X15）**：`JournalAppend.entries: Vec<Box<RawValue>>` + `Txn::put_raw` + `Staged` + `digest_head`/`hash_texts` 全链路实现并通过测试（adapter 66 / durable 108、`sbytes` 不变），但 20 轮实测 **1727.9 → 1748.7MB（+1MB/轮）**：省掉的 `Value` 树被 `to_string`+`from_string` 的整份文本复制抵消。全部反向编辑已回退，回退后 `totalMB=1727.9` 与 R2 逐项一致。R5/R6 同时量化关闭：R5 归 R8、R6 只是 `fs::metadata` |
| 2026-11-19 | **R2 落地（L25）**：`RecordStore::read_transaction`（默认 = `transaction`）+ `store::txn_read` + `run_once` 抽出；`FileStore` 加 `documents` 缓存（`stamp(len,mtime,created)` → `Arc<Vec<u8>>`，读前读后双 stamp 夹读才缓存，`DOCUMENT_SCOPES=4`），只读事务的 `put`/`schedule` 报错，**写事务 `read_only=false` 永不查缓存**；`StagedTxn.bytes` 改 `Arc<Vec<u8>>` 共享；`query.rs` 8 个只读入口切 `with_session_read`（`prune_events` 保留写）。20 轮实测 **2160.9 → 1727.9MB（−21.7MB/轮，−20%）**，`djournal:read` 544.5→207.9MB，`txns` 76→16–17/轮；`peak_live` 9.90→11.21MB（缓存常驻）。反向验证：禁用 `cached_document` → “read the file once” 断言失败。验证：durable 108 / adapter 全绿（含 `three_process_test`）/ acp 181 / harness 734 / fmt 干净 |
| 2026-11-19 | **R1 落地（L24）**：`settle_finished_turn_uncertainties_cached` 用 `SessionRuntime.projection`（`Mutex<Option<JournalProjection>>`）+ `catch_up` 只 fold 新增事件；无悬挂直接返回，有悬挂才 reconcile 并清缓存；`settle_uncertain_with_cache` 拆出便于单测（无悬挂保留缓存 / 追加 dispatch 仍被 settle / reconcile 后缓存失效）。20 轮实测 **3043.8 → 2160.9MB（−44.1MB/轮，−29%）**，`acp:settle` 505.8 → 142.8MB、`acp:reconcile` 527.9 → 0；`peak_live` 8.96 → 9.90MB（缓存常驻）。验证：acp 22 targets / 181 passed / 4 ignored / fmt 干净 |
| 2026-11-19 | **R9–R15 探测（L23 标记 + A/B）**：`acp:settle` + `acp:reconcile` 合计 **≈51.1MB/轮**（短路 A/B：3043.8 → 2021.9MB/20 轮）；`flush:done` 10.6MB/轮 落在 flush **之后**（tick 收尾 + turn ending 写事务，随轮线性增长 ⇒ B3）；ACP **无 snapshot 实现**（trait 默认 `Ok(None)`，也是每次从 seq 0 折叠的根因）；`checkpoint:fields` 0.35KB/轮、`reasoning:provider` 147KB/轮、`context:*` 0.01MB/轮、models.json 仅 2.9KB、provider 无逐 delta 序列化 ⇒ R10–R15 全部关闭。验证：harness lib 734 / acp 181 passed / fmt 干净 |
| 2026-11-19 | **免重放调查（只读，已还原探针）**：实测 19/20 轮 `recorder=baseline`、仅首轮空 journal replay ⇒ 证伪 X1；定位 `jentries=757/轮` 来自 R1 的 `TurnViews::read` 全量折叠 + 约 12–15 次 `limit=1/2/4` 的小读（各付一次 1.36MB `fs::read`）。本文档重写：新增 R1–R18、X1–X13，补齐 L15–L22 |
| 2026-11-19 | **B 方案评估（只读）**：纠正「turn 文档携带整份 checkpoint」（`TurnData` 无该字段，checkpoint 在 `thread-data:<hash>:checkpoint`）与「44.6MB/轮由 checkpoint 引起」。磁盘实测：scope 1.36MB、blobs 192/23.4MB、16 次全量重写/轮。重划分为 B1（不做）/ B2a（R3）/ B2b（R7）/ B3（R8） |
| 2026-11-19 | 落地 L22（方案 A）：本地推导 `Head` 取代提交后二次 `head()`，`djournal:stored` 12.7MB/轮 → 0（20 轮累计 2007.6MB ≈98MB/轮）；新增 `the_head_a_commit_returns_is_the_one_a_reader_sees`；41 targets / 351 passed / 4 ignored，fmt 干净 |
| 2026-11-19 | 落地 L21：`flush`/`atomic::commit`/适配器 `commit` 加密标记 ⇒ 找到成本中心（durable commit 路径 ≈95%，`djournal:facts` 44.6MB/轮）；同时确认 core `atomic::commit` 在 ACP 不触发（X5） |
| 2026-11-19 | 落地 L20：label 改 task-local、补 6 个思考阶段标记；`reasoning:provider` 从 123MB 降到 2.9MB 累计（X2） |
| 2026-11-19 | L18/L19 A/B 实测：probe `checkpoint` 10.2→8.2MB/轮、`finish` 6.5→2.2MB/轮；`peak_live` 与 live 零变化；ACP `sbytes` 42,867,384 逐字节相同 |
| 2026-11-19 | 落地 L19（`Arc<String>`）、L18（`hash_events`）、M2b 复核结论（X7）；`docs` 记录「M2b 不做暂存」 |
| 2026-11-19 | 落地 L17（M2a 第一步：`externalize` 流式 + `store_blob_streamed`）；`sbytes` 字节不变，`peak_live` 仍 16.36 |
| 2026-11-19 | 落地 L15（线程局部探针）：capture 内部几乎不分配；`checkpoint:payload` 2.88MB/轮；`appended`=0 |
| 2026-11-19 | 落地 L13（M4 流式写盘）+ L14（M1 `Arc<str>`）：`appended` 15.95→15.34MB、`commit` 25.0→24.41MB，峰值不变 |
| 2026-11-19 | 落地 L12：哈希遍历顺带算转义长度、`checkpoint_payload` 精确预分配；**再次纠正归因**（X4） |
| 2026-11-19 | 落地 L10（P1-1 `Msg.content` Arc）+ L9（四 context Arc）+ L8（context move）：`peak_live` 16.74→**16.35MB**、`request_built` 14.59→13.65MB |
| 2026-11-19 | 建立本文档：L1–L7、40 轮基准（19.68 → 16.74MB）与阶段归因；核实 P0-1 依据有误（X9） |
