# HIGRESS 迁移执行历史（归档）

> 本文件是 [`../HIGRESS__GAP_TRACKER.md`](../HIGRESS__GAP_TRACKER.md) 的执行历史归档（截止 2026-10-31）。
> **现行状态以 tracker 为准**；本文件只读取证，不再更新。
> 内容来源：原 GAP_TRACKER 中 §5 之后各节，原文未改写（节标题：6. 静态复核记录（2026-10-22） · 7. P0 修复记录（2026-10-22） · 8. 执行记录（2026-10-28）— 关闭 P1-1 ~ P1-4 · 9. 执行记录（2026-10-31）— P2 收口）。

## 6. 静态复核记录（2026-10-22）

### 6.0 复核方式
- **纯静态核对**：未执行 `cargo build` / `cargo test`，未修改任何 Rust 代码；本节唯一改动对象为本文件。
- 手段：逐文件读取 Java 源码（`agentscope-extensions-higress` 4 主类 + 4 测类）与 Rust crate（`src/{lib,client,search_result,toolkit}.rs`），用行数/属性计数/符号比对；辅以 `codegraph` 确认 harness `McpClientManager` / `McpServerConfig` / `McpToolInfo` 契约，读 `~/.cargo/registry` 的 `rust-mcp-sdk 1.0.1` + `rust-mcp-schema 0.10.3` 源码确认 MCP 类型字段。
- 结论可信度：行数、类数、测试数、符号映射为**逐条实测**；「30 test 通过」为**上一轮实测 + 本轮属性数复核一致**（本轮未重跑）。

### 6.1 实测统计

| 维度 | Java | Rust |
|---|---|---|
| 主类 / 主文件数 | 4（`HigressMcpClientBuilder` / `HigressMcpClientWrapper` / `HigressToolkit` / `HigressToolSearchResult`） | 4（`client.rs` / `search_result.rs` / `toolkit.rs` / `lib.rs`） |
| 主代码行数（非空） | **1050**（414+305+111+220） | **1353**（828+309+197+19） |
| 主代码行数（含空行/注释） | 1146 | 1487 |
| 纯代码行（去注释去空行） | 472 | — |
| 测试类 / 测试文件数 | 4 | 2（`client.rs` / `search_result.rs` 内联 `mod tests`） |
| 测试行数（非空） | **1016**（219+380+128+289） | — |
| 测试方法 / 用例数 | **74**（25+23+8+18 个 `@Test`） | **30**（`#[test]` 27 + `#[tokio::test]` 3） |
| `#[ignore]` / `should_panic` | 无 `@Disabled` | 无 `#[ignore]` / 无 `#[should_panic]` |
| doc-test | — | 0（源码无 ```` ``` ```` code fence） |

> Java 测试方法 74 vs Rust 用例 30：Rust 侧为「参数化/纯内存」压缩后的子集，非 1:1 数量映射（逐条覆盖关系见 §6.3）。
>
> **2026-10-28 更新**：Rust 侧用例数 30 → **57**（`#[test]` 30 + `#[tokio::test]` 27，另 1 个 doc test）；P1-1 ~ P1-4 的测试缺口已补齐，最新数字见 §8。以下 §6.1 ~ §6.5 为 2026-10-22 静态复核快照，保留原文以便比对。

### 6.2 对齐结论表

