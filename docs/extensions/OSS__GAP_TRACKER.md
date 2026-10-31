# OSS 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **OSS** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-oss\`
> **Java 子模块**：(单模块)
> **Rust 目标**：`crates/agentscope-extensions-oss/`（波次 B）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-oss` |
| Java 子模块 | (单模块) |
| Java 规模（2026-10-22 实测） | 主代码 **795 行 / 5 个类**（`OssAgentStateStore` 314、`OssBaseStore` 268、`OssDistributedStore` 77、`OssRemoteSnapshotClient` 81、`OssSnapshotSpec` 55；含空行/注释共 880 行）；测试 **290 行 / 2 个类 / 20 个 `@Test`**（137+153；含空行/注释共 336 行） |
| Rust crate | `crates/agentscope-extensions-oss` |
| Rust 规模（2026-10-22 实测；**用例（2026-10-29 静态计数）：默认 64（`tests/` 62 + `src/guard.rs` 内联 2）+ 1 doc-test / `--features s3` 75 + 2 doc-test**） | `src/` **8 文件 1210 行 / 非空 1064 行**（base_store 265、client 224、distributed_store 131、error 53、guard 65、lib 52、snapshot 128、state_store 292）；`tests/` **5 文件 525 行 / 非空 446 行**；用例 **41 个 `#[test]`/`#[tokio::test]` + 1 个 `no_run` doc-test = 42**（2026-10-22 口径，非现行），`src/` 内 `cfg(test)` 命中 **0** |
| 迁移波次 | B |
| 当前状态 | **已完成**（P1/P2 清零；2026-10-31 实测 64 passed / 0 failed（默认，+1 doc-test）/ 75（`--features s3`，+2 doc-test））—— 契约 5/5 齐全，`DistributedStore` 五槽已可完整产出（§4.2-1 已关闭）；真实剩余见 §4.1；**2026-10-31 全量复核新增：生产阻断 1 项（OSS-01，已修复，见 §3.5）+ P1 3 项（OSS-02/03/04）+ 待裁定 2 项（OSS-09/10），见 §3.5**。 |

> 2026-10-31 全量对齐复核：逐文件核对结论见 §3.5（`OSS-01`…`OSS-20`）与 §4.3（`OSS-D01`…`D04` / `OSS-X01`…`X12`）。结论：5/5 Java 类均已映射、契约齐全；**生产阻断项 1 个（OSS-01，s3 后端在 async 上下文 panic）**，P1 3 个（OSS-02/03 无真 CAS、OSS-04 无超时重试），其余为 P2 与 2 项待裁定。
> 勘误（不改历史条目）：§2/§3.1 记的「core `AgentStateStore` 无 per-key 删除」已过时——core trait 现含 `delete_key`（`crates/agentscope-core/src/state/store.rs:85`），本 crate 已覆写并返回 `Result`（`src/state_store.rs:390-402`），crate 内固有方法保留为 strict 语义入口。

## 2. Java 清单与依赖映射
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `OssAgentStateStore` | OSS 后端的 `core.state.AgentStateStore`；状态以 JSON 存于 `{prefix}{user}/{session}/{key}.json`，列表另有 `.list.json` + `.list.hash` | `OssAgentStateStore`（`state_store.rs`） | 实现 `agentscope_core::state::store::AgentStateStore`（async_trait）；复用 `core::state::list_hash` 的 `compute_hash`/`needs_full_rewrite` 做增量 append 检测。**已对齐（history/OSS__MIGRATION_HISTORY.md-1）**：per-key `delete_key(userId, sessionId, key)` 作为 **crate 固有方法**实现（core trait 无此方法；照 MONGODB `delete_key` 先例，不改 core/harness），删除 `.json`/`.list.json`/`.list.hash` 三个对象 |
| `OssBaseStore` | OSS 后端的 harness `BaseStore`；`{prefix}{ns...}/{key}.json` + `.version` | `OssStore`（`base_store.rs`） | 实现 `agentscope_harness::filesystem::remote::store::BaseStore`（同步）；CAS/版本语义以 harness 为准（version 从 1 起、`put_if_version(0)`=仅当不存在时写）。`supports_atomic_session_storage` 两侧均未覆写 → 同为 `false`（行为等价） |
| `OssRemoteSnapshotClient` | OSS 后端的 `RemoteSnapshotClient`；`{prefix}{snapshotId}.tar` | `OssRemoteSnapshotClient`（`snapshot.rs`） | 实现 `agentscope_harness::agent::sandbox::snapshot::RemoteSnapshotClient`（async_trait）；入参由 Java `InputStream` 改为 `&[u8]` |
| `OssSnapshotSpec` | 便捷 `SandboxSnapshotSpec` | `OssSnapshotSpec`（`snapshot.rs`） | 实现 `SandboxSnapshotSpec`，内部包装 harness `RemoteSnapshotSpec`（id 校验/`RemoteSandboxSnapshot` 接线复用）。**差异**：Java 另有 `(endpoint, accessKeyId, accessKeySecret, bucket, prefix)` 便捷构造器（内部 `OSSClientBuilder`）；Rust 无 SDK 故无此构造器（P2） |
| `OssDistributedStore` | 组合者：state store + base store + snapshot spec（+ guard） | `OssDistributedStore`（`distributed_store.rs`） | 共享同一 `OssClient`，按 `state/`/`store/`/`snapshot/` 子前缀组合；另暴露 `distributed_store()` 产出 harness `DistributedStore`（字段不足见 §4.2） |
| (harness `SandboxExecutionGuard`) | **Java OSS 模块不定义自己的 guard**：`OssDistributedStore` 未覆写 `sandboxExecutionGuard()` → 落回 `DistributedStore` 默认 `SandboxExecutionGuard.noop()` | `OssSandboxExecutionGuard`（`guard.rs`） | **不新建平行类型**（遵守 EXTENSIONS_GAP_TRACKER §2.1）：透明包装 harness `SandboxExecutionGuard`，语义完全委托。**注**：这是 Rust 侧比 Java 多出的便利类型（Java 无 `Oss*Guard`），不属于「对齐缺口」 |
| `com.aliyun.oss.OSS` | 阿里云 OSS SDK 客户端 | `OssClient` trait（`client.rs`）+ `InMemoryOssClient` + （`s3` feature）`S3OssClient` | 后端抽象（put/get/exists/delete/list_page/list_all/shutdown）。**已于裁定 8 改为通用 `s3` feature（history/OSS__MIGRATION_HISTORY.md）**；`lib.rs` 文档已在 history/OSS__MIGRATION_HISTORY.md-4 更正为「feature 只负责**编译**客户端、绝不自动选后端；默认只有 `InMemoryOssClient`；需显式配置 bucket/region/endpoint/凭证」。**仍缺**：阿里云 OSS 原生 SDK 路径（走 S3 兼容端点代替，P2） |
| `JsonUtils` / Jackson | JSON 编解码 | `serde_json` | 直接映射 |
| `ListHashUtil` | 列表变更检测 | `agentscope_core::state::list_hash` | 直接复用，无平行实现 |

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| `com.aliyun.oss:aliyun-sdk-oss:3.18.5` | 阿里云 OSS 对象读写 | `rust-s3` 0.38（optional，feature `s3`）→ `S3OssClient` | **已完成（2026-10-29 勘误）**：原记「`aliyun` 空壳 feature + 实机后端待接入」，实际裁定 8 已删该 feature，改为通用 `s3` feature + `S3OssClient`（S3 兼容端点：AWS S3/MinIO/Ceph/阿里云 OSS-S3），依据 history/OSS__MIGRATION_HISTORY.md（`tests/s3.rs` 7 例 + `src/s3.rs` 内联 4 例，`httpmock` 离线端到端）。**仍缺**阿里云原生 SDK 路径（P2） |
| Jackson (`JsonUtils`) | JSON | `serde` / `serde_json`（workspace） | 已用 |
| `agentscope-core` (provided) | AgentStateStore/ListHash | `agentscope-core`（path 依赖） | 实现其 trait，不重建 |
| `agentscope-harness` (provided) | BaseStore/RemoteSnapshotClient/SandboxSnapshotSpec/Guard | `agentscope-harness`（path 依赖） | 实现其 trait，不重建 |

