# Durable 迁移与后续加固 · 全面审查报告

> 审查范围：`546a0460950645dbc92b80e0b2e86e6b3d1fd246` 及其之后到 `0917b81` 的全部提交（共 **41 个**，`dev` 线性历史，无 merge）。
> 审查日期：2026-11-15。审查对象：当前工作树（HEAD `0917b81`，工作区干净）。
> 依据文档：`docs/DURABLE_MIGRATION_PLAN.md`（单一事实来源）、`docs/DURABLE_API_DESIGN.md`、`docs/cloud/durable-handoff-2026-11-14.md`、`docs/cloud/CF_DEPLOYMENT_ARCHITECTURE.md`、`docs/MIGRATION_GAP_TRACKER.md`。
> 本报告是**审核结论**，不重复计划文档的里程碑叙述；凡与计划文档不一致处都单独标出。

---

## 0. 摘要

**规模**：41 个提交、133 个文件、`+21,514 / −5,142` 行（2026-11-14 → 2026-11-15）。新增两个 crate：`agentscope-durable`（2,957 行 src + 954 行测试）与 `agentscope-durable-adapter-agentscope`（1,398 行 src + 3,439 行测试），两 crate 合计 **44 文件 / +15,264 行**；同时删除 `AgentSession`/`SessionAnswer`（−3,675 行）与 `controlplane` 扩展（M0，`ea4b1cc`，在本范围之前）。

**结论**：D1–D7 七项决策全部落地，M0–M5 全部完成，`0.2.0` 已发布；三进程 `suspend/resume/duplicate` 验收门（D7）以「副作用 1 次、`tool/result` 1 条、模型调用 2 次」的诚实口径通过。核心不变量（事务原子性、receipt 先于围栏、revision CAS、epoch 单调、终态不可逆、Unknown 永不算成功、`appliedAt` 不可变、就绪五分支、工具三态、事件幂等与游标绑定）**逐条核对成立**，且绝大多数带反向验证。

**但发布后的 19 个修复提交说明：第一版 durable 路径在真实负载下并不稳。** 修复链覆盖 5 类真实缺陷（永不终态的失败、未决工具结果让会话永久不可开、Windows `os error 5` 让 journal 从未落盘、陈旧锁误判、把 turn 位置当分页 limit）。这些都属于「契约写对了、边界没覆盖」，不是设计错误。

**问题统计**（详见 §6）：**必须修复 7 项**、建议改进 20 项、可接受/已知取舍 9 项。最严重的两条：

1. **file store 可能留下「本进程 pid 的孤儿锁」**：`Drop` 删除失败被 `let _ =` 吞掉、`acquire_lock` 写锁体失败被吞掉、`stale_body` 的读取不重试。一旦发生，锁体里的 pid 永远「活着」，该 scope 此后每次事务都要等满 `LOCK_TIMEOUT=60s` 再报 `scope is locked`，长驻宿主进程下等同于永久失效。**本会话实测复现**：`cargo test -p agentscope-durable -p agentscope-durable-adapter-agentscope -p agentscope-extensions-acp -j 2` 并发跑时，`store::file::lock_liveness_tests::only_one_contender_reclaims_a_stale_lock` 30 次取锁有 **28 次**等满 60s 报错（`left: 28 / right: 0`），用例耗时 300.04s 后 FAILED；同用例单独跑 0.19s 通过。
2. **读放大没有随写放大一起治理**：`backend/query.rs:53` 的 `keys(&prefix, None, 100_000)` 丢弃了 `after` 游标，而 file store 的 `keys()` 对每个 key 做完整 JSON 解析 → 读完整条 journal 是 **O(n²)**；且一次 tick 会对整条 journal 做三次全量重放（`harness_adapter.rs:115`、`execution_host.rs:102`、`recorder` 打开），模型调用期间还每 50ms 解析整个 scope 文档（`harness_adapter.rs:52,209`）。这与「低消耗高性能」最高规则直接冲突。

---

## 1. 审查方法

| 项 | 做法 |
|---|---|
| 提交级审查 | 41 个提交逐个 `git show`，按里程碑分组；核对其与 `DURABLE_MIGRATION_PLAN.md` §2 的声明是否一致 |
| 代码审查 | 4 个并行只读审核面：①durable core（types/store/backend/runtime/session）②adapter + core journal 桥接 ③ACP 换驱动 + `AgentSession` 删除 + acting 续跑 ④19 个修复提交 |
| 实测 | `cargo test -p agentscope-durable -p agentscope-durable-adapter-agentscope -p agentscope-extensions-acp -j 2 --no-fail-fast`（31 targets / 229 passed，1 target FAILED）；`cargo test -p agentscope-durable --lib` 复跑（28 passed，1.75s）；`cargo check -p agentscope-durable -p agentscope-durable-adapter-agentscope --all-targets` |
| 未做 | 全量 `cargo test --workspace`（仓库既有口径：207 targets / 6410 passed / 0 failed / 8 ignored）；Linux/macOS 验证；真机断电/掉电验证；`cargo fmt --check` 本轮未重跑 |

审核固定维度：依赖最小必要与版本一致、协议/状态机正确性、单测充分且离线、`unwrap`/panic 风险、schema 与 id 校验、是否引入被限制 crate、死代码/未接线模块、trait 方法是否漏实现。

---

## 2. 提交全景

### 2.1 分组

