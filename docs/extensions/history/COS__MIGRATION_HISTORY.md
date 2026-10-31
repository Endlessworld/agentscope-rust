# COS 迁移执行历史（归档）

> 本文件是 [`../COS__GAP_TRACKER.md`](../COS__GAP_TRACKER.md) 的执行历史归档（截止 2026-10-31）。
> **现行状态以 tracker 为准**；本文件只读取证，不再更新。
> 内容来源：原 GAP_TRACKER 中 §5 之后各节，原文未改写（节标题：6. 静态复核记录（2026-10-22） · 附录 A：`list_hash` 跨语言约束（裁定 7 登记，2026-10-22） · 2026-10-28 执行记录：`DistributedStore` 复合槽位投影（§2.2 / P1-ⓐ 已解决） · 2026-10-28 第二轮执行记录：分段上传 / 预签名 URL / STS · 5. 2026-10-31 收口执行记录（P2-③ `NotFound` 预留变体 / `needs_full_rewrite` 约束复核））。

## 6. 静态复核记录（2026-10-22）

### 6.1 复核方式
- **静态核对，零编译、零测试执行**（`cargo build` / `cargo test` 全程未运行）。
- 手段：逐文件通读 Java 5 个主类 + 4 个测试类 / Rust 9 个 `src/*.rs`；`rg` 检索符号与依赖；`git show e01809d:...` 取历史实现复核旧行为；`git log -S "sha2"` 复核依赖收敛。
- 复核者：静态复核子智能体（只读 + 仅改本 tracker）。

### 6.2 实测统计（静态口径）

| 项 | Java | Rust |
|---|---|---|
| 主代码文件 | 5 个 `.java` | 9 个 `src/*.rs`（无 `tests/` 目录） |
| 主代码总行 | 971 | 1719 |
| 主代码非空行 | **882** | **1517** |
| 主代码去空去注释行 | 709 | 1266 |
| 测试文件 | 4 个测试类 | 0（全部内联于 `src/`） |
| 测试总行 | 898 | — |
| 测试非空行 | **760** | — |
| 测试用例数 | **62 个 `@Test`**（无 `@ParameterizedTest`/`@TestFactory`） | **31**（19 × `#[test]` + 12 × `#[tokio::test]`） |
| doctest | — | **2 个，均为 `no_run`**（`lib.rs:27`、`agent_state_store.rs:32`）→ 只编译不执行 |

- Rust 31 test 分布：`agent_state_store` 12、`base_store` 5、`distributed_store` 5、`snapshot` 5、`guard` 3、`object_store` 1、`layout` 0、`error` 0、`lib` 0。
- Java 62 `@Test` 分布：`CosAgentStateStoreTest` 20、`CosRemoteSnapshotClientTest` 18、`CosBaseStoreTest` 16、`CosDistributedStoreTest` 8。
- **行数/类数核对结论**：Java 主 882 / 测 760 与 rollup 一致（**属实**）；Rust 9 文件 / 1517 非空行 / 31 test 属实（rollup 记「31+2」中的 2 为 doctest，口径需注明为 `no_run`）。

### 6.3 对齐结论表

