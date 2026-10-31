# MEM 迁移执行历史（归档）

> 本文件是 [`../MEM__GAP_TRACKER.md`](../MEM__GAP_TRACKER.md) 的执行历史归档（截止 2026-10-31）。
> **现行状态以 tracker 为准**；本文件只读取证，不再更新。
> 内容来源：原 GAP_TRACKER 中 §5 之后各节，原文未改写（节标题：静态复核记录（2026-10-22） · 7. 执行记录（2026-10-28） · 2026-10-31 loop 收口记录）。

## 静态复核记录（2026-10-22）

### 6.1 复核方式
- **纯静态**：只读源码 + 文本统计，**未运行 `cargo build` / `cargo test`**，未修改任何 Rust 代码。
- 统计口径：行数 = `Get-Content` 总行数；「非空」= 剔除空行；「代码」= 再剔除 `//`、`/* */`、`*` 注释行（仅用于交叉验证）。
- 测试计数：正则 `^\s*#\[test\]` 与 `#\[tokio::test`，逐文件求和；Java 侧用 `@Test` 计数。
- 一致性核对：逐条比对 §2 的「Java 类 → Rust 对应」映射是否真实存在于源码（类型名 + 关键方法 + 行为）。

### 6.2 实测统计
| 项 | 实测值 | 与 tracker 原声明 |
|---|---|---|
| Rust 源文件数 | **15**（`src/*.rs` 4 + `src/{mem0,bailian,reme}/*.rs` 各 3~4） | ✅ 一致 |
| Rust 总行数 | **2993**（生产 2045 + 内联测试 948；非空 2697） | ⚠️ 原写「~2100 行」，实为总行数 2993 / 生产 2045；已更正 |
| Rust 测试函数 | **66**（`#[test]` 40 + `#[tokio::test]` 26） | ✅ 与「66 test」完全一致 |
| Rust 测试分布 | `bailian/memory.rs` 17、`mem0/memory.rs` 15、`reme/memory.rs` 12、`mem0/dto.rs` 11、`mem0/client.rs` 6、`error.rs` 3、`http.rs` 2；`bailian/dto.rs`、`reme/dto.rs`、`bailian/client.rs`、`reme/client.rs`、三个 `mod.rs`、`lib.rs` 均 **0** | ✅ 逐文件复核一致（17+15+12+11+6+3+2 = 66） |
| Java 主代码 | 25 文件 / **5543** 行（非空 4821 / 纯代码 2596） | ✅ 原「~4821 行」= 非空行，口径一致 |
| Java 测试 | 18 文件 / **6850** 行（非空 5746 / 纯代码 5322）/ **326** 个 `@Test` | ✅ 原「~5746 行」= 非空行，口径一致；`@Test` 数原未记录，已补 |
| Java 子模块类数 | mem0 **9**、bailian **8**、reme **8**（共 25） | ⚠️ 原 reme 写「7 类」，实为 8；已更正 |
| Java 测试类数 | mem0 8、bailian 8、reme 2（共 18） | ✅ 一致 |
| Java `@Test` 分布 | mem0 **143**（8 类）、bailian **141**（8 类）、reme **42**（2 类） | — |
| Java DTO 测试类 | **12 类** / **161** 个 `@Test`（mem0 6 类 89 个、bailian 6 类 72 个；reme **0** 个 DTO 测试类） | ⚠️ §6.4 原写「16 个 DTO 测试类、约 150 个 `@Test`」，实为 12 类 / 161 个；见 E13 |

### 6.3 逐子模块对齐结论

| 子模块 | Java 类数 | Rust 现状 | 结论 |
|---|---|---|---|
| **mem0** | 9 | 9 类全部落地：`Mem0Message` / `Mem0ApiType` / `Mem0AddRequest` / `Mem0AddResponse` / `Mem0SearchRequest` / `Mem0SearchResponse` / `Mem0SearchResult` / `Mem0Client` / `Mem0LongTermMemory` | **等价（2 处差异）**。字段名、`NON_NULL` 语义、`filters` 的 `ALWAYS`、`user_id` 镜像、Java 无参默认值（`infer=true`/`output_format=v1.1`/`async_mode=true`/`immutable=false`）、双端点（`/v1/memories/`+`/v2/memories/search/` vs `/memories`+`/search`）、双鉴权头（`Authorization: Token` / `X-API-Key`）、v1.0 裸数组 + v1.1 `results` 双响应形状、`top_k=5`、metadata 合并进 filters、`<compressed_history>` 过滤、`User/System→user`、`Assistant/Tool→assistant` —— **全部逐条核实一致**。差异：(a) Java `convertToMem0Message` **不填 `name`**，Rust 会带出 `Msg.name` → 外发报文多一个 `name` 键（§6.4 新增 P2）；(b) Java `Builder.build()` 校验 `apiBaseUrl` 非空，Rust 全链**无该校验**（§3.2 新增 P1）。另：「错误是否可见」（`retrieve` 严格 vs `retrieve_lossy` 吞错）属 API 设计取舍，已记录 |
| **memory-bailian** | 8 | 8 类全部落地：`BailianMessage` / `BailianAddRequest` / `BailianAddResponse` / `BailianSearchRequest` / `BailianSearchResponse` / `BailianMemoryNode` / `BailianMemoryClient` / `BailianLongTermMemory` | **等价（1 处差异）**。`meta_data` vs `metadata` 命名差异、`snake_case` 覆盖、`Bearer` 鉴权、key/baseUrl 空白即拒、`top_k=10`/`min_score=0.3`、`enable_rerank/judge/rewrite` 默认 false、`project_id`→`project_ids` 单元素、仅 `User`/纯 `Assistant` 可记录、`blank` 与 `trim().is_empty()` 判定、节点 content 非空才拼接 —— **全部核实一致**。差异：Java `AutoCloseable.close()` 无对应；`BailianAddResponse` 的 Java 内嵌同名 `BailianMemoryNode`（无 `created_at/updated_at`）在 Rust 侧复用顶层 `BailianMemoryNode`（多两个可选字段，反序列化向后兼容）；另 Rust `BailianMemoryClient` 会剥掉 base URL 末尾 `/` 而 Java 不会（Rust 更宽容，属**良性差异**）；`BailianLongTermMemoryConfig.api_key` 在 Rust 生产代码中**从未被读取/校验**（§6.4 新增 P2） |
| **reme** | 8 | 8 类全部落地：`ReMeMessage` / `ReMeTrajectory` / `ReMeAddRequest` / `ReMeAddResponse` / `ReMeSearchRequest` / `ReMeSearchResponse` / `ReMeClient` / `ReMeLongTermMemory`（另提独立 `ReMeMemoryItem`/`ReMeAddMetadata`/`ReMeUpdateResult`/`ReMeSearchMetadata`） | **等价（1 处差异）**。`workspace_id` 复用 `userId`、`top_k=5`、无鉴权头、空/`null` 响应体回退 `{}`、`retrieve` 优先 `answer` 再回落 `memory_list[].content` 换行拼接、仅 `User`/纯 `Assistant`、`<compressed_history>` 过滤 —— **全部核实一致**。差异：Java `shutdown()` 无对应；Java 构造器校验 `apiBaseUrl` 非空，Rust `ReMeClient` **不校验**（§3.2 新增 P1）。另 Java `ReMeLongTermMemory.record` **不吞错**（`client.add().then()` 透传），Rust `record` 亦返回 `Result` —— 此项一致 |