| 阶段 | 提交 | 规模 | 内容 |
|---|---|---|---|
| **M1 语义 + 重命名** | `546a046` | 70 文件 +1988/−1276 | `core::session` → `core::journal` 全量重命名；新增 `agent::{ThreadKey, RunOutcome, AgentExecutionHost, AgentExecutionPosition, ExecutionBoundary}`、`Error::HostPaused`、`GenerateReason::HostPaused`；事件/头格式**硬切 v2**（v1 不可读） |
| **M2 durable 骨架** | `aaaf53c` | 29 文件 +6905 | `agentscope-durable`：`types/`（Java `DurableTypes` 全量）、`store`（memory/file）、`backend`（`TxnBackend`）、`runtime`、`executor`、`harness_adapter`、`session_backend` |
| **M3 桥接与接线** | `7a4bb6d` `df0dfa5` `27f7236` `3939f1d` `e49fede` `269f453` | 37 文件 +3557/−135 | `BackendThreadJournal`（与 turn revision 同事务）、`AgentScopeExecutionHost`（位置/终局恢复/HostPaused）、工具三态、`AgentState`↔checkpoint、三进程验收、M4 语义映射文档 |
| **M4 门面与适配器** | `bda0951` `9e83e1a` `ca0d18c` `f04d7c8` `71e2b8b` | 36 文件 +2499/−168 | signal 三类（steer/inject/respond）、suspend 与 cancel 分离、`DurableSession` 门面（wakeup/task/history/事件通道）、可执行 `AgentScopeHarnessAdapter` + 租约借用；ACP 接线**首次回退**并记录三条阻塞 |
| **M4-e 换驱动 + 删 AgentSession** | `7e4a498` `ad50390` `35dab62` `bd7b0e0` | 27 文件 +1092/−3964 | tick 内把 durable cancel/pause 桥接成 `CancelSignal`；ACP 改由 `SessionRuntime` + `drive_turn` 驱动；删除 `AgentSession`/`SessionAnswer`/`InputReceipt`/`HarnessAgent::session()`；版本 0.2.0 |
| **发布后功能补齐** | `08a35c8` `85068cf` `2cd4ebd` `844214a` `31c51b3` | 11 文件 +2196/−36 | 执行守卫用例独立二进制；**acting 阶段续跑入口**（消费 `acting_iteration`）；workspace 依赖清理；Cloudflare 文档归档；`resumeStep` step 边界 |
| **修复与加固** | 19 个提交（§5） | 44 文件 +2356/−190 | 失败终态化、未决工具结果三处对账、Windows `os error 5`、锁语义、写放大治理、journal 分页 |

逐提交明细见 §10 附录 A。

---

## 3. 架构审查

### 3.1 依赖方向（与计划 §1.1 的对照）

```
计划：agentscope-durable ──→ core
实际：agentscope-durable ──→ （无内部依赖，仅 serde/tokio/sha2/libc/windows-sys）
      agentscope-durable-adapter-agentscope ──→ durable + harness + core
      agentscope-harness ──→ core（不依赖 durable）✔
      agentscope-extensions-acp ──→ durable + adapter + harness + core（新增两层）
```

- 「durable 不依赖 harness」的硬约束成立，且比计划更严（连 core 都不依赖，是纯契约 crate）。这是本次迁移最值得保留的结构决策。
- 代价：ACP 现在同时依赖 harness 与 durable/adapter，`agentscope-extensions-acp` 成为唯一知道「durable + harness」两个世界的 crate。建议在该 crate 的 `lib.rs` 文档里写清理由（当前只有 `Cargo.toml` 注释）。
- 未发现被限制 crate 引入；`tokio` 在 `agentscope-durable` 里只在 `#[tokio::test]` 使用，按最小必要应移到 `[dev-dependencies]`（P2）。

### 3.2 事务与不变量核对

| 不变量 | 位置 | 结论 |
|---|---|---|
| 一次 commit = 一个 session 事务（事实+状态+checkpoint+工具记录） | `backend/commit.rs`、`store.rs::txn` | ✅ 成立；`failed_transaction_leaves_no_trace` 覆盖 |
| receipt 检查先于 `owned()`（失去租约的重放仍能取回收据） | `backend/commit.rs:36-43` | ✅ 成立 |
| `expectedRevision` CAS | `backend/commit.rs`、`backend_thread_journal.rs:276` | ✅ 成立（`a_stale_revision_or_a_stale_epoch_cannot_commit`） |
| epoch 是 **thread** 级单调围栏，`claim` 才 bump | `backend/identity.rs` | ✅ 成立（同 owner 未过期返回原租约不 bump） |
| 终态不可逆（Commit 禁止 `Queued/Pausing/Cancelling`） | `types/commit.rs:230` | ✅ 成立 |
| `Succeeded` 要求所有工具 `applied` 且结果 terminal | `backend/commit.rs:437` | ✅ 成立（`an_unsettled_tool_blocks_a_terminal_commit_until_it_is_applied`） |
| `Unknown` 永不算 `Succeeded` | `backend/commit.rs:463-475` | ✅ 成立 |
| `appliedAt` 不可变 | `backend/commit.rs:352-358` | ✅ 成立 |
| 事件 `++sequence` + `eventId` 幂等 + 游标绑定 session | `backend/*`、`types/commit.rs` | ✅ 成立；⚠️ 事件 id 用毫秒时间戳（`tools.rs:258`），同毫秒两次 progress 会 CONFLICT |
| checkpoint 相同内容不产生新位置、已有 checkpoint 时 `null` 非法 | `backend/commit.rs` | ✅ 成立 |
| 就绪五分支（无待办 −1 / PAUSED −1 / 活跃租约 expiresAt / cancel·PAUSING nextAttemptAt / wait 未满足 deadline，`deadline==0` 即 −1） | `backend/mod.rs:218-241` | ✅ 语义成立；⚠️ 缺终态守卫，已 settle 的 turn 仍留在索引里，每次 settle 多一次 claim + 整文档写 |
| 工具三态（`attempted` 单向 + `result.state` + `applied/appliedAt`） | `backend/tools.rs`、`adapter/tool_facts.rs` | ✅ 成立；⚠️ `NOT_FOUND` 不可记录成立，但 server tool 的 `tool/result` 可能回滚整批（§6.2-13） |
| 不可重试失败直接终结 turn | `backend/turn.rs:598`（`76eda5b`） | ✅ 成立 |

### 3.3 存储与锁（多宿主）

当前语义（`3972c2e` 之后的最终形态，符合「多宿主并存」最高规则）：

- 每 scope 一个 JSON 文档 + 临时文件 `write` + `rename`；work 索引独立锁 `locks/work-index.lock`，锁序固定 **scope → index**（无环）。
- 锁陈旧判定：**持锁进程存活是唯一权威**。存活（含同进程其它线程）绝不按 mtime 回收；死 pid 立即接管；仅 pid 不可读才回退 `LOCK_STALE_UNKNOWN=30min`。
- 抢占用 `rename` 一次性 + 回读校验（避免两个竞争者各删一次锁导致双方同时持锁）；`ScopeLock` 只删自己那份。
- `retry_while_held` 只重试 raw os error **5/32**（Windows 瞬时占用），25ms 指数到 400ms、10 次（≈2.8s）；其它错误首试上抛。
- 正确性兜底仍是 durable 租约/epoch：同一 thread 只有一个执行写者。

