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
| Rust 规模（2026-10-22 实测；**§8 后用例：默认 61 + 1 doc-test / `--features s3` 72 + 2 doc-test，待实测**） | `src/` **8 文件 1210 行 / 非空 1064 行**（base_store 265、client 224、distributed_store 131、error 53、guard 65、lib 52、snapshot 128、state_store 292）；`tests/` **5 文件 525 行 / 非空 446 行**；用例 **41 个 `#[test]`/`#[tokio::test]` + 1 个 `no_run` doc-test = 42**，`src/` 内 `cfg(test)` 命中 **0** |
| 迁移波次 | B |
| 当前状态 | **进行中（P1 已全部关闭，见 §8；§6/§7 为历史快照）**。契约 5/5 齐全，默认 feature **61 test + 1 doc-test** / `--features s3` **72 test + 2 doc-test**（待主智能体跑 cargo 确认）；仍存的结构性限制只有「无法产出完整 `DistributedStore`」（受 harness struct 约束，§4.2-1） |

## 2. Java 清单与依赖映射
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `OssAgentStateStore` | OSS 后端的 `core.state.AgentStateStore`；状态以 JSON 存于 `{prefix}{user}/{session}/{key}.json`，列表另有 `.list.json` + `.list.hash` | `OssAgentStateStore`（`state_store.rs`） | 实现 `agentscope_core::state::store::AgentStateStore`（async_trait）；复用 `core::state::list_hash` 的 `compute_hash`/`needs_full_rewrite` 做增量 append 检测。**已对齐（§8-1）**：per-key `delete_key(userId, sessionId, key)` 作为 **crate 固有方法**实现（core trait 无此方法；照 MONGODB `delete_key` 先例，不改 core/harness），删除 `.json`/`.list.json`/`.list.hash` 三个对象 |
| `OssBaseStore` | OSS 后端的 harness `BaseStore`；`{prefix}{ns...}/{key}.json` + `.version` | `OssStore`（`base_store.rs`） | 实现 `agentscope_harness::filesystem::remote::store::BaseStore`（同步）；CAS/版本语义以 harness 为准（version 从 1 起、`put_if_version(0)`=仅当不存在时写）。`supports_atomic_session_storage` 两侧均未覆写 → 同为 `false`（行为等价） |
| `OssRemoteSnapshotClient` | OSS 后端的 `RemoteSnapshotClient`；`{prefix}{snapshotId}.tar` | `OssRemoteSnapshotClient`（`snapshot.rs`） | 实现 `agentscope_harness::agent::sandbox::snapshot::RemoteSnapshotClient`（async_trait）；入参由 Java `InputStream` 改为 `&[u8]` |
| `OssSnapshotSpec` | 便捷 `SandboxSnapshotSpec` | `OssSnapshotSpec`（`snapshot.rs`） | 实现 `SandboxSnapshotSpec`，内部包装 harness `RemoteSnapshotSpec`（id 校验/`RemoteSandboxSnapshot` 接线复用）。**差异**：Java 另有 `(endpoint, accessKeyId, accessKeySecret, bucket, prefix)` 便捷构造器（内部 `OSSClientBuilder`）；Rust 无 SDK 故无此构造器（P2） |
| `OssDistributedStore` | 组合者：state store + base store + snapshot spec（+ guard） | `OssDistributedStore`（`distributed_store.rs`） | 共享同一 `OssClient`，按 `state/`/`store/`/`snapshot/` 子前缀组合；另暴露 `distributed_store()` 产出 harness `DistributedStore`（字段不足见 §4.2） |
| (harness `SandboxExecutionGuard`) | **Java OSS 模块不定义自己的 guard**：`OssDistributedStore` 未覆写 `sandboxExecutionGuard()` → 落回 `DistributedStore` 默认 `SandboxExecutionGuard.noop()` | `OssSandboxExecutionGuard`（`guard.rs`） | **不新建平行类型**（遵守 EXTENSIONS_GAP_TRACKER §2.1）：透明包装 harness `SandboxExecutionGuard`，语义完全委托。**注**：这是 Rust 侧比 Java 多出的便利类型（Java 无 `Oss*Guard`），不属于「对齐缺口」 |
| `com.aliyun.oss.OSS` | 阿里云 OSS SDK 客户端 | `OssClient` trait（`client.rs`）+ `InMemoryOssClient` + （`s3` feature）`S3OssClient` | 后端抽象（put/get/exists/delete/list_page/list_all/shutdown）。**已于裁定 8 改为通用 `s3` feature（§7）**；`lib.rs` 文档已在 §8-4 更正为「feature 只负责**编译**客户端、绝不自动选后端；默认只有 `InMemoryOssClient`；需显式配置 bucket/region/endpoint/凭证」。**仍缺**：阿里云 OSS 原生 SDK 路径（走 S3 兼容端点代替，P2） |
| `JsonUtils` / Jackson | JSON 编解码 | `serde_json` | 直接映射 |
| `ListHashUtil` | 列表变更检测 | `agentscope_core::state::list_hash` | 直接复用，无平行实现 |

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| `com.aliyun.oss:aliyun-sdk-oss:3.18.5` | 阿里云 OSS 对象读写 | `aliyun-oss-rust-sdk`（optional，feature `aliyun`） | 通过 `OssClient` trait 隔离；默认不编译（避免强拉未 CU 验证的 SDK）。实机后端待接入 |
| Jackson (`JsonUtils`) | JSON | `serde` / `serde_json`（workspace） | 已用 |
| `agentscope-core` (provided) | AgentStateStore/ListHash | `agentscope-core`（path 依赖） | 实现其 trait，不重建 |
| `agentscope-harness` (provided) | BaseStore/RemoteSnapshotClient/SandboxSnapshotSpec/Guard | `agentscope-harness`（path 依赖） | 实现其 trait，不重建 |

## 3. 详细缺口清单
### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 骨架 + Cargo 依赖 | pom.xml | 已完成 | 依赖仅写入本 crate `Cargo.toml`；未改根 `[workspace.dependencies]`，未改 core/harness。**冗余依赖**：`futures`、`chrono` 在 `src/`+`tests/` 中零引用（`chrono` 仅 3 处注释误命中），建议清理（P2-1） |
| `OssClient` 后端抽象 + `InMemoryOssClient` | `OSS` (SDK) | 已完成 | `client.rs`；put/get/exists/delete/list_page/list_all/shutdown；key 前缀归一 + snapshotId 校验辅助。**分页已补齐（§8-2）**：新增 `InMemoryOssClient::with_page_size(n)` / `page_size()`，`list_page` 受页帽约束（`max_keys=0` → 空页且 `truncated=false`）；`tests/pagination.rs` 13 用例覆盖首页/续传/最后一页/精确边界/空结果/`list_all` 多页聚合 |
| `OssAgentStateStore` | `OssAgentStateStore` | 已完成 | 实现 `AgentStateStore`（save/save_list/get/get_list/exists/delete/list_session_ids/close）；`__anon__` 默认用户；三后缀 `.json`/`.list.json`/`.list.hash`。**per-key `delete_key` 已落地（§8-1）**（crate 固有方法，仿 MONGODB 先例，不改 core trait） |
| `OssStore`（`BaseStore`） | `OssBaseStore` | 已完成 | 实现 `BaseStore`（get/put/put_if_version/search/delete）；`search` 按 key 排序分页、排除 `.version`；key 去前导 `/`；**错误传播已对齐（§8-3）**：新增固有方法 `search_strict -> OssResult<Vec<StoreItem>>`（列表 key 不在 namespace prefix 下 / item JSON 损坏 → `OssError::Backend`/`Json`，对齐 Java 抛 `RuntimeException`），trait `search` 委托它并在错误时返回空页 + `tracing::error!`（不再静默伪造 item key） |
| `OssRemoteSnapshotClient` + `OssSnapshotSpec` | `OssRemoteSnapshotClient`/`OssSnapshotSpec` | 已完成 | 实现 `RemoteSnapshotClient`/`SandboxSnapshotSpec`；`.tar` 布局；内部复用 harness `RemoteSnapshotSpec`。**缺 Java 的 endpoint/AK/SK 便捷构造器**（P2-2） |
| `OssDistributedStore` | `OssDistributedStore` | 已完成 | 组合 state/base store + snapshot spec + guard；子前缀 `state/`/`store/`/`snapshot/`；`distributed_store()` 产出 harness 组合 |
| `OssSandboxExecutionGuard` | (harness guard) | 已完成 | 透明包装 harness `SandboxExecutionGuard`（不新建平行类型，遵守 §2.1）。**注意**：Java OSS 模块**没有** guard 类也不覆写 `sandboxExecutionGuard()`，本类型属 Rust 侧便利增量，非对齐项 |