> 三子模块均**无「缺失」类**，也无「部分」类（即不存在只搬了签名、没搬行为的空壳）。

### 6.4 新发现缺口

| 优先级 | 缺口 | 依据 | 影响 |
|---|---|---|---|
| **P1** | **Java 吞错语义在 Rust 主 API 上不一致** | `Mem0LongTermMemory.retrieve` / `BailianLongTermMemory.record`+`retrieve` / `ReMeLongTermMemory.retrieve` 均以 `onErrorReturn("")` / `onErrorComplete()` 终止；Rust 主方法返回 `Result`，吞错版本另命名 `retrieve_lossy`/`record_lossy`。**Mem0 的 `record` 在 Java 侧无吞错**（`client.add(request).then()` 直接透传），Rust 也透传——此项一致；**ReMe 的 `record` 同样无吞错**，Rust 亦透传——此项亦一致 | 上层若直接调用 `retrieve`/`record`，行为比 Java 更严格（会抛错而非静默）。**不改变默认调用即产生行为差异**，需汇总层显式选择 |
| **P1** | **`api_base_url` 必填校验缺失（Mem0 / ReMe）** | Java：`Mem0LongTermMemory.Builder.build()` 抛 `"apiBaseUrl is required"`；`ReMeLongTermMemory` 构造器同样抛 `"apiBaseUrl is required"`。Rust：`Mem0Client::assemble` 与 `ReMeClient::with_transport` **均不校验**空 base URL（`rg` 实测两个 client 文件中 `api_base_url` 只有赋值/拼接/取值，无 `is_empty`/`trim` 校验） | `Mem0Client::new("")` / `ReMeClient::new("")` 可成功构造，运行期拼出 `/v1/memories/`、`/summary_personal_memory` 等**无主机 URL**，错误从「构造期 `IllegalArgumentException`」推迟到「网络期」。Bailian 侧已有该校验 → 三厂商行为不一致 |
| **P1** | **`bailian/dto.rs`、`reme/dto.rs` 零单测** | 两文件无 `#[cfg(test)]` 模块（实测 0 个测试）；`mem0/dto.rs` 有 11 个 | DTO 字段名/serde 属性对齐只靠人工比对，回归无防护。Java 侧对应 **12 个** DTO 测试类、**161** 个 `@Test`（mem0 6 类 89 个 + bailian 6 类 72 个；reme 无 DTO 测试类） |
| **P1** | **传输层超时/资源释放粒度** | Java：`connectTimeout(30s)` + `readTimeout(timeout)` + `writeTimeout(30s)`，另有 `shutdown()`/`close()`；Rust `ReqwestTransport` 只有单一 `.timeout(timeout)`，无 `close()` | 连接建立慢或写入阻塞时，Rust 的行为与 Java 不同；`reqwest::Client` 靠 Drop 释放，无法主动 shutdown |
| **P2** | **未使用依赖** | `Cargo.toml` 声明 `futures`、`agentscope-harness`，全 crate 源码零引用（`rg` 实测 0 命中）；`tokio` 仅用于 26 个 `#[tokio::test]`，却放在 `[dependencies]` 而非 `[dev-dependencies]` | 编译期冗余、依赖面扩大；`tokio` 误置于运行时依赖会让下游 crate 被动拉入 tokio |
| **P2** | **Builder/便捷构造器覆盖不全** | `Mem0AddRequest` 20 个 setter → Rust 6 个 `with_*`；`Mem0SearchRequest` 缺 8 个 `with_*`（`version`/`fields`/`rerank`/`keyword_search`/`filter_memories`/`threshold`/`org_id`/`project_id`） | 字段是 `pub`，功能可达，仅链式 API 缺失；移植 Java 调用代码时需改写为字段赋值 |
| **P2** | **`Mem0SearchRequest.user_id` 直接赋值绕过 `filters` 镜像** | Java `Mem0SearchRequest.setUserId` 保证 `user_id` 自动同步进 `filters`；Rust 仅 `with_user_id()` 做镜像，直接写 `req.user_id = Some(..)` 会漏同步 | 迁移 Java 调用代码时若改用字段赋值，搜索过滤条件会**静默丢失**，导致跨用户数据串扰 |
| **P2** | **Mem0 消息体多带 `name` 字段** | Java `Mem0LongTermMemory.convertToMem0Message` 只填 `role`/`content`；Rust `convert_to_mem0_message` 额外填 `msg.name()` | 外发 JSON 多一个 `name` 键。Mem0 侧该字段合法故大概率无害，但与 Java 报文**形状不一致**，用 mock 断言请求体的调用方会看到差异 |
| **P2** | **`BailianLongTermMemoryConfig.api_key` 是「死字段」** | Java `BailianLongTermMemory` 构造器校验 `apiKey` 非空后自建 client；Rust 改为外部注入 client，`config.api_key` 在**生产代码中从未被读取**（`rg` 仅命中声明、构造函数与测试） | 空 key 搭配一个合法 client 可通过 `BailianLongTermMemory::new`，校验被静默绕过；字段本身会误导调用方以为它生效 |
| **P2** | **无指标 / span** | 仅 4 处 `tracing::warn!`（`bailian/memory.rs:239,252`、`mem0/memory.rs:184`、`reme/memory.rs:141`），无 `#[instrument]`、无 metric | 与 Java 的 Slf4j 基本对等，增强项 |