**审查结论**：方向正确，但残留两个窗口（§6.1 第 1 条）：①`reclaim_stale` 的「放回」分支在 `create_new` 失败时直接丢弃已移出的活锁，可能让某个 holder 在没有锁文件的情况下写 scope；②`Drop`/`acquire_lock` 的写入与删除失败被吞掉，可留下活 pid 的孤儿锁（本会话实测复现）。

### 3.4 写放大与读放大

`ad7c721` 的手段（对照 `9663ea3` 基准清单）：

| 手段 | 状态 | 实测/审查 |
|---|---|---|
| 写一次（一个 turn 一次 commit） | 部分 | 续租 headroom `max(ttl/2,5s)` 让 flush 前的续租不落盘：62 次整文档重写 → 34 次 |
| 写小 | 部分 | `RecordingPolicy` 不记请求体、工具结果外置（沿用 core 既有策略） |
| 回收 | ✅ | `backend/compact.rs`：阈值 1 MiB，在提交事务内顺带压缩（旧 checkpoint payload→标记、旧 history、保留最近 1024 条事件），实测 1,116,648 → 386,989 B |
| 保留期剪枝 | 部分 | `prune_events` 已入 trait + `query::prune_events`，`events(None)` 从 `retained_after` 起读；**生产无调用者**，只在 >1 MiB 压缩时生效 |
| 不在内存里再复制 | ❌ | 峰值 ≈3–5× scope 字节；`execution_host`/`harness_adapter`/`recorder` 每 tick 三次全量重放 |

一个含 2 次工具调用的 turn 实测：**188 事务 / 34 次整文档重写 / 1,951,768 B**（治理前 216 / 62 / 3,490,683 B）。回归守卫 `write_traffic_test`（`scope_writes <= 40`、`scope_bytes < 2.4 MB`）。

**审查结论**：写侧已从平方级压到线性，读侧仍是平方级（§6.1 第 4 条）。且 `write_traffic_test` 只测单 turn、单进程，`bytes_written >= scope_bytes` 恒真、`40 * scope_bytes` 过松，跨 turn 二次增长与多宿主并发写入量都无守卫。

### 3.5 ACP 驱动循环

- `SessionRuntime { session, control, journal }`：同一 thread 三视图，`control` 必须用**另一个 owner**（否则会取走/放掉正在跑的 tick 的租约）——理由成立，已记录在计划 §2.10。
- `drive_turn`（`session.rs:696-719`）：每 20ms 一次 `session.tick()`，终态或 `Paused/Pausing/Waiting` 交还；`TurnDriver::finish` 5s 宽限，超时只告警不 abort（abort 会让 turn 停在 `Running` 直到租约过期）。
- 取消（`session.rs:3418-3461`）：`control.cancel(...)` 写 durable 意图 → 等 turn 终态（活会话 10s / 关闭中 500ms）→ `driver.finish()` → `StopReason::Cancelled`。
- 事件与转录一律读 **durable journal**（harness journal 已不是写者）。
- 关键语义修正：被 `interrupt()` 的 invocation 结束时 core 返回带 `GenerateReason::Interrupted` 的**正常 reply**，不是 `Err(Interrupted)`；adapter 必须据此让出，否则中断被落成 `failed`（首版 ACP 换驱动失败的真实根因之一）。

**审查结论**：主路径正确且有用例守卫（`acp_interrupted_turn_test`、`acp_session_cancel_test`、`acp_load_replay_test`）。残留：取消一个 `Waiting` turn 时 `drive_turn` 已退出、无人再 tick，turn 停在 `Cancelling`（§6.1 第 5 条）；20ms 空转在多宿主下会产生 MB/s 级整 scope 重写。

### 3.6 core 侧：acting 续跑与 step 边界

- `85068cf`：`run_agent_loop` 追加输入后判 `acting_resume`（`position.acting_iteration > 0` 且存在未应答 `tool_use`），命中则 `entry_iter = acting_iteration`、`continue_acting = true`，`execute_iteration` 首轮跳过 reasoning 直接 acting（对齐 Java `resumeAgent`）。4 条用例覆盖（0 次模型调用、无 position 不误入、无待办仍推理、预算计入）。
- `31c51b3`：`InvocationRecorder::resume_step(iteration)` 关掉已开 step 后追加 `step/resumed`（`recorder.rs:1474`），复用同一 `step_id` 槽位使后续 `step(iter)` 把它 `continued` 掉；重复调用为 no-op（比 Java 严格）。

**审查结论**：语义等价于 Java，但**durable 侧没有消费 `step/resumed`**——`execution_host.rs:109` 只折叠 `step/start`。续跑途中崩溃后，`acting_iteration` 会回退一格（重启多花一次模型调用、iteration 编号重复）。

---

## 4. 里程碑验收核对（对照计划声明）

| 声明 | 审查结论 | 证据 |
|---|---|---|
| M2 有意缺口：`history()` 返回空页 | ❌ **已过期** | `session_test::history_projects_the_committed_outputs_in_queue_order` 证明已实现（仍只投影 turn 输出，无消息转录） |
| M2 缺口：`resources/delegate/restore/prune_events` 未入 trait | ⚠️ **部分过期** | `prune_events` 已入 trait 与 `query::prune_events`；`resources`/`delegate`/`restore` 仍未接线 |
| M2 缺口：`Steer/Inject` 返回 `unavailable` | ❌ **已过期** | `session_test::{a_steer_reaches_the_turn_the_writer_holds, an_inject_lands_on_the_active_turn_under_its_own_key}` |
| M2 缺口：模型事实只有类型未接线 | ✅ 仍成立 | `backend/model.rs` 的 `ModelCallView` 等无消费者 |
| D7「三进程、副作用 1 次、`tool/result` 1 条」 | ✅ 达成 | `three_process_test::{suspend_resume_and_duplicate_share_only_the_store, written_facts_outlive_the_writer_process}`；模型调用 = 2 次，与 Java `ProcessRecoveryTest` 同值 |
| D7「模型调用 1 次」 | ⚠️ 不可达（已改用例） | 原 `#[ignore]` 用例已改为断言 2 次的 `a_resumed_process_repeats_the_tool_decision_but_not_its_effect`；根本原因是 durable adapter 第 1 轮提交 `acting_iteration = 0`，acting 续跑分支不触发 |
| M4-e-3「ACP 不再驱动 `AgentSession`」 | ✅ 达成 | `session.rs` 的 `SessionRuntime`/`drive_turn`；`AgentSession` 零引用 |
| M4-e-4「删 `AgentSession`/`SessionAnswer`」 | ✅ 达成 | `35dab62` 13 文件 −3,675 行；4 处 `abandon_turn` 语义已在计划 §2.11 逐条归属 |
| D4「事件格式 v2，v1 不可读」 | ✅ 达成 | `JournalEvent::SCHEMA_VERSION = 2`、`SessionHeader::FORMAT_VERSION = 2` |
| D6「删 controlplane」 | ✅ 达成（范围外 `ea4b1cc`） | 40 文件 / 714 KB；`agentscope-rs` feature 已移除 |