| Java 类 / 能力 | Rust 对应（真实存在） | 行为覆盖判定 |
|---|---|---|
| `HigressMcpClientBuilder.create/sseEndpoint/streamableHttpEndpoint/header(s)/queryParam(s)/timeout/initializationTimeout/toolSearch(query[,topK])` | `client.rs` `HigressMcpClientBuilder::create/sse_endpoint/streamable_http_endpoint/header/headers/query_param/query_params/timeout/initialization_timeout/tool_search/tool_search_with_top_k` | ✅ **签名全覆盖**；`tool_search_with_top_k` 返回 `Result`（Java 抛 `IllegalArgumentException`）属合理映射；`build()` 已真建连（P0-1 已修复） |
| `HigressMcpClientBuilder.buildSync/buildAsync` | `client.rs` `build(&self, manager)` / `build_async(&self, manager)` | ✅ **等价**（P0-1 已修复）：`build()` = `build_config()` + `wrapper.initialize()`，后者经 `HigressMcpTransport::connect_server` 创建并 connect 底层 MCP client（生产实现 `McpClientManager`），失败原样返回、`initialized` 保持 false |
| `HigressMcpClientBuilder.validateConfiguration` | `client.rs` `fn validate()` | ✅ 三条校验（endpoint 空、transport 未设、toolSearch query 空）逐条对齐 |
| `HigressMcpClientWrapper.initialize` | `client.rs` `initialize()` | ✅ 幂等 ✅；**真建连**：`connect_server` + `initialization_timeout` 超时包装，失败保持 `initialized=false` 并向上报错（测试 `initialize_connects_and_records_full_config` / `initialize_propagates_connection_failure` / `initialize_applies_initialization_timeout`）；并发调用由 `init_guard` 串行化（`initialize_is_idempotent_and_only_connects_once`）（P0-1 已修复） |
| `HigressMcpClientWrapper.listTools`（tool search 分支） | `client.rs` `list_tools()` → `search_tools_with_top_k(query, top_k)` | ✅ **与 Java L164 两参重载一致**（含 `topK`）；转换走 `convert_to_mcp_tool` ✅；缓存 `cache_tools` ✅ |
| `HigressMcpClientWrapper.listTools`（非 tool search 分支） | `client.rs` `list_tools()` → `manager.find_client_id_by_server` + `manager.list_tools` | ✅ 委托语义对齐（Java `delegateClient.listTools()`）；前提是调用方已用 `to_server_config()` 建连 |
| `HigressMcpClientWrapper.callTool(name,args)` | `client.rs` `call_tool(name, arguments)` | ✅ 委托 + `is_error` 告警分支对齐 |
| `HigressMcpClientWrapper.callTool(name,args,meta)` | `client.rs` `call_tool_with_meta(name, arguments, meta)`；两参 `call_tool` 委托为 `meta=None` | ✅ 2026-10-28 已对齐：`meta` 原样传给 `HigressMcpTransport::call_tool_by_server` 直至 `McpClientManager::call_tool_by_server_with_meta`，`None` 即不发 `_meta`（P1-1 已关闭） |
| `HigressMcpClientWrapper.close` | `client.rs` `close()` | ⚠️ 本地状态复位 ✅；**未 `delegateClient.close()`**（Rust 侧连接归 harness，P2-3）—— **2026-10-29 勘误**：P2-3 已于 2026-10-28 关闭，`close()` 现调 `HigressMcpTransport::disconnect_by_server` 并复位状态（用例 `close_disconnects_and_resets_state`，见 §7.2 第 5 条与 §8.1） |
| `HigressMcpClientWrapper.getName/isToolSearchEnabled/getCachedTool` | `name()` / `is_tool_search_enabled()` / `get_cached_tool()` | ✅ 一一对应 |
| `HigressMcpClientWrapper.searchTools(query)` / `searchTools(query,topK)` | `search_tools(query)` / `search_tools_with_top_k(query, top_k)` | ✅ 单参只发 `{"query": q}`、两参发 `query`+`topK`（P2-1 已修）；`topK` 数值类型由 Java `int` → Rust `usize`（Java 负值不可表达，Rust 侧更严格） |
| `HigressMcpClientWrapper.convertToMcpTool` | `client.rs` `convert_to_mcp_tool()` | ✅ name/title/description/inputSchema/**outputSchema** 均映射（outputSchema 见 P1-2 已关闭；非 object 的 outputSchema 因 `ToolOutputSchema` 只能表达 object 而降级为 `None`）；inputSchema 仍只保留 `properties`+`required`（P2-2 待办） |
| `HigressMcpClientWrapper.propagateMeta`（继承自 core `McpClientWrapper`） | **缺失** | ❌ 无 `get/set_propagate_meta`（P2-4） |
| `HigressToolSearchResult.parse` | `search_result.rs` `HigressToolSearchResult::parse(&CallToolResult)` | ✅ `isError` → `structuredContent` 优先 → `content` 文本 JSON 回退 → 空 content 报错，**逐条对齐**；`parse(null)` 由类型系统排除（Java 返回 `"CallToolResult is null"`） |
| `HigressToolSearchResult.isSuccess/getErrorMessage/getTools/getToolNames/toString` | `is_success()` / `error_message()` / `tools()` / `tool_names()` / `Display` | ✅ 语义对齐；`toString` 文本格式微差（Rust `{:?}` 带引号，Java `List.toString()` 不带）（P2-5） |
| `HigressToolSearchResult.ToolInfo` | `HigressToolInfo`（`name/description/title/input_schema/output_schema`） | ✅ 5 字段对齐；`name` 缺失时 Java 为 `null`、Rust 为 `""`（Rust 更安全，Java 侧会在 `Tool.builder(null,..)` NPE） |
| `HigressToolkit`（继承 core `Toolkit`） | `toolkit.rs` `HigressToolkit`（newtype 包装 core `Toolkit`） | ⚠️ `new()/with_config()/tool_names()/get_tool()/get_higress_mcp_client()` ✅；**未继承 core `Toolkit` 的公开方法集**（需 `.toolkit()` / `.into_toolkit()`）（P2-6） |
| `HigressToolkit.registerMcpClient` | `toolkit.rs` `register_mcp_client(Arc<dyn McpToolSource>)` | ✅ 2026-10-28 已对齐：接受**任意** MCP 客户端（Java 基类语义由 `McpToolSource` 承接），`as_higress()` 等价 `instanceof` 决定是否缓存；`enable_tools`/`disable_tools`/`group`/`tool_name_prefix`/`preset_parameters`/`propagate_meta` 全部可用；注册时经 core `ToolRegistration::mcp_client` 落 `set_mcp_client_name`（P1-3 已关闭） |
| `HigressToolkit` 的 `registration().mcpClient(...).enableTools(...).group(...).apply()` | `toolkit.rs` `HigressToolkit::registration() -> McpClientRegistration`（`.mcp_client(...).enable_tools(...).group(...).apply().await`） | ✅ 链式 API 已暴露（P1-3 已关闭）；分组 / preset / MCP 归属复用 core `ToolRegistration`，过滤与 `toolNamePrefix` 为本 crate 适配（core 无对应能力） |
| Java `x_higress_tool_search` 协议契约 | `TOOL_SEARCH_NAME = "x_higress_tool_search"` + `search_tools_with_top_k` 写入 `{"query","topK"}` | ✅ **1:1**（Java L330 `Map.of("query", query, "topK", topK)`） |
| **Higress WASM / 插件配置契约** | — | ✅ **Java 侧不存在**：全仓 `agentscope-java` 无任何 `wasm`/`WASM` 关键字命中，`higress.md` 文档亦无插件配置章节；Higress 的 WASM 插件属**网关侧**能力，不在本 Java 模块契约内 → **Rust 无需对齐，无缺口** |

### 6.3 新发现缺口

> **状态（2026-10-31 收口）**：P1-1 ~ P1-4 / P0-1 / P2-1 / P2-3 已于 2026-10-28 关闭；**P2-4（wrapper 级 `propagate_meta`）、P2-5（`Display` 文本）、P2-6（`toolkit()`/`into_toolkit()`，原记「待办」实为早已落地）、P2-7（query 合并/编解码）已于 2026-10-31 关闭**；**P2-2 定性为「设计决定（不迁移）」**（`rust-mcp-schema` 0.10.3 `ToolInputSchema` 为封闭结构体，`$schema`/`additionalProperties` 无表示位）；**P2-8 为跨模块需求**（harness `McpServerConfig` 无 transport 字段）。逐条依据与用例见 §9。

| 编号 | 优先级 | 缺口 | 证据（Java / Rust） |
|---|---|---|---|
| P0-1 | ~~**P0**~~ 已修复 | ~~Builder 不建连，`build()` 返回的客户端不可用~~ → **已修复（2026-10-28）**：`build(&self, manager)` 现在 `build_config()` + `wrapper.initialize()`，`initialize()` 经 `HigressMcpTransport::connect_server` 真实 create+connect 底层 client，受 `initialization_timeout` 约束，失败向上返回并保持 `initialized=false`；`transport_type` / `timeout` / `initialization_timeout` 全部投影进 `McpConnectRequest`。测试：`initialize_connects_and_records_full_config`（断言 manager 可查到 client id 且 5 项配置全量透传）、`initialize_propagates_connection_failure`、`initialize_applies_initialization_timeout`、`initialize_is_idempotent_and_only_connects_once`、`initialize_rejects_transport_mismatch`。 | Java `HigressMcpClientBuilder.java:311-332, 357-379`；Rust `client.rs` `build()` / `initialize()` |
| P1-1 | **P1** | **`call_tool` 缺 meta 透传**：Java `callTool(name, args, meta)` 三参重载原样把 `meta` 交给 delegate（`HigressMcpClientWrapperTest#testCallTool_PassesMetaStraightThrough` 专门钉住，含 `null` 透传）；Rust `call_tool(tool_name, arguments)` 无 meta 形参，恒走 `call_tool_by_server`（meta 固定 `None`），harness 的 `call_tool_by_server_with_meta` 未被使用 → **traceId 等 `_meta` 无法透传到 Higress 网关**。 | Java `:240-272`；Rust `client.rs:466-493` |
| P1-2 | **P1** | **`convert_to_mcp_tool` 丢弃 `outputSchema`**：Java `convertToMcpTool` 在 `toolInfo.outputSchema() != null` 时 `builder.outputSchema(...)`；Rust `client.rs:556` 恒为 `output_schema: None`。→ 工具 schema 丢信息（模型侧看不到输出结构）。 | Java `:223-225`；Rust `client.rs:544-562` |
| P1-3 | **P1** | **`HigressToolkit` 注册能力大幅弱于 Java**：① 只能注册 `HigressMcpClientWrapper`（Java `registerMcpClient(McpClientWrapper)` 接受任意 wrapper，按 `instanceof` 决定是否缓存，Java 有 `testRegisterMcpClient_NonHigressClient` 钉住）；② 无 `enableTools`/`disableTools`/`group`/`toolNamePrefix`/`presetParameters`/`propagateMeta` 过滤与分组参数（Java 走 core `McpClientManager.registerMcpClient` 8 参重载 + `Toolkit.registration()` 流式 API，且 Higress 官方文档 `docs/v2/*/integration/infrastructure/higress.md` 明确宣传 `toolkit.registration().mcpClient(client).enableTools(...).group(...).apply()`）；③ 注册的工具**未调用 `toolkit.set_mcp_client_name()`**，丢失 core 的 MCP 客户端归属标注（Java `RegisteredToolFunction.mcpClientName`）。 | Java `HigressToolkit.java:99-111`、core `McpClientManager.java:73-230`、core `Toolkit.java:1030-1053`；Rust `toolkit.rs:67-80` |
| P1-4 | **P1** | **核心链路（tool-search 成功路径）无测试且不可测**：Java 有 `testListTools_WithToolSearch` / `..._Error` / `..._CachesTools` / `testListTools_WithoutToolSearch_CachesTools` / `testSearchTools_WithQueryAndTopK`（断言请求体 `topK==3`）/ `testConvertToMcpTool_*` 共 8+ 用例；Rust 仅覆盖失败路径。根因：`HigressMcpClientWrapper` 直接依赖**具体类型** `Arc<McpClientManager>`，**无 trait 抽象**，无法像 Java 用 Mockito mock delegate 那样注入桩 → 缓存路径、转换路径、请求体断言均无法离线验证（§3.3 的 `HigressToolkit` P2 测试同样因此挂起）。建议引入 `trait HigressMcpTransport { list_tools / call_tool }` 并让 `McpClientManager` 实现之。 | Java `HigressMcpClientWrapperTest.java:144-188, 282-305, 336-377`；Rust `client.rs:329-337`、`:859-895` |
| P2-1 | **P2** | **单参 `search_tools` 仍发 `topK` + 文档注释自相矛盾**：`client.rs:497-498` 注释称「omits `topK`」，实现却在 `:513` 无条件写入 `topK`；Java 单参重载只发 `{"query": q}`。 | Java `:317-320`；Rust `client.rs:495-516` |
| P2-2 | ~~**P2**~~ 设计决定（不迁移） | **`build_input_schema` 只保留 `properties` + `required`**：Java 把 `inputSchema` 整张 map 原样交给 `McpSchema.Tool.builder(name, map)`；Rust 丢弃 `$schema`/`additionalProperties`/`type` 变体等键，且 `properties` 中非对象值被 `filter_map` 丢弃。 | Java `:210-227`；Rust `client.rs:565-582` |
| P2-3 | **P2** | **`close()` 不释放底层连接**：Java `close()` 调 `delegateClient.close()`（并 try/catch 吞异常）；Rust 仅复位本地标志 + 清缓存，不 `manager.disconnect/remove_client`。**（2026-10-29 勘误：已修复——`close()` 现调 `disconnect_by_server` 并清理本地状态，用例 `close_disconnects_and_resets_state`）** | Java `:279-294`；Rust `client.rs` `close()` |
| P2-4 | ~~**P2**~~ 已关闭 2026-10-31 | **缺 `propagate_meta` 开关**：Java core `McpClientWrapper` 有 `getPropagateMeta()/setPropagateMeta()`；Higress wrapper 继承之，Rust 侧无对应 API（harness `McpServerConfig::propagate_meta` 仅在 `McpServerRegistrar` 路径生效，Higress 自行 `to_server_config()` 时未设置 → 默认 `true`）。 | Java core `McpClientWrapper.java:101-115`；Rust `client.rs:100-104` |
| P2-5 | ~~**P2**~~ 已关闭 2026-10-31 | **`toString` 文本格式微差**：Java `HigressToolSearchResult{tools=[a, b]}`；Rust `HigressToolSearchResult{tools=["a", "b"]}`（`{:?}` 带引号）。Java 断言仅 `contains("tools=")`，故不破坏测试契约。 | Java `:211-217`；Rust `search_result.rs:181-193` |
| P2-6 | ~~**P2**~~ 已关闭（早已落地，2026-10-31 核实） | **`HigressToolkit` 非 `Toolkit` 子类**：Java `HigressToolkit extends Toolkit`，可直接 `ReActAgent.builder().toolkit(higressToolkit)`；Rust 是 newtype，`ReActAgent` 需要 core `Toolkit`，须 `.into_toolkit()`（或 `.toolkit()` 借用）。 | Java `HigressToolkit.java:56`；Rust `toolkit.rs:22-25, 56-58`；core `react_agent.rs:1126` |
| P2-7 | ~~**P2**~~ 已关闭 2026-10-31 | **query 参数语义弱于 Java**：Java core `extractEndpoint()` 会 **URL-decode 原 URL query、按键合并（新增优先）、并 `URLEncoder.encode` 后拼接**；Rust `endpoint_with_query()` 直接 `k=v&k2=v2` 追加、无编码、同名键会产生重复参数。 | Java core `McpClientBuilder.java:748-800`；Rust `client.rs:84-96` |
| P2-8 | ~~**P2**~~ 转跨模块需求（harness） | **transport 选择未透传**：Rust `HigressClientConfig.transport_type` 仅存于配置，`to_server_config()` 不携带；harness 依 URL 是否含 `/sse` 自行嗅探（`mcp_client_manager.rs:540-542`），与 Java 显式 `sseTransport/streamableHttpTransport` 不等价（URL 含 `/sse` 的 StreamableHTTP 端点会被误判）。 | Java `:413-416`；Rust `client.rs:100-104`、`harness/mcp_client_manager.rs:217` |

### 6.4 勘误列表（原文 → 实际）

| # | 原文位置 | 原文 | 实际 |
|---|---|---|---|
| 1 | §1 模块概览 | 「Java 规模（初测）: 主代码 ~1071 行 / 测试 ~971 行」 | **1050 / 1016**（非空行；含空行 1146 / 1252）。rollup `EXTENSIONS_GAP_TRACKER.md` L36 记的 `1071 / 971` 同样与实测不符（主多记 21、测少记 45），建议汇总方一并校正 |
| 2 | §3.3 测试表 | 「`HigressMcpClientBuilder` 校验测试 … **15 个用例**」 | **13 个** builder/config 用例（`client.rs` 21 = 13 builder + 3 `convert_to_mcp_tool` + 1 `tool_info_new` + 4 wrapper） |
| 3 | §3.3 测试表 | 「`HigressMcpClientWrapper` 行为测试 … **7 个用例**」 | 拆分口径应为 **4（wrapper）+ 3（`convert_to_mcp_tool`）= 7**；若把 `tool_info_new` 计入 builder 则 builder 为 14。原文未说明归属，属口径不清 |
| 4 | §4.1.1 第 2 步 | 「`search_tools(&self, query)` 单参方法 … 在其内部委托 `search_tools_with_top_k`」+ `client.rs:497` 注释「omits `topK`」 | 实现与注释**矛盾**：单参 `search_tools` 实际仍发送 `topK`（`client.rs:513`），与 Java 单参重载（只发 `query`）不一致 → 登记 P2-1 |
| 5 | §4.1.1 标题 | 「本轮修复（search_tools topK 参数不匹配）」 | 修复**方向正确且已生效**：`list_tools` 的 tool-search 分支现携带配置 `topK`，与 Java L164 一致；但**未消除**单参 `search_tools` 的 topK 语义偏差（见上一条） |
| 6 | §2.1 依赖映射 | 「`java.time.Duration` → `std::time::Duration` **仅作为配置保存**（连接由 harness 完成）」 | 已不变：`timeout`/`initialization_timeout` 现随 `McpConnectRequest` 传递给 harness 连接层（`initialize()` 真建连，P0-1 已修复），连接本身仍由 harness 执行 |
| 7 | §4.1 | 「**已完成**：4 个 Java 主类全部迁移；… `cargo build` / `cargo test` 全绿」 | 编译/测试结论本轮**未复跑**（静态复核），沿用上一轮实测；「4 个主类全部迁移」在**类型层面**成立；行为层面原缺 builder 建连 / meta 透传 / toolkit 注册过滤（P0-1 / P1-1 / P1-3），其中 P0-1 与 P1-1/P1-3 均已于 2026-10-28 修复 |
| 8 | §3.3 | 「【校正】本 crate 原无 `#[cfg(test)]` … 本轮已补齐 30 个离线用例」 | 数量核实**一致**（`#[test]` 27 + `#[tokio::test]` 3 = 30；`client.rs` 21 + `search_result.rs` 9）；但「30 个」覆盖的是**子集**，Java 侧共 **74** 个 `@Test`，差集集中在 tool-search 成功路径 / 缓存路径 / meta 透传 / toolkit 注册过滤（P1-1 ~ P1-4） |