| Java 类 → Rust 对应 | 存在？ | 行为覆盖判定 |
|---|---|---|
| `CosAgentStateStore` → `CosAgentStateStore` | ✅ | key 布局（`.json`/`.list.json`/`.list.hash`）、`__anon__`、前缀归一化（`\`→`/`、去首 `/`、补尾 `/`）、blank 默认值 **全部对齐**；`save_list` 的 hash/追加判定已委托 core。**缺 `delete(user,session,key)` 与 `close()`**（core trait 无此二方法） |
| `CosBaseStore` → `CosStore`（`BaseStore`） | ✅ | `{ns}/{key}.json` + `.version`、version 0→+1、`put_if_version(0)`=create-if-absent、`search` 排序+offset/limit、key 去前导 `/` **全部对齐**；`supports_atomic_session_storage` 两边均 `false` **一致**；Rust 额外加进程内 mutex（强化） |
| `CosDistributedStore` → `CosDistributedStore` | ✅ | 子前缀 `state/`/`store/`/`snapshot/` 对齐；默认前缀 `agentscope/` 对齐。差异：Java 每次访问新建实例，Rust 构造时缓存（无害）；Rust 多提供真实 guard（Java 为 `noop()` 默认，**增强**） |
| `CosRemoteSnapshotClient` → `CosRemoteSnapshotClient` | ✅ | `{prefix}{id}.tar` 对齐；prefix 归一化对齐；Rust 额外做 `/`、`\`、`..`、`\0` 校验（**加固**） |
| `CosSnapshotSpec` → `CosSnapshotSpec` | ✅ | 均为「包裹 harness `RemoteSnapshotSpec`」结构，语义等价 |
| （Java 无）→ `CosSandboxExecutionGuard` | ✅ | 包装 harness guard，保留 `try_enter`/`release`/`is_held`/`active_count`；**未**实现跨进程协调（已记为设计决定不迁移） |
| `com.qcloud.cos.COSClient` → `CosObjectStore` + `InMemoryCosObjectStore` | ⚠️ | 抽象层齐备，**真实后端完全缺失**（含签名） |

### 6.4 新发现缺口

| 编号 | 优先级 | 缺口 | 归属 |
|---|---|---|---|
| P1-① | P1 | **core `needs_full_rewrite` 连续第二次追加仍判全量重写**：存的是整表哈希、比的是 `existing_count` 前缀哈希 → 追加一次后 `stored_hash`=旧整表哈希，下次比前缀（较短）必不等。**Java `ListHashUtil` 同款问题**，故本 crate 与 Java 一致、非本 crate 缺陷 | core（汇总方） |
| P1-② | P1 | ~~**测试覆盖缺口**：31 test vs Java 62 `@Test`（≈50%）~~ → **已补齐（2026-10-28，第二轮后 105）**：现 **105** test（`httpmock` HTTP 层 + 官方签名金标向量 + key 字面量断言 + 分段/预签名）。Java 独有用例（`putStripsLeadingSlashFromKey`、`deleteSingleKey_removesStateListAndHash`、`builderRejectsNullClient/BlankBucket`、`close_shutsDownClient`、`emptyNamespace_usesPrefixOnly`、`nullPrefix_treatedAsEmpty`、`prefixWithLeadingSlash_isStripped`、批量删除）均有对应：`key_with_leading_slash_is_normalized`、`builder_rejects_missing_object_store`、`builder_rejects_invalid_configuration`、`close_is_idempotent_and_blocks_further_requests`、`empty_namespace_uses_the_prefix_only`、`prefix_edge_cases_normalize_like_java`、`delete_many_*` | 本 crate（**已完成**） |
| P1-③ | P1 | **`base_store` 无法经 `HarnessAgent::builder().distributed_store(...)` 自动接线**：harness `DistributedStore` 仅 2 字段，而 Java `HarnessAgent` 会用 `distributedStore.baseStore()` 自动注入 `RemoteFilesystemSpec`/`StoreBackedPeriodicGate`/`SkillUsageStore` | harness（汇总方，汇总 tracker §2.2 已裁定） |
| P0 | — | **无新增 P0**。核心链路（state / base store / snapshot / guard / 组合者）产出契约齐全且行为对齐 | — |
| P2-① | P2 | ~~Java `AgentStateStore.delete(user,session,key)`（删 3 类对象）与 `close()` 在 Rust core trait 中**不存在**~~ → **已完成（2026-10-29）**：core trait 新增 `delete_key`（默认返回错误），`CosAgentStateStore` 覆盖为一次批量删除 `.json`/`.list.json`/`.list.hash`；`close()` 定为**不迁移**（Rust 后端共享 `Arc<dyn ObjectStore>`，生命周期由 crate 管理，`CosAgentStateStore::close()` 已具备等价能力） | core + 本 crate（**已完成**） |
| P2-② | P2 | ~~`Cargo.toml` 冗余依赖：`serde` / `futures` / `chrono` 声明但**零使用**~~ → **已删除（2026-10-28）**：三者均从本 crate `Cargo.toml` 移除，复核 `rg '\bserde\b|futures|chrono'` 仅余 `synchronous` 注释子串 | 本 crate（**已完成**） |
| P2-③ | P2 | ~~`CosError::Backend` / `NotFound` 变体从未被构造~~ → `Backend` 现由 HTTP 传输层构造；新增 `Service{status,code,message}`（403/404 区分）与 `Closed`；`NotFound` 仍无构造点，**保留为预留变体**（`From<CosError> for core Error` 走 `to_string()`，删除会动公开 API） | 本 crate（**部分完成**） |
| P2-④ | P2 | snapshot id 校验 Rust 比 Java **更严**（Java 仅拒 blank）→ 加固，非缺失，仅需在文档中明示 | 已在本文件 §2 明示 |
| P2-⑤ | P2 | ~~根 `[workspace.dependencies]` 的 `object_store = "0.14"` 全 workspace 无 crate 引用且不在 `Cargo.lock`~~ → **已删除（2026-10-29）**，见汇总 tracker §4.15.2 | 汇总方（**已完成**） |
| P2-⑥ | P2 | 错误传播语义差异：Java `save`/`get`/`getList` 失败抛 `RuntimeException`；Rust `AgentStateStore` trait 返回 `()` → Rust 侧 `tracing::error!` 吞掉错误（`agent_state_store.rs:134/172/178`）。**已裁定（2026-10-29）**：不改旧签名（会动 7 个存储 crate 全部实现与调用点）；需要错误通道的路径改用新方法（如 `delete_key` 返回 `Result`） | core trait 签名决定（**已完成**） |
| P2-⑦ | P2 | ~~真实适配器还需覆盖分页 `listObjects`（marker 循环）与批量 `deleteObjects`（1000/批）~~ → **已覆盖（2026-10-28）**：trait 新增 `list_page`（`continuation-token`/`max-keys`）与 `delete_many`（HTTP 后端 `POST /?delete` 1000/批 + `Content-MD5`），`list_prefix` 自动翻页、`CosAgentStateStore::delete` 改走批量删除 | 本 crate（**已完成**） |

### 6.5 勘误列表（原文 → 实际）

| # | 原文 | 实际 | 处置 |
|---|---|---|---|
| 1 | §4.1「**31** lib unit tests + 2 doctests = **33 passed**」 | 31 = **19 × `#[test]` + 12 × `#[tokio::test]`**；2 个 doctest 均为 **`no_run`**（只编译不执行）→ 实际执行 **31**，非 33 | §4.1 已改写并注明口径 |
| 2 | §1「**31** 内联 test + 2 doctests」 | 数字正确，但未区分 `#[test]`/`#[tokio::test]`，且未说明 doctest 为 `no_run` | §1 已补明细 |
| 3 | §4.1「31 passed / 0 failed + 2 doctests」（§3.1 单元测试行） | 同上 | §3.1 已改写 |
| 4 | §2.1「`ListHashUtil` → `agentscope_core::state::{compute_hash, needs_full_rewrite}`（`state::list_hash`）」 | 正确，但**遗漏** core `compute_hash` 用 `DefaultHasher`（SipHash-1-3）而 Java 用 `String.hashCode()` → 两侧 hash 值**永不相等**（跨语言不兼容） | §2.1 + §2.2 已补「跨语言 hash 值」行 |
| 5 | §3.1「修复：纯追加不再恒判全量重写」表述为已彻底修复 | 属实（单次追加已修），但**连续第二次追加**仍判全量重写（core Case 3 结构所致，Java 同款） | §2.2 + §3.1 已补限定 + P1-① |
| 6 | §3.2「真实 COS SDK 适配器 → 需 … HTTP 签名（自实现或接第三方）」 | 表述偏轻：实际**既无 SDK 也无自建签名**，且 workspace 无 `sha1`/`hmac` 依赖 → **从零开始** | §3.2 + §4.2 第 1 条已改写 |
| 7 | §2 映射表未记录 Java `CosDistributedStore` 的 `sandboxExecutionGuard()` 用 harness 默认 `noop()` | 已核实（Java 该类**无** `sandboxExecutionGuard` 覆写）→ Rust 提供真实 guard 属**增强**，非缺失 | §2 映射表已补 |
| 8 | §2 映射表未记录 Java `CosBaseStore` 未覆写 `supportsAtomicSessionStorage()` | 已核实（Java `BaseStore` 默认 `false`，`CosBaseStore` 未覆写）→ Rust 亦 `false`，**一致** | §2 映射表已补 |
| 9 | §4.2 第 3 条「harness `DistributedStore` 字段不足」表述笼统 | 已核实 Java 侧 `HarnessAgent` 会用 `distributedStore.baseStore()` **自动接线**多处 → Rust 侧丧失该自动接线能力 | §4.2 第 3 条已补 + P1-③ |
| 10 | §4.1「已知问题：无编译错误；真实 COS SDK 未接入」 | 遗漏 3 项：① `serde`/`futures`/`chrono` 冗余依赖 ② `CosError::Backend`/`NotFound` 死变体 ③ 根 `object_store` 依赖全局无引用 | §4.1「已知问题」已补 |
| 11 | §3.2「`ListHashUtil` 精确对齐」残留 PS2 表述 | 「跨语言共读同一 bucket 时首次写入会自愈」**属实**（core Case 3 必不等 → 全量重写 → hash 被覆盖）；但需补充「**两侧 hash 值永不相等**」的根因说明 | §2.2 已补 |