> **2026-10-28 更新**：本节 P1 三项（`api_base_url` 空校验、传输层 connect 超时、`bailian/dto.rs` + `reme/dto.rs` 单测）已落地，详见 §7。下表「影响」列保留为问题发现时的原始描述（不再逐行改写）。

### 6.5 勘误列表

| # | 原文 | 实际 | 处理 |
|---|---|---|---|
| E1 | 「Rust crate 文件清单（15 个源文件，**~2100 行**）」 | 15 个源文件，**2993 行**（生产 2045 + 内联测试 948） | 已改 §1.1 标题 |
| E2 | 「Java 规模（初测） 主代码 ~4821 行 / 测试 ~5746 行」 | 25 文件 / 5543 总行（非空 **4821**）；18 文件 / 6850 总行（非空 **5746**）；**326** 个 `@Test` | 数字本身正确（非空行口径），已补文件数与 `@Test` 数，并标注口径 |
| E3 | 「§2.3 reme 子模块，**7 类**」 | **8 类**（`ReMeMessage`/`ReMeTrajectory`/`ReMeAddRequest`/`ReMeAddResponse`/`ReMeSearchRequest`/`ReMeSearchResponse`/`ReMeClient`/`ReMeLongTermMemory`） | 已改 §2.3 与 §3.1 |
| E4 | 「ReMe DTO + 客户端 + 适配器 `ReMe*` **7 类**」 | **8 类** | 已改 §3.1 |
| E5 | §2.1/§2.2/§2.3 说明列把 `retrieve` 描述为直接等价 Java `retrieve` | Java `retrieve` 是**吞错版**（`onErrorReturn("")`），Rust 主 `retrieve` 返回 `Result`；吞错版为 `retrieve_lossy` | 已在 §2.1 补注、§3.2 新增条目、§4.2 裁定措辞更新 |
| E6 | §2.3「`ReMeSearchResponse.memories()` 提取 `memory_list[].content`」 | 路径为 `metadata.memory_list[].content`（Java 同为 `getMetadata().getMemoryList()`） | 已改 §2.3 |
| E7 | §3.1「Mem0 DTO serde 对齐 … **6 个 DTO**」 | 实际 **7 个类型**（6 个 DTO + `Mem0ApiType` 枚举） | 已改 §3.1 |
| E8 | §2.2「`BailianSearchRequest` … `top_k`/`min_score`」暗示 DTO 自带默认 | Java `BailianSearchRequest` 构造器**不设**默认值，10/0.3 来自 `BailianLongTermMemory.Builder`；Rust 同构（`effective_top_k()`/`effective_min_score()` 兜底） | 已改 §2.2 |
| E9 | §4.1「`ContentBlockKind` 定义于 `msg.rs:386`」 | 定义于 **`msg.rs:401`**（386 是 `content_block_kind()` 辅助函数内） | 已改 §4.1 |
| E10 | §4.1「新增 54 个（原有 2 个）」 | 无法静态验证历史增量；**当前总数 66** 已逐文件核实。措辞改为只陈述总数与分布 | 已改 §4.1 第 3 条 |
| E11 | §1「当前状态 **已完成**（build + test 全绿，66 passed / 0 failed）」 | 测试函数数 66 已静态核实；但 build/test **本次未复跑**，且 §3.2 仍有 2 项 P1「待办」 | 已改为「已完成（实现层）… 未重新编译，§3.2 遗留 2 项 P1」，并在 §4.1 标注「本次未执行」 |
| E12 | §3.3「指标/可观测性：仅 `tracing` 依赖已声明，**未埋点**」 | 实际有 4 处 `tracing::warn!`（三处 `*_lossy` + bailian `record_lossy`） | 已改 §3.3 |
| E13 | §6.4「Java 侧对应 **16 个 DTO 测试类**、约 **150** 个 `@Test`」 | 实为 **12 个** DTO 测试类（mem0 6 + bailian 6，**reme 0 个**）、**161** 个 `@Test`（89 + 72） | 已改 §6.2 + §6.4 |
| E14 | §6.4「`Mem0Client` 未校验 `api_base_url` …… 故**行为等价但校验点前移**；记录以免后续误判为缺口」 | **结论错误**：Rust 侧**根本没有**该校验（`Mem0Client::assemble` 不校验，`Mem0LongTermMemoryConfig` 也不含 `api_base_url`），且「必须先构造 client」**不构成**类型层面的必填保证——`Mem0Client::new("")` 可正常构造。ReMe 同理 | 已删除原「非缺口」结论，改为 §3.2 + §6.4 的 **P1 真实缺口** |
| E15 | §6.2「Java 测试类数 mem0 8、bailian 8、reme 2」（未标总数） | 补「共 18」，并新增「Java `@Test` 分布：mem0 143 / bailian 141 / reme 42」 | 已改 §6.2 |
| E16 | §2.1「`mem0.Mem0Message` …… 字段 `role`/`content`/`name`；`name` 为 `NON_NULL`」（读作 Java 转换时会带 `name`） | Java `convertToMem0Message` **只设 `role`/`content`**，从不填 `name`；Rust `convert_to_mem0_message` 会带出 `Msg.name` → 外发报文多一个 `name` 键 | 已在 §2.1 补注，并在 §3.3 + §6.4 登记为 P2 差异 |
| E17 | §3.3「未使用依赖 …… `tokio` 仅测试用」 | 补充：`tokio` 被放在 `[dependencies]` 而非 `[dev-dependencies]`，会污染下游 crate 的运行时依赖 | 已改 §3.3 + §6.4 |
| E18 | §3.3「`Mem0SearchRequest` 便捷构造器覆盖度 …… Rust 缺 8 个 `with_*`」（未提副作用） | 补：Rust 只有 `with_user_id()` 会把 `user_id` 镜像进 `filters`；**直接写 `pub user_id` 字段会漏镜像**（Java `setUserId` 保证同步），属静默丢过滤条件的风险 | 已改 §3.3 + §6.4 |