---

## 5. 缺陷修复链（19 个发布后提交）

| 提交 | 根因 | 修法 | 守卫 | 残余风险 |
|---|---|---|---|---|
| `76eda5b` | `record_failure` 把不可重试失败写成 `waiting`，`ready_at` 对它返回 `next_attempt_at` → 每 ~30s 重拉、必然再失败、**永不终态**；ACP `waiting` 无超时分支 | 不可重试直接置 `Failed`（`turn.rs:598`）；重放旧记录时把无 `waiting` key 的 `waiting` 收敛到 `Failed`；ACP 补 `WAITING_STALL_TIMEOUT=300s` | `an_unrecoverable_failure_ends_the_turn_instead_of_waiting_forever` | 300s stall 对合法长等待会误报（turn 仍可恢复） |
| `75b0d8b` | 最终回复 push 在最后一次 `publish_context` 之后 → 持久化 `context` 尾部停在输入 → 下轮把同一条后台通知再答一遍（自激循环） | 回复入栈后补一次 `publish_context`；`InboxMiddleware` 按 hint id 与 `state.context` 去重 | **无测试** | producer 复用 hint id 会静默丢通知；无 id 的 hint 绕过去重 |
| `28366c3` | `journal::recovery::reconcile` 零调用者 → 有未决 dispatch 的会话此后每次打开都被 `Explicit reconciliation required` 拒绝 | 打开会话前读投影，按 id 造 `Interrupted` 结果调 reconcile，失败只告警 | `an_uncertain_tool_call_is_settled_before_the_session_opens` | 不区分 turn 是否已终结 |
| `2cfc773` | offload 只在内存句柄留占位，journal 永远只有 dispatch | offload 时写 `ToolResultState::Suspended` 的 `tool/result`，投影跳过写回 state | `tests/async_tool_journal_test.rs` | Suspended → durable `Pending` 永不 `concluded`，`Succeeded` 被拒（§6.1 第 7 条） |
| `b30672e` | 等锁 5s < 陈旧判定 30s | `LOCK_TIMEOUT = 60s` | 无 | 60s 阻塞式 `thread::sleep` 落在 async 路径，占满一个 worker |
| `5b6784e` | 锁体早已写 pid，但陈旧判定只看 mtime → 死锁要空等 30s | `holder_alive`（unix `kill(pid,0)` / win `OpenProcess`+`GetExitCodeProcess`）为唯一权威 | 3 个 `lock_liveness_tests` | pid 复用 → 最多 60s 后 `unavailable`（方向正确，不误抢） |
| `bccb4a3` | `File::open` 只读句柄在 Windows `FlushFileBuffers` 必 EACCES → **journal 从未落盘**（187 万行刷屏）；durable 侧无瞬时占用重试 | journal 侧改 `OpenOptions::new().write(true)`；durable 侧 `retry_while_held`（仅 5/32） | `local_sync_flushes_a_freshly_written_file`、`a_held_target_is_retried_but_a_hard_error_is_not` | 修法正确，无新增 fsync 放大 |
| `6d9c6d2` | ①ACP 只在 build 时对账；②`SubagentSessionRepair` 在 turn 已结束返回 `Ok(false)` → 子会话 key 永久坏 | 抽 `settle_uncertain_tool_outcomes`，prompt 入口按「无活 turn + latest 已结束」结算；repair 走同一函数 | 3 条（acp session / materialize / agent_spawn） | — |
| `7a1fd7f` | 生产码本就对可重试错误重试 | 纯测试提交：可重试续租不丢租约、FENCED 停止心跳 | 2 条 | — |
| `96b433d` | 重试时 recorder 打开即被未决 dispatch 拒 | `execute` 内先 settle 再 open；`concluded() = terminal ∪ Unknown`，`Unknown` 永不许 `Succeeded` | `a_retry_settles_the_tool_call_the_failed_attempt_left_unanswered`、`an_unknown_tool_outcome_allows_failure_but_never_success` | 每 tick 全量 `JournalProjection::read` |
| `768e8cd` | journal 已派发但 turn 无 execution 记录的 `Interrupted` 结果被 `NOT_FOUND` 拒绝整批 | `Unknown` + 无记录 → 跳过；其它状态仍 `NOT_FOUND` | `tests/tool_facts_test.rs` | 语义收紧正确，ghost result（Success + 未知）仍失败 |
| `3972c2e` | 多宿主并存：活进程按 mtime 被误回收、跨 scope 同 work id 并发写索引、Windows 瞬时占用未全覆盖 | pid 存活唯一权威 + rename 抢占回读 + work 索引独立锁 + `retry_while_held` 全覆盖 | 6 条并发用例 | 见 §6.1 第 1 条（孤儿锁 / 放回丢锁） |
| `ad7c721` | 写放大：每次续租都整文档重写；scope 无限增长；事件无剪枝 | 续租 headroom、`compact.rs`（1 MiB 阈值，事务内压缩）、`prune_events` + `retained_after` | `a_scope_over_the_threshold_is_compacted_without_losing_a_fact`、2 条 prune/分页、`write_traffic_test` | 压缩每次超阈值都做 3 次全量 `keys`；压不下 1 MiB 的 scope 会永久每次付费 |
| `b810a32` | `journal_position`（1015）被当单页 limit 传入，`clamp_limit` 只收 1..=1000 → thread 超 1000 条后每次 open 都失败 | 真分页（`PAGE=1000`，按 `seq` 推进，`!full_page \|\| !progress` 退出） | `a_thread_past_one_journal_page_still_opens`、`paging_after_a_prune_returns_every_retained_event_exactly_once` | 每页仍从头全量扫 key |
| `4eaa088` | 幂等校验同一缺陷模式（`read_after(expected_seq, events.len())`） | 按页补齐 | **无新增用例** | 缺 >1000 条批次用例；`harness_adapter_test.rs:278` 仍把 `journal_position` 当 limit |