## 3. 详细缺口清单
### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 骨架 + Cargo 依赖 | pom.xml | 已完成 | 依赖仅写入本 crate `Cargo.toml`；未改根 `[workspace.dependencies]`，未改 core/harness。feature 现仅 `s3`（`rust-s3` 0.38 optional，裁定 8，见 history/OSS__MIGRATION_HISTORY.md）。**冗余依赖（2026-10-29 复核仍未清理）**：`futures`、`chrono`、`serde` 声明但零引用（`chrono` 命中均为注释里的 synchronous 子串），建议清理（P2-1） |
| `OssClient` 后端抽象 + `InMemoryOssClient` | `OSS` (SDK) | 已完成 | `client.rs`；put/get/exists/delete/list_page/list_all/shutdown；key 前缀归一 + snapshotId 校验辅助。**分页已补齐（history/OSS__MIGRATION_HISTORY.md-2）**：新增 `InMemoryOssClient::with_page_size(n)` / `page_size()`，`list_page` 受页帽约束（`max_keys=0` → 空页且 `truncated=false`）；`tests/pagination.rs` 13 用例覆盖首页/续传/最后一页/精确边界/空结果/`list_all` 多页聚合 |
| `OssAgentStateStore` | `OssAgentStateStore` | 已完成 | 实现 `AgentStateStore`（save/save_list/get/get_list/exists/delete/list_session_ids/close）；`__anon__` 默认用户；三后缀 `.json`/`.list.json`/`.list.hash`。**per-key `delete_key` 已落地（history/OSS__MIGRATION_HISTORY.md-1）**（crate 固有方法，仿 MONGODB 先例，不改 core trait） |
| `OssStore`（`BaseStore`） | `OssBaseStore` | 已完成 | 实现 `BaseStore`（get/put/put_if_version/search/delete）；`search` 按 key 排序分页、排除 `.version`；key 去前导 `/`；**错误传播已对齐（history/OSS__MIGRATION_HISTORY.md-3）**：新增固有方法 `search_strict -> OssResult<Vec<StoreItem>>`（列表 key 不在 namespace prefix 下 / item JSON 损坏 → `OssError::Backend`/`Json`，对齐 Java 抛 `RuntimeException`），trait `search` 委托它并在错误时返回空页 + `tracing::error!`（不再静默伪造 item key） |
| `OssRemoteSnapshotClient` + `OssSnapshotSpec` | `OssRemoteSnapshotClient`/`OssSnapshotSpec` | 已完成 | 实现 `RemoteSnapshotClient`/`SandboxSnapshotSpec`；`.tar` 布局；内部复用 harness `RemoteSnapshotSpec`。**缺 Java 的 endpoint/AK/SK 便捷构造器**（P2-2） |
| `OssDistributedStore` | `OssDistributedStore` | 已完成 | 组合 state/base store + snapshot spec + guard；子前缀 `state/`/`store/`/`snapshot/`；`distributed_store()` 产出 harness 组合 |
| `OssSandboxExecutionGuard` | (harness guard) | 已完成 | 透明包装 harness `SandboxExecutionGuard`（不新建平行类型，遵守 §2.1）。**注意**：Java OSS 模块**没有** guard 类也不覆写 `sandboxExecutionGuard()`，本类型属 Rust 侧便利增量，非对齐项 |