### 6.5 复核后建议的 rollup 状态值

> **2026-10-28 更新**：P1×4 与 **P0-1** 关闭后，本模块**无已知 P0/P1 级缺口**；仅剩若干 P2。建议 rollup 状态：`已完成（build + test 通过，57 lib test + 1 doc test；P0-1 建连与 P1-1 ~ P1-4 已关闭；遗留 P2×5，见 HIGRESS__GAP_TRACKER.md §6.3/§8）`。

- **建议值：`已完成`**（保持不变）+ 状态描述中追加限定语。
- **理由**：① 4 个 Java 主类均有真实存在的 Rust 对应实现，模块可编译、57 lib test 全绿，**核心链路（builder 建连 + `x_higress_tool_search` 选工具 + 结果解析 + 工具缓存 + toolkit 注册）代码路径完整**；② 剩余缺口全部属于**契约增强类**（meta 透传 / 注册过滤 / 可测性），无「Java 有、Rust 完全无对应能力」的整类缺失；③ 原 P0-1（builder 不建连）已于 2026-10-28 修复，`build()` 经 `initialize()` 真建连且失败可观测。
- **建议状态描述**（供汇总方登记）：`已完成（build + test 通过，57 lib test + 1 doc test；P0-1 builder 建连与 P1-1 ~ P1-4 已关闭；遗留 P2×5，见 HIGRESS__GAP_TRACKER.md §6.3/§8）`。
- **若汇总方按缺口加权**，可将遗留 P2 项视为「已完成（需二次收尾）」而非「已完成」，但**不建议降级为「部分完成」**——因主类映射与协议契约已 1:1，仅差增强项。