**链条质量**：`76eda5b → 6d9c6d2 → 96b433d → 768e8cd` 是一条清晰的因果链（失败终态化 → 对账补调用点 → 对账不阻断提交），`b810a32 → 4eaa088` 是同一缺陷模式的两个调用点。**没有互相冲突或补丁叠补丁**；每个根因都有现场证据（错误串、日志行数、scope 大小）。

---

## 6. 问题清单

### 6.1 必须修复（P0/P1）

**1. file store 可留下「活 pid 的孤儿锁」→ scope 长期不可用；`reclaim_stale` 可丢弃活锁 → 双写窗口**

- 位置：`store/file.rs:474-486`（`Drop` 读失败或 `remove_file` 失败均被吞）、`store/file.rs:496-503`（`write_all`/`flush` 失败被吞 → 锁体为空）、`store/file.rs:606-625`（`stale_body` 用裸 `fs::read_to_string`，不重试）、`store/file.rs:540-585`（放回分支 `create_new` 失败时 `remove_file(aside)` 丢掉活锁）。
- 现象（本会话实测）：`only_one_contender_reclaims_a_stale_lock` 在并发负载下 30 次取锁有 28 次等满 60s 报错；单跑 0.19s 通过。
- 影响：长驻宿主进程里一次删除/写入失败即让该 scope 此后每次事务等 60s 再失败（进程内 pid 永远「活着」，没有任何回收路径）；放回分支失败则是丢更新（两个 writer 同时写同一 scope）。
- 建议：①`ScopeLock` 增加进程内持有表（`path → stamp`），`stale_body` 遇到 `pid == self` 但不在表内时视为遗留锁立即接管；②`Drop` 删除失败时把锁体改写成墓碑（如 `0 0`）并回退 mtime，使其可被回收；③写 scope 前回读校验 `body == stamp`（`verify()`），不等则放弃本次提交；④`stale_body` 的读也走 `retry_while_held`。

**2. `queue_ids` 硬编码 10,000 上限 → 长会话静默永久停摆**

- 位置：`backend/mod.rs:187-196`。
- 现象：第 10,001 个 turn 对 `current_turn` 不可见 → `ready_at` 返 −1 → 该 thread 不再被调度，而 `submit` 仍然成功（无任何错误）。
- 建议：按 `after` 分页或只索引非终态 turn（配合 §6.2-1 的终态清理），并对截断显式报错。

**3. `write_atomic` 无 `fsync`：模块文档承诺在断电下不成立**

- 位置：`store/file.rs:1-5`（承诺「崩溃只留旧或新文档」）、`store/file.rs:412-429`（`fs::write` + `rename`，无 `sync_all`、无目录 fsync）。
- 影响：进程崩溃安全（rename 原子），**掉电/蓝屏不安全**（可留零长度或半写文档）。对本地常驻部署是可接受取舍，但必须与文档口径一致；若要承诺掉电安全则需 `sync_all` + 目录 fsync（显著增加写入成本，与「低消耗」冲突，需用户裁决）。

**4. 读放大未治理：O(n²) 的 journal 读取 + 每 tick 三次全量重放 + 50ms 全 scope 解析**

- 位置：`backend/query.rs:53`（`keys(&prefix, None, 100_000)` 丢 `after`）、`store/file.rs` 的 `keys()` 逐键完整 JSON 解析、`harness_adapter.rs:115`（`settle_uncertain_tool_outcomes` → `JournalProjection::read`）、`execution_host.rs:102`（`read_events` 从 0 读起再按 turn 过滤）、`harness_adapter.rs:52,209`（`STREAM_CONTROL_POLL=50ms` + `frame()` 解析整个 scope）。
- 影响：tick 成本随历史线性（×3），长会话下单次模型调用期间持续数十 MB/s 解析，直接违反「低消耗高性能」最高规则。
- 建议：`keys` 下推 `after`（journal key 已 20 位零填充，字典序即序号序）；`frame()` 换轻量控制查询或指数退避；`execution_host` 从 checkpoint 位置起读并只折叠当前 turn。

**5. 取消 `Waiting`/`Pausing` turn 后无人推进，turn 停在 `Cancelling`**

- 位置：`session.rs:696-719`（`drive_turn` 遇 `Waiting` 即返回）、`session.rs:3418-3461`（取消只等 terminal，不自推进）。
- 现象：客户端已收到 `StopReason::Cancelled`，但 turn 未终态；`session/close` 的 500ms 宽限后永久停住，直到下一条 prompt 的 tick 才收敛。
- 建议：取消路径在等待期自己 tick（或把 turn 置为终态 `Cancelled`），并补一条「取消 waiting turn 后 turn 必须 terminal」的用例。

**6. acting 续跑的两个正确性缺口**

- (a) `execution_host.rs:109` 只折叠 `step/start`，忽略 `step/resumed`（`recorder.rs:1474` 新增）→ 续跑途中崩溃后 `acting_iteration` 回退一格（多一次模型调用、iteration 编号重复）。
- (b) `react_agent.rs` 续跑只按「状态里有未应答 `tool_use`」判断，而结算只覆盖 dispatched-without-result；若 `tool/result` 已入 journal 但检查点落后（`backend_thread_journal.rs:308` 批次无 state 事件时沿用旧检查点），该 call 既非 uncertain 又仍 pending → **会再执行一次**（`commit.rs:295-300` 对重复 dispatch 只 `continue`，不阻止本地副作用）。建议续跑前用 `frame.tools` 的已有结果兜底。

**7. 异步工具（offload）的终态语义不闭合**

- 位置：`tool_facts.rs:151`（`Suspended → Pending` 落盘）、`backend/commit.rs:437`（`Pending` 不 `concluded` → 拒绝任何终态）。
- 现象：装了 offload 的宿主（ACP `coding_agent.rs:150`）在有后台工具时 `settle_turn(Succeeded)` 会被 `External operations are not settled` 拒绝 → turn 可能以 `Failed` 收尾而副作用仍在跑。
- 建议：把「等待后台工具」建模成 `Waiting`（带 wait key/deadline），而不是 `Pending`；或让 executor 的 `query` 结果驱动 `accept_result`。