### 3.2 测试（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| AgentStateStore 测试 | `OssAgentStateStoreTest`（137 行 / 10 `@Test`） | 已完成（P1-1 已补） | `tests/agent_state_store.rs`（**18 用例**，实测 `#[test]`+`#[tokio::test]` 计数）：builder 校验、key 布局、`__anon__`、单值/列表往返、append 重写、exists、list_session_ids、delete、close、prefix 归一，以及 **per-key `delete_key` 4 用例**（history/OSS__MIGRATION_HISTORY.md-1） |
| BaseStore 测试 | `OssBaseStoreTest`（153 行 / 10 `@Test`） | 已完成（P1-3 已补） | `tests/base_store.rs`（**17 用例**）：builder 校验、put/get+version、CAS 命中/未命中/expected=0、search 分页排序、排除 `.version`、delete、去前导 `/`、命名空间隔离；**新增 4 个**：多页 `search`、`search_strict` 拒绝越界 key、传播列表错误、拒纳损坏 JSON（history/OSS__MIGRATION_HISTORY.md-3） |
| 分页测试 | —（Java 无对应测试类） | **已完成（P1-2）** | `tests/pagination.rs`（**13 用例**）：`page_size` 钳位、首页+续传令牌、续传、最后一页、精确边界页、无匹配空页、token 越过末 key、`max_keys=0`、调用方 `max_keys` 小于页帽、`list_all` 多页聚合/空结果、state store `list_session_ids` 与 session `delete` 跨页 |
| Snapshot 测试 | —（Java 无对应测试类） | 已完成 | `tests/snapshot.rs`（75 行 / **7 用例**）：上传/下载/存在往返、缺失失败、空桶校验、非法 id 拒绝、spec build、persist/restore |
| Guard + DistributedStore 测试 | —（Java 无对应测试类） | 已完成 | `tests/guard_and_distributed.rs`（88 行 / **6 用例**）：guard 委托语义/可重入、空桶校验、子前缀组合、harness `DistributedStore` 组装、`dyn BaseStore` 可用性 |
| 文档测试 | — | 已完成 | `lib.rs` doc example（`no_run`，1 个；因 `no_run` 只编译不执行，实际断言价值为 0） |
| **lib 内联单测** | — | **风格差异（非缺口）** | `src/` 内 `#[cfg(test)]`/`mod tests` 命中 0 处（仅 `s3` feature 下 `src/s3.rs` 有 4 个内联用例）。**history/OSS__MIGRATION_HISTORY.md 结论并维持**：不构成独立阻塞项；P1-2a 要求的**分页构造器**已于 history/OSS__MIGRATION_HISTORY.md-2 补齐 |

## 3.5 2026-10-31 全量对齐复核清单（唯一编号）