### 6.6 状态取值核对
- 本文件使用的状态值均在 §0 定义集合内：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- 本次新增条目一律取 `待办` 或 `已完成`，未引入新取值。
- §1「当前状态」栏沿用 `已完成`，但附注「实现层完成、未重新编译、§3.2 遗留 P1 待办」，避免与 §3.2 的 `待办` 自相矛盾。
- 【2026-10-28 更新】§3.2 现有 **2 行 `待办`**（真实网络联调 / Java 测试逐条对照）；原 3 项已置 `已完成`（`api_base_url` 校验、传输层 connect 超时、DTO 单测），其中 writeTimeout 与 `close()` 另记作 `设计决定（不迁移）`。§3.3 仍有 **9 行 `待办`**（P2 增强类）。

### 6.7 本轮复核追加（2026-10-22 第二次静态复核）
> 幂等说明：本轮为**同一天的第二次复核**，对 §6.1~§6.6 **就地修正**，未重复追加复核节。

#### 6.7.1 本轮新增/修正的可验证声明
| 声明 | 复核方式 | 结果 |
|---|---|---|
| Rust 15 文件 / 2993 行 / 66 test | 逐文件 `Get-Content` 计数 + 正则 `^\s*#\[test\]` / `#\[tokio::test` | ✅ 全部一致（见 §6.2） |
| Java 主 25 文件 5543 行（非空 4821）/ 测 18 文件 6850 行（非空 5746）/ 326 `@Test` | 同上 | ✅ 全部一致 |
| 「Java 类 → Rust 对应」映射真实存在 | 逐个 Java 类打开源码确认类名/关键方法/字段，再在 Rust 侧确认同名类型与行为 | ✅ 25/25 映射**全部真实存在**，无「空壳类」 |
| 「已完成 / 66 test」与代码一致 | 静态核实测试函数总数 = 66，且分布与 §6.2 吻合 | ✅ **一致**（但 build/test 本身本轮仍未复跑） |
| Rust `Mem0SearchResult` 时间戳 serde 往返 | Rust `chrono::DateTime<FixedOffset>` + `+00:00`→`Z` 归一化；Java `JacksonJsonCodec` 未开 `WRITE_DATES_AS_TIMESTAMPS`，Jackson `JavaTimeModule` 默认输出 ISO-8601 `Z` 形式 | ✅ 两侧报文一致（Java 测试 `Mem0SearchResultTest` 断言 `"created_at":"2025-01-15T10:30:00Z"`，Rust 测试断言 `2024-01-02T03:04:05Z`） |
| Java `FAIL_ON_UNKNOWN_PROPERTIES=false` vs Rust serde 默认忽略未知字段 | 读 `JacksonJsonCodec.createDefaultObjectMapper()`；Rust serde 默认（无 `deny_unknown_fields`）亦忽略 | ✅ 宽容度一致；Java 类上的 `@JsonIgnoreProperties(ignoreUnknown=true)` 在 Rust 侧由 serde 默认行为等价覆盖 |
| §4.2 「不实现 core `Memory` trait / 不重建 `LongTermMemory`」 | 在 `agentscope-core` 全文检索 `LongTermMemory`/`long_term` → **0 命中**；core `memory/mod.rs` 只有会话缓冲语义 `Memory` trait；Java 侧 `LongTermMemory.java:70` 确为 `@Deprecated(forRemoval = true, since = "2.0.0")` | ✅ 裁定前提成立，Rust core 无对应体系可接；Java 的 `StaticLongTermMemoryHook`/`LongTermMemoryTools`/`LongTermMemoryMode`（core 内 18 处引用）**在 Rust core 中完全无对应物** |
| Java `Mem0Client` 端点/鉴权、ReMe 端点、Bailian 端点 | 逐一比对常量与 header 拼装 | ✅ 6 个端点常量与 3 种鉴权方式全部 1:1 命中 |

#### 6.7.2 本轮新发现/复核的缺口汇总（§6.4 节内口径：P0: 0 / P1: 3 / P2: 6）
| 优先级 | 缺口 | 落点 |
|---|---|---|
| P1 | `api_base_url` 必填校验缺失（Mem0 / ReMe） | §3.2 新增行、§6.4 新增行、§6.5 E14 |
| P1 | `bailian/dto.rs` / `reme/dto.rs` 零单测（Java 侧 12 类 161 个 `@Test` 无对应） | §6.4（修正 Java 侧数字） |
| P1 | 传输层超时/资源释放粒度（connect/write 超时、`shutdown`/`close`） | §6.4（原已列，保留） |
| P2 | Mem0 消息体多带 `name` 字段 | §3.3、§6.4、§6.5 E16 |
| P2 | `BailianLongTermMemoryConfig.api_key` 为死字段、校验被绕过 | §3.3、§6.4 |
| P2 | `Mem0SearchRequest.user_id` 直接赋值绕过 `filters` 镜像 | §3.3、§6.4、§6.5 E18 |
| P2 | `tokio` 误置于 `[dependencies]`（应入 `[dev-dependencies]`） | §3.3、§6.4、§6.5 E17 |
| P2 | 无指标 / span（仅 4 处 `tracing::warn!`） | §3.3、§6.4（原已列，保留） |
| P2 | Rust `BailianMemoryClient` 剥除 base URL 末尾 `/`，Java 不剥（**良性差异**，Rust 更宽容） | §6.3 bailian 行 |

#### 6.7.3 剩余真实缺口的优先级判定
> 口径说明：下表按**本 tracker 的缺口台账**统计，与 §6.4 的「§6.4 节内列出的行」为**不同口径**，二者不冲突。