### 3.2 测试（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| AgentStateStore 测试 | `OssAgentStateStoreTest`（137 行 / 10 `@Test`） | 已完成（P1-1 已补） | `tests/agent_state_store.rs`（**18 用例**，实测 `#[test]`+`#[tokio::test]` 计数）：builder 校验、key 布局、`__anon__`、单值/列表往返、append 重写、exists、list_session_ids、delete、close、prefix 归一，以及 **per-key `delete_key` 4 用例**（§8-1） |
| BaseStore 测试 | `OssBaseStoreTest`（153 行 / 10 `@Test`） | 已完成（P1-3 已补） | `tests/base_store.rs`（**17 用例**）：builder 校验、put/get+version、CAS 命中/未命中/expected=0、search 分页排序、排除 `.version`、delete、去前导 `/`、命名空间隔离；**新增 4 个**：多页 `search`、`search_strict` 拒绝越界 key、传播列表错误、拒纳损坏 JSON（§8-3） |
| 分页测试 | —（Java 无对应测试类） | **已完成（P1-2）** | `tests/pagination.rs`（**13 用例**）：`page_size` 钳位、首页+续传令牌、续传、最后一页、精确边界页、无匹配空页、token 越过末 key、`max_keys=0`、调用方 `max_keys` 小于页帽、`list_all` 多页聚合/空结果、state store `list_session_ids` 与 session `delete` 跨页 |
| Snapshot 测试 | —（Java 无对应测试类） | 已完成 | `tests/snapshot.rs`（75 行 / **7 用例**）：上传/下载/存在往返、缺失失败、空桶校验、非法 id 拒绝、spec build、persist/restore |
| Guard + DistributedStore 测试 | —（Java 无对应测试类） | 已完成 | `tests/guard_and_distributed.rs`（88 行 / **6 用例**）：guard 委托语义/可重入、空桶校验、子前缀组合、harness `DistributedStore` 组装、`dyn BaseStore` 可用性 |
| 文档测试 | — | 已完成 | `lib.rs` doc example（`no_run`，1 个；因 `no_run` 只编译不执行，实际断言价值为 0） |
| **lib 内联单测** | — | **风格差异（非缺口）** | `src/` 内 `#[cfg(test)]`/`mod tests` 命中 0 处（仅 `s3` feature 下 `src/s3.rs` 有 4 个内联用例）。**§6.4 结论并维持**：不构成独立阻塞项；P1-2a 要求的**分页构造器**已于 §8-2 补齐 |

## 4. 项目状态与下一步
### 4.1 当前状态
- **已完成**：`agentscope-extensions-oss` 全量实现，产出契约 4 类 trait 实现 + 组合者 + 后端抽象；§8（2026-10-26）关闭 P1-1/P1-2/P1-3/P1-7。
- **验证命令与结果**（原会话记录；§8 改动**尚未由子智能体跑 cargo**，待主智能体统一验证）：
  - `cargo build -p agentscope-extensions-oss` → Finished（本 crate 无 error/warning）
  - `cargo test -p agentscope-extensions-oss`（基线，§8 前）→ **42 passed / 0 failed**
    - `agent_state_store` 15 ✓ / `base_store` 13 ✓ / `guard_and_distributed` 6 ✓ / `snapshot` 7 ✓ / doc-test 1 ✓
  - **§8 后预期用例数**：默认 feature **61 test + 1 doc-test**（`agent_state_store` 18 / `base_store` 17 / `pagination` 13 / `guard_and_distributed` 6 / `snapshot` 7）；`--features s3` **72 test + 2 doc-test**（再 + 内联 `s3` 4 + `tests/s3.rs` 7）
- **文件清单**（实测行数）：
  - `crates/agentscope-extensions-oss/Cargo.toml`（feature 已改为 `s3`，见 §7）
  - `src/lib.rs`、`src/error.rs`、`src/client.rs`、`src/state_store.rs`、`src/base_store.rs`、`src/snapshot.rs`、`src/guard.rs`、`src/distributed_store.rs`、`src/s3.rs`（feature `s3`）
  - `tests/common/mod.rs`、`tests/agent_state_store.rs`、`tests/base_store.rs`、`tests/snapshot.rs`、`tests/guard_and_distributed.rs`、`tests/s3.rs`（feature `s3`）、**`tests/pagination.rs`（§8-2 新增）**
- **已知问题**（历史快照，§6 静态复核新增；P1 已在 §8 关闭）：
  - ~~实机 OSS 后端未接线（P0-1）~~ → 已由裁定 8（`s3` + `S3OssClient`，§7）替代实现；`lib.rs` 文档已于 §8-4 更正。
  - ~~缺 per-key `delete`（P1-1）~~ → §8-1 已实现 `OssAgentStateStore::delete_key`。
  - ~~`list_page` 分页/续传分支零覆盖、无分页构造器（P1-2）~~ → §8-2 已补构造器 + 13 用例。
  - ~~`search` prefix 不匹配静默返回错值（P1-3）~~ → §8-3 已改为 `search_strict` 返回 `OssError`。
  - `futures`/`chrono`/`serde`/`tokio`（`[dependencies]` 中的）零引用；`OssError::SnapshotNotFound`/`Backend` 未被构造（P2-1/P2-3）。
  - Java `OssBaseStore.putIfVersion` 用 `long`，Java `OssAgentStateStore` 另有 `supportsVersioning`/`getVersioned`/`saveIfVersion` 版本化 API；Rust 侧 `OssAgentStateStore` **未覆写** `supports_versioning`/`get_versioned`/`save_if_version`（默认 `false`/降级为无条件写），与 Java 一致，但意味着 OSS 状态存储**无乐观并发**（P2-7）。

### 4.2 下一步与阻塞（需汇总方裁定）
1. **harness `DistributedStore` 字段不足**（与 EXTENSIONS_GAP_TRACKER §2.2 同项）——**已解决（2026-10-28）**：harness `DistributedStore` 已增 `base_store` / `sandbox_snapshot_spec` / `sandbox_execution_guard` 三槽 + `SandboxGuard` trait；`OssDistributedStore::distributed_store()` 已用 builder 填满 5 槽（`OssStore` / `OssSnapshotSpec` / `OssSandboxExecutionGuard`），且 `OssSandboxExecutionGuard` 已 `impl SandboxGuard`。详见文末执行记录。
2. **实机阿里云后端**：`aliyun-oss-rust-sdk` 为 optional（feature `aliyun`），默认关闭。**本轮复核已定性：这不是「依赖选型待裁定」，而是「实机后端代码根本不存在」**（`src/` 内 `cfg(feature = "aliyun")` 命中 0 处，见 §6.5 P0-1）。建议二选一：① 实现 `AliyunOssClient`（`impl OssClient`，置于 `cfg(feature = "aliyun")` 模块内）；② 或改用 `opendal`(oss service) / `object_store` 重写后端 trait。`OssClient` trait 已隔离后端，切换成本低。**同时须修正 `src/lib.rs:19-20` 的误导性文档（P1-7）**（已于 §8-4 定稿）。
3. **未改根 `[workspace.dependencies]`**：本 crate 的 `aliyun-oss-rust-sdk` 为本地 `[dependencies]` optional，未登记到根；如需统一版本请汇总方处理。
4. **实机 OSS live 验证待凭据**：端到端需真实 OSS endpoint/AK/SK（同类于 MIGRATION_GAP_TRACKER §7.3）。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-oss` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-oss` / `cargo test -p agentscope-extensions-oss` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-oss/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. 静态复核记录（2026-10-22）