> 复核基线：Java `main` @ `1524b3191`；Rust 工作树 HEAD 见仓库。本表为本轮逐文件核对结论，**存量 §3.1–§3.2 历史清单保持原样未重新编号**。
> 编号规则：`OSS-NN`（缺口，状态列取值 `待办` / `进行中` / `已完成` / `设计决定（不迁移）` / `待裁定`）；§4.3 差异另有 `OSS-D<NN>` / `OSS-X<NN>` 系列。编号本模块内唯一、单调递增，不复用历史编号。
> **2026-10-31 裁定（汇总方已确认）**：本文件的 `待裁定` 项已全部裁定，状态列更新为 `待办` / `设计决定（不迁移）` / `已完成`；裁定结论、依据与执行批次见 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` §3.3。
> 正文其它位置出现的旧「待裁定 N 项」为复核时点快照，一律以 §3.5/§3.6 表格的状态列为准。
> 测试口径：**既有实测 64 passed（默认，+1 doc-test）/ 75 passed（`--features s3`，+2 doc-test）**；本轮**未运行** cargo，静态计数为默认 76 / `--features s3` **90**（`tests/strict_errors.rs` 12 例 + `tests/async_bridge.rs` 3 例晚于上述实测口径）。

### 3.5.1 缺口与待裁定

| 编号 | 优先级 | 缺口项（可执行描述） | Java 参考（文件#符号） | Rust 现状（文件:行 / rg 零命中） | 执行状态 | 下一动作 |
|---|---|---|---|---|---|---|
| OSS-01 | P0 | `S3OssClient` 用私有运行时 `block_on` 驱动同步 `OssClient`，而 `AgentStateStore` 的方法全是 `async fn` 且内部直接调用 `client.put/get/list`：宿主在 tokio 线程里 `.await` 存储方法即触发 `Cannot start a runtime from within a runtime` panic（s3 后端生产阻断） | `OssAgentStateStore.java#save`/`#get`（Java 同步 + OSS SDK 同步，无运行时嵌套问题） | `src/s3.rs:101`（`self.runtime.block_on(future)`）、`src/state_store.rs:106-121,284-286`（async fn 内同步 `client.put`）；`rg 'spawn_blocking|block_in_place' -g '*.rs'` 仅命中 `src/lib.rs:72`、`src/s3.rs:15` 两处**文档注释**，零调用点；`tests/s3.rs` 全部用同步 `#[test]` 绕开，组合路径无覆盖 | 已完成 | 引入执行器 seam：`S3OssClient` 改为「专有工作线程 + oneshot」或让 store 层统一 `tokio::task::spawn_blocking` 包裹 `OssClient` 调用；验收：新增 `tests/async_bridge.rs`，在 `#[tokio::test]` 内对 httpmock 执行 `OssAgentStateStore::save/get` 与 `OssStore::put/search` 不 panic 且数据往返正确 【2026-10-31 落地：采用「专有工作线程 + channel」——`S3Worker` 在自建线程内部建立 Tokio 运行时，`run()` 把 `Arc<Bucket>` 交给该线程创建并 `block_on` future，结果经 mpsc 回传（`Sender` 用 `Mutex` 包裹，因为 `mpsc::Sender` 是 `!Sync` 而 `OssClient: Send + Sync`）；调用方线程永不进入运行时（current-thread 与 multi-thread 均安全，也不再有 `Runtime::drop` 嵌套问题）；新增 `tests/async_bridge.rs` 3 例：`state_store_save_and_get_from_a_tokio_runtime_do_not_panic`、`base_store_put_from_a_tokio_runtime_does_not_panic`、`base_store_search_from_a_multi_thread_runtime_do_not_panic`（src/s3.rs / tests/async_bridge.rs）】 |
| OSS-02 | P1 | `BaseStore::put_if_version` 是「读 version → 比较 → 写」的非原子序列，且 `supports_atomic_session_storage()` 未覆写（默认 false）→ OSS 后端无法承载 session journal，并发 CAS 双方都可能成功（静默丢更新） | `OssBaseStore.java#putIfVersion`（同样的 read-check-write；亦未覆写 `BaseStore#supportsAtomicSessionStorage`，`BaseStore.java:29` 为默认 false） | `src/base_store.rs:147-170`（`read_version_strict` 后用普通 `put`）、`rg 'supports_atomic' crates/agentscope-extensions-oss` 零命中（其它存储 crate 均有覆写）；harness 默认 `filesystem/remote/store.rs:67`；`agent/session/store.rs:44` 直接拒绝非原子存储 | 待办 | 给 `OssClient` 增 `get_with_etag` + `put_conditional(if_match / if_none_match)`：CAS 用 S3 `If-Match: <etag>`/`If-None-Match: *`，`412` 映射为 typed 冲突（现状落入 `OssError::S3Http`，`src/s3.rs:336-346`），真 CAS 后覆写 `supports_atomic_session_storage() -> true`；验收：并发两 writer 仅一个返回 true + 可作 `WorkspaceSessionLogStore` 后端 |
| OSS-03 | P1 | state store 无版本化 CAS：`supports_versioning()` 保持 false，harness state checkpoint 走「非版本化 → 无条件写」分支，多写者互相覆盖（`ConflictPolicy` 永不生效） | `OssAgentStateStore.java`（同样未覆写 versioned API；Java 侧亦无 ETag 条件写） | `src/state_store.rs:414-419`（注释声明保持 core 默认）；`tests/strict_errors.rs:169`（断言 `supports_versioning()==false` 且 `save_if_version` 返回 `UNVERSIONED`）；harness 退化点 `agent/middleware/state_checkpoint.rs:143-146` | 待办 | 依赖 OSS-02 的条件写 seam：实现 `get_versioned`（版本取自对象 ETag/代次）与 `save_if_version`（If-Match），冲突返回 `UNVERSIONED` 且不落盘；验收：两并发 `save_if_version` 一成功一冲突，`supports_versioning()==true` |
| OSS-04 | P1 | 无请求超时、无重试/退避：`rust-s3` 无内建重试，同步调用无 deadline → 单次网络 hang 会永久占住调用线程（叠加 OSS-01 的 `block_on`）；5xx/429 首次失败即抛错 | Java 走 OSS SDK 默认 `ClientConfiguration`（`maxErrorRetry=3` + 连接/读超时） | `rg 'timeout|retry|backoff' -g '*.rs'` 在本 crate 仅 `src/error.rs:43` 注释命中，`src/s3.rs` 无任何设置 | 待办 | `S3OssClientBuilder` 增 `timeout(Duration)`、`max_retries`、指数退避 + 抖动；`OssError::is_retryable()`（5xx/429/超时）；验收：httpmock 连续 500 两次后成功、超时返回 typed 错误（不 panic、不无限等待） |
| OSS-05 | P2 | 会话删除逐 key DELETE：`delete_session_strict` 对 `list_all` 结果线性删除，N 个对象 = N 次请求，无批量删除 | `OssAgentStateStore.java#deleteKeys`（1000/批 `DeleteObjectsRequest#withQuiet(true)`） | `src/state_store.rs:196-204`（`for key in self.client.list_all(&prefix)? { self.client.delete(&key)? }`）；`OssClient` 无 `delete_many`（`src/client.rs:45-95` 全方法清单） | 待办 | `OssClient` 增 `delete_many(&[String])`（S3 `POST /{bucket}/?delete`），state store 按 1000 分批；验收：httpmock 断言 N 个 key 只发 1 次批量请求、缺 key 仍幂等 |
| OSS-06 | P2 | `search` 每翻一页都枚举 namespace 下**全部** key 再内存排序切页：大 namespace 下 O(n) 列表 + 全量内存驻留，翻页越深越慢 | `OssBaseStore.java#search`（同一实现：listAll + `Collections.sort` + subList） | `src/base_store.rs:191-240`（`list_all` → `sort` → 切片）、`src/client.rs:73-92`（`list_all` 循环翻页） | 待办 | 直接用服务端有序翻页：请求 `max_keys = offset + limit` 或 `start_after`/continuation token，避免全量列举；验收：`search(limit=10, offset=0)` 只发 1 次 list 且 `max-keys` 有界（httpmock 断言） |
| OSS-07 | P2 | 大对象无 multipart 上传：单次 PUT 受 5 GiB 限制，超大 sandbox 快照/文件会直接失败；`put` 也无 `Content-Type`/`Content-MD5` 校验 | `OssRemoteSnapshotClient.java#upload`（单次 `putObject`，OSS 同为 5 GB 上限——Java 侧亦未分片） | `src/snapshot.rs:61-66`（`client.put(&key, data)` 单次）、`src/s3.rs:350-352`（`bucket.put_object`） | 待办 | `OssClient` 增可选 `put_multipart(key, data, part_size)`（CreateMultipartUpload/UploadPart/Complete）；验收：httpmock 断言分片序列，<5 GiB 路径行为不变 |
| OSS-08 | P2 | S3 后端与 store 层「组合」零覆盖：`tests/s3.rs` 只测 `S3OssClient` 五种方法，`OssAgentStateStore`/`OssStore` + `S3OssClient` 从未一起被驱动（OSS-01 正因此漏网） | `OssAgentStateStoreTest.java` / `OssBaseStoreTest.java`（用 mock `OSS` 直接覆盖 store 层，各 10 `@Test`） | `rg 'S3OssClient' crates/agentscope-extensions-oss` 仅 `src/s3.rs`、`tests/s3.rs`、`src/lib.rs` 文档；`tests/s3.rs` 用同步 `#[test]` | 待办 | 新增 `tests/s3_store.rs`（httpmock）：state 单值/列表读写、`put_if_version` CAS、`search` 分页、snapshot 上传下载；验收：≥6 例、含至少 2 例 `#[tokio::test]`（与 OSS-01 联动） 【2026-10-31：OSS-01 的 `tests/async_bridge.rs` 已覆盖 S3 后端上的 state `save/get` 与 base `put`/`search` 组合路径（3 例 `#[tokio::test]`），`put_if_version` CAS / snapshot / 分页仍待补】 |
| OSS-09 | P1 | MSRV 冲突：本 crate `rust-version = "1.85"`（`rust-s3 0.38` 为 edition 2024），workspace 为 `1.80.0` —— 是否提升 workspace MSRV，或改用 `object_store`/`opendal` 后端 | `pom.xml`（Java 21；无对应约束） | `crates/agentscope-extensions-oss/Cargo.toml:7-9`（注释已说明原因）；根 `Cargo.toml` `rust-version = "1.80.0"` | 待办 | 汇总方裁定：提升 workspace MSRV 至 1.85（影响全仓 CI 镜像）或替换后端依赖；裁定前保持现状 【裁定 2026-10-31：提升 workspace MSRV 至 1.85（rust-s3 0.38 需 edition 2024）】 【2026-10-31：需改根 `Cargo.toml` 的 `rust-version`，超出本 crate 可改范围，由主智能体处理】 |
| OSS-10 | P2 | 是否需要阿里云 OSS **原生**能力（对象标签、SSE-KMS、OSS-HDFS、图片处理、`x-oss-*` 头）：当前唯一路径是 S3 兼容端点，无法覆盖这些特性 | `pom.xml#com.aliyun.oss:aliyun-sdk-oss:3.18.5`（原生 `OSS` 客户端） | `crates/agentscope-extensions-oss/Cargo.toml`（依赖仅 `rust-s3`，feature 仅 `s3`）；`rg 'aliyun' -g '*.rs' src` 零命中 | 设计决定（不迁移） | 若宿主确需 OSS 原生特性，评估 `aliyun-oss-rust-sdk`/`opendal` 并新增第二后端（保持 `OssClient` trait 不变）；否则在 `lib.rs` 能力边界处明示「仅 S3 兼容子集」 【裁定 2026-10-31：仅 S3 兼容子集；原生特性将来走 opendal 第二后端（可选 feature）】 【2026-10-31：设计决定项，由主智能体统一文档收口，本子智能体未改】 |