## 7. P0 修复记录（2026-10-22）

### 7.1 缺陷复核

`crates/agentscope-extensions-higress/src/client.rs` 原 `initialize()` 只把本地 `initialized` 置 `true`，从未调用 `McpClientManager::create_client/connect`；`build()` 也只 `new` + `initialize`。因此：

- 非 tool-search 的 `list_tools()` 调用 `find_client_id_by_server` 永远返回 `None`，稳定报 `No connected MCP client for server '...'`；
- `transport_type` / `timeout` / `initialization_timeout` 三个配置项只存不读，实际全部无效；
- 连接失败也无法暴露，`build()` 会返回一个「自称已初始化」的对象。

Java 侧 `HigressMcpClientBuilder.buildSync/buildAsync` 先构建 delegate client，再 `wrapper.initialize()`；`HigressMcpClientWrapper.initialize()` 委托 `delegateClient.initialize()` 真正建连，成功后才置 `initialized = true`。本次修复按此语义把「建连」放进 `initialize()`（`build()` 复用同一路径），而不是只改 `build()`。

### 7.2 实现方式

`McpClientManager` 是具体类型（内部直接开 socket，无 trait 抽象、无 `#[cfg(test)]` 替身），无法注入/观测。按其真实 API（`create_client` → `connect` → `find_client_id_by_server` / `list_tools` / `call_tool_by_server` / `disconnect`）在 higress crate 内定义了**窄 trait `McpConnection`**（仅覆盖 wrapper 实际使用的 5 个操作，不是平行 MCP 抽象）：