- **P0：0**。三厂商 25 个 Java 类在 Rust 侧 1:1 落地且行为等价，核心链路（record / retrieve / DTO 序列化 / 鉴权 / 端点）无缺失，**不构成阻塞**。
- **P1：§3.2 共 5 项待办**。全部为「验证 / 工程完备性 / 校验前移」类，**无「Java 有能力、Rust 没有」的能力缺口**：(1) 真实网络联调（需凭据，非代码缺口）；(2) Java 测试逐条对照迁移（覆盖率 66/326 ≈ 20%，缺口集中在 Java 侧 MockWebServer 传输层细节与 Builder 逐字段断言）；(3) 吞错语义取舍（`retrieve` 严格 vs `retrieve_lossy`——Java 等价语义**已实现**，只差默认口径裁定）；(4) **`api_base_url` 校验缺失（Mem0 / ReMe）**——本轮新确认，是 P1 中**唯一有行为风险**的一项；(5) 传输层资源释放粒度（connect/write 超时、`shutdown`/`close`，功能等价）。 （2026-10-29 勘误：本段为 2026-10-22 快照——`api_base_url` 校验与 `connect` 超时已于 2026-10-28 修复，实测 96 passed，覆盖率 96/326 ≈ 29%；当前剩余见 §3.2/§3.3/§4.2）
- **P2：§3.3 共 9 项待办**（重试/退避、指标、未使用依赖、2 个 Builder 覆盖度、`close()`、`name` 字段差异、`api_key` 死字段、`user_id` 镜像绕过）。均为增强 / API ergonomics / 报文形状微差，不阻塞汇总。

#### 6.7.4 rollup 状态建议
- **建议 rollup 状态值：`已完成`**（实现层）。
- 理由：(a) Java 25 个类 **100% 有真实对应的 Rust 实现**，无缺失类、无空壳类、无「只搬签名未搬行为」的情况；(b) 66 个离线测试函数数量与代码**完全一致**，DTO serde 往返、消息转换、请求构造、鉴权头、端点解析、空查询短路、mock HTTP 全链路均有覆盖；(c) §3.2 的 P1 **全部是「验证/工程完备性」或「API 校验前移」类**，不构成「Java 有能力、Rust 没有」的**能力缺口**——其中 `api_base_url` 校验是唯一有行为风险的项，但影响面限于「构造期报错 vs 运行期报错」，不改变正常路径行为；(d) 本轮为**纯静态复核、未重新编译**，故建议 rollup 保留「实现层完成」的措辞，并在 rollup 行注明「静态复核通过；build/test 未在本轮复跑」，以免与 §3.2 的 `待办` 项自相矛盾。
- **若汇总方要求「零待办才可置完成」**：可先补齐 §3.2 的 `api_base_url` 校验（改动量极小，仅 `mem0/client.rs` 与 `reme/client.rs` 各加一处 `trim().is_empty()` 判断 + 对应单测），再将状态提为无保留的 `已完成`；其余 P1/P2 可留待后续波次。

## 7. 执行记录（2026-10-28）

> **历史快照**：本节记录 2026-10-28 轮次（96 test），其中 §7.6 的「未完成」项已于 **2026-10-31** 关闭，最新状态以 §3 / §4 / §8 为准。

> 执行范围：仅 `crates/agentscope-extensions-mem/**` 与本文档；**未改**根 `Cargo.toml`、`agentscope-core`、`agentscope-harness` 及其它 crate。**新增依赖：无**（复用已有 `serde`/`serde_json`/`reqwest`）。

### 7.1 改动清单

| 文件 | 改动 |
|---|---|
| `src/mem0/client.rs` | `assemble` 增加 `trim().is_empty()` 校验 → `MemError::Config("apiBaseUrl is required")`；`assemble` / `with_transport` 改返回 `Result<Self>`；`with_api_type` 传递错误；base URL 先 `trim()` 再剥末尾 `/`；+3 测试 |
| `src/reme/client.rs` | 同构校验（`with_transport` → `Result<Self>`）；+5 测试（该文件原本无 `mod tests`） |
| `src/mem0/memory.rs`、`src/reme/memory.rs` | 测试内 3 处 `with_transport` 调用点补 `.expect("valid base url")`（**生产代码未改**） |
| `src/http.rs` | 新增 `pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30)`；`ReqwestTransport::new` 加 `.connect_timeout(...)`；+1 测试；doc 注明 reqwest 无独立 write timeout |
| `src/bailian/dto.rs` | +11 个 serde 测试 |
| `src/reme/dto.rs` | +10 个 serde 测试 |
| `src/lib.rs` | 新增「Construction is validated」文档小节，说明三厂商构造期校验与 `with_transport` 返回 `Result` |

### 7.2 签名变更说明（唯一破坏性变更）

`Mem0Client::with_transport` 与 `ReMeClient::with_transport`：`-> Self` → `-> Result<Self>`。

- **理由**：这两个构造函数是**唯一**能绕过 base URL 校验的入口（`Mem0Client`/`ReMeClient` 字段私有、无 `Default`）；保持 `-> Self` 就无法让空 URL 在构造期失败。返回 `Result` 与同 crate 的 `BailianMemoryClient::with_transport -> Result<Self>` 风格一致。
- **影响面**：全仓 `rg "Mem0Client::with_transport|ReMeClient::with_transport"` 仅命中本 crate 的 4 处测试调用点，已同步；**无跨 crate 调用方**，故无下游破坏。
- **未改动**：`Mem0Client::new` / `with_api_type` / `ReMeClient::new` / `with_timeout` 签名本就返回 `Result<Self>`，仅行为由「恒 Ok」变为「空 URL 时 Err」，正常路径不变。

### 7.3 验证命令与实测

| 命令 | 结果 |
|---|---|
| `cargo build -p agentscope-extensions-mem` | **exit 0**，本 crate **0 warning**（`agentscope-harness` 的 5 个 warning 属既有、未越界修改） |
| `cargo test -p agentscope-extensions-mem` | **96 passed / 0 failed / 0 ignored**（exit 0）；静态计数 `#[test]` **67** + `#[tokio::test]` **29** = 96 |

### 7.4 新增测试名单