### 3.5.2 本轮复核确认已对齐（抽样，不求穷举）

| 编号 | 能力 | Java 参考 | Rust 证据（文件:行 + 测试名） | 执行状态 |
|---|---|---|---|---|
| OSS-11 | 对象 key 布局三套：state `{key}.json`/`{key}.list.json`/`{key}.list.hash`、BaseStore `{ns...}/{key}.json`+`.version`、snapshot `{prefix}{id}.tar` | `OssAgentStateStore.java#stateObjectKey`、`OssBaseStore.java#dataObjectKey`、`OssRemoteSnapshotClient.java#objectKey` | `src/state_store.rs:31-34,242-270`；`src/base_store.rs:36-38,260-277`；`src/snapshot.rs:56-59`；测试 `save_single_state_uses_expected_object_key`、`put_writes_data_and_version` | 已完成 |
| OSS-12 | 前缀归一（`\`→`/`、去前导 `/`、补尾 `/`、空则回落默认）与 builder 校验（client 必填、bucket 非空白） | `OssAgentStateStore.java#normalizePrefix`、`OssBaseStore.java#normalizePrefix` | `src/client.rs:246-268`；`src/state_store.rs:433-465`；`src/base_store.rs:311-345`；测试 `key_prefix_is_normalized_with_trailing_slash`、`builder_rejects_missing_client`、`builder_rejects_blank_bucket` | 已完成 |
| OSS-13 | 空 user → `__anon__`；空 sessionId 拒绝；`list_session_ids` 按首段切分并跳过空段 | `OssAgentStateStore.java#normalizeUser`/`#sessionPrefix`/`#listSessionIds` | `src/state_store.rs:232-240,296-304,206-231`；测试 `null_user_uses_anon`、`delete_key_rejects_blank_session_id`、`list_session_ids_extracts_ids` | 已完成 |
| OSS-14 | 列表增量写检测走 **core `list_hash`**，无平行实现 | `OssAgentStateStore.java#save`(List) + `ListHashUtil` | `src/state_store.rs:29`（`use agentscope_core::state::{compute_hash, needs_full_rewrite}`）、`:133`、`:147`；`rg 'fn compute_hash|fn needs_full_rewrite' src` 零命中；测试 `save_list_append_rewrites_when_count_grows` | 已完成 |
| OSS-15 | continuation token 分页三层贯通：`OssClient::list_all` 跟 token、`InMemoryOssClient` 页帽模拟、`S3OssClient` 透传 `max-keys`+`continuation-token` | `OssAgentStateStore.java#listAllKeys`（`setContinuationToken` 循环） | `src/client.rs:62-92,177-217`；`src/s3.rs:397-424`（`max_keys.min(MAX_KEYS_PER_PAGE)`）；测试 `tests/pagination.rs`（13 例）与 `list_page_uses_max_keys_and_continuation_token` | 已完成 |
| OSS-16 | 缺失语义：get→`None`、exists→`false`、delete 幂等（404/`NoSuchKey` 均视为成功） | `OssAgentStateStore.java#getString`（`doesObjectExist` 先行） | `src/s3.rs:326-332`（`is_not_found`）、`:355-395`（get/exists/delete）；测试 `get_missing_object_returns_none`、`exists_uses_head_and_reports_presence`、`delete_is_idempotent` | 已完成 |
| OSS-17 | put 覆盖写语义 + `.version` 递增 + `put_if_version(0)` 创建语义；version 缺失读作 0 | `OssBaseStore.java#put`/`#putIfVersion`/`#readVersion` | `src/base_store.rs:130-170,280-291`；测试 `put_writes_data_and_version`、`put_if_version_returns_true_on_match`、`put_if_version_returns_false_on_mismatch`、`put_if_version_zero_creates_only_when_absent` | 已完成 |
| OSS-18 | 错误可观测性：每个 trait 方法配 `*_strict` 双胞胎，trait 层降级为「空值 + `tracing::error!`」且**绝不伪造数据**（对齐 Java `RuntimeException` 的可捕获性） | `OssAgentStateStore.java`（全方法 `RuntimeException`）、`OssBaseStore.java`（同） | `src/state_store.rs:104-223`（save/save_list/get/get_list/exists/delete/list_session_ids 的 strict 变体）、`src/base_store.rs:118-240`；测试 `tests/strict_errors.rs`（12 例，含 `state_trait_methods_degrade_to_empty_without_fabricating_data`） | 已完成 |
| OSS-19 | 快照读写：`.tar` 布局、缺失与后端失败区分（`OssError::SnapshotNotFound` vs `Backend`）、复用 harness `RemoteSnapshotSpec` 做 id 校验与 `RemoteSandboxSnapshot` 接线 | `OssRemoteSnapshotClient.java#upload`/`#download`、`OssSnapshotSpec.java` | `src/snapshot.rs:61-96,105-140`；测试 `tests/snapshot.rs`（7 例）、`snapshot_download_distinguishes_missing_from_backend_failure` | 已完成 |
| OSS-20 | 组合者：`state/`/`store/`/`snapshot/` 子前缀 + `distributed_store()` 填满 harness 四槽（state/base/snapshot/guard） | `OssDistributedStore.java#agentStateStore`/`#baseStore`/`#sandboxSnapshotSpec` | `src/distributed_store.rs:79-137`；`src/guard.rs:70-96`（`SandboxGuard` 委托、`is_distributed()` 保持 false）；测试 `tests/guard_and_distributed.rs`（7 例，含 `distributed_store_fills_every_composite_slot`） | 已完成 |