### 6.2 建议改进（P2）

| # | 位置 | 问题 |
|---|---|---|
| 1 | `backend/mod.rs:218-241` | `ready_at` 缺终态守卫：已 settle 的 turn 仍以 `ready=0`（Failed 则 `next_attempt_at`）留在索引，每次 settle 多一次 claim + 整文档写 |
| 2 | `backend/turn.rs:432-455` | 旧 `waiting` 自愈分支无 `assert_owned`，过期同 epoch 写者可改 turn |
| 3 | `backend/turn.rs:245-262` | `Respond` 绕过 `accept_signal`，不校验 withdrawn/consumed/终态 |
| 4 | `backend/tools.rs:33-50` | 每次 tick 都 `save_turn`；`apply_run`/`apply_step` **全仓零生产者**（`RunStatus` 永为 Prepared→Stopped，`started_at` 永不设置） |
| 5 | `backend/tools.rs:258` | 事件 id 用毫秒时间戳，同毫秒两次 progress → CONFLICT |
| 6 | `backend/query.rs:205,307`、`backend/commit.rs:586`、`backend/compact.rs:104,151,178` | 多处 `keys(prefix, None, 100_000)` 全量扫描；>10 万条静默截断 |
| 7 | `store/file.rs:245-292` | `ready()` 读整个 work 目录（含所有 session）再按 queue 过滤，成本随线程总数线性 |
| 8 | `backend/compact.rs:181-200` | 只删事件本体、保留 event-id 标记 → 事件相关 key 仍线性增长 |
| 9 | `backend/mod.rs:74-90` | `already_granted` 用进程时钟比 store 写入的 `txn.now()`，跨主机时钟偏快可能漏续租 → FENCED |
| 10 | `backend/turn.rs:337,410,490` | `now + ttl/delay` 无 `checked_add`（公开 `claim` 可传 `Duration::MAX`） |
| 11 | `types/execution.rs:78-90` | `execution_id = turn_(70) + call_id(≤256)` 超 `json::text` 的 256 上限 → 合法 ToolCall 使 commit 报 INVALID |
| 12 | `adapter/execution_host.rs:340-348` | `finish` 在 `completed` 时直接 `Ok(())`、不 await `continuation` → 跳过 `commit_loop_state`（当前只因 checkpoint 已含回复才安全，属隐式依赖） |
| 13 | `adapter/tool_facts.rs:82-90` | 只跳过 server tool 的 intent，其 `tool/result` 仍推导成 result；若该 call 无 attempted 记录 → `Execution has not been dispatched` 使整批回滚（与 `commit.rs:339-346` 对 Unknown 的宽容不对称） |
| 14 | `adapter/backend_thread_journal.rs:220-232` | 幂等只比对 `event_id`，不比内容（durable receipt 比 digest）→ rebase 后同 id 不同内容被静默吞掉 |
| 15 | `adapter/checkpoint.rs:28-32` | `decode` 不校验 `state_version`；`AgentState.session_id/reply_id` 无 `#[serde(default)]` → 老快照硬失败 |
| 16 | `adapter/execution_host.rs:345` | `state.lock().unwrap()`（锁中毒即 panic，唯一 unwrap） |
| 17 | `adapter/harness_adapter.rs:299-312` | 取消/暂停/中断映射正确；但 `check_control` 对 cancel 返回 `HostPaused` 只是让出，终态依赖 frame 判定，语义分散 |
| 18 | `crates/agentscope-durable/Cargo.toml:19` | `tokio` 只在测试使用，应为 dev-dependency |
| 19 | `core/src/journal/{recorder,models,export,execution}.rs` | 重命名后 context key 仍为 `"agentscope.session.*"`（6 处），与 `journal` 命名不一致 |
| 20 | `execution_host.rs:13-15`、`react_agent.rs:1946` | 注释仍称「acting_iteration 无消费者 / resume 分支未迁移」，已被 `85068cf`/`31c51b3` 推翻 |

### 6.3 可接受 / 已知取舍

1. `Unknown` 永不算成功：实际已生效的调用会报 `Failed` —— 用户拍板的诚实语义。
2. `300s WAITING_STALL_TIMEOUT` 对合法长等待会误报 stall，但 turn 仍可恢复。
3. pid 复用 → 最多 60s 等待后 `unavailable`（不误抢，方向正确）。
4. `history()` 仍只投影 turn 输出，无消息转录；ACP 走 journal 转录，无回退。
5. `_agentscope/respond` 的 wait key 仍依赖「interaction id == wait key」，没有通用 interaction 记录。
6. ACP 路径不经过 `HarnessAdapter::settle` 默认实现（adapter 自己 settle）。
7. 每个 session 一次 `TxnBackend::open`（file store 每次加锁打开），LRU 淘汰时随 runtime 释放，未做全局缓存。
8. 内存峰值 ≈3–5× scope 字节（1 MiB 阈值下可控）；`InMemoryStore::transaction` 持锁跑闭包，嵌套 `with_session` 会死锁（当前无嵌套调用点）。
9. 死代码（已知、留档）：`model.rs:296 unsettled_tool`（编译告警）、`invocation_count`、`ThreadData.settled`（恒 0）、`resource_position`（恒 0）、`Restoration/ResourceRequest/DelegationTarget/EventFilter/LiveEvent/ModelCallView` 无消费者、`ExecutionBaseline`/`prepare_with_baseline` 失去消费者、`with_tool_facts(false)`/`pin()` 仅测试使用。

---

## 7. 测试与验证

### 7.1 本会话实测

| 命令 | 结果 |
|---|---|
| `cargo test -p agentscope-durable -p agentscope-durable-adapter-agentscope -p agentscope-extensions-acp -j 2 --no-fail-fast` | **31 targets / 229 passed / 0 failed**，另有 1 target **FAILED**（`agentscope-durable --lib`：27 passed / 1 failed，`only_one_contender_reclaims_a_stale_lock` 300.04s，`left: 28 / right: 0`）；`EXITCODE=101` |
| `cargo test -p agentscope-durable --lib`（单独复跑） | **28 passed / 0 failed / 1.75s**（`EXITCODE=0`）→ 前一失败为**负载敏感**，但失败形态（28/30 次等锁 60s 超时）指向上文 §6.1-1 的真实饥饿窗口 |
| `cargo test -p agentscope-durable --lib only_one_contender_reclaims_a_stale_lock`（单跑） | 0.19s 通过 |
| `cargo check -p agentscope-durable -p agentscope-durable-adapter-agentscope --all-targets` | 通过；`agentscope-durable` 有 1 条 `dead_code` 告警（`model.rs:296 unsettled_tool`） |