- 为 `McpClientManager` 实现 `connect_server` = `create_client` + `connect`；连接失败时 `remove_client` 清理半注册实例并返回 `Err`；
- 测试内用 `RecordingConnection` 记录收到的 `McpConnectRequest`（含 `McpServerConfig` 的 url/headers + transport + timeout + initialization_timeout），并模拟已连接的工具列表；
- `HigressMcpClientWrapper` 内部持有 `Arc<dyn McpConnection>`；公开 API `new/build/build_async` 仍接收 `Arc<McpClientManager>`，无破坏性变更。

行为变化：

1. `initialize()` 真正建连；`initialization_timeout` 通过 `tokio::time::timeout` 约束建连；失败/超时返回 `Err` 且 `is_initialized()` 保持 `false`。
2. 幂等：`tokio::sync::Mutex` 串行化 check-then-connect；已初始化或 manager 中已有同 server 连接时直接复用，不重复建连。
3. `timeout` 通过 `with_request_timeout` 约束 `list_tools` / `call_tool`（含 tool-search 路径）实际调用。
4. `transport_type`：harness 仅按 URL 是否含 `/sse` 选传输，`McpServerConfig` 无 transport 字段。现在 `initialize()` 会校验配置与 URL 路由是否一致，不一致返回明确 `Err`，不再静默忽略（例如 `sse_endpoint(".../mcp")` 会报错）。
5. `close()` 现在会 `disconnect_by_server` 并清理本地状态（顺带修复原 P2-3）。
6. `with_options()` 保留，但不再伪造「已初始化」：无 endpoint 且 manager 无既有连接时 `initialize()` 返回明确错误。

### 7.3 测试（36 passed / 0 failed）

新增用例（改前必失败）：

- `initialize_connects_and_records_full_config`：`initialize()` 后 `find_client_id_by_server("higress")` 命中，且记录到的请求含 URL（含 query）、headers、`transport`、`timeout=5s`、`initialization_timeout=7s`。改前 manager 从未收到任何配置。
- `list_tools_after_initialize_does_not_report_missing_client`：非 tool-search 模式 `list_tools()` 返回 fake 的 `echo/add` 并写入缓存。改前稳定报 `No connected MCP client`。
- `initialize_is_idempotent_and_only_connects_once`：两次 `initialize()` 只产生 1 次 connect。
- `initialize_propagates_connection_failure`：连接失败返回 `Err` 且未标记初始化。
- `initialize_applies_initialization_timeout`：建连超过 `initialization_timeout` 返回超时 `Err`。
- `initialize_rejects_transport_mismatch`：SSE 配置配非 `/sse` URL 时返回明确错误，且未发起连接。
- `close_disconnects_and_resets_state`：`close()` 后连接被移除、状态复位。