- `mem0::client::tests`：`blank_base_url_is_rejected_at_construction`、`new_rejects_blank_base_url`、`base_url_is_trimmed_before_building_the_request_url`
- `reme::client::tests`：`blank_base_url_is_rejected_at_construction`、`new_rejects_blank_base_url`、`base_url_is_trimmed_and_trailing_slash_stripped`、`add_posts_to_summary_endpoint`、`blank_response_body_falls_back_to_default`
- `http::tests`：`connect_timeout_matches_java_thirty_seconds`
- `bailian::dto::tests`（11）：`message_roundtrips_role_and_content`、`add_request_uses_meta_data_key_not_metadata`、`add_request_skips_absent_fields`、`add_request_serializes_snake_case_fields`、`search_request_serializes_snake_case_and_flags`、`search_request_has_no_dto_level_defaults`、`memory_node_deserializes_snake_case_fields`、`memory_node_serializes_nothing_when_empty`、`add_response_deserializes_request_id_and_nodes`、`search_response_preserves_node_order`、`responses_tolerate_missing_and_unknown_fields`
- `reme::dto::tests`（10）：`message_roundtrips_role_and_content`、`trajectory_wraps_messages_and_skips_absent_field`、`add_request_serializes_workspace_id_and_trajectories`、`search_request_defaults_top_k_to_five`、`search_request_with_parts_serializes_query_and_top_k`、`memory_item_deserializes_snake_case_fields`、`memory_item_serializes_nothing_when_empty`、`add_response_deserializes_metadata_and_update_result`、`search_response_memories_extracts_contents_in_order`、`search_response_without_metadata_has_no_memories`

> 全部离线：仅用 `crate::http::mock::MockTransport` 或纯 `serde_json`，无网络、无 `#[ignore]`。

### 7.5 反向验证（回退实现 → 观察 FAILED → 恢复）

| # | 回退内容 | 观察结果 | 恢复 |
|---|---|---|---|
| 1 | `mem0/client.rs`：`assemble` 的 `if trimmed.is_empty() {` → `if false {`（文本相同，替换连带命中 `search` 的空响应体校验） | `cargo test` → **FAILED：93 passed / 3 failed**；失败项：`mem0::client::tests::blank_base_url_is_rejected_at_construction`（`unwrap_err()` on `Ok(Mem0Client { api_base_url: "" ... })`）、`new_rejects_blank_base_url`、`search_rejects_empty_body`（`assertion failed: matches!(err, MemError::Transport(_))`） | 还原后 **96 passed / 0 failed** |
| 2 | `http.rs`：`DEFAULT_CONNECT_TIMEOUT` 30s → 10s | `cargo test` → **FAILED：95 passed / 1 failed**；`http::tests::connect_timeout_matches_java_thirty_seconds`（`assertion left == right failed: left: 10s`） | 还原后 **96 passed / 0 failed** |
| 3 | `bailian/dto.rs`：`rename = "meta_data"` → `rename = "metadata"` | `cargo test` → **FAILED：95 passed / 1 failed**；`bailian::dto::tests::add_request_uses_meta_data_key_not_metadata`（`dto.rs:296`） | 还原后 **96 passed / 0 failed** |

> 注：#1 的回退范围意外覆盖了 `Mem0Client::search` 的空响应体校验（两处 `if trimmed.is_empty()` 文本相同），因此多出 1 个失败项——反而反向证明了该替换确实生效；两个判断现均已恢复（`git diff` 核对，`mem0/client.rs` 无残留 `if false`）。

### 7.6 未完成 / 需汇总方裁定

1. **真实网络联调**（§3.2）：2026-10-28 全部离线、未对真实端点回归（需凭据）——**2026-10-31 已关闭：设计决定（环境受限）+ `tests/live_endpoints.rs`，见 §8**。
2. **`writeTimeout(30s)` 与 `shutdown()`/`close()`**：按任务要求记为 **设计决定（不迁移）**——Rust/reqwest 无独立 write timeout、`reqwest::Client` 无 `close`，未硬造。
3. **Java 测试逐条对照迁移**（§3.2）与 **吞错语义默认口径**——**2026-10-31 已关闭：映射表见 §8.2、裁定见 §4.2 第 4 条**。
4. **校验层位置（请裁定）**：本次校验落在 **client 构造**（比 Java 的 adapter builder 更早，且覆盖所有 adapter 路径），未在 `Mem0LongTermMemory::new` / `ReMeLongTermMemory::new` 重复校验（`Arc<Mem0Client>` 已不可能持空 URL）。若汇总方要求错误必须在 adapter 层抛出，需额外 wrapper。
   > 2026-10-31 结论：保留「校验在 client 构造」的既有做法（`Bailian` 因注入 client 的形态，另在 adapter `new` 校验 `api_key`/`user_id`）；无需 wrapper，见 §8。

## 2026-10-31 loop 收口记录

> 依据 `.agentscope/workspace/loop-2026-10-31.md`：本回合子智能体**只做代码与本文档编辑，不运行 cargo / rustc / clippy / fmt**，验证由主智能体统一执行（`cargo test -p agentscope-extensions-mem -j 2`）。
> 对齐基线：Java `agentscope-extensions-mem` HEAD `1524b3191`（2026-10-30，该模块自 2026-10-23 起无增量提交，`git log -- agentscope-extensions/agentscope-extensions-mem` 仅 1 条 2.2.0-RC2 重构提交）。
> 结论：**本模块 §3.1 / §3.2 / §3.3 / §4.2 无未关闭条目**；3 项为「已完成（设计决定，不迁移）」。

### 8.1 改动清单（仅 `crates/agentscope-extensions-mem/**` 与本文档）