### 6.6 复核结论

> **本节为 2026-10-22 静态复核快照**（当时零编译、零测试）。其中「真实 SDK 适配」条目已被 §4.3（v5 签名 + HTTP 后端）与 §4.4（分段/预签名/STS）取代，保留作历史对照。

- **§4.2「真实 SDK 适配」现状判定**：Rust 侧为**自建抽象 trait**（`CosObjectStore`），**非**自建签名、**非**第三方 SDK；`q-sign-algorithm` 签名**完全未实现**（静态可判定：`rg 'q-sign-algorithm|Hmac|Sha1|sha1|secretId'` 全 crate 零命中）。**→ 已于 2026-10-28 推翻：`signature.rs` + `http_object_store.rs` 已落地，见 §4.3。**
- **`list_hash` 收敛到 core**：**属实**。本 crate 无平行实现（`rg 'fn compute_list_hash|fn needs_full_rewrite'` → 零命中），`sha2` 已从 `Cargo.toml` 移除（`git log -S "sha2"` 确认提交 `daa700d`）。
- **「追加恒全量重写」偏离修复**：**属实**。历史实现经 `git show e01809d:.../agent_state_store.rs` 确认为 `!compute_list_hash(values).starts_with(h)`，全量 SHA-256 无前缀性质 → 恒 `true`；现委托 core 后单次纯追加返回 `false`（回归测试内联保留）。
- **B 波次统一产出契约**：**5/5 齐全**（`CosDistributedStore` / `CosAgentStateStore` / `CosStore` / `CosSnapshotSpec` / `CosSandboxExecutionGuard`），且均可从 `CosDistributedStore` 访问（`agent_state_store()`/`base_store()`/`sandbox_snapshot_spec()`/`sandbox_execution_guard()` + `shared_*` trait-object 便捷方法）。
- **建议 rollup 状态值**：**保持 `已完成`**（rollup 行 12 现值「已完成（build + test 通过，31+2 test；…）」）。理由：P0 核心链路无缺口、产出契约 5/5 齐备、list_hash 收敛与追加偏离修复均经静态复核属实；剩余项均为 P1/P2 且其中 P1-①/P1-③ 属 core/harness 层（汇总方）、P1-② 属测试覆盖增强而非功能缺失。**唯一需在 rollup 同步的措辞修正**：「31+2 test」应表述为「**31 unit test（19 `#[test]` + 12 `#[tokio::test]`）+ 2 `no_run` doctest（只编译）**」，并追加「**真实 COS SDK 适配（含 `q-sign-algorithm` 签名）仍未开始**」。 （2026-10-29 勘误：本段为 2026-10-22 快照，已失效——现为 **105 passed**，真实 COS v5 签名适配器 / 分段上传 / 预签名 / STS 均已落地，见 §5/§6；rollup 现值见汇总 §1.1）