验证命令与结果：

```
cargo check -p agentscope-extensions-higress   # EXIT=0
cargo test  -p agentscope-extensions-higress   # 36 passed; 0 failed
```

### 7.4 未完成 / 已知限制

- **未做真实网关集成测试**：单元测试用 `RecordingConnection` 隔离网络；真实 SSE/Streamable HTTP 握手依赖外部 Higress 网关，本环境不可用。
- **transport 仍受 harness URL 启发式约束**：`/sse` 之外无法强制 SSE。当前策略是「校验并报错」而非「底层改传输」，因为修改 `McpServerConfig`/manager 超出本 crate 允许范围。
- **timeout 是 wrapper 层超时**，不是 `McpClientManager` 内部请求超时；`tokio::time::timeout` 能及时向调用方返回错误，但不会取消 manager 已持有的底层请求（manager 无该能力）。
- P1-4（tool-search / meta 透传 / toolkit 注册等测试缺口）仅因可注入 `McpConnection` 而具备继续补测的基础，本次未补齐；P2-1/P2-2/P2-4~P2-7 未处理。**【2026-10-28 更新】** P1-1 ~ P1-4 与 P2-1 已在 §8 关闭（`McpConnection` 更名为公开 trait `HigressMcpTransport` 并补齐 21 个用例）；P2-2 / P2-4 ~ P2-8 仍待办。

## 8. 执行记录（2026-10-28）— 关闭 P1-1 ~ P1-4

> 本轮为**执行**（含编译 + 测试 + 反向验证），非静态复核。改动范围：`crates/agentscope-extensions-higress/src/{client,toolkit,lib}.rs` + 本 tracker。**未修改**根 `Cargo.toml`、`agentscope-core/**`、`agentscope-harness/**`、其它 crate（harness/core 仅只读查阅 API）。

### 8.1 逐条落地（Java 依据 → Rust 落点）

| 编号 | Java 依据（文件:行号） | Rust 实现 |
|---|---|---|
| **P1-1** meta 透传 | `HigressMcpClientWrapper.java:240-243`（两参重载 → `callTool(name,args,null)`）、`:245-272`（三参原样交 delegate）；`HigressMcpClientWrapperTest.java:238-260` `testCallTool_PassesMetaStraightThrough`（钉住含 null 的原样传递） | `client.rs`：**保留**两参 `call_tool(name, args)`（委托 `meta=None`），**新增** `call_tool_with_meta(name, args, meta)`；两者走同一内部路径，最终到 `HigressMcpTransport::call_tool_by_server(…, meta)` → `McpClientManager::call_tool_by_server_with_meta`。`None` 全程保持 `None`（不造空 map），故线上不发 `_meta` |
| **P1-2** outputSchema | `HigressMcpClientWrapper.java:223-225`（`if (outputSchema != null) builder.outputSchema(...)`） | `client.rs::convert_to_mcp_tool` 补 `output_schema`；新增 `build_output_schema()`（`{"type":"object"\|缺省}` → `ToolOutputSchema`，非 object 无法表达时降级 `None`，已在 doc 注明） |
| **P1-3** Toolkit 注册能力 | `HigressToolkit.java:99-116`（`registerMcpClient` 覆写 + `instanceof` 缓存）、`McpClientManager.java:130-190/203-330`（enable/disable/prefix/preset/propagateMeta 语义）、`Toolkit.java:1030-1330`（`ToolRegistration.apply` 的 mcpClient 分支） | `toolkit.rs` 重写：① 新增 `trait McpToolSource`（`name`/`initialize`/`list_tools`/`call_tool(name,args,meta)`/`as_higress()`），`register_mcp_client(Arc<dyn McpToolSource>)` 接受任意客户端，`as_higress()`（默认 `None`，Higress 实现返回 `Some(self)`）等价 Java `instanceof` 决定是否缓存；② 新增 `McpClientRegistration`（`HigressToolkit::registration()`）提供 `mcp_client`/`enable_tools`/`disable_tools`/`group`/`tool_name_prefix`/`preset_parameters(_for)`/`propagate_meta(_for)`/`apply`；③ 工具改由 core `McpTool`（`with_async_handler` + `with_propagate_meta` + `with_title`）承载，**经 core `ToolRegistration::group/preset_parameters/mcp_client` 提交** → 自动落 `Toolkit::set_mcp_client_name`（要求 ③）。**core 无等价能力的两项在本 crate 适配并在代码注释说明**：`enableTools`/`disableTools` 过滤（`should_register_tool`，enable 优先，空表=不限制，同 `McpClientManager.shouldRegisterTool`）、`toolNamePrefix` 重命名（模型侧名加前缀，网关仍收远端原名）；另 Java 会把 preset 键从 schema 中隐藏（`McpTool.convertMcpSchemaToParameters`），core 仅在调用时合并 → 本 crate 在 `tool_parameters()` 里剔除 preset 键。per-tool `propagateMeta` 覆写命中未注册工具时 fail-loud（同 `McpClientManager.java:250-282`，错误含排序后的工具名） |
| **P1-4** 可测性 | Java `HigressMcpClientWrapperTest.java:144-188`（tool-search 成功/缓存）、`:282-305`（`testSearchTools_WithQueryAndTopK` 断言 `topK`）、`:336-377`（`convertToMcpTool_*`） | `client.rs`：`McpConnection` → **公开窄 trait `HigressMcpTransport`**（`connect_server`/`find_client_id_by_server`/`list_tools`/`call_tool_by_server(…,meta)`/`disconnect_by_server`），`impl HigressMcpTransport for McpClientManager` 为生产实现（内部转 `call_tool_by_server_with_meta`），新增 `HigressMcpClientWrapper::with_transport(config, Arc<dyn HigressMcpTransport>)` 注入点（`new/build` 不收变），`McpConnectRequest` 一并公开 |
| **P2-1**（顺带）| `HigressMcpClientWrapper.java:317-319` 单参只发 `{"query": q}` | `client.rs::search_tools` 不再附带 `topK`，与 doc 注释一致；两参 `search_tools_with_top_k` 不变，`list_tools` 的 tool-search 分支仍走两参（Java `:164`） |
| P2-3 清理 | — | 删除无调用者的 `pub(crate) fn text_content`（原 dead-code warning），本 crate 现 0 warning |