| 文件 | 改动 |
|---|---|
| `src/retry.rs`（新增） | `RetryPolicy`（`none()` = Java 单次尝试；`new()` 钳制 `max_attempts>=1`、`multiplier>=1.0`；`backoff_before_retry()`；`is_retryable()` 只认 Transport / 408 / 429 / 5xx）+ `RetryTransport` 装饰器（含 `retries()` 计数）。7 个测试 |
| `src/telemetry.rs`（新增） | `MemMetrics`（`requests` / `failures` / `latency_ms_total`）+ `MemMetricsSnapshot` + `InstrumentedTransport`（`mem.http.post` span：`url` / `status` / `latency_ms`）。3 个测试 |
| `src/lib.rs` | `pub mod retry; pub mod telemetry;` + 新增「Error semantics（吞错口径表）」「Resilience and observability」文档节；导出 `RetryPolicy` / `RetryTransport` / `MemMetrics` / `MemMetricsSnapshot` / `InstrumentedTransport` / `DEFAULT_CONNECT_TIMEOUT` |
| `src/mem0/client.rs` | 新增 `with_api_type_and_policy`（链 `Retry(Instrumented(Reqwest))`）+ `metrics()`；`assemble` 置 `metrics: None`；`search` 序列化前 `request.sync_filters()`。+4 测试 |
| `src/mem0/dto.rs` | `Mem0AddRequest` +14 个 `with_*`；`Mem0SearchRequest` +8 个 `with_*`、`user_id` 改**私有**并配 `set_user_id` / `user_id()` / `sync_filters()`、`filters` 加 `#[serde(default)]`。+3 测试 |
| `src/mem0/memory.rs` | `convert_to_mem0_message` **不再带 `name`**（与 Java 报文一致）；`record` rustdoc 说明无 `record_lossy` 的依据。+2 测试 |
| `src/bailian/client.rs` | 新增 `with_timeout_and_policy` + `metrics()`，`metrics` 字段；新增 `mod tests`（3 条） |
| `src/bailian/memory.rs` | `new()` 增 `config.api_key` 非空校验（`"apiKey cannot be null or blank"`）；+2 测试 |
| `src/reme/client.rs` | 新增 `with_timeout_and_policy` + `metrics()`，`metrics` 字段；+1 测试 |
| `Cargo.toml` | 删除 `agentscope-harness`、`futures`；`tokio` 保留（退避 `sleep` 生产依赖）并注明理由 |
| `tests/live_endpoints.rs`（新增） | 3 个环境变量门控的真实端点往返用例（无 `#[ignore]`） |

### 8.2 Java 测试类 → Rust 用例映射（18 类 / 326 `@Test` → 121 单元测试 + 3 集成）

| # | Java 测试类（`@Test`） | Rust 对应用例（代表，模块::测试名） | 关闭口径 |
|---|---|---|---|
| 1 | `Mem0MessageTest` (11) | `mem0::dto::message_serializes_name_only_when_present` | 语义（`name` 可缺省/NON_NULL）已覆盖；其余为 Java bean 样板（构造器/setter/`toString`） |
| 2 | `Mem0AddRequestTest` (21) | `mem0::dto::{add_request_carries_java_defaults, add_request_serializes_snake_case_identifiers, add_request_builder_covers_every_java_setter}` | 20 个 setter → 逐字段断言（键名 + 值）；`testNullMessagesHandling` 由 `Option<Vec<_>>` 类型关闭 |
| 3 | `Mem0AddResponseTest` (9) | `mem0::dto::add_response_deserializes_results_and_message` | `results: Option<Vec<JsonMap>>` 覆盖空/null；`toString` 无语义 |
| 4 | `Mem0ClientTest` (31) | `mem0::client::{platform_auth_uses_token_scheme, self_hosted_auth_uses_x_api_key, blank_api_key_omits_authorization_header, self_hosted_uses_self_hosted_endpoints, base_url_is_trimmed_before_building_the_request_url, search_rejects_empty_body, add_propagates_http_status_and_body, invalid_json_surfaces_as_a_json_error, default_construction_exposes_metrics}` | 端点/鉴权/双响应形状/错误透传全覆；`testHttpTimeout`、`testShutdown` 属 MockWebServer 设施与 §3.2 设计决定 |
| 5 | `Mem0LongTermMemoryTest` (23) | `mem0::memory` 15 条（`build_search_request_uses_top_k_5_and_merges_config`、`record_posts_all_configured_identifiers_without_a_name_key`、`record_surfaces_http_errors_like_java`、`retrieve_handles_array_and_object_search_shapes`、`retrieve_msgs_maps_hits_to_assistant_msgs`、`retrieve_lossy_*` 等） | Builder 三标识符 + 过滤 + 双响应形状 + 严格/吞错双路径全覆盖；`null` 入参由类型系统关闭 |
| 6 | `Mem0SearchRequestTest` (25) | `mem0::dto::{search_request_has_java_defaults_and_always_emits_filters, search_request_mirrors_identifiers_into_filters, search_request_builder_covers_every_java_setter, search_request_keeps_user_id_mirrored_into_filters}` + `mem0::client::search_wire_payload_always_carries_the_user_id_filter` | `filters` ALWAYS、8 个新 setter、`user_id` 镜像（含反序列化后 wire 校验）全覆 |
| 7 | `Mem0SearchResponseTest` (9) | `mem0::dto::search_response_accepts_missing_results` | `results` 缺省/多结果由同一 `Option<Vec<_>>` 覆盖 |
| 8 | `Mem0SearchResultTest` (14) | `mem0::dto::{search_result_serde_roundtrips_timestamps, search_result_maps_to_core_msg_with_metadata, search_result_without_memory_maps_to_empty_text}` | 时间戳 `Z` 归一化、metadata/categories/score 映射、`memory=None` 降级 |
| 9 | `BailianAddRequestTest` (13) | `bailian::dto::{add_request_uses_meta_data_key_not_metadata, add_request_skips_absent_fields, add_request_serializes_snake_case_fields}` | `meta_data` 键名、空值省略、snake_case 全覆 |
| 10 | `BailianAddResponseTest` (10) | `bailian::dto::{add_response_deserializes_request_id_and_nodes, responses_tolerate_missing_and_unknown_fields}` | 内嵌节点复用顶层类型，缺字段/未知字段容忍 |
| 11 | `BailianLongTermMemoryTest` (43) | `bailian::memory` 19 条（`is_recordable_rejects_tool_use_assistant_and_system`、`convert_messages_applies_bailian_filters`、`new_rejects_blank_api_key`、`build_search_request_uses_config_values`、`record_posts_optional_identifiers_and_metadata`、`record_lossy_swallows_http_errors`、`retrieve_joins_memory_node_contents` 等） | 43 项里语义项全覆盖（过滤矩阵、默认 10/0.3、enable_* 三旗、可选标识符、严格 vs 吞错）；`testCloseMethod` → 设计决定 |
| 12 | `BailianMemoryClientTest` (26) | `bailian::client::{default_construction_defaults_the_base_url_and_installs_metrics, injected_transports_are_not_instrumented, policy_constructor_is_equivalent_to_the_default_one}` + `bailian::memory::record_posts_to_add_endpoint_with_bearer_auth`、`client_rejects_blank_api_key`/`client_rejects_blank_base_url` | Builder 校验矩阵、`Bearer`/`Content-Type`、报文序列化覆盖；`testClose`/`testCloseMultipleTimes` → 设计决定 |
| 13 | `BailianMemoryNodeTest` (12) | `bailian::dto::{memory_node_deserializes_snake_case_fields, memory_node_serializes_nothing_when_empty}` | 事件类型/时间戳由 serde 往返与 `skip_serializing_if` 覆盖；add/update/delete 差异仅字段组合 |
| 14 | `BailianMessageTest` (13) | `bailian::dto::message_roundtrips_role_and_content` | 两字段 DTO，1 条往返即等价 |
| 15 | `BailianSearchRequestTest` (17) | `bailian::dto::{search_request_serializes_snake_case_and_flags, search_request_has_no_dto_level_defaults}` | 无 DTO 级默认 + 三个 enable flag + `project_ids` 已断言 |
| 16 | `BailianSearchResponseTest` (7) | `bailian::dto::search_response_preserves_node_order` | 顺序/空列表同一模型 |
| 17 | `ReMeClientTest` (18) | `reme::client::{base_url_is_trimmed_and_trailing_slash_stripped, add_posts_to_summary_endpoint, blank_response_body_falls_back_to_default, default_construction_installs_metrics_and_policy_is_configurable}` | 端点/无鉴权头/空体回退 `{}`/构造校验全覆；`testShutdown` → 设计决定 |
| 18 | `ReMeLongTermMemoryTest` (24) | `reme::memory` 12 条（`is_recordable_rejects_tool_use_assistant`、`render_prefers_answer_then_joins_memories`、`build_requests_use_user_id_as_workspace`、`record_posts_trajectory_to_summary_endpoint`、`retrieve_lossy_*` 等） | `answer` 优先、`memory_list` 回退、`workspace_id` 复用、严格/吞错双路径 |