## 4. 项目状态与下一步
### 4.1 当前状态
- **已完成**：`agentscope-extensions-oss` 全量实现，产出契约 4 类 trait 实现 + 组合者 + 后端抽象；history/OSS__MIGRATION_HISTORY.md（2026-10-26）关闭 P1-1/P1-2/P1-3/P1-7；2026-10-28 关闭 P1-4（`distributed_store()` 填满 5 槽 + `impl SandboxGuard`，见文末执行记录）。
- **验证命令与结果**（原会话记录；2026-10-31 主智能体实测：默认 **64 passed / 0 failed** + 1 doc-test，`--features s3` **75 passed / 0 failed** + 2 doc-test）：
  - `cargo build -p agentscope-extensions-oss` → Finished（本 crate 无 error/warning）
  - `cargo test -p agentscope-extensions-oss`（基线，history/OSS__MIGRATION_HISTORY.md 前）→ **42 passed / 0 failed**
    - `agent_state_store` 15 ✓ / `base_store` 13 ✓ / `guard_and_distributed` 6 ✓ / `snapshot` 7 ✓ / doc-test 1 ✓
  - **用例（2026-10-29 静态计数）**：默认 feature **64 test + 1 doc-test**（`agent_state_store` 18 / `base_store` 17 / `pagination` 13 / `guard_and_distributed` 7 / `snapshot` 7 / `src/guard.rs` 内联 2）；`--features s3` **75 test + 2 doc-test**（再 + 内联 `s3` 4 + `tests/s3.rs` 7）。执行记录：2026-10-28 默认 62 passed、`--features s3` 71 passed
- **文件清单**（实测行数）：
  - `crates/agentscope-extensions-oss/Cargo.toml`（feature 已改为 `s3`，见 history/OSS__MIGRATION_HISTORY.md）
  - `src/lib.rs`、`src/error.rs`、`src/client.rs`、`src/state_store.rs`、`src/base_store.rs`、`src/snapshot.rs`、`src/guard.rs`、`src/distributed_store.rs`、`src/s3.rs`（feature `s3`）
  - `tests/common/mod.rs`、`tests/agent_state_store.rs`、`tests/base_store.rs`、`tests/snapshot.rs`、`tests/guard_and_distributed.rs`、`tests/s3.rs`（feature `s3`）、**`tests/pagination.rs`（history/OSS__MIGRATION_HISTORY.md-2 新增）**、**`tests/async_bridge.rs`（OSS-01 新增；3 例 `#[tokio::test]`，`#![cfg(feature = "s3")]`）**
- **MSRV（2026-10-29 裁定）**：`rust-s3 0.38.0` 声明 `edition = "2024"`（需 rustc ≥ 1.85），故本 crate 保留本地 `rust-version = "1.85"`（`Cargo.toml` 已注明原因），**不提升 workspace 的 `rust-version = "1.80.0"`**；已在 rustc 1.96.1 上验证通过。
  - ~~实机 OSS 后端未接线（P0-1）~~ → 已由裁定 8（`s3` + `S3OssClient`，history/OSS__MIGRATION_HISTORY.md）替代实现；`lib.rs` 文档已于 history/OSS__MIGRATION_HISTORY.md-4 更正。
  - ~~缺 per-key `delete`（P1-1）~~ → history/OSS__MIGRATION_HISTORY.md-1 已实现 `OssAgentStateStore::delete_key`。
  - ~~`list_page` 分页/续传分支零覆盖、无分页构造器（P1-2）~~ → history/OSS__MIGRATION_HISTORY.md-2 已补构造器 + 13 用例。
  - ~~`search` prefix 不匹配静默返回错值（P1-3）~~ → history/OSS__MIGRATION_HISTORY.md-3 已改为 `search_strict` 返回 `OssError`。
  - ~~`futures`/`chrono`/`serde`/`tokio`（`[dependencies]` 中的）零引用；`OssError::SnapshotNotFound`/`Backend` 未被构造（P2-1/P2-3）~~ → **已关闭（2026-10-31，见 history/OSS__MIGRATION_HISTORY.md）**：`serde`/`futures`/`chrono` 删除、`tokio` 改 optional 且仅由 `s3` feature 引入；两个错误变体均已有构造点。
  - Java `OssBaseStore.putIfVersion` 用 `long`，Java `OssAgentStateStore` 另有 `supportsVersioning`/`getVersioned`/`saveIfVersion` 版本化 API；Rust 侧 `OssAgentStateStore` **未覆写** `supports_versioning`/`get_versioned`/`save_if_version`（默认 `false`/降级为无条件写），与 Java 一致，但意味着 OSS 状态存储**无乐观并发**（P2-7）。