### 8.2 测试（57 lib passed / 0 failed；另有 1 doc test passed）

新增 21 个用例（`client.rs` +11、`toolkit.rs` +10）：

- `client::tests::tool_search_lists_converted_tools_and_sends_query_with_top_k` — 成功路径：`x_higress_tool_search` 返回的 tool 转成 `McpTool`，断言 `name/title/description/input_schema.required/output_schema`，并断言请求体 `query=="weather"`、`topK==3`、`meta.is_none()`。
- `client::tests::tool_search_results_are_cached_and_served_without_a_second_call` — 缓存命中：`get_cached_tool()` 可用且网关调用次数仍为 1。
- `client::tests::list_tools_without_tool_search_caches_every_tool` — 无 tool-search 时全量缓存，且不调用 tool-search 工具。
- `client::tests::tool_search_failure_surfaces_as_list_tools_error` / `tool_search_falls_back_to_text_content_payload` — 失败上抛、`content` 文本回退。
- `client::tests::call_tool_with_meta_passes_meta_straight_through` / `call_tool_without_meta_passes_none` — meta 原样透传（含 server 路由断言）与 `None` 透传（两参与显式 `None` 各一次）。
- `client::tests::search_tools_single_argument_omits_top_k` — 单参请求体无 `topK`。
- `client::tests::convert_to_mcp_tool_forwards_output_schema` / `..._drops_non_object_output_schema` / `..._handles_typeless_object_output_schema` — outputSchema 三类形态（连同既有 3 个 input schema 用例，`convert_to_mcp_tool` 共 6 个用例）。
- `toolkit::tests::register_mcp_client_registers_every_tool`（全量注册 + `initialize` 先行 + `mcp_client_name` 标注）、`non_higress_client_is_not_cached`（Java `testRegisterMcpClient_NonHigressClient`）、`higress_client_is_cached_after_registration`、`registration_filters_with_enable_and_disable_tools`（enable 优先 / 空表=全部）、`registration_applies_prefix_group_and_presets`（前缀名、分组、preset 从 schema 隐藏且进入 toolkit、远端调用名不变）、`registered_tool_propagates_tool_call_metadata`、`propagate_meta_false_omits_metadata`、`propagate_meta_per_tool_override_applies_to_that_tool_only`、`propagate_meta_per_tool_override_rejects_unknown_tool`、`apply_without_client_fails`。

命令与结果：

```
cargo build -p agentscope-extensions-higress   # EXIT=0，本 crate 0 warning
cargo test  -p agentscope-extensions-higress   # EXIT=0
  test result: ok. 57 passed; 0 failed; 0 ignored   (lib)
  test result: ok.  1 passed; 0 failed             (doc-tests: registration() 示例)
```

### 8.3 反向验证（故意回退实现 → 确认 FAILED → 恢复）

| 轮次 | 回退内容 | 结果 |
|---|---|---|
| 1 | `build_output_schema` 恒 `None` + `call_tool_with_meta` 忽略 `meta`（恒传 `None`） | `test result: FAILED. 53 passed; 4 failed` — `client::tests::call_tool_with_meta_passes_meta_straight_through`、`convert_to_mcp_tool_forwards_output_schema`、`convert_to_mcp_tool_handles_typeless_object_output_schema`、`tool_search_lists_converted_tools_and_sends_query_with_top_k` |
| 2 | `should_register_tool` 恒 `true` + `resolve_propagate_meta` 恒 `true` + 单参 `search_tools` 重新附带 `topK` | `test result: FAILED. 53 passed; 4 failed` — `client::tests::search_tools_single_argument_omits_top_k`、`toolkit::tests::registration_filters_with_enable_and_disable_tools`、`propagate_meta_false_omits_metadata`、`propagate_meta_per_tool_override_applies_to_that_tool_only` |
| — | 全部恢复后重跑 | `57 passed; 0 failed`（+ 1 doc test） |

### 8.4 与 Java 的残留差异（非缺陷，已登记）

1. **`propagateMeta` 的 wrapper 级开关**：Java 继承 core `McpClientWrapper.get/setPropagateMeta`（连接级实时开关）；Rust 的连接级开关在 harness `McpServerConfig::propagate_meta` + `McpClientManager::set_propagate_meta`，本 crate 未在 wrapper 上再包一层 → P2-4 仍为「部分完成」。
2. **`ToolOutputSchema` 只能表达 object**：Java 把 `outputSchema` map 原样透传；Rust 类型系统限制下，`type != object` 的输出 schema 降级为 `None`（不会谎报 object 形状）。
3. **`group` 语义**：Java `McpClientManager` 要求分组已存在（`validateGroupExists`），core `ToolRegistration.group` 会按需创建 → 本 crate 采用 core 语义（不存在则创建），已在 `group()` doc 注明。
4. **`initialize()` 时机**：`apply()`/`register_mcp_client()` 现在先 `initialize()` 再 `list_tools()`（同 Java），未初始化且无 endpoint 的客户端会得到明确错误（原先报 `No connected MCP client`）。
5. **`McpToolSource` 是 Rust 侧替身**：Java 的 `McpClientWrapper` 基类在 Rust 无对应，本 crate 用窄 trait 承接其「任意客户端 + `instanceof` 缓存」语义；其它 C 波次模块若也需 MCP 客户端包装，可考虑上提（见 §4.2 第 2 条）。