## 附录 A：`list_hash` 跨语言约束（裁定 7 登记，2026-10-22）

> 来源：`EXTENSIONS_GAP_TRACKER.md` §4.5 裁定 7。core `compute_hash` 用 `DefaultHasher`（SipHash-1-3），Java 用 `String.hashCode()`，两侧 hash **永不相等**。

- **约束 1（可接受）**：单语言内**自愈**——首次写入走全量重写，数据无损，**不需要迁移脚本**。
- **约束 2（禁止）**：**不得让 Java 与 Rust 混部共享同一份带 list 的状态**。否则每次写入都退化为全量重写：功能正确，但增量优化完全失效（性能退化）。
- **触发条件**：若未来确需混部，再实现 Java `String.hashCode()` 的 32 位截断语义，并在两侧加交叉校验用例。
- **红线**：不要为了让两侧相等而改 `compute_hash` 的算法——那会让**已落库**的 hash 全部失配，触发一次性全量重写风暴。

## 2026-10-28 执行记录：`DistributedStore` 复合槽位投影（§2.2 / P1-ⓐ 已解决）

### 背景

harness `DistributedStore` 新增三个可选复合槽位（`base_store` / `sandbox_snapshot_spec` / `sandbox_execution_guard`）与 `SandboxGuard` trait。本 crate 的 `to_harness_distributed_store()` 从「只投 agent_state_store」改为全量投影。