### 4.2 下一步与阻塞（需汇总方裁定）
1. **harness `DistributedStore` 字段不足**（与 EXTENSIONS_GAP_TRACKER §2.2 同项）——**已解决（2026-10-28）**：harness `DistributedStore` 已增 `base_store` / `sandbox_snapshot_spec` / `sandbox_execution_guard` 三槽 + `SandboxGuard` trait；`OssDistributedStore::distributed_store()` 已用 builder 填满 5 槽（`OssStore` / `OssSnapshotSpec` / `OssSandboxExecutionGuard`），且 `OssSandboxExecutionGuard` 已 `impl SandboxGuard`。详见文末执行记录。
2. **实机阿里云后端** → **已完成（2026-10-29 勘误）**：原记「`aliyun-oss-rust-sdk` 为 optional（feature `aliyun`），默认关闭」「`cfg(feature = aliyun)` 零命中、实机后端代码根本不存在」「建议二选一：实现 `AliyunOssClient` 或改用 `opendal` / `object_store` 重写后端 trait」。实际：裁定 8（2026-10-22，history/OSS__MIGRATION_HISTORY.md）已删除空壳 feature，改为通用 `s3` feature + `S3OssClient`（`src/s3.rs`，`impl OssClient` 全方法；`tests/s3.rs` 7 例 + 内联 4 例，`httpmock` 离线端到端），`lib.rs` 误导性文档已在 history/OSS__MIGRATION_HISTORY.md-4 定稿。**真实剩余**：①无阿里云 OSS 原生 SDK 路径（走 S3 兼容端点，P2）；②`rust-s3` 0.38 需 rustc ≥ 1.85 而 workspace `rust-version = 1.80.0`（MSRV 冲突，待汇总方裁定）；③无真实云凭据 live 验证（见下条）。
3. **未改根 `[workspace.dependencies]`**：本 crate 的 `rust-s3` 0.38（optional，feature `s3`）与 dev-dep `httpmock` 0.7 为本地声明，未登记到根；如需统一版本请汇总方处理。
4. **实机 OSS live 验证待凭据**：端到端需真实 OSS endpoint/AK/SK（同类于 MIGRATION_GAP_TRACKER history/OSS__MIGRATION_HISTORY.md）。

### 4.3 与 Java 的已知差异（有意为之，非缺口）

> 2026-10-31 复核归并：本节为 OSS 模块全部「不迁移 / 行为不同」项的集中清单，与 §3.5 缺口编号互不重叠。

#### 4.3.1 设计决定（不迁移）

| 编号 | 项 | Java 参考 | 不迁移理由（源码级依据） | 影响面 |
|---|---|---|---|---|
| OSS-D01 | 预签名 URL / V4 分片签名策略 | `OssAgentStateStore.java`、`OssBaseStore.java`、`OssRemoteSnapshotClient.java`（**均无**预签名调用：`rg 'presign|Presign|generatePresigned' src/main/java` 零命中；签名由 OSS SDK 内部完成） | 本模块的签名是 SDK 内部细节，业务侧从不生成 URL；Rust 侧由 `rust-s3` 内部完成 SigV4，store 层不暴露 presign，避免为无调用方的能力臆造 API（如宿主需要，应显式加 `presign_get` 并定义过期语义，另立条目） | 无（当前无调用方） |
| OSS-D02 | `OssSnapshotSpec(endpoint, ak, sk, bucket, prefix)` 便捷构造器 | `OssSnapshotSpec.java` 第二个构造器（内部 `new OSSClientBuilder().build(...)`） | Rust 不托管 SDK 客户端生命周期：凭据/endpoint 由 `S3OssClient::builder()` 构造出 `Arc<dyn OssClient>` 后注入（`src/snapshot.rs:105-118`）。在 spec 里再建客户端会导致同一 bucket 两份连接池/运行时 | 宿主需两段式构造：先建 client，再 `OssSnapshotSpec::new` |
| OSS-D03 | 阿里云原生 SDK（`com.aliyun.oss:aliyun-sdk-oss:3.18.5`） | `pom.xml#com.aliyun.oss:aliyun-sdk-oss` | 已有裁定 8：改走通用 S3 协议（`Cargo.toml` 依赖仅 `rust-s3`，feature 仅 `s3`；`rg 'aliyun' -g '*.rs' src` 零命中），可同时覆盖 AWS S3/MinIO/Ceph/R2/OSS-S3；OSS 原生特性需求见 §3.5 的 OSS-10 待裁定 | 对象标签/SSE-KMS/OSS-HDFS/图片处理不可用 |
| OSS-D04 | `InputStream` 流式快照上传/下载 | `OssRemoteSnapshotClient.java#upload`（`InputStream`）/`#download`（返回 `InputStream`） | harness 契约本身是字节切片：`RemoteSnapshotClient::upload(&self, id: &str, data: &[u8])`、`download -> Vec<u8>`（`src/snapshot.rs:61,68`），无流式通道可接；大对象内存峰值问题由 §3.5 的 OSS-07（multipart）缓解 | 单快照需整块驻留内存 |

#### 4.3.2 设计决定与已知差异