## 9. 执行记录（2026-10-31）— P2 收口

> 本轮为**代码执行**（未跑 cargo，由主智能体统一验证）。改动仅限 `crates/agentscope-extensions-higress/src/{client,search_result}.rs`；未改根 `Cargo.toml`、core、harness 与其它 crate。

### 9.1 逐条处置

| 编号 | 处置 | 源码依据 / 落点 | 代表用例 |
|---|---|---|---|
| **P2-4** | **已关闭**：wrapper 级 `propagate_meta()` / `set_propagate_meta(&self, bool)`（`AtomicBool`，保留 Java 实时开关语义）+ `HigressClientConfig.propagate_meta` + `HigressMcpClientBuilder::propagate_meta(bool)`；`to_server_config()` 与 `initialize()` 均把开关投影进 `McpServerConfig.propagate_meta`（harness `mcp_client_manager.rs:66` 字段、`:98` setter） | Java core `McpClientWrapper.java:101-115`（get/set）；`client.rs` 新字段与方法 | `client::tests::propagate_meta_defaults_true_and_projects_into_server_config`、`client::tests::initialize_applies_propagate_meta_switch` |
| **P2-5** | **已关闭**：`Display` 改为 `HigressToolSearchResult{tools=[a, b]}`（`join(", ")`，无引号） | Java `HigressToolSearchResult.java:211-217`（`List.toString()`）；`search_result.rs` `impl Display` | `search_result::tests` 既有 Display 用例（断言 `tools=`） |
| **P2-6** | **已关闭（早已落地）**：`HigressToolkit::toolkit() -> &Toolkit`（`toolkit.rs:137`）与 `into_toolkit() -> Toolkit`（`:142`）均已存在，可直接喂 `ReActAgent.builder().toolkit(...)`；tracker 原表记为「待办」属**过时行** | Java `HigressToolkit extends Toolkit`；`toolkit.rs:137/142` | —（API 核实） |
| **P2-7** | **已关闭**：`endpoint_with_query()` 改为「既有 query 先 URL-decode → 配置参数覆盖同名键 → 按 Java `URLEncoder` 语义重新编码」；新增 `decode_query_component`/`encode_query_component`/`hex_value`（`+`↔空格、`%XX`、保留 `.-*_`）。Rust 仍返回完整 URL（含 scheme/host），Java `extractEndpoint()` 只返回 `path?query`（其 transport 需要），已在 doc 注明 | Java core `McpClientBuilder.java:740-795`；`client.rs` | `client::tests::endpoint_with_query_merges_duplicate_keys_and_encodes_like_java`（同时断言既有 `token` 被覆盖、`&`→`%26`、空格→`+`） |
| **P2-2** | **设计决定（不迁移）**：`rust-mcp-schema 0.10.3` 的 `ToolInputSchema` 是封闭结构体（仅 `type: "object"` + `properties` + `required`，见 `rust-mcp-schema-0.10.3/src/generated_schema/2025_06_18/mcp_schema.rs` 的 `pub struct ToolInputSchema` 与 `impl ToolInputSchema::new`），**没有** `$schema`/`additionalProperties`/`$defs` 的表示位；Java 侧 `McpSchema.Tool.builder(name, Map)` 保留的是无类型 map，因此能原样透传。`properties`/`required` 已完整保留，非对象 property 被丢弃是 MCP 规范要求（property 必须是 schema 对象）。已在 `build_input_schema` 加 **DESIGN:** 注释 | Java `HigressMcpClientWrapper.java:210-227`；Rust `client.rs::build_input_schema` | `client::tests::convert_to_mcp_tool_*`（既有 6 个） |
| **P2-8** | **跨模块需求（未关闭）**：`McpServerConfig` 无 transport 字段，harness 只能按 `url.contains("/sse")` 选 SSE/StreamableHTTP（`mcp_client_manager.rs:533-535`，调用点 `:216`）。本 crate 已做到「配置与 URL 路由不一致时 `initialize()` 明确报错」（`initialize_rejects_transport_mismatch`），但**无法显式指定传输**。需 harness 在 `McpServerConfig` 增 `transport: McpTransportKind`（或在 `McpConnectRequest` 同级透传），**超出本 crate 写入范围** | Java `HigressMcpClientBuilder.java:413-416`（显式 `sseTransport`/`streamableHttpTransport`） | `client::tests::initialize_rejects_transport_mismatch`（既有） |
| 真实网关联调 | 仍为**不做**：单测用 `RecordingConnection`，真实 SSE/StreamableHTTP 握手依赖外部 Higress 网关，本环境不可用（非缺口，沿用 §7.4） | — | — |

### 9.2 新增用例（4 个）

- `client::tests::endpoint_with_query_merges_duplicate_keys_and_encodes_like_java`
- `client::tests::propagate_meta_defaults_true_and_projects_into_server_config`
- `client::tests::initialize_applies_propagate_meta_switch`
- （`Display` 由既有 `search_result` 用例覆盖，未新增）

预期基线：**60 lib test + 1 doc test**（原 57 + 3）。验证命令：`cargo test -p agentscope-extensions-higress`。

### 9.3 反向验证建议（由主智能体执行）

1. 把 `to_server_config()` 的 `.propagate_meta(...)` 与 `initialize()` 的 `server.propagate_meta = ...` 去掉 → 应从 `propagate_meta_defaults_true_and_projects_into_server_config`、`initialize_applies_propagate_meta_switch` FAILED。
2. `endpoint_with_query` 回退为「`k=v&` 直接拼接」→ 应从 `endpoint_with_query_merges_duplicate_keys_and_encodes_like_java` FAILED。