### 投影映射表

| harness 槽位 | 类型 | 来源 | 说明 |
|---|---|---|---|
| `agent_state_store` | `Arc<dyn AgentStateStore>` | `self.agent_state_store.clone()` | `CosAgentStateStore`，前缀 `{prefix}state/` |
| `isolation_scope` | `IsolationScope` | 未设置（builder 默认） | 由调用方在 `HarnessAgent` 侧覆盖 |
| `base_store` | `Arc<dyn BaseStore>` | `self.base_store.clone()` | `CosStore`，前缀 `{prefix}store/` |
| `sandbox_snapshot_spec` | `Arc<dyn SandboxSnapshotSpec>` | `self.snapshot_spec.clone()` | `CosSnapshotSpec`，前缀 `{prefix}snapshot/` |
| `sandbox_execution_guard` | `Arc<dyn SandboxGuard>` | `Arc::new(self.execution_guard.clone())` | `CosSandboxExecutionGuard`；新增 `impl SandboxGuard` |

### 代码改动

- `src/distributed_store.rs::to_harness_distributed_store()`：填满 5 槽（builder，不用 `..Default::default()`）。
- `src/guard.rs`：`impl SandboxGuard for CosSandboxExecutionGuard`，四方法委托固有方法（仍委托 harness `SandboxExecutionGuard`，未改行为）。
- 测试：`src/distributed_store.rs::harness_projection_fills_every_composite_slot`。

### 验证

`cargo test -p agentscope-extensions-cos` → **74 passed / 0 failed**（第一轮基线；第二轮后为 **105 passed**，见 §4.4）。

### 未完成 / 语义差异

- 无留空槽位。
- 分布式协调仍是 strategy-deferred（guard 只是 harness 进程内计数 + 保留 object store），与 Java COS `noop()` 默认相比属**增强**，语义未变。

## 2026-10-28 第二轮执行记录：分段上传 / 预签名 URL / STS

### 新增文件

| 文件 | 规模 | 内容 |
|---|---|---|
| `src/multipart.rs`（新） | 230 非空行 / 8 test | 常量（阈值/分片尺寸上下限/最大分片数/默认并发）、`MultipartPlan::range`、`plan_parts`、`normalize_etag`、`complete_multipart_body` |
| `src/presign.rs`（新） | 217 非空行 / 6 test | `presign_url`（复用 `signature::sign_request` 的 `authorization`）、`resolve_expire_secs`、`MAX_PRESIGN_EXPIRE_SECS` |

### 改动文件

| 文件 | 改动 |
|---|---|
| `src/http_object_store.rs` | `CosHttpConfig`/builder 新增 `multipart_threshold`/`multipart_part_size`/`multipart_concurrency` + 校验；`put` 阈值切换；`put_multipart` + `initiate/upload_part/complete/abort`；`presign_get`/`presign_put`；`with_credentials`；`query_string`/`escape_xml` 改 `pub(crate)`；+17 test |
| `src/lib.rs` | `pub mod multipart; pub mod presign;` + 常量/`MAX_PRESIGN_EXPIRE_SECS` 导出；文档段重写 |
| `src/distributed_store.rs` | 删除 test 模块内无用的 `SandboxGuard` 导入（清 clippy 告警，与本轮功能无关） |
| `Cargo.toml` | **未改**（无新依赖：`tokio` 的 `JoinSet` 已在 `rt-multi-thread` 内） |

### 请求序列与断言维度