> 口径说明：Java 326 个 `@Test` 中相当一部分是 getter/setter/`toString`/`null` 构造器/`MockWebServer`（`shutdown()`、`writeTimeout`）样板，Rust 侧不以「1 Java 测试 = 1 Rust 测试」凑数；上表按**语义**逐类关闭，未覆盖项均给出依据。

### 8.3 §3.2 / §3.3 关闭对照（条目 → 落点 → 测试）

| 条目 | 落点 | 代表测试 |
|---|---|---|
| 吞错口径裁定（P1） | `lib.rs` 文档表 + 各 `*_lossy` 方法；`mem0/memory.rs` / `bailian/memory.rs` / `reme/memory.rs` | `mem0::memory::retrieve_lossy_returns_empty_on_http_status_error`、`bailian::memory::record_lossy_swallows_http_errors`、`reme::memory::retrieve_lossy_returns_empty_on_http_error` |
| 测试密度 96 → 121（P1） | 见 §8.2 | `mem0::dto::add_request_builder_covers_every_java_setter`、`mem0::client::add_propagates_http_status_and_body` |
| 真实端点联调（P1 → 设计决定） | `tests/live_endpoints.rs` | `mem0_live_round_trip_when_configured`、`bailian_live_round_trip_when_configured`、`reme_live_round_trip_when_configured`（未设 env 即通过） |
| 重试/退避/超时（P2） | `src/retry.rs`、三 client `*_and_policy` | `retry::tests::retries_transport_errors_until_success`、`retry::tests::stops_after_max_attempts` |
| 指标/span（P2） | `src/telemetry.rs`、三 client `metrics()` | `telemetry::tests::records_failed_attempts_and_returns_the_error_unchanged`、`reme::client::default_construction_installs_metrics_and_policy_is_configurable` |
| 未使用依赖（P2） | `Cargo.toml` | `rg 'agentscope_harness|futures' crates/agentscope-extensions-mem` 零命中 |
| Builder 覆盖度 ×2（P2） | `mem0/dto.rs` | `add_request_builder_covers_every_java_setter`、`search_request_builder_covers_every_java_setter` |
| Mem0 报文多 `name`（P2） | `mem0/memory.rs::convert_to_mem0_message` | `convert_message_matches_java_and_omits_name`、`record_posts_all_configured_identifiers_without_a_name_key` |
| `api_key` 死字段（P2） | `bailian/memory.rs::BailianLongTermMemory::new` | `bailian::memory::new_rejects_blank_api_key` |
| `user_id` 绕过 filters（P2） | `mem0/dto.rs`（私有字段 + `set_user_id` + `sync_filters`）、`mem0/client.rs::search` | `search_request_keeps_user_id_mirrored_into_filters`、`search_wire_payload_always_carries_the_user_id_filter` |

### 8.4 反向验证计划（本轮禁跑 cargo，交由主智能体执行）

| # | 回退内容 | 预期失败用例 |
|---|---|---|
| 1 | `mem0/memory.rs::convert_to_mem0_message` 恢复 `name: msg.name().map(str::to_string)` | `convert_message_matches_java_and_omits_name`、`record_posts_all_configured_identifiers_without_a_name_key` |
| 2 | `bailian/memory.rs::new` 删掉 `api_key` 校验 | `new_rejects_blank_api_key` |
| 3 | `mem0/dto.rs::sync_filters` 改为空实现 | `search_request_keeps_user_id_mirrored_into_filters`、`search_wire_payload_always_carries_the_user_id_filter` |
| 4 | `retry.rs` 的 `is_retryable` 恒返 `false` | `retries_transport_errors_until_success`、`retries_server_errors_but_not_client_errors` |
| 5 | `telemetry.rs` 去掉 `metrics.record(...)` | `records_successful_requests`、`records_failed_attempts_and_returns_the_error_unchanged` |

### 8.5 未关闭项

- **无**。三项设计决定：① 真实端点自动化（环境受限，env 门控替代）；② `writeTimeout` / `shutdown()` / `close()`（reqwest 无对应能力）；③ Java bean 样板测试与 `MockWebServer` 设施不逐条搬运（§8.2 已给依据）。
- 跨模块需求：**无新增**。既有非阻塞建议仅「`ContentBlockKind` 可在 `agentscope_core::message` 层 re-export」（core 变更，本 crate 不做）。
