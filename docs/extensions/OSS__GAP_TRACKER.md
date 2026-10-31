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
| 当前状态 | **已完成**（P1/P2 清零；2026-10-31 实测 64 passed / 0 failed（默认，+1 doc-test）/ 75（`--features s3`，+2 doc-test））—— 契约 5/5 齐全，`DistributedStore` 五槽已可完整产出（§4.2-1 已关闭）；真实剩余见 §4.1。 |

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
  - `tests/common/mod.rs`、`tests/agent_state_store.rs`、`tests/base_store.rs`、`tests/snapshot.rs`、`tests/guard_and_distributed.rs`、`tests/s3.rs`（feature `s3`）、**`tests/pagination.rs`（history/OSS__MIGRATION_HISTORY.md-2 新增）**
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

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-oss` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-oss` / `cargo test -p agentscope-extensions-oss` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-oss/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. 执行历史（已归档）

- 执行记录 / 静态复核 / 勘误 / 反向验证明细：[`history/OSS__MIGRATION_HISTORY.md`](./history/OSS__MIGRATION_HISTORY.md)
- 本文件只保留现行状态（§0–§5）；历史条目一律写进上述归档文件，不再回填本文。