| 步骤 | 请求 | 签名 `q-url-param-list` | 断言 |
|---|---|---|---|
| initiate | `POST /{key}?uploads`（`Content-Type: application/octet-stream`） | `uploads` | 响应 XML `<UploadId>`；缺失/空 → `CosError::Backend("… no UploadId")` |
| part | `PUT /{key}?partNumber=N&uploadId=..` | `partnumber;uploadid` | 请求体逐字节相等；响应头 `ETag` 去引号（`normalize_etag`）；缺失 ETag → Backend 错误 |
| complete | `POST /{key}?uploadId=..`（`application/xml`） | `uploadid` | 完成 XML 按 partNumber 升序、`<ETag>"…"</ETag>` 引号化并 XML 转义 |
| abort | `DELETE /{key}?uploadId=..` | `uploadid` | 分片/完成失败后必发；`404 NoSuchUpload` 视为成功；abort 自身失败仅 `tracing::warn!`，不掩盖原错误 |

### 测试清单（新增 31 = 8 + 6 + 17）

- `multipart::tests`（8）：`defaults_match_the_java_transfer_manager`、`configured_part_size_is_used_when_it_covers_the_object`、`part_size_grows_to_stay_within_the_part_count_cap`、`plan_rejects_illegal_part_sizes_and_bodies`、`plan_accepts_boundary_sizes`、`etags_are_unquoted`、`complete_body_lists_parts_in_order`、`complete_body_escapes_etags`。
- `presign::tests`（6）：`url_reuses_the_authorization_chain`、`only_host_is_signed`、`method_changes_the_signature`、`extra_parameters_are_signed_and_encoded`、`object_paths_are_encoded_for_the_url`、`expiry_is_validated_and_defaults_to_the_client_setting`。
- `http_object_store::tests`（17）：`put_below_threshold_uses_a_single_put`、`put_at_threshold_switches_to_multipart`、`multipart_uploads_sorted_parts_and_completes`（2.5 MiB / 3 片 / 并发 2，逐片字节相等）、`multipart_aborts_when_a_part_fails`、`multipart_aborts_when_complete_is_rejected`、`multipart_abort_tolerates_a_missing_upload`、`multipart_initiate_failure_does_not_abort`、`multipart_initiate_requires_an_upload_id`、`multipart_rejects_an_empty_body`、`multipart_settings_are_validated_and_exposed`、`with_credentials_swaps_the_session_token`、`presign_get_builds_a_complete_signed_url`、`presign_get_signature_is_pinned`（**固定时钟 + 公网域名，整条 URL 字面量钉位**：`…&q-signature=78d1df6ab1f8351625eab6f5f0ea072d382e4dc7`，已用 PowerShell/.NET HMACSHA1/SHA1 **独立重算交叉验证**）、`presign_expiry_is_honoured_and_validated`、`presign_put_signs_the_put_method`、`presign_includes_the_session_token_as_a_query_parameter`、`presign_is_rejected_after_close`。

### 反向验证（逐个回退实现 → 观察 FAILED → 恢复）

| # | 回退 | 结果 |
|---|---|---|
| 1 | `put_multipart` 失败路径删掉 `abort_multipart` | `cargo test -p agentscope-extensions-cos multipart_` → **6 passed / 2 failed**（`multipart_aborts_when_a_part_fails`、`multipart_aborts_when_complete_is_rejected`）→ 已恢复 |
| 2 | `should_use_multipart` 的 `>=` 改 `>` | `put_at_threshold_switches_to_multipart` → **0 passed / 1 failed** → 已恢复（Java `shouldUseMultipartUpload` 也是 `>=`） |
| 3 | `etags.sort_by_key(part_number)` 改 `Reverse` | `multipart_uploads_sorted_parts_and_completes` → **0 passed / 1 failed** → 已恢复 |
| 4 | 预签名 STS token 不再进 query 参数 | `cargo test -p agentscope-extensions-cos presign_` → **5 passed / 1 failed**（`presign_includes_the_session_token_as_a_query_parameter`）→ 已恢复 |

### 验证结果

- `cargo test -p agentscope-extensions-cos` → **105 passed / 0 failed / 0 ignored**（+3 个 `no_run` doctest，0 failed）
- `cargo clippy -p agentscope-extensions-cos --all-targets` → 本 crate **零告警**；`cargo fmt -p agentscope-extensions-cos -- --check` → **通过**

### 语义差异 / 未做（如实记录）