仓库既有全量口径（`ad7c721` 时记录）：**207 targets / 6410 passed / 0 failed / 8 ignored**；`cargo fmt --all -- --check` 干净。本会话未重跑全量。

### 7.2 测试资产

| crate | `#[test]`/`#[tokio::test]` 函数数 | 集成测试文件 |
|---|---|---|
| `agentscope-durable` | 51 | `backend_contract.rs`(8)、`session_test.rs`(21) |
| `agentscope-durable-adapter-agentscope` | 44 | `checkpoint`(4)、`compaction`(3)、`execution_host`(8)、`harness_adapter`(13)、`thread_journal_bridge`(4)、`three_process`(4)、`tool_facts`(6)、`write_traffic`(1) |
| `agentscope-extensions-acp` | 165 | 14 个 ACP 集成套件（cancel / interrupted / load_replay / event_flow / compact / capabilities / …） |

三 crate 内**无任何 `#[ignore]`**；反向验证有记录：`76eda5b`、`28366c3`、`6d9c6d2`、`96b433d`、`768e8cd`、`b810a32`、`85068cf`、`31c51b3`、`ad50390`。

### 7.3 测试缺口

1. `75b0d8b`（回复回写 + inbox 去重）**零测试**——用户可见的回归修复无守卫。
2. `b30672e`（等锁上限）、`4eaa088`（幂等分页）无新增用例。
3. 取消 `Waiting`/`Pausing` turn 的终态无用例（§6.1-5）。
4. `step/resumed` 的折叠与位置恢复无用例（§6.1-6a）。
5. 多宿主并发 tick 的写入量无断言（`write_traffic_test` 只测单进程、单 turn）；跨 turn 的二次增长无守卫。
6. `three_process_test::the_store_keeps_the_state_after_the_writer_process_exits` 在未设 `DURABLE_WRITER_ROOT` 时静默 `return`（永远绿的空用例）。
7. 场景 A（`suspend_resume_and_duplicate_share_only_the_store`）是同进程三 Worker + `InMemoryStore`，真正的「两个进程并发写同一 root」在适配层无覆盖。

---

## 8. 文档与发布

- **0.2.0 破坏性变更**（`bd7b0e0`，全部 crate 版本 + 内部 path 依赖版本同步）：①D2 全量重命名（`core::session` → `core::journal`、`Session*` → `Thread*/Journal*/Invocation*`、`HarnessAgent` session API 换 thread API）；②D3 删除 `AgentSession`/`SessionAnswer`/`InputReceipt`/`HarnessAgent::session()`；③D4 事件与头格式 v2（v1 会话文件不可读）；④D6 删除 `controlplane` 扩展与 `agentscope-rs` feature；⑤新增两个 durable crate；⑥`ExtAnswer::to_session_answer` → `to_answer_text`；⑦新增 `step/resumed` 事件类型。
- **问题**：仓库无 `CHANGELOG`/release note，v1 不可读（D4）这一条对用户不可见。建议在 `docs/` 或 `README` 补一节「0.2.0 迁移须知」。
- **新增文档**：`docs/DURABLE_API_DESIGN.md`、`docs/DURABLE_MIGRATION_PLAN.md`（含 M2–M4-e-4 逐里程碑落地记录与阻塞记录，质量高）、`docs/cloud/{durable-handoff-2026-11-14,CF_DEPLOYMENT_ARCHITECTURE,CF_MIGRATION_TASKS}.md`（Cloudflare 兼容边界：七个端口、两个 Driver、四条硬规则 R1–R4）。
- **文档与实现不符**：`DURABLE_MIGRATION_PLAN.md` §2.1 的「有意缺口」第 1、3、4 条已过期（见 §4）；建议就地更新，避免后续接手者按过期缺口重复实现。

---

## 9. 遗留缺口与后续建议

### 9.1 功能缺口（M6 及以后）

1. `durable-worker`（Brain/Hands）、`durable-{jdbc,http,mcp,rocketmq,objects,sandbox}` 后端、Postgres 后端、protocol JSON-Schema 全量——按计划 M6 独立验收。
2. `resources`/`delegate`/`restore` 未入 `SessionBackend` trait（原语 `resource_position`/`journal_branch` 已预留但恒 0）。
3. 模型事实（`ModelCall/ModelRequest/TokenUsage`）只有类型未接线；`history()` 无消息转录。
4. 大对象外置（>16 KiB 未做）；`settled` 前缀未物化。
5. digest 用键排序 canonical JSON 而非 Java RFC 8785（两边不互通，不影响正确性）。

### 9.2 性能（按「低消耗高性能」判据）

1. **最大杠杆**：跨 `InvocationRecorder` 批次合并写入。当前每批次都是 core 的耐久边界（工具意图必须先落盘），合并需改 core 耐久契约，按「不确定宁可不动」留待单独评估。
2. 读侧：`keys` 下推 `after`、`frame()` 轻量化、`execution_host` 从 checkpoint 起读——三项合计可把 tick 成本从 O(history) 降到 O(1) 量级。
3. `compact` 记「上次压缩后大小」，压不下去的 scope 跳过一轮，避免永久付费。
4. 事件 id 改单调计数（避免同毫秒 CONFLICT）。

### 9.3 流程建议

1. 本批 41 个提交**全部未 push**；`docs/cloud/` 三份文档已随 `844214a` 入库。
2. `dev` 保持线性（`git log --merges` 为空），符合最高规则。
3. 建议把本报告 §6.1 的 7 条转为 issue/待办，其中第 1、2、4 条优先（分别影响可用性、长会话存活、性能上限）。

---

## 10. 附录

### A. 逐提交明细（正序）