| 编号 | 项 | Rust 实现 | 与 Java 的差异 | 为何保留（收益/依据） |
|---|---|---|---|---|
| OSS-X01 | 后端 seam | `OssClient` trait（`src/client.rs:45-95`）+ `InMemoryOssClient` + `s3` feature 的 `S3OssClient` | Java 直接持有 `com.aliyun.oss.OSS`，测试靠 Mockito mock | 后端可替换、默认构建零 HTTP 依赖、全部单测离线可跑（`tests/common/mod.rs`）；也是 OSS-02/03 条件写扩展的落点 |
| OSS-X02 | 错误通道 | 每个 trait 方法配 `*_strict`（`save_strict`/`search_strict`/…）返回 `OssResult<T>`，trait 方法委托并在失败时 `tracing::error!` 后返回空值 | Java 一律 `throw new RuntimeException(...)`，store 层直接中断调用方 | core/harness 的 `save`/`get`/`search` 签名无 `Result`，无法抛错；strict 变体保留「后端故障 ≠ 无数据」的区分（`tests/strict_errors.rs:80,229`） |
| OSS-X03 | `put_if_version(0)` 语义 | `expected_version == 0` 时用 `client.exists(data_key)` 判定，仅在数据对象不存在时写（`src/base_store.rs:161-166`） | Java 只比较 `readVersion == 0`：当数据对象存在但 `.version` sidecar 丢失时会**覆盖**既有数据 | 对齐 harness `BaseStore` 契约「0 = write only if the key does not yet exist」（`filesystem/remote/store.rs:45-49`），避免幽灵写 |
| OSS-X04 | 列表增量写判定的 `existing_count` | 恒从 `.list.json` 读取长度，与 hash 是否存在解耦（`src/state_store.rs:138-146`） | Java 仅当 `storedHash != null` 才读旧列表，故 `existingCount` 恒为 0 | 修复「hash 丢失 + 列表缩短/清空」永不重写、残留旧列表对象的缺陷；用例 `state_save_list_rewrites_when_the_hash_sidecar_is_missing`（`tests/strict_errors.rs:139`） |
| OSS-X05 | 非法 `.version` 内容 | `read_version_strict` 解析失败 → `OssError::Backend`（`src/base_store.rs:280-291`），`get` 的 strict 变体暴露该错误 | Java `Long.parseLong` 失败被 `catch (NumberFormatException)` **静默吞成 0**，随后可能把版本写回 1 | 静默归零会造成版本回退与 CAS 假命中；显式报错让运维发现被篡改/损坏的 sidecar（用例 `unparsable_version_object_is_a_backend_error_in_strict_mode`） |
| OSS-X06 | `search` 中越界 key / 坏 JSON | 显式 `OssError::Backend`（前缀不匹配）或 `OssError::Json`（`src/base_store.rs:191-240`）；trait 层返回空页 + 日志 | Java 用 `key.substring(prefix.length(), ...)`，越界抛 `StringIndexOutOfBoundsException`（不可读），坏 JSON 抛 Jackson 包装异常 | 错误可分类、可断言；绝不返回用错误 key 拼出的 `StoreItem`（用例 `search_strict_errors_when_listed_key_is_outside_namespace_prefix`） |
| OSS-X07 | 快照 id 校验 | 拒绝空/`/`/`\`/`..`/NUL（`src/client.rs:232-243`），经 `object_key` 前置校验 | Java 仅 `snapshotId == null || isBlank()` 抛 `IllegalArgumentException` | 防对象 key 注入（`../` 逃逸到其它前缀）；与 harness 内置后端 `validate_snapshot_id` 行为一致 |
| OSS-X08 | `get` 请求次数/竞态 | 单次读取，缺失即 `None`（`src/client.rs:50-51`、`src/s3.rs:355-370`） | Java 先 `doesObjectExist` 再 `getObject`（两次请求），期间被删会抛 `RuntimeException` 而非「不存在」 | 少一次往返 + 消除 TOCTOU；语义更接近「不存在」 |
| OSS-X09 | `list_session_ids` 顺序 | `BTreeSet` 去重后返回字典序（`src/state_store.rs:206-231`） | Java `HashSet` 无序 | 结果可断言、方便上层做稳定 diff/分页；集合语义不变 |
| OSS-X10 | sandbox 执行 guard | `OssSandboxExecutionGuard` 透明包装 harness `SandboxExecutionGuard`，并显式 `is_distributed() == false`（`src/guard.rs:70-96`） | Java OSS 模块**没有** guard 类，也不覆写 `DistributedStore#sandboxExecutionGuard()` → 回落到 `SandboxExecutionGuard.noop()` | 遵守「不新建平行类型」裁定（EXTENSIONS_GAP_TRACKER §2.1），同时让宿主的 `DistributedStore` 四槽齐备；`is_distributed()==false` 明示 OSS 无服务端锁，禁止跨进程假设 |
| OSS-X11 | `close()` 生命周期 | `OssAgentStateStore::close()` 只调 `client.shutdown()`（`src/state_store.rs:67-69`），客户端由宿主以 `Arc<dyn OssClient>` 共享 | Java 每个 store 独占 `OSS` 实例并在 `close()` 里 `shutdown()` | 一个进程/bucket 只需一份连接池；副作用：共享 client 时关闭会影响同 client 的其它 store（宿主负责生命周期，`OssDistributedStore` 不主动 close） |
| OSS-X12 | per-key 删除的请求数 | 直接 3 次 `DELETE`（幂等，`src/state_store.rs:83-99`） | Java 每个后缀先 `doesObjectExist` 再 `deleteObject`（最多 6 次请求） | S3/OSS 的 DELETE 对缺失对象返回成功或 404，均视为成功，无需预检；请求数减半 |

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-oss` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-oss` / `cargo test -p agentscope-extensions-oss` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-oss/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. 执行历史（已归档）

- 执行记录 / 静态复核 / 勘误 / 反向验证明细：[`history/OSS__MIGRATION_HISTORY.md`](./history/OSS__MIGRATION_HISTORY.md)
- 本文件只保留现行状态（§0–§5）；历史条目一律写进上述归档文件，不再回填本文。