- **Java 模块自身没有 multipart / presign 代码**（只用 `COSClient` 单次 `putObject`，单对象上限 5 GiB）。因此本次不是“照搬 Java 实现”，而是**对齐 Java 侧所依赖的 SDK 默认行为**（`TransferManagerConfiguration` / `TransferManagerUtils` 字节码证据见 §3.3.1）+ COS 服务端限制。
- 并发默认 1（对齐 `createDefaultExecutorService` 的单线程池）；需要并行时显式 `multipart_concurrency(n)`（`JoinSet` 按 chunk 限并发）。
- 只签 `host`（预签名），因此不支持需要客户端发送自定义头的场景；`response-*` 类参数可经 `presign_url` 的 `params` 扩展（`presign_get/put` 目前不暴露——保持 API 最小）。
- STS **不做自动刷新**：token 过期后调用方需 `with_credentials(...)` 换新（共享连接池与 `closed` 标志）。
- 无 `ListParts` 断点续传、无分片级重试（失败即 abort 回滚）、无上传进度回调、`Upload Part`/`Complete` 不发送 `Content-MD5`（COS 文档标可选）；内存峰值约 2× 对象大小（整体副本 + 单片副本），未做真正流式上传。

## 5. 2026-10-31 收口执行记录（P2-③ `NotFound` 预留变体 / `needs_full_rewrite` 约束复核）

> 范围：仅 `crates/agentscope-extensions-cos/**` 与本文件；未动根 `Cargo.toml`/core/harness。
> **未跑 cargo**（按约定由主智能体统一验证）。

### 5.1 逐项处置

| 条目 | 处置 | 源码级依据 |
|---|---|---|
| `CosError::NotFound` 预留变体（§4.1 / §3.2 残留 P2-③） | **已关闭**（接线而非删除） | `src/snapshot.rs::download`：`Ok(None) => Err(CosError::NotFound(key).into())`；`impl From<CosError> for core::Error`（同文件）将错讯固定为 `Error::Tool("snapshot not found: {key}")`，与旧内联文案（`Snapshot not found in COS:`）语义一致但可分类 |
| `needs_full_rewrite` 约束复核（总 tracker 硬约束：`stored_hash == None && existing_count == 0` 必须返回 `false`，且不得用 SHA-256 前缀比较） | **满足，无需改代码** | core `state::list_hash::needs_full_rewrite` 仅三条分支：缩量 → `true`；`stored_hash.is_none() && existing_count > 0` → `true`；否则 `has_changed(compute_hash(prefix), stored_hash)`，而 `has_changed(_, None) == false` ⇒ 空数据无 hash ⇒ `false`。本 crate `save_list` 的 `existing_count` 取自**列表实体长度**、与 hash 对象解耦（`agent_state_store.rs` 注释已明述），且全 crate `rg 'fn compute_hash|fn needs_full_rewrite|starts_with\('` 零命中（无本地实现、无前缀比较） |
| per-key `delete` 与 core trait | **已完成** | `agent_state_store.rs` 内 `async fn delete_key(...) -> core Result<()>` 直接写 trait 方法（非仅固有方法），一次 `delete_many` 删 `.json`/`.list.json`/`.list.hash`，错讯前缀 `COS per-key delete failed:`；用例 `delete_key_removes_value_list_and_hash_only` |
| `close()` 无 trait 对应 | **已完成（设计决定，不迁移）** | **依据**：core `AgentStateStore` 无 `close`（生命周期由持有 `Arc<dyn CosObjectStore>` 的调用方管理）；`CosAgentStateStore::close()` 作为固有方法委托 `CosObjectStore::close()`，不强迫 trait 变更 |

### 5.2 改动与用例

| 文件 | 改动 |
|---|---|
| `src/snapshot.rs` | `download` 缺失分支改 `CosError::NotFound`；新增用例 `download_missing_snapshot_reports_not_found`（同时断言 `CosError::NotFound` 的 `Display` 文案） |

用例数：`snapshot` 5 → **6**（2 `#[test]` + 4 `#[tokio::test]`）；整包 **105 → 106** lib unit tests（3 个 `no_run` doctest 不变）。

### 5.3 未完成 / 残留（沿用 §4.1 / §4.2）

- **跨模块**：core `needs_full_rewrite` Case 3 在「连续第二次纯追加」时仍判全量重写（与 Java 同款，非本 crate 缺陷）——需 core 层裁定，见回执「跨模块需求」。
- 分片上传长尾（`ListParts` / 分片级重试 / 进度回调）、STS 自动刷新、预签名只签 host：均已在 §4.2/§4.4 记为设计边界，本轮不涉。
- 真实 COS 云 live 验证仍需凭据。