| # | 提交 | 日期 | 文件 / 行 | 主题 |
|---|---|---|---|---|
| 1 | `546a046` | 11-14 | 70 / +1988 −1276 | core session→journal 重命名、durable 语义类型、事件 v2 |
| 2 | `aaaf53c` | 11-15 | 29 / +6905 −1 | 新增 `agentscope-durable` 骨架 |
| 3 | `7a4bb6d` | 11-15 | 11 / +480 | ThreadJournal 桥接（同事务） |
| 4 | `df0dfa5` | 11-15 | 7 / +929 −20 | 执行宿主接入 ReActAgent 循环 |
| 5 | `27f7236` | 11-15 | 11 / +825 −97 | 工具三态接线 |
| 6 | `3939f1d` | 11-15 | 6 / +602 −16 | AgentState ↔ checkpoint |
| 7 | `e49fede` | 11-15 | 2 / +722 −1 | 三进程验收（D7） |
| 8 | `269f453` | 11-15 | 1 / +57 | M4 语义映射文档 |
| 9 | `bda0951` | 11-15 | 10 / +1022 −106 | signal 三类、suspend/cancel 分离、`DurableSession` 门面 |
| 10 | `9e83e1a` | 11-15 | 8 / +467 −22 | 门面补齐 wakeup/respond/task/history/事件通道 |
| 11 | `ca0d18c` | 11-15 | 8 / +912 −6 | 可执行 HarnessAdapter + 租约借用 |
| 12 | `f04d7c8` | 11-15 | 5 / +77 −9 | tick 让出判定增强；ACP 接线回退 |
| 13 | `71e2b8b` | 11-15 | 5 / +21 −25 | 清理依赖告警 |
| 14 | `7e4a498` | 11-15 | 3 / +390 −32 | durable cancel/pause → `CancelSignal` |
| 15 | `ad50390` | 11-15 | 8 / +591 −257 | ACP 改由 durable 驱动 |
| 16 | `35dab62` | 11-15 | 13 / +45 −3675 | 删除 `AgentSession`/`SessionAnswer` |
| 17 | `bd7b0e0` | 11-15 | 3 / +66 −66 | 版本 0.2.0 |
| 18 | `08a35c8` | 11-15 | 2 / +22 −12 | 执行守卫用例独立二进制 |
| 19 | `85068cf` | 11-15 | 2 / +310 −18 | acting 阶段续跑入口 |
| 20 | `2cd4ebd` | 11-15 | 1 / +0 −3 | 移除未引用的 workspace 依赖 |
| 21 | `844214a` | 11-15 | 3 / +1616 | Cloudflare 文档归档 |
| 22 | `31c51b3` | 11-15 | 3 / +245 −11 | acting 续跑写 step 边界 |
| 23 | `76eda5b` | 11-15 | 3 / +88 −2 | 不可重试失败直接终结 turn |
| 24 | `75b0d8b` | 11-15 | 2 / +38 | 回复回写共享上下文 + 通知去重 |
| 25 | `28366c3` | 11-15 | 1 / +162 −3 | 打开会话前对账未知工具结果 |
| 26 | `2cfc773` | 11-15 | 2 / +252 −1 | offload 落 Suspended 结果 |
| 27 | `b30672e` | 11-15 | 1 / +6 −1 | 等锁上限覆盖陈旧锁回收 |
| 28 | `5b6784e` | 11-15 | 3 / +118 | 按 pid 存活即时回收死锁 |
| 29 | `bccb4a3` | 11-15 | 3 / +102 −17 | Windows os error 5 的两个来源 |
| 30 | `6d9c6d2` | 11-15 | 4 / +451 −60 | 未决工具结果不再让会话不可开 |
| 31 | `7a1fd7f` | 11-15 | 1 / +140 | 租约续期边界测试 |
| 32 | `96b433d` | 11-15 | 7 / +301 −10 | 重试前结算 + Unknown 永不算 Succeeded |
| 33 | `768e8cd` | 11-15 | 2 / +118 | 未知 execution 的 interrupted 结算 |
| 34 | `869f073` | 11-15 | 2 / +154 | 记录架构原则与写优化基准 |
| 35 | `3972c2e` | 11-15 | 1 / +503 −66 | 多宿主锁安全与瞬时占用重试 |
| 36 | `1dde33d` | 11-15 | 2 / +21 | 记录锁语义与写放大实测 |
| 37 | `ad7c721` | 11-15 | 10 / +1269 −6 | 写放大治理 + scope 压缩 + 事件剪枝 |
| 38 | `f0bbbda` | 11-15 | 2 / +13 | 记录实测数据 |
| 39 | `b810a32` | 11-15 | 3 / +146 −11 | journal 读取按页分页 |
| 40 | `4eaa088` | 11-15 | 1 / +18 −1 | 幂等校验按页读取 |
| 41 | `0917b81` | 11-15 | 2 / +11 | 记录 Page limit 回归 |

### B. 验证命令

```powershell
# 范围
git rev-list --count 546a046..HEAD          # 40（+546a046 共 41）
git log --merges --oneline -5                # 空（线性历史）

# 定向测试
cargo test -p agentscope-durable -p agentscope-durable-adapter-agentscope -p agentscope-extensions-acp -j 2 --no-fail-fast
cargo test -p agentscope-durable --lib

# 静态检查
cargo check -p agentscope-durable -p agentscope-durable-adapter-agentscope --all-targets
```

### C. 审查覆盖声明

- **通读**：`agentscope-durable` 全部 src（`types/*`、`store/{memory,file}`、`backend/{mod,turn,commit,tools,model,identity,query,compact}`、`runtime`、`session`、`session_backend`、`harness_adapter`、`executor`、`error`、`json`）；`agentscope-durable-adapter-agentscope` 全部 src；`agentscope-extensions-acp/src/session.rs` 的关键段（`SessionRuntime`/`drive_turn`/取消路径/超时常量）；`agentscope-core/src/agent/react_agent.rs` 的 acting 续跑段；`journal/recorder.rs` 的 `resume_step`；`DURABLE_MIGRATION_PLAN.md` §1–§2.13、§4–§7。
- **抽样/未逐行**：`runtime.rs` 与 `file.rs` 的测试体、`session_test.rs` 全部用例体、ACP `session.rs` 的渲染与 MCP 段、`journal/atomic.rs`/`snapshot.rs`/`inbox.rs` 全文。
- **未执行**：全量 workspace 测试、Linux/macOS 构建、断电/掉电验证、`cargo fmt --check`。
- 本报告中的「必须修复」项均已核对到具体代码行；仅「实测」结论（§6.1-1、§7.1）来自本会话真实运行，其余为静态审查结论。