### 6.1 复核方式
- **纯静态**：逐文件阅读 + `Select-String`/`Get-Content` 计数。**未执行** `cargo build` / `cargo test`（本子智能体被禁止编译），因此 §4.1 的 42 passed 属**原会话记录**，本次仅校验「用例条数是否与源码一致」，不校验运行结果。
- 比对对象：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-oss`（5 主类 + 2 测试类）↔ `E:\local-github\agentscope-rust\crates\agentscope-extensions-oss`（8 src + 5 tests）。
- 顺带核对 trait 契约源：`agentscope-core/src/state/store.rs`、`agentscope-core/src/state/list_hash.rs`、`agentscope-harness/src/filesystem/remote/store.rs`、`agentscope-harness/src/agent/sandbox/snapshot.rs`、`agentscope-harness/src/agent/sandbox/guard.rs`、`agentscope-harness/src/agent/sandbox/manager.rs`、`agentscope-harness/src/agent/session/store.rs`、`agentscope-harness/src/agent/distributed_store.rs`、`agentscope-harness/src/agent/harness_agent.rs`。
- **Java 侧基线**：`agentscope-core/src/main/java/io/agentscope/core/state/AgentStateStore.java`（per-key `delete` 为 `default`）、`agentscope-core/.../ListHashUtil.java`（哈希算法）、`agentscope-harness/.../agent/DistributedStore.java`（5 组件 + `sandboxExecutionGuard()` 默认 noop）、`agentscope-harness/.../filesystem/remote/store/BaseStore.java`（`putIfVersion` 的 `0` 语义 + `supportsAtomicSessionStorage`）、`agentscope-harness/.../agent/sandbox/snapshot/*`（`validateId` 强校验）。
- **计数口径**：对 Java 与 Rust 均分别统计「总行数 / 非空行数 / 有效行数（去空行 + 去纯注释行）」三种口径，避免原文的混用（见 §6.2 口径说明与 §6.6 勘误 #13）。
- **未修改任何 Rust 代码**；本轮唯一写入文件为 `docs/extensions/OSS__GAP_TRACKER.md`（UTF-8）。rollup `EXTENSIONS_GAP_TRACKER.md` 由汇总方维护，本次**未改动**（另注：该文件当前以 GB18030 编码存储，非 UTF-8，汇总方写入时需注意编码）。

### 6.2 实测统计

> **口径说明（本次复核统一）**：原文把 Java 侧按「**非空行**」统计（795/290）、Rust 侧按「**总行数**」统计（1210/525），两者口径不一致，易误读。本次复核同时给出 **总行数** 与 **非空行数**，并保留「有效代码行（去空行 + 去纯注释行）」作为第三口径。

| 项 | Java | Rust |
|---|---|---|
| 主代码文件 / 行数（总） | 5 个类 / **880 行**（349+297+85+90+59） | **8 文件 / 1210 行**（265+224+131+53+65+52+128+292） |
| 主代码非空行 | **795 行**（314+268+77+81+55） | **1064 行**（235+198+115+43+55+49+112+257） |
| 主代码有效行（去空行/纯注释行） | 614 行 | 842 行 |
| 测试文件 / 行数（总） | 2 类 / **336 行**（158+178） | **5 文件 / 525 行**（174+171+17+75+88） |
| 测试非空行 | **290 行**（137+153） | **446 行**（150+148+13+63+72） |
| 测试有效行 | 260 行 | 432 行 |
| 测试用例数 | **20**（`@Test` 10+10） | **41**（`#[test]`/`#[tokio::test]`）+ **1** doc-test(`no_run`) = **42** |
| `src/` 内联单测 | 不适用 | **0**（`#[cfg(test)]` / `mod tests` 零命中） |
| 分发：`agent_state_store` 15 / `base_store` 13 / `snapshot` 7 / `guard_and_distributed` 6 | — | 与 §4.1 声明**完全一致**（静态计数逐文件复核通过） |

**关于「42 test（41 集成 + 1 doc-test），全在 `tests/`，lib 无内联用例」——属实。** 逐文件实测：`agent_state_store.rs` 15、`base_store.rs` 13、`guard_and_distributed.rs` 6、`snapshot.rs` 7（合计 41），`lib.rs` 1 个 `no_run` doc-test。`src/` 内无任何 `#[cfg(test)]`、无 `mod tests`。
**doc-test 复核**：`src/` 内共 4 个代码围栏，其中 `base_store.rs:6` 与 `state_store.rs:6` 为 ` ```text `（不产生 doc-test），仅 `lib.rs:22` 为 ` ```no_run ` → **doc-test 数量确为 1**，与声明一致。

**Java 类数复核**：主代码 5 个类（`OssAgentStateStore` / `OssBaseStore` / `OssDistributedStore` / `OssRemoteSnapshotClient` / `OssSnapshotSpec`），测试 2 个类（`OssAgentStateStoreTest` / `OssBaseStoreTest`）——与 §1 声明一致。每个 Java 主类内含 1 个 `public static class Builder` 嵌套类（不计为独立类）。

### 6.3 对齐结论表

| Java 类 / 能力 | Java 路径 | Rust 现状 | 判定 |
|---|---|---|---|
| `OssAgentStateStore` | `src/main/java/io/agentscope/extensions/oss/OssAgentStateStore.java` | `src/state_store.rs` 全量实现（7 个 trait 方法 + `close()` 便捷方法） | **等价**（除 per-key delete，见下） |
| ├ per-key `delete(userId, sessionId, key)` | 同上 L197-215 | **无**（core `AgentStateStore` trait 未定义该方法） | **缺失（P1-1）** |
| ├ 错误传播：失败抛 `RuntimeException` | 同上 L98/130/145/166/180/193/213/232/279（**实测 9 处**） | 全部 `tracing::error!`/`warn!` 后静默返回（`save` 返回 `()`、`get` 返回 `None`、`get_list` 返回空 `Vec`） | **部分（P1-3，语义偏软）** |
| ├ `close()` | 同上 L236-239 | `state_store.rs:53` `close()` 便捷方法（core trait 无 `close`，Java 接口有 `default close()`） | **等价** |
| `OssBaseStore` | `OssBaseStore.java` | `src/base_store.rs` 全量实现（5 方法） | **等价** |
| ├ `put_if_version` CAS | L117-134（`currentVersion != expected` → false；**无** expected=0 特判） | `base_store.rs:70-104`（**多** expected==0 → `!exists(data_key)` 特判） | **等价偏强**（harness 语义要求：Java `BaseStore.putIfVersion` 文档 L65-66 明写「`0` = 仅当 key 不存在时写」；Java 版因 `readVersion` 缺失时返回 0 而偶然通过，Rust 改为显式 `exists` 判定，结论一致且更严谨） |
| ├ `supportsAtomicSessionStorage` | Java `BaseStore.java:29-31` 默认 `false`，`OssBaseStore` 未覆写 | Rust `BaseStore` 默认 `false`（`store.rs:63`），`OssStore` 未覆写 | **等价**（双方均 `false`；后果一致：`harness/src/agent/session/store.rs:44` 会拒绝该 store 作为 session journal） |
| ├ 错误传播 | 抛 `RuntimeException`（**实测 6 处**） | `get` → `None`、`put`/`delete` → 记日志 | **部分（P1-3）** |
| `OssRemoteSnapshotClient` | `OssRemoteSnapshotClient.java` | `src/snapshot.rs` 实现 `RemoteSnapshotClient` | **等价**（`InputStream`→`&[u8]`） |
| ├ snapshotId 校验 | Java 仅 `null`/blank 检查（L71-73），**不拦** `/` `\` `..` | Rust `client.rs:186-194` 额外拒绝 `/` `\` `..` NUL（对齐 harness `snapshot.rs:42-52`） | **Rust 更严（可接受）**：Java `OssRemoteSnapshotClient` 独立实现了弱校验，未复用 harness 的 `LocalSandboxSnapshot.validateId`（Java `LocalSandboxSnapshot.java:144-153` 才是强校验版）；Rust 走 harness `RemoteSnapshotSpec.build` 强校验路径 |
| ├ 下载缺失快照 | 抛 `FileNotFoundException`（L59） | `snapshot.rs:73` 返回 `Error::Tool("Snapshot not found in OSS: …")` | **等价**（错误类型不同但均失败） |
| `OssSnapshotSpec` | `OssSnapshotSpec.java` | `src/snapshot.rs` 包装 harness `RemoteSnapshotSpec` | **等价**（`build` 完全委托 `RemoteSnapshotSpec::build`） |
| ├ `(endpoint, ak, sk, bucket, prefix)` 便捷构造器 | 同上 L48-58（内部 `OSSClientBuilder`） | **无**（无 SDK 依赖） | **缺失（P2-2，可接受）** |
| `OssDistributedStore` | `OssDistributedStore.java` | `src/distributed_store.rs` | **部分（P1-4）** |
| ├ `agentStateStore()` | L63-70 → `prefix + "state/"` | `oss_agent_state_store()` → 同前缀 | **等价** |
| ├ `baseStore()` | L72-79 → `prefix + "store/"` | `oss_base_store()` → 同前缀 | **等价** |
| ├ `sandboxSnapshotSpec()` | L81-84 → `prefix + "snapshot/"` | `oss_snapshot_spec()` → 同前缀 | **等价** |
| ├ `sandboxExecutionGuard()` | **未覆写** → Java `DistributedStore.java:110-112` 默认 `SandboxExecutionGuard.noop()` | `sandbox_execution_guard()` 返回 `OssSandboxExecutionGuard`（内含 harness `SandboxExecutionGuard::new()`，**非 `noop()`**，但 harness 的 `noop()` 实现即 `Self::new()`，`guard.rs:27-29`） | **行为等价**（Rust 多出便利类型，非缺口） |
| ├ 一个完整 `DistributedStore` | 实现 `DistributedStore` 接口（5 组件齐全，Java `DistributedStore.java:303-309` `build()` 校验 `agentStateStore`+`baseStore` 非空） | `distributed_store()` 只填 `agent_state_store` | **部分（P1-4，受 harness struct 限制，见 §4.2-1）** |
| `SandboxExecutionGuard`（harness 接口） | Java 为 `@FunctionalInterface`（`SandboxExecutionGuard.java:59`），OSS 无自有实现 | `OssSandboxExecutionGuard` 透明包装 harness struct | **等价**（且遵守 §2.1 不造平行类型；Java 接口 → Rust struct 的整体形态差异属 harness 裁定，见 rollup §2.1） |
| 实机 `com.aliyun.oss.OSS` 后端 | `aliyun-sdk-oss:3.18.5`（`pom.xml:49-53` 硬依赖） | `OssClient` trait + `InMemoryOssClient`；`aliyun-oss-rust-sdk` 为 optional 且**零 `cfg(feature)` 使用点** | **缺失（P0-1，唯一 P0）** |
| `JsonUtils`/Jackson | `JsonUtils.getJsonCodec()` | `serde_json` | **等价** |
| `ListHashUtil` | `core.state.ListHashUtil`（`computeHash`/`needsFullRewrite`/`hasChanged`） | `agentscope_core::state::list_hash`（`compute_hash`/`needs_full_rewrite`/`has_changed`，经 `core/src/state/mod.rs:19` 再导出） | **等价**（无平行实现；采样阈值 5、采样位点 0/1-4/1-2/3-4/last、Case 1-3 判定逐条对齐，已逐行比对 `ListHashUtil.java:76-170` ↔ `list_hash.rs:15-71`） |

**B 波次统一产出契约（rollup `EXTENSIONS_GAP_TRACKER.md` L55「各存储模块的**统一产出契约**」）核对结果：5/5 齐全，类型与位置逐个复核通过。**

| 契约件 | 位置（实测） | 状态 |
|---|---|---|
| `OssDistributedStore` | `distributed_store.rs:35`（`pub struct OssDistributedStore`） | ✅ 存在（但见 P1-4） |
| `OssAgentStateStore` | `state_store.rs:30`（`pub struct OssAgentStateStore`） | ✅ 存在，实现 core `AgentStateStore`（`state_store.rs:108`） |
| `OssStore`（BaseStore 实现） | `base_store.rs:30`（`pub struct OssStore`） | ✅ 存在，实现 harness `BaseStore`（`base_store.rs:38`） |
| `OssSnapshotSpec` | `snapshot.rs:90`（`pub struct OssSnapshotSpec`） | ✅ 存在，实现 `SandboxSnapshotSpec`（`snapshot.rs:116`） |
| `OssSandboxExecutionGuard` | `guard.rs:26`（`pub struct OssSandboxExecutionGuard`） | ✅ 存在（包装 harness，非平行类型） |

> 补充：契约件均在 `src/lib.rs:43-49` 以 `pub use` 顶层再导出（`OssStore`/`OssAgentStateStore`/`OssSnapshotSpec`/`OssSandboxExecutionGuard`/`OssDistributedStore`），下游可直接 `agentscope_extensions_oss::Xxx` 引用，**契约可发现性无问题**。

### 6.4 「lib 无内联用例」覆盖风险评估与建议
- **风险定性：中低，但存在明确盲区。** 41 个用例全部走 `tests/`（= 只测 public API），这意味着：
  1. `OssClient::list_page` 的**分页/续传令牌**分支（`client.rs:145-174`）零覆盖——`next_token`/`truncated` 逻辑只有在 `InMemoryOssClient` 单页返回时被间接走到；`list_all` 的循环续传路径（`client.rs:67-85`）实际**从未被 2 页以上场景触发**，而真实 OSS 恰恰必然分页（Java 侧 `listAllKeys`/`search` 都显式翻页）。
     **补充核实**：本轮复查确认 `InMemoryOssClient` **没有**任何分页构造器（`max_keys` 完全由调用方传入、`list_all` 内部硬编码 `1000`），因此**即使想测多页也无处下手**——这使该项从「暂缺用例」升级为「缺少可测性构造器 + 缺用例」双重缺口。
  2. `OssStore::search` 的 `strip_prefix`/`strip_suffix` 双 `unwrap_or(data_key)` 回退（`base_store.rs:124-129`）在 prefix 不匹配时会产出**整条 key 作为 item_key** 的静默错值，无测试守护。
     **补充核实**：Java 侧对应实现 `OssBaseStore.java:167-168` 用的是**无保护的 `substring`**（prefix 不匹配会抛 `StringIndexOutOfBoundsException`，被 L182 的 `catch` 转成 `RuntimeException`）；Rust 侧用 `unwrap_or` 兜底 → **错误形态不同**（Java 报错，Rust 静默错值），属行为偏离，应并入 P1-3 一并修正。
  3. `OssAgentStateStore::save_list` 的 `rewrite == false` 分支（不重写 list、仅更新 hash）无直接断言——现有 `save_list_append_rewrites_when_count_grows` 只覆盖了「长度增长 → 重写」。
     **补充核实**：该分支在 Java 侧由 `needsFullRewrite` 的 Case 3（前缀未改动 → 不重写）驱动，且 Java 条件 `needsFullRewrite || values.size() != existingCount`（`OssAgentStateStore.java:123`）与 Rust `state_store.rs:162-164` **完全同构**；两侧在 `rewrite == false` 时同样无条件写 hash（Java L128 / Rust L170）→ 行为一致，只是无用例守护。
  4. `guard.rs` 的 `from_harness()` / `inner()`、各 `bucket_name()` 访问器零引用零测试（`bucket_name()` 全 crate 0 处调用）。
     **补充核实**：`bucket_name()` 在本 crate 内**零调用点**（`state_store.rs:43`、`base_store.rs:171`、`snapshot.rs:45` 三处定义，0 处调用）；`from_harness()`/`inner()` 同样零调用；`OssSandboxExecutionGuard::active_count()` 已被 `tests/guard_and_distributed.rs:22` 覆盖，但 `from_harness`/`inner` 未覆盖。
  5. **本轮新增**：`OssAgentStateStore` **未覆写** core trait 的 `supports_versioning` / `get_versioned` / `save_if_version`（`core/src/state/store.rs:68/76/98`），三者走默认实现 → `supports_versioning()` 恒 `false`、`save_if_version` 降级为无条件 `save` 并返回 `UNVERSIONED`。这与 Java `OssAgentStateStore` 同样未覆写 `supportsVersioning`/`getVersioned`/`saveIfVersion`（`AgentStateStore.java:87/104/134` 均为 `default`）**行为一致**，但意味着 **OSS 状态存储不具备乐观并发能力**（P2-7，非本轮新缺陷）。
  6. **本轮新增**：`OssError::SnapshotNotFound`（`error.rs:30`）与 `OssError::Backend`（`error.rs:38`）全 crate **零构造点**；`snapshot.rs:73` 用内联 `Error::Tool("Snapshot not found in OSS: …")` 而非 `SnapshotNotFound` 变体。
  7. **本轮新增**：`base_store.rs:113` 注释称「`.version` 和 `.list.json` must be excluded」，但过滤器 `k.ends_with(".json") && !k.ends_with(".version")` **不排除 `.list.json`**（Java `OssBaseStore.java:152` 同样不排除 → 双方共同行为，非单侧缺陷，但注释表述会误导）。
- **建议（按性价比排序）**：
  - **P1-2a**：给 `InMemoryOssClient` 增加**分页构造器**（如 `InMemoryOssClient::with_page_size(n)`，令 `list_page` 忽略传入的 `max_keys` 而使用内部上限），在 `tests/` 增加 2 个用例覆盖 `list_page` 续传 + `list_all` 多页聚合；这是当前唯一「真实后端必然命中、而内存后端默认不命中」的路径，风险最高。**注：原建议「加 `with_page_size`」未指出当前 `InMemoryOssClient` 缺少分页能力——不加则造不出多页场景，此处已补正。**
  - **P1-2b**：补 1 个 `save_list` 幂等/未变更用例（同内容二次 `save_list` → list 对象未被重写，可用「写后手工篡改 list 内容再 save_list，断言未被覆盖」验证）。
  - **P1-2c**：修正 `base_store.rs:113` 注释，并补 1 个用例锁定 `search` 在 prefix 不匹配时的预期行为（Java 抛异常 / Rust 返回空 `Vec` 均可，但需**二选一并写进文档**，禁止当前的静默错值）。
  - **P2-4**：`src/` 内补 `#[cfg(test)] mod tests` 覆盖 `normalize_prefix` / `strip_leading_slashes` / `validate_snapshot_id` 三个纯函数（当前仅经集成用例间接覆盖，边界值如 `""`/`"\\\\"`/`"/"` 未测）。此项与 `EXTENSIONS_GAP_TRACKER` 中 COS/POSTGRESQL 的内联单测惯例一致。
  - **P2-8**：为 `OssSandboxExecutionGuard::from_harness`/`inner` 与三个 `bucket_name()` 访问器补用例或删除（当前均零调用，且 `distributed_store.rs:125`/`snapshot.rs:125` 用 `#[allow(dead_code)]` 抑制）。
  - **不建议**把 `tests/` 用例搬进 `src/`：现有集成测试分层（按组件分文件 + `common` 复用）清晰，无需重构。
  - **关于「lib 零内联测试」的定性（本轮结论）**：经评估，**不构成本模块的独立阻塞项**。理由：① 41 个集成用例已覆盖 5 个契约件的公开 API 主干；② 本 crate 逻辑密度低（842 有效行，其中约 40% 为 builder/派生样板），私有纯函数仅 3 个（`normalize_prefix`/`strip_leading_slashes`/`validate_snapshot_id`）且均被集成用例间接覆盖；③ `EXTENSIONS_GAP_TRACKER` 中 COS/POSTGRESQL 的内联单测惯例**并非全波次强制**（SKILLS/MEM/STUDIO 等 crate 同样以 `tests/` 为主）。真正需要补的是 **P1-2a 的分页构造器**（可测性缺口），而非机械地补 `src/` 内联用例。建议 §3.2 该行状态维持 `待办（P1-2）`，但把「与同类 crate 不一致」的表述降级为「风格差异，非功能缺口」。

### 6.5 新发现缺口（按优先级）

> **2026-10-26 状态：本节 P1-1 / P1-2 / P1-3 / P1-7 已于 §8 关闭，P0-1 已由裁定 8（§7）替代实现；下文保留为历史快照。**

**P0**
1. **P0-1 实机 OSS 后端不存在（空壳 feature）**：`Cargo.toml:13` 声明 `aliyun = ["dep:aliyun-oss-rust-sdk"]`，但 `src/` 内 `cfg(feature = "aliyun")` 命中 **0** 处，`aliyun-oss-rust-sdk` 未被任何代码 `use`。后果：`--features aliyun` 编译后**仍然只能拿到 `InMemoryOssClient`**，crate 在生产环境**无法连接真实 OSS**。Java 侧 `pom.xml` 硬依赖 `aliyun-sdk-oss:3.18.5` 且 5 个类全部直接操作 `OSS`。当前 tracker 把此项写成「实机后端待接入 / 需汇总方裁定」（§4.2-2），属**低估**——这不是「待裁定」而是**未实现**。建议：要么实现 `AliyunOssClient`（`impl OssClient`，`cfg(feature = "aliyun")`），要么在 §1/§3.1 明确标注「本 crate 交付范围仅含内存后端，实机后端为后续波次」，避免下游误以为可直接投产。

**P1**
1. **P1-1 缺 per-key `delete`**：Java `AgentStateStore` 接口有 `default void delete(userId, sessionId, key)`（`AgentStateStore.java:208-210`）且 `OssAgentStateStore` 覆写之（`OssAgentStateStore.java:197-215`，删 `.json`/`.list.json`/`.list.hash` 三个对象）。Rust core `AgentStateStore` trait（`store.rs:58`）只有 session 级 `delete`，OSS crate 因此无此能力。影响：单 key 状态无法清理，只能整 session 删除；Java 迁移过来的调用方会**编译失败或语义降级**。
   **本轮补充**：**同波次 MONGODB crate 已有可照抄的先例**——`agentscope-extensions-mongodb/src/state_store.rs:438` 提供 `pub async fn delete_key(user_id, session_id, key) -> Result<(), MongoError>`（删 `states`/`versions`/`hashes` 三字段），并在 `state_store.rs:814` 有配套用例。因此**不必修改 core trait**（`delete_key` 作为 crate 固有方法即可），建议 OSS 侧照此实现 `OssAgentStateStore::delete_key`，同时删掉 `.json`/`.list.json`/`.list.hash` 三个对象 → 可把本项从 P1 降为 P2 工作量级。
2. **P1-2 `list_page`/`list_all` 分页路径零覆盖 + 缺分页构造器** + `src/` 无内联单测（详见 §6.4）。
3. **P1-3 错误传播语义弱化**：Java **15 处** `throw new RuntimeException(...)`（`OssAgentStateStore` 9 处 + `OssBaseStore` 6 处，实测）；Rust 全部降级为 `tracing::error!` + 返回 `None`/空/`()`。`BaseStore::get` 读失败与「key 不存在」不可区分，`AgentStateStore::get_list` 反序列化失败静默返回空数组——**可能把后端故障当成「无数据」，导致 agent 静默丢上下文**。
   **本轮补充**：`OssStore::search` 的 prefix 不匹配场景，Java 抛异常（`OssBaseStore.java:167-168` 无保护 `substring`）而 Rust 静默返回错值（`base_store.rs:124-129` 的 `unwrap_or` 兜底）→ **错误形态亦被弱化**。建议至少在 `get`/`get_list`/`search` 引入可区分返回，或提供 `strict` 构造开关。
4. **P1-4 `OssDistributedStore` 无法产出完整 `DistributedStore`**：harness `DistributedStore`（`distributed_store.rs:14-17`）仅 `agent_state_store` + `isolation_scope`；`HarnessAgent` 也只消费 `agent_state_store`（`harness_agent.rs:1759-1761`、`1883-1892`），**无 `base_store`/`sandbox_snapshot_spec`/`sandbox_execution_guard` 的 builder 入口**。因此 Java 的 `OssDistributedStore implements DistributedStore`（一次配置全量自动接线，Java `DistributedStore.java:303-309` 的 `build()` 还强制校验 `agentStateStore`+`baseStore` 非空）在 Rust **无法等价**。此项已登记 §4.2-1，但应提升可见度：下游若照 Java 用法写 `agent.builder().distributed_store(oss.distributed_store())`，**filesystem/snapshot/guard 全部不会生效**。
5. **P1-7 crate 级文档与实现不符（误导性文档）**：`src/lib.rs:19-20` 写「a concrete Alibaba Cloud backend can be supplied by enabling the `aliyun` feature (see crate `Cargo.toml`)」——**开启 feature 后并不会得到任何实机后端**（`src/` 内 `cfg(feature = "aliyun")` 命中 0 处，P0-1）。这是 crate 面向下游的**第一入口文档**，与 P0-1 叠加会放大误用风险（下游以为开 feature 即可投产）。建议随 P0-1 一并修正：改为「本 crate 当前仅提供 `InMemoryOssClient`；实机后端需自行实现 `OssClient` 或待后续波次提供」。

**P2**
1. **P2-1 冗余依赖**：`futures`、`chrono` 在 `src/`+`tests/` 中零引用（`chrono` 的 3 处命中均为注释里的 "synchronous" 子串）。**本轮扩大核查**：`serde`（`Cargo.toml:18`）在 `src/`+`tests/` 中**零直接引用**（crate 只用 `serde_json`，无 `derive(Serialize)`、无 `serde::` 路径）；`tokio` 在 `[dependencies]`（`Cargo.toml:22`）中于 `src/` **零引用**，仅 `[dev-dependencies]`（`Cargo.toml:32`）的 `#[tokio::test]` 真正需要 → `tokio` 应只保留在 `[dev-dependencies]`，`serde` 可移除。建议一次性清理 4 个依赖。
2. **P2-2 缺 `OssSnapshotSpec(endpoint, ak, sk, bucket, prefix)` 便捷构造器**（Java L48-58）。Rust 无 SDK 依赖，属可接受差异，但应在 §2 映射表注明「不迁移」而非留白。
3. **P2-3 死错误变体**：`OssError::SnapshotNotFound`（`error.rs:30`）与 `OssError::Backend`（`error.rs:38`）全 crate **零构造点**；`snapshot.rs:73` 用的是内联 `Error::Tool("Snapshot not found in OSS: ...")` 而非该变体。建议二选一：统一用 `SnapshotNotFound`，或删除。
4. **P2-4 `base_store.rs:113` 注释与代码不符**：注释称「`.version` 和 `.list.json` must be excluded」，但过滤器 `k.ends_with(".json") && !k.ends_with(".version")` **不会排除 `.list.json`**（Java L152 同样不排除，属双方共同行为）。注释应改为「`.version` 被排除；`.list.json` 因前缀隔离不会与 store 命名空间冲突」。
5. **P2-5 `delete` 未批量**：Java `deleteKeys` 每 1000 个一批 `deleteObjects`（quiet），Rust 逐 key `delete` 且忽略错误。大 session 删除时请求数 1000×，且部分失败不可感知。
6. **P2-6 `as_arc_base_store` 死代码**：`distributed_store.rs:129` 定义的 public 辅助函数全 crate 零调用（测试里直接 `Arc::new(...)`）。
7. **P2-7 OSS 状态存储无乐观并发**：`OssAgentStateStore` 未覆写 core trait 的 `supports_versioning`/`get_versioned`/`save_if_version`（`core/src/state/store.rs:68/76/98`），三者走默认实现 → `supports_versioning()` 恒 `false`、`save_if_version` 降级为无条件写并返回 `UNVERSIONED`。**与 Java 一致**（Java `OssAgentStateStore` 同样未覆写 `AgentStateStore.java:87/104/134` 的 default 方法）→ **不是迁移缺口**，但与 Java `OssBaseStore` 的 `put_if_version` 能做 CAS 形成对比，登记为能力说明（若后续要在 OSS 上启用版本化，需同时实现 `.version` 侧车对象）。
8. **P2-8 零调用公开访问器**：`OssSandboxExecutionGuard::from_harness`/`inner`、`OssAgentStateStore::bucket_name`/`key_prefix`、`OssStore::bucket_name`/`key_prefix`、`OssRemoteSnapshotClient::bucket_name`/`key_prefix` 中，`bucket_name()` 三处定义**零调用**、`from_harness`/`inner` 零调用；`distributed_store.rs:125` 与 `snapshot.rs:125` 还用 `#[allow(dead_code)]` 掩盖了 `_DEFAULT_PREFIXES`/`_assert_remote_snapshot_used` 两处占位桩。建议补用例或删除死代码。

### 6.6 勘误列表（原文 → 实际）

| # | 位置 | 原文 | 实际 |
|---|---|---|---|
| 1 | §1「Java 规模（初测）」 | 主代码 ~795 行 / 测试 ~290 行 | **正确**（795/290），但缺类数与用例数；已补为 5 类 / 2 测试类 / 20 `@Test` |
| 2 | §1 | 无 Rust 规模行 | **新增**：8 src 文件 1210 行 / 5 tests 文件 525 行 / 41+1 用例 / `src/` 内联单测 0 |
| 3 | §2 `OssAgentStateStore` 行 | 未提及 per-key delete 差异 | Java 覆写了 `delete(userId, sessionId, key)`，Rust 无（core trait 缺该方法） |
| 4 | §2 `OssSnapshotSpec` 行 | 「内部包装 harness `RemoteSnapshotSpec`」 | 正确，但漏记 Java 另有 endpoint/AK/SK 便捷构造器未迁移 |
| 5 | §2 `(harness SandboxExecutionGuard)` 行 | 「隔离键执行租约」（暗示 Java 侧有此组件） | **Java OSS 模块无任何 guard 类**，且 `OssDistributedStore` 未覆写 `sandboxExecutionGuard()`；`OssSandboxExecutionGuard` 是 Rust 侧增量，非对齐项 |
| 6 | §2 `com.aliyun.oss.OSS` 行 | 「实机后端经 feature `aliyun`（`aliyun-oss-rust-sdk`）提供」 | **证伪**：feature 存在但**无任何 `cfg(feature)` 代码**，实机后端并未提供（P0-1） |
| 7 | §3.1 crate 骨架行 | 未提依赖冗余 | `futures`/`chrono` 零引用 |
| 8 | §3.2 各测试行 | 只写用例数，未标行数/Java 对照 | 已补行数与 Java `@Test` 数（15 vs 10、13 vs 10）；Snapshot/Guard/Distributed **Java 侧无对应测试类**（原文 `—` 正确） |
| 9 | §3.2 文档测试行 | 「`lib.rs` doc example（no_run）」 | 正确；补充说明 `no_run` 只编译不执行，断言价值为 0 |
| 10 | §4.1「已知问题：无」 | 无已知问题 | **证伪**：至少 4 项（P0-1 实机后端空壳、P1-1 per-key delete、P1-2 分页零覆盖、P1-3 错误静默） |
| 11 | §4.2-2 | 「实机阿里云后端…是否改为 crates.io 常规依赖 / 改用 `opendal` 需汇总方裁定」 | 定性偏差：不是「依赖选型待裁定」，而是**代码尚未实现**（零 `cfg(feature)` 使用点） |
| 12 | §4.1 42 passed 拆分 | 15/13/6/7 + 1 doc-test | **正确**（静态计数完全吻合） |
| 13 | §1 / §6.2 行数口径 | Java 795/290（**非空行**）与 Rust 1210/525（**总行数**）并列 | **口径不一致**：同一张表混用两种口径，易被误读为「Rust 代码量是 Java 的 1.5 倍」。已改为三口径并列：总行数（Java 880/336，Rust 1210/525）、非空行（Java 795/290，Rust 1064/446）、有效行（Java 614/260，Rust 842/432）。**注**：本轮下发的参考值「Rust src 8 文件 1064 行 / tests 5 文件 446 行」正是**非空行**口径，与 rollup 一致；1210/525 为**总行数**，两者都对但不可混用 |
| 14 | §6.3「Java 7 处 `RuntimeException`」 | 7 处 | **实测 15 处**：`OssAgentStateStore` 9 处（L98/130/145/166/180/193/213/232/279）+ `OssBaseStore` 6 处（L99/113/132/182/196/254）。已更正（§6.5 P1-3） |
| 15 | §6.3「`OssAgentStateStore` 全量实现（8 方法）」 | 8 方法 | **7 个 trait 方法**（`save`/`save_list`/`get`/`get_list`/`exists`/`delete`/`list_session_ids`）+ 1 个 `close()` 便捷方法 + 1 个 builder = 9 个公开成员；原文「8 方法」口径不明，已改为显式列举 |
| 16 | §6.3「错误传播：失败抛 `RuntimeException`」行号列表 | L98/130/145/166/180/193/213/232（8 个） | 漏了 `OssAgentStateStore.java:279`（`getString` 内部），且 `OssBaseStore` 侧 6 处**完全未列**。已补全 |
| 17 | §6.4 建议 P1-2a | 「给 `InMemoryOssClient` 加 `with_page_size(n)`…测试构造器」 | **表述不完整**：未指出当前 `InMemoryOssClient` **根本没有分页能力**（`max_keys` 由调用方传入、`list_all` 内部硬编码 1000），不加构造器就造不出多页场景。已补正为「加构造器 + 补 2 用例」 |
| 18 | §6.3「Rust snapshotId 校验更严 → 等价」 | 「Rust 更严：额外拒绝 `/` `\` `..` NUL」 | **事实正确但归因需修正**：Java `OssRemoteSnapshotClient.java:71-73` 只做 blank 检查；Java harness 的强校验在 `LocalSandboxSnapshot.java:144-153`（`validateId`），Java `RemoteSnapshotSpec.build`（L49-51）**不校验** → Java 远程路径本身弱校验。Rust 走 harness `RemoteSnapshotSpec.build`（含 `validate_snapshot_id`）故更强。判定由「Rust 更严」细化为「Rust 更严（可接受）」并注明来源 |
| 19 | §6.3「`sandboxExecutionGuard()` → 默认 `noop()`」 | 「Rust 多出（非缺口）」 | **补证**：Java `DistributedStore.java:110-112` 默认返回 `SandboxExecutionGuard.noop()`，Java `SandboxExecutionGuard.java:78-79` 的 `noop()` 返回 `NoopSandboxExecutionGuard.INSTANCE`；Rust harness `guard.rs:27-29` 的 `noop()` 实现即 `Self::new()` → **两侧 `noop()` 语义等价（都不加限制）**，判定由「Rust 多出」改为「行为等价（Rust 多出便利类型，非缺口）」 |
| 20 | §6.5 P1-1 建议 | 「建议在 core trait 增加 `async fn delete_key(...)`」 | **建议过重**：同波次 MONGODB crate 已有先例（`agentscope-extensions-mongodb/src/state_store.rs:438` `pub async fn delete_key(...)`，含配套用例 L814）→ 无需改 core trait，OSS 侧加固有方法即可。建议已改写 |
| 21 | §6.5 P2-1 | 「`futures`、`chrono` 零引用」 | **范围偏窄**：本轮扩大核查确认 `serde`（`Cargo.toml:18`）同样零直接引用、`tokio` 在 `[dependencies]`（`Cargo.toml:22`）中于 `src/` 零引用（仅 `[dev-dependencies]` 需要）→ 冗余依赖实为 **4 个**（`futures`/`chrono`/`serde`/`tokio`） |
| 22 | §6.5 P2-3 | `OssError::Backend`（`error.rs:39`） | 行号偏移：该变体实际在 **`error.rs:38`**（`error.rs:37` 是 `#[error(...)]` 属性行，`error.rs:36` 是文档注释）。已更正；`SnapshotNotFound` 在 `error.rs:30` 正确 |
| 23 | §6.2 「`src/` 内 `cfg(test)` 命中 0」 | 仅查 `cfg(test)` | **已扩展复核**：同时查 `mod tests` 亦为 0 命中；并核实 `src/` 共 4 个代码围栏，`base_store.rs:6`/`state_store.rs:6` 为 ` ```text `（不产生 doc-test），仅 `lib.rs:22` 为 ` ```no_run ` → **doc-test 确为 1 个**，41+1=42 属实 |
| 24 | §2 `com.aliyun.oss.OSS` 行 | 未提 crate 文档误导 | **新发现**：`src/lib.rs:19-20` 声称开启 `aliyun` feature 即可获得实机后端，与实现不符（P0-1/P1-7）。已在 §2 与 §6.5 登记 |

### 6.7 建议 rollup 状态值
- **建议 `EXTENSIONS_GAP_TRACKER` 第 13 行（OSS 行）状态由「已完成」调整为「进行中」**（该 rollup 的合法状态集合为：`待办`/`进行中`/`部分完成`/`已完成`/`阻塞`/`设计决定（不迁移）`）。
- 理由：`已完成` 隐含「可投产」。但本 crate 的**唯一实机后端（阿里云 OSS）根本不存在**（P0-1：feature 空壳、零 `cfg(feature)` 代码），生产环境只能跑 `InMemoryOssClient`；且 crate 首入口文档 `lib.rs:19-20` 仍误导性宣称开 feature 即可用（P1-7），误用风险被放大。同时 Java 的核心能力 per-key `delete` 缺失（P1-1）、`OssDistributedStore` 无法等价（P1-4）、错误传播被弱化（P1-3）。
- **为什么不是「部分完成」**：「部分完成」在 rollup 中用于「主干已落地但仍有大批长尾」（见 CONTROLPLANE/POSTGRESQL/JDBC 行）；本 crate 的 **5/5 产出契约齐全、5 个 Java 类全部映射、42 条测试属实、trait 复用无平行实现**——「结构对齐」这一维度已完全达标，仅卡在「实机后端未实现」这一个单点上。用「部分完成」会低估已完成的工作量。
- **为什么不是「待办」/「阻塞」**：没有任何一步卡在等待外部裁定上；P0-1 是**纯实现工作**（写一个 `impl OssClient`），不是依赖选型争议或环境阻塞。
- **为什么保留「已完成」不推荐**：唯一实机后端缺失是**功能性缺口而非增强项**，Java 侧 5 个类全部直接操作 `OSS` SDK（`pom.xml:49-53` 硬依赖），Rust 侧当前**无法连接真实 OSS**，「已完成」会被下游直接当作可投产结论。
- 若汇总方坚持 `已完成`，则必须在该行备注追加：`内存后端交付完成；实机 aliyun 后端未实现（P0-1，feature 为空壳），不可直接投产；缺 per-key delete（P1-1）、DistributedStore 无法全量接线（P1-4）`。
- **配套建议**：rollup 该行的「Java 主/测行数」`795 / 290` **正确**（非空行口径，与本表一致），**无需改动**；但「42 test（41 集成 + 1 doc-test），全在 `tests/`，lib 无内联用例」这一表述**属实**，可保留，建议追加「（`src/` 内联单测 0，属风格差异非功能缺口）」以免被误读为待办项。

## 7. 裁定 8 执行记录（2026-10-22）

### 7.1 裁定摘要
- `EXTENSIONS_GAP_TRACKER.md` §4.5 第 8 条：**使用 `rust-s3` 实现通用 S3 存储**，feature 名 `s3`。
- **删除空壳 `aliyun` feature** 及其未使用的 `aliyun-oss-rust-sdk` 依赖（原 `src/` 内 `cfg(feature = "aliyun")` 命中 0）。
- **默认 feature 不启用 `s3`**；`OssClient`/`InMemoryOssClient` 与全部既有存储语义保持不变。

### 7.2 实现范围
| 项 | 内容 |
|---|---|
| 新增 | `src/s3.rs`：`S3OssClient` + `S3OssClientBuilder`，实现 `OssClient` 全部 5 个必需方法（`shutdown` 沿用默认 no-op） |
| 配置 | bucket（必填）/ region（默认 `us-east-1`）/ 自定义 `endpoint` / `access_key` + `secret_key`（可回退 `AWS_ACCESS_KEY_ID`、`AWS_SECRET_ACCESS_KEY`）/ 可选 `session_token`（可回退 `AWS_SESSION_TOKEN`）/ `path_style`（自定义 endpoint 时默认 `true`）/ `env_credentials(false)` 可关闭环境变量回退以便确定性校验 |
| 同步适配 | 结构体持有独立 Tokio multi-thread runtime（2 worker，`enable_all`），每次调用 `Runtime::block_on`；**不会**每次新建 runtime。模块与 crate 文档均注明「不要在 Tokio worker 线程直接调用，需 `spawn_blocking`/`block_in_place`」 |
| 错误映射 | `error.rs` 新增 `#[cfg(feature = "s3")]` 结构化变体：`S3(#[from] s3::error::S3Error)`、`S3Config(String)`、`MissingCredentials`、`S3Http { status, body }`；不再把后端错误吞成无类型字符串 |
| 依赖 | `Cargo.toml`：`s3 = ["dep:rust-s3"]`；`rust-s3 = { version = "0.38", optional = true, default-features = false, features = ["tokio-rustls-tls", "fail-on-err"] }`（rustls，无 native-tls/OpenSSL）；删除 `aliyun` feature 与 `aliyun-oss-rust-sdk`；dev-dependency 增加 `httpmock = "0.7"` |
| 文档 | `lib.rs` 删除「启用 aliyun 即得实机后端」的误导描述，改为 `s3` feature 说明 + 配置方式；新增 `no_run` doctest（按 `cfg(feature = "s3")` 分支，默认 feature 下为空 `main`，`--features s3` 下编译真实 builder 用法）；`s3.rs` 另有模块级 `no_run` doctest |
| 未改动 | `base_store.rs` / `state_store.rs` / `snapshot.rs` / `guard.rs` / `distributed_store.rs` 语义未改；未改其它 crate、未改根 `Cargo.toml` 的 `[workspace.dependencies]` |

### 7.3 `OssClient` 方法级对照
| trait 方法 | `S3OssClient` 实现 |
|---|---|
| `put` | `Bucket::put_object`（覆盖写）；非 2xx 返回 `OssError::S3` / `OssError::S3Http` |
| `get` | `Bucket::get_object`；HTTP 404 或响应体含 `NoSuchKey` → `Ok(None)`；2xx → `Ok(Some(bytes))` |
| `exists` | `Bucket::head_object`；2xx → `true`，404/`NoSuchKey` → `false`，其余 → 错误 |
| `delete` | `Bucket::delete_object`；2xx 或 404 均视为成功（幂等） |
| `list_page` | 真实 `Bucket::list_page(prefix, None, continuation_token, None, Some(max_keys))`；`max_keys` 原样下发（上限 1000，0 返回空页），`continuation_token` 原样下发；映射 `contents` / `is_truncated` / `next_continuation_token`，不是「全量列表切片」 |
| `list_all` | 未覆写，沿用 trait 默认实现（通过 `list_page` + `next_token` 翻页） |
| `shutdown` | 沿用默认 no-op；runtime 随 `S3OssClient` drop 回收 |

### 7.4 测试用例（改前失败原因）
> 改前 `S3OssClient` 与 `s3` feature **不存在**，`tests/s3.rs` 全部用例在编译阶段即失败（`unresolved import agentscope_extensions_oss::S3OssClient`）；不存在「只走新代码路径的空测试」。

**`src/s3.rs` 内联单测（4）**：
- `s3::tests::builder_rejects_blank_bucket`
- `s3::tests::builder_requires_credentials_without_env_fallback`
- `s3::tests::builder_defaults_region_and_path_style_for_custom_endpoint`
- `s3::tests::builder_can_disable_path_style`

**`tests/s3.rs` 真实 HTTP 端到端（`httpmock`，7）**：
- `put_then_get_round_trip`：PUT→GET 往返（改前无 `S3OssClient`，无法编译）
- `get_missing_object_returns_none`：404 + `NoSuchKey` → `Ok(None)`
- `exists_uses_head_and_reports_presence`：HEAD 200/404 → `true`/`false`
- `delete_is_idempotent`：DELETE 204 连续两次 + DELETE 404 均返回 `Ok`
- `list_page_uses_max_keys_and_continuation_token`：两页列表；第一页断言 `max-keys=2`、`list-type=2`、`prefix=logs/` 且拿到 `token-page-2`；第二页 mock **强制要求**请求携带 `continuation-token=token-page-2` 与 `max-keys=2`，否则 404——直接证明 token 与 `max_keys` 真实下发，而非全量切片
- `forbidden_response_maps_to_typed_backend_error`：PUT 403 → 结构化 `OssError::S3`（含 403/AccessDenied 信息）
- `configuration_validation_rejects_missing_bucket_and_credentials`：缺 bucket → `BlankBucket`；缺凭证 / 只有 access key → `MissingCredentials`

### 7.5 测试命令与真实结果
| 命令 | 结果 |
|---|---|
| `cargo test -p agentscope-extensions-oss`（默认 feature） | **41 passed + 1 doc-test**（`agent_state_store` 15 / `base_store` 13 / `guard_and_distributed` 6 / `snapshot` 7 / `s3` 0；doc-test 1），0 failed |
| `cargo test -p agentscope-extensions-oss --features s3` | **52 passed + 2 doc-tests**（内联 `s3` 单测 4 + `agent_state_store` 15 + `base_store` 13 + `guard_and_distributed` 6 + `tests/s3.rs` 7 + `snapshot` 7；doc-test 2），0 failed |
| `cargo check -p agentscope-extensions-oss --features s3` | Finished，本 crate **0 error / 0 warning**（仅 core/harness 既有 warning） |
| 环境 | Windows / PowerShell 5.1；`rustc 1.96.1`、`cargo 1.96.1`；文件均以 UTF-8 写入 |

### 7.6 未覆盖项 / 限制
- **无真实云账号**：未对 AWS S3 / MinIO / Ceph / 阿里云 OSS-S3 做 live 验证；仅用本地 `httpmock` 验证真实 HTTP 请求/响应链路。
- **未校验 SigV4 签名**：`httpmock` 接受任意 `Authorization` 头，签名正确性依赖 `rust-s3` 自身；也未验证 `session_token` 是否实际出现在 `x-amz-security-token`（仅验证配置可传入）。
- **未覆盖虚拟主机风格**：端到端仅走「自定义 endpoint + path-style」；默认 AWS 无 endpoint 的 subdomain-style 路径未做 HTTP 级测试。
- **环境变量回退未直接测试**：测试使用 `env_credentials(false)` 保证确定性，避免并行测试修改全局环境变量；回退逻辑仅代码审阅覆盖。
- **MSRV**：`rust-s3 0.38.0` 声明 `edition = "2024"`（需 rustc ≥ 1.85），而 workspace `rust-version = "1.80.0"`；本轮在 rustc 1.96.1 上验证通过，若需严格 1.80 MSRV 需汇总方另行裁定依赖版本。
- **网关差异**：部分 S3 兼容网关对缺失对象 DELETE 返回 404、对 HEAD 返回 403 等行为可能不同；当前按「404/NoSuchKey = 不存在」处理，未在真实网关矩阵中验证。

## 8. P1 关闭记录（2026-10-26）

> 本轮只改 `crates/agentscope-extensions-oss/**`（含本 tracker），**未动**根 `Cargo.toml` / `agentscope-core` / `agentscope-harness` / 其它 crate。
> **未跑 cargo**（编译/测试由主智能体统一验证），因此下文「用例数」为静态计数，尚未实测。

### 8.1 逐项处置

#### 8.1-1 P1-1 per-key `delete` → 已关闭
- 新增 `OssAgentStateStore::delete_key(user_id, session_id, key) -> OssResult<()>`（`src/state_store.rs`，async 固有方法），一次删除 `.json` / `.list.json` / `.list.hash` 三个对象。
- 与 MONGODB 先例一致（`agentscope-extensions-mongodb/src/state_store.rs` 的 `delete_key`）：**crate 固有方法，不改 core/harness trait**。
- 语义：幂等（对象不存在不报错，`OssClient::delete` 本就幂等，Java 先 `doesObjectExist` 再删是等价优化）；`session_id` 空白 → `OssError::BlankIdentifier("sessionId")`；首个后端错误 `?` 上抛（Java 抛 `RuntimeException`）。
- **改前如何失败**：`delete_key` 不存在 → `tests/agent_state_store.rs` 编译失败（`E0599 no method named delete_key`）；旧用例 `delete_key_removes_state_and_list_objects` 实际只测了 session 级 `delete`（注释自认「emulate key delete」），无任何 per-key 断言。
- 用例（4）：`delete_key_removes_state_and_list_objects`（三对象消失 + 兄弟 key/会话存活 + 随后 session delete 仍清空）、`delete_key_is_idempotent_for_missing_objects`、`delete_key_is_scoped_to_one_user_and_session`、`delete_key_rejects_blank_session_id`。

#### 8.1-2 P1-2 分页零覆盖 + 无分页能力 → 已关闭
- `InMemoryOssClient` 新增 `page_size: Option<usize>` 字段、`with_page_size(n)`（`n` 钳位到 ≥1）与 `page_size()`；`list_page` 实际页大小 = `min(max_keys, cap)`；`max_keys == 0` 直接返回空页且 `truncated=false`（修正了旧实现「`truncated=true` 但无 token」的自相矛盾）。
- `Client::list_all` 仍走默认实现（每次 1000），因此页帽只由替身控制——与真实 OSS/S3 的分页行为一致。
- **改前如何失败**：多页用例写不出来——`list_page(prefix, 1000, None)` 一次返回全部 key、`truncated` 恒 `false`、续传分支不可达；`tests/pagination.rs` 引用 `with_page_size` 会 `E0599`。
- 用例（13，`tests/pagination.rs`）：`page_size_is_exposed_and_clamped_to_at_least_one`、`list_page_returns_first_page_and_continuation_token`、`list_page_resumes_after_continuation_token`、`list_page_last_page_is_not_truncated`、`list_page_exact_boundary_last_page_is_not_truncated`、`list_page_without_matches_returns_empty_page`、`list_page_token_past_last_key_returns_empty_page`、`list_page_zero_max_keys_returns_empty_untruncated_page`、`list_page_honours_caller_max_keys_below_page_cap`、`list_all_aggregates_every_page`、`list_all_returns_empty_vec_for_missing_prefix`、`state_store_list_session_ids_spans_pages`、`state_store_session_delete_spans_pages`。
- 配套 store 侧用例（`tests/base_store.rs`）：`search_spans_multiple_pages`。
- 未做：session `delete` 仍逐 key 删除（未做 Java 的「1000/批 `deleteObjects`」，P2-5 保留）。

#### 8.1-3 P1-3 错误传播弱化（`search` prefix 不匹配）→ 已关闭
- 新增 `OssStore::search_strict(namespace, limit, offset) -> OssResult<Vec<StoreItem>>`（`src/base_store.rs`）：
  - 列表 key 不以 namespace prefix 开头 → `OssError::Backend("listed object key '<key>' is outside namespace prefix '<prefix>'")`（对齐 Java 未加保护的 `substring` 抛 `RuntimeException`）；
  - item JSON 反序列化失败 → `OssError::Json`（旧实现用 `if let Ok(..)` 静默跳过）；
  - 读对象 / 列表本身失败 → `?` 上抛。
- `BaseStore::search`（无错误通道）委托 `search_strict`，错误时 `tracing::error!` + 返回空页；**不再用 `unwrap_or(data_key)` 静默伪造 item key**（改前的错值：整条 object key 被当成 `item_key`）。
- **改前如何失败**：`search_strict_errors_when_listed_key_is_outside_namespace_prefix` 中 `search_strict` 不存在（`E0599`）；即便去掉该调用，旧实现会返回 `item.key == "test/store/ns2/rogue.json"` 而不是报错。`search_strict_rejects_malformed_item_json` 在旧实现下返回空 `Vec`（静默跳过），`Err(OssError::Json(_))` 断言失败。
- 用例（4）：`search_strict_errors_when_listed_key_is_outside_namespace_prefix`、`search_strict_propagates_listing_errors`、`search_strict_rejects_malformed_item_json`、`search_spans_multiple_pages`（后者属 P1-2，一并加在同一文件）。
- 未做：`BaseStore::get`/`put`/`delete` 与 `AgentStateStore::save`/`get`/`get_list` 的静默降级仍未提供 strict 变体（本轮只关闭被点名的 `search`；其余保留为 P1 待办）。

#### 8.1-4 P1-7 误导性 crate 文档 → 已关闭
- 重写 `src/lib.rs` 的「Backend abstraction」段：后端**绝不自动选择**——默认 feature 只有 `InMemoryOssClient`（进程内存）；`s3` feature（默认关）只负责**编译** `S3OssClient`，开启 feature 本身不创建后端、不访问任何 bucket；必须显式构造并配置 bucket（必填）/ region（默认 `us-east-1`）/ endpoint（MinIO / Ceph / R2 / OSS-S3 必需，自定义 endpoint 自动 path-style）/ access key + secret key（可回退 `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`，缺则 `OssError::MissingCredentials`），并补充分页契约与 `with_page_size` 说明。
- 同时修正 `src/client.rs` 模块文档里残留的 `aliyun-oss-rust-sdk` / feature `aliyun` 描述（裁定 8 已删该依赖）。
- **改前如何失败**：纯文档项，无测试可失败；判据是 `cargo doc` 无 warning 且文案与代码一致。`lib.rs` 的 `no_run` doctest 保持原样（默认 feature 编译空 `main`，`--features s3` 编译真实 builder 用法）。

### 8.2 改动文件清单
| 文件 | 改动 |
|---|---|
| `src/state_store.rs` | +`delete_key`（P1-1）；模块文档注明 per-key vs session 级删除 |
| `src/client.rs` | +`InMemoryOssClient::with_page_size` / `page_size`；`list_page` 页帽与 `max_keys=0` 语义；模块文档去掉 `aliyun` 残留（P1-2/P1-7） |
| `src/base_store.rs` | +`search_strict`；`search` 改为委托；修正 `.list.json` 注释；模块文档补「Search error semantics」（P1-3） |
| `src/lib.rs` | 「Backend abstraction」段落重写（P1-7） |
| `tests/agent_state_store.rs` | 旧 `delete_key_*` 用例改为真测 per-key；+3 用例（15 → 18） |
| `tests/base_store.rs` | +4 用例（13 → 17），含 2 个测试内 fake `OssClient`（越界列表 / 列表失败） |
| `tests/pagination.rs` | **新增**，13 用例 |
| `docs/extensions/OSS__GAP_TRACKER.md` | §1/§2/§3.1/§3.2/§4.1 状态 + 本 §8 |

### 8.3 未做 / 仍需保留的待办
- **P1-4（已解决，2026-10-28）**：`OssDistributedStore` 现已产出完整 harness `DistributedStore`（5 槽），见文末执行记录；原「受 harness struct 限制」不再成立。
- **P2-1** 冗余依赖（`futures`/`chrono`/`serde`/`tokio`）、**P2-3** 死错误变体、**P2-4**（注释已一并修正）、**P2-5** 批量删除、**P2-6** 死代码、**P2-7** OSS 状态存储无乐观并发（与 Java 一致）、**P2-8** 零调用访问器：未做。
- **真实云 live 验证**：仍需 OSS/S3 凭据，参见 §7.6。
- **`search` 之外的静默降级**：见 §8.1-3「未做」。

## 2026-10-28 执行记录：`DistributedStore` 复合槽位投影（P1-4 / §4.2-1 已解决）

### 背景

harness `DistributedStore` 新增三个可选复合槽位（`base_store` / `sandbox_snapshot_spec` / `sandbox_execution_guard`）与 `SandboxGuard` trait。本 crate 的 `distributed_store()` 改为 builder 写法并接线全部槽位。

### 投影映射表

| harness 槽位 | 类型 | 来源 | 说明 |
|---|---|---|---|
| `agent_state_store` | `Arc<dyn AgentStateStore>` | `self.oss_agent_state_store()?` | 前缀 `{keyPrefix}state/` |
| `isolation_scope` | `IsolationScope` | 未设置（builder 默认） | 由调用方在 `HarnessAgent` 侧覆盖 |
| `base_store` | `Arc<dyn BaseStore>` | `self.oss_base_store()?`（`OssStore`） | 前缀 `{keyPrefix}store/` |
| `sandbox_snapshot_spec` | `Arc<dyn SandboxSnapshotSpec>` | `self.oss_snapshot_spec()?`（`OssSnapshotSpec`） | 前缀 `{keyPrefix}snapshot/` |
| `sandbox_execution_guard` | `Arc<dyn SandboxGuard>` | `Arc::new(self.guard.clone())`（`OssSandboxExecutionGuard`） | 透明包装 harness guard；新增 `impl SandboxGuard` |

### 代码改动

- `src/distributed_store.rs::distributed_store()`：填满 5 槽（builder，不用 `..Default::default()`）。
- `src/guard.rs`：`impl SandboxGuard for OssSandboxExecutionGuard`，四方法委托固有方法（语义仍是 harness 进程内计数，未改行为）。
- 测试：`tests/guard_and_distributed.rs::distributed_store_fills_every_composite_slot`。

### 验证

- `cargo test -p agentscope-extensions-oss`（默认 feature）→ **62 passed / 0 failed**。
- `cargo test -p agentscope-extensions-oss --features s3` → **71 passed / 0 failed**。

### 未完成 / 语义差异

- 无留空槽位；guard 仍是进程内语义（OSS 无分布式锁后端，与 Java 一致：Java OSS 模块根本没有 guard）。


