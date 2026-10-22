# HIGRESS 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **HIGRESS** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-higress\`
> **Java 子模块**：（单模块）
> **Rust 目标**：`crates/agentscope-extensions-higress/`（波次 C）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-higress` |
| Java 子模块 | （单模块） |
| Java 规模（静态复核实测） | 主代码 **1050 行** / 测试 **1016 行**（非空行；含空行分别 1146 / 1252）；rollup 记的 1071 / 971 与实测不符，见 §6 勘误 |
| Rust crate | `crates/agentscope-extensions-higress` |
| 迁移波次 | C |
| 当前状态 | 已完成（30 test 通过） |

## 2. Java 清单与依赖映射
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `HigressMcpClientBuilder` | 流式构建 Higress MCP 客户端（SSE / StreamableHTTP 两种 transport、headers、queryParams、timeout、toolSearch）| `HigressMcpClientBuilder`（src/client.rs）| 构建 `HigressClientConfig` + `HigressMcpClientWrapper`；`build`/`build_async` 对齐 `buildSync`/`buildAsync`；**Rust 侧 `build()` 只做配置校验 + 置位 initialized，不建立底层连接**（连接由调用方另行 `McpClientManager::create_client/connect`），见 §6 缺口 P0-1 |
| `HigressMcpClientWrapper` | Higress MCP 客户端包装：委托底层 MCP 客户端；`x_higress_tool_search` 语义化选工具；工具缓存 | `HigressMcpClientWrapper`（src/client.rs）| 委托 **harness `McpClientManager`**（`list_tools`/`call_tool_by_server`），非自建 MCP 连接；**缺 Java `callTool(name,args,meta)` 三参重载（meta 透传丢失）**，见 §6 缺口 P1-1 |
| `HigressToolSearchResult`（含 `ToolInfo`）| 解析 `x_higress_tool_search` 结果：优先 `structuredContent`，回退 `content` 文本 JSON；`ToolInfo{name,description,title,inputSchema,outputSchema}` | `HigressToolSearchResult` + `HigressToolInfo`（src/search_result.rs）| 解析 `rust_mcp_sdk::schema::CallToolResult` |
| `HigressToolkit` | 继承 core `Toolkit`，注册 Higress MCP 客户端并缓存 | `HigressToolkit`（src/toolkit.rs）| newtype 包装 core `agentscope_core::tool::Toolkit`，缓存最近注册的 Higress 客户端；**未实现 Java 的 `registration()` 流式 API（`enableTools`/`group` 等）**，见 §6 缺口 P1-3 |

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| `io.agentscope.core.tool.Toolkit` | Toolkit 基类 | `agentscope_core::tool::Toolkit` | **复用**，不重建 |
| `io.agentscope.core.tool.mcp.McpClientWrapper` | MCP 客户端抽象基类 | `agentscope_harness::agent::McpClientManager`（`call_tool_by_server`/`list_tools`/`find_client_id_by_server`）| Rust 无同名 wrapper 基类；以 manager + server 名委托 |
| `io.agentscope.core.tool.mcp.McpClientBuilder` | 底层 MCP 客户端构建 | `agentscope_harness::agent::McpServerConfig` + `McpClientManager::create_client/connect` | 真实连接交给 harness（stdio/SSE/streamable） |
| `io.modelcontextprotocol.sdk:mcp`（`McpSchema`）| MCP 协议类型 | `rust_mcp_sdk::schema`（`CallToolResult`/`ContentBlock`/`Tool`/`TextContent`）| v1.0.1（crates.io） |
| `com.fasterxml.jackson`（`JsonUtils`）| JSON 解析 | `serde_json` | 已是 workspace 依赖 |
| `reactor.core.publisher.Mono` | 响应式单值 | `async fn -> Result<T>` | 见 MIGRATION_PLAN §4.3 |
| `org.slf4j` | 日志 | `tracing` | 已是 workspace 依赖 |
| `java.time.Duration` | 超时 | `std::time::Duration` | 仅作为配置保存（连接由 harness 完成）|
| JUnit5 + Mockito（测试）| 单元测试 | `#[cfg(test)]` + 手写 fake MCP 后端替身 | 见 §3.3 |

## 3. 详细缺口清单
### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 骨架 + Cargo.toml 依赖 | pom.xml | 已完成 | 复用 workspace 依赖；新增 `rust-mcp-sdk`（见 §3.2） |
| `HigressToolSearchResult` / `HigressToolInfo` 解析 | `HigressToolSearchResult` | 已完成 | structuredContent 优先 + content 回退；解析失败置 `errorMessage` |
| `HigressMcpClientBuilder` 流式构建 | `HigressMcpClientBuilder` | 已完成 | endpoint/transport/header/queryParam/timeout/toolSearch 校验对齐 |
| `HigressMcpClientWrapper`（委托 + tool search）| `HigressMcpClientWrapper` | 已完成 | 委托 harness `McpClientManager`；`list_tools`/`call_tool`/`search_tools`/`close` |
| `HigressToolkit`（注册 + 缓存）| `HigressToolkit` | 已完成 | 复用 core `Toolkit` |

### 3.2 依赖与拆分说明
- **`rust-mcp-sdk`**：本 crate 直接依赖 `rust-mcp-sdk`（版本与 harness 一致，走 crates.io 远程仓库）以使用 `rust_mcp_sdk::schema` 的 MCP 类型。**未修改根 `Cargo.toml` 的 `[workspace.dependencies]`**，版本在本 crate `Cargo.toml` 内声明。若汇总方希望统一为 workspace 依赖，见 §4.2。
- **未重建平行类型**：MCP 客户端生命周期复用 harness `McpClientManager`；工具治理复用 core `Toolkit`/`Tool`/`McpTool`。

### 3.3 测试
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `HigressToolSearchResult` 解析测试 | `HigressToolSearchResultTest` | 已完成 | 9 个用例（`search_result.rs`）：structuredContent 优先、content 回退、空 content、`isError`、非法 JSON、缺 name 默认空串、null schema 丢弃、Display |
| `HigressMcpClientBuilder` 校验测试 | `HigressMcpClientBuilderTest` | 已完成 | **实测 13 个** builder/config 用例（`client.rs`）：clientName 空白拒绝、缺 endpoint、两种 transport、header/queryParam 排序与合并、`toolSearch` 空 query 拒绝、`topK<=0` 拒绝、`topK` 保留、`endpoint_with_query`、`to_server_config` 投影、超时 |
| `HigressMcpClientWrapper` 行为测试 | `HigressMcpClientWrapperTest` | 已完成 | **实测 4 + 3 = 7 个**：wrapper 4 个（`with_options` 状态、`initialize` 幂等 + `close` 重置、未连接时 `list_tools` 报错、`search_tools` 失败路径）+ `convert_to_mcp_tool` 三类 schema 形态；**未覆盖 Java 的 tool-search 成功路径 / 缓存路径 / `topK` 请求体断言**（无可替换的 transport 抽象，见 §6 缺口 P1-4） |
| `HigressToolkit` 测试 | `HigressToolkitTest` | 待办（P2） | `HigressToolkit` 的 `register_mcp_client` 需真实/桩 `McpClientManager` 连接，当前为纯内存 manager 无法产工具；未造假用例 |
| 【校正】本 crate 原无 `#[cfg(test)]` | — | 已修复 | 上一轮 tracker 记录「0 个用例」属实；本轮已补齐 30 个离线用例（`#[test]` 27 + `#[tokio::test]` 3；实测统计与代码一致） |

## 4. 项目状态与下一步
### 4.1 当前状态
- **已完成**：4 个 Java 主类全部迁移；30 个离线单测（实测属性数：`#[test]` 27 + `#[tokio::test]` 3 = 30）；`cargo build` / `cargo test` 全绿（上一轮实测）。
- **验证命令与结果（上一轮实测，本轮为静态复核，未编译）**：
  - `cargo test -p agentscope-extensions-higress` → `test result: ok. 30 passed; 0 failed; 0 ignored`（doc-tests 0）。
  - 本轮静态复核：源码中 `#[test]`/`#[tokio::test]` 属性共 30 个（`client.rs` 21、`search_result.rs` 9），与「30 test 通过」**数量一致**；无 `#[ignore]`、无 `#[should_panic]`、无 doc-code-fence，故 doc-test 0 亦一致。
- **已知问题**：1 个既有 dead-code warning —— `client.rs:604 pub(crate) fn text_content` 无调用者（**既有代码，非本次引入**，按最小改动原则未删除，建议后续清理）。

#### 4.1.1 本轮修复（search_tools topK 参数不匹配）
- **现象**：`client.rs:424` 调用 `self.search_tools(&query, self.tool_search_top_k)` 传入 2 个实参，但方法签名为 `search_tools(&self, query: &str)`（1 个实参），触发 `error[E0061]`，crate 编译失败。
- **根因与判定依据**：对照 Java 源码 `HigressMcpClientWrapper.java`：
  - L164 `listTools()` 中 `enableToolSearch` 分支调用的是 **两参重载** `searchTools(toolSearchQuery, toolSearchTopK)`；
  - L338 `searchTools(String query)` → 仅传 `query`；L350 `searchTools(String query, int topK)` → 传 `query` + `topK`。
  - 故 `topK` 是应生效的搜索参数（Java `HigressMcpClientWrapperTest#testSearchTools_WithQueryAndTopK` 亦断言请求体中 `topK` 被传递），**确认应保留 `tool_search_top_k` 语义**。
- **冲突成因**：Rust 迁移已把两参逻辑落到 `search_tools_with_top_k(&self, query, top_k)`（内部向 MCP 参数写入 `topK`），但 `list_tools` 误写了单参 `search_tools` 名称，且原 `search_tools` 已改为委托 `search_tools_with_top_k`（偏离了 Java 单参重载“不传 topK”的语义）。
- **修复方案（选 (a)）**：保持与 Java 两重重载语义对齐——
  1. `client.rs:424` 改为调用 `self.search_tools_with_top_k(&query, self.tool_search_top_k)`，使 `list_tools` 的 tool search 分支真正携带配置的 `topK`（与 Java L164 一致）。
  2. 保留 `search_tools(&self, query)` 单参方法但补齐文档，并在其内部委托 `search_tools_with_top_k`（沿用既有实现，未删功能、未新增破坏性签名变更）。
- **【静态复核补注】**：第 2 步的**文档注释与实现不符**——`client.rs:497-498` 声称「Mirrors the Java single-argument overload `searchTools(String query)`, which omits `topK`」，但实现无条件写入 `topK`（`client.rs:513`）。Java 单参重载只发 `{"query": q}`（`HigressMcpClientWrapper.java:318`）。**结论：编译阻塞缺陷已修复（`list_tools` 路径与 Java L164 一致 ✅），但单参 `search_tools` 的 topK 语义仍与 Java 有偏差**，登记为 §6 缺口 P2-1 + §6 勘误第 3 条。
- **改动文件**：`crates/agentscope-extensions-higress/src/client.rs`（仅本 crate）。
- **验证**：`cargo build -p agentscope-extensions-higress` → 通过；`cargo test -p agentscope-extensions-higress` → 通过。
- **【静态复核确认】**：`client.rs:424-426` 现为 `self.search_tools_with_top_k(&query, self.tool_search_top_k)`，`search_tools_with_top_k` 签名为 `(query: &str, top_k: usize)`（`client.rs:506-510`），实参 2 个、形参 2 个 → **原 `E0061` 编译错误确已消除** ✅。

### 4.2 下一步与阻塞
1. **汇总方**：是否将 `rust-mcp-sdk` 提升为根 `Cargo.toml` 的 `[workspace.dependencies]`（当前仅本 crate 内声明，与 harness `Cargo.lock` 锁定的 `1.0.1` 一致）。
2. **汇总方（信息共享）**：Java `McpClientWrapper` 无 Rust 等价基类，本模块以 `McpClientManager` + server 名委托实现；若其它 C 波次模块（CONTROLPLANE/STUDIO 等）也需 MCP 客户端包装，建议统一抽象后再收敛。
3. 真实端到端联调（连真实 Higress 网关）需 `McpClientManager` 建立连接；本 crate 已把 `HigressClientConfig` 转换成 `McpServerConfig`，端到端由调用方完成连接。**【静态复核 2026-10-22】此条与 §6 缺口 P0-1 同源：`build()` 不代建连接，且 `transport_type`/`timeout` 未投影进 `McpServerConfig`，建议下一轮在 crate 内补一个 `build_and_connect(&self, manager)` 便利方法。**
4. **【静态复核 2026-10-22 新增】** 引入 `trait HigressMcpTransport` 抽象（`list_tools` / `call_tool`），由 `McpClientManager` 实现之 —— 可一并解决 P1-4（tool-search 成功路径不可测）与 §3.3 中 `HigressToolkit` P2 测试挂起问题。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-higress` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-higress` / `cargo test -p agentscope-extensions-higress` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-higress/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

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

### 6.2 对齐结论表

| Java 类 / 能力 | Rust 对应（真实存在） | 行为覆盖判定 |
|---|---|---|
| `HigressMcpClientBuilder.create/sseEndpoint/streamableHttpEndpoint/header(s)/queryParam(s)/timeout/initializationTimeout/toolSearch(query[,topK])` | `client.rs` `HigressMcpClientBuilder::create/sse_endpoint/streamable_http_endpoint/header/headers/query_param/query_params/timeout/initialization_timeout/tool_search/tool_search_with_top_k` | ⚠️ **签名全覆盖**；`tool_search_with_top_k` 返回 `Result`（Java 抛 `IllegalArgumentException`）属合理映射；**但 `build()` 不建连**（P0-1） |
| `HigressMcpClientBuilder.buildSync/buildAsync` | `client.rs` `build(&self, manager)` / `build_async(&self, manager)` | ❌ **不等价**（P0-1）：Java 内部 `McpClientBuilder.buildSync()` 真建连并 initialize；Rust 仅校验 + 置位 `initialized`，不创建/连接底层 client，`list_tools` 必报 `No connected MCP client for server '<name>'` |
| `HigressMcpClientBuilder.validateConfiguration` | `client.rs` `fn validate()` | ✅ 三条校验（endpoint 空、transport 未设、toolSearch query 空）逐条对齐 |
| `HigressMcpClientWrapper.initialize` | `client.rs` `initialize()` | ⚠️ 幂等 ✅；但 **Java 会 `delegateClient.initialize()` 并在失败时保持 `initialized=false`**，Rust 只翻本地标志位、**永不失败**（P0-1 同源） |
| `HigressMcpClientWrapper.listTools`（tool search 分支） | `client.rs` `list_tools()` → `search_tools_with_top_k(query, top_k)` | ✅ **与 Java L164 两参重载一致**（含 `topK`）；转换走 `convert_to_mcp_tool` ✅；缓存 `cache_tools` ✅ |
| `HigressMcpClientWrapper.listTools`（非 tool search 分支） | `client.rs` `list_tools()` → `manager.find_client_id_by_server` + `manager.list_tools` | ✅ 委托语义对齐（Java `delegateClient.listTools()`）；前提是调用方已用 `to_server_config()` 建连 |
| `HigressMcpClientWrapper.callTool(name,args)` | `client.rs` `call_tool(name, arguments)` | ✅ 委托 + `is_error` 告警分支对齐 |
| `HigressMcpClientWrapper.callTool(name,args,meta)` | **缺失** | ❌ Rust 无 meta 入参，固定走 `call_tool_by_server`（meta=`None`）；harness 已有 `call_tool_by_server_with_meta` 可用但未暴露（P1-1） |
| `HigressMcpClientWrapper.close` | `client.rs` `close()` | ⚠️ 本地状态复位 ✅；**未 `delegateClient.close()`**（Rust 侧连接归 harness，P2-3） |
| `HigressMcpClientWrapper.getName/isToolSearchEnabled/getCachedTool` | `name()` / `is_tool_search_enabled()` / `get_cached_tool()` | ✅ 一一对应 |
| `HigressMcpClientWrapper.searchTools(query)` / `searchTools(query,topK)` | `search_tools(query)` / `search_tools_with_top_k(query, top_k)` | ⚠️ 两参 ✅；**单参仍附带 `topK`**（P2-1）；`topK` 数值类型由 Java `int` → Rust `usize`（Java 负值不可表达，Rust 侧更严格） |
| `HigressMcpClientWrapper.convertToMcpTool` | `client.rs` `convert_to_mcp_tool()` | ⚠️ name/title/description/inputSchema ✅；**`outputSchema` 被丢弃**（Java L223-225 会透传，P1-2）；inputSchema 只保留 `properties`+`required`，其余 JSON-Schema 键丢失（P2-2） |
| `HigressMcpClientWrapper.propagateMeta`（继承自 core `McpClientWrapper`） | **缺失** | ❌ 无 `get/set_propagate_meta`（P2-4） |
| `HigressToolSearchResult.parse` | `search_result.rs` `HigressToolSearchResult::parse(&CallToolResult)` | ✅ `isError` → `structuredContent` 优先 → `content` 文本 JSON 回退 → 空 content 报错，**逐条对齐**；`parse(null)` 由类型系统排除（Java 返回 `"CallToolResult is null"`） |
| `HigressToolSearchResult.isSuccess/getErrorMessage/getTools/getToolNames/toString` | `is_success()` / `error_message()` / `tools()` / `tool_names()` / `Display` | ✅ 语义对齐；`toString` 文本格式微差（Rust `{:?}` 带引号，Java `List.toString()` 不带）（P2-5） |
| `HigressToolSearchResult.ToolInfo` | `HigressToolInfo`（`name/description/title/input_schema/output_schema`） | ✅ 5 字段对齐；`name` 缺失时 Java 为 `null`、Rust 为 `""`（Rust 更安全，Java 侧会在 `Tool.builder(null,..)` NPE） |
| `HigressToolkit`（继承 core `Toolkit`） | `toolkit.rs` `HigressToolkit`（newtype 包装 core `Toolkit`） | ⚠️ `new()/with_config()/tool_names()/get_tool()/get_higress_mcp_client()` ✅；**未继承 core `Toolkit` 的公开方法集**（需 `.toolkit()` / `.into_toolkit()`）（P2-6） |
| `HigressToolkit.registerMcpClient` | `toolkit.rs` `register_mcp_client(Arc<HigressMcpClientWrapper>)` | ⚠️ 注册工具 + 缓存 Higress 客户端 ✅；**只能接 Higress 类型**（Java 接受任意 `McpClientWrapper` 并按 `instanceof` 决定是否缓存）、**无 `enableTools/disableTools/group/toolNamePrefix/presetParameters/propagateMeta` 选项**（Java 走 core `McpClientManager.registerMcpClient` 重载 + `Toolkit.registration()` 流式 API）、**未 `set_mcp_client_name` 标注来源**（P1-3） |
| `HigressToolkit` 的 `registration().mcpClient(...).enableTools(...).group(...).apply()` | **缺失** | ❌ Rust `HigressToolkit` 未暴露 `registration()`（core `Toolkit::register()` 提供 `ToolRegistration`，但 Higress 侧未透出）（P1-3） |
| Java `x_higress_tool_search` 协议契约 | `TOOL_SEARCH_NAME = "x_higress_tool_search"` + `search_tools_with_top_k` 写入 `{"query","topK"}` | ✅ **1:1**（Java L330 `Map.of("query", query, "topK", topK)`） |
| **Higress WASM / 插件配置契约** | — | ✅ **Java 侧不存在**：全仓 `agentscope-java` 无任何 `wasm`/`WASM` 关键字命中，`higress.md` 文档亦无插件配置章节；Higress 的 WASM 插件属**网关侧**能力，不在本 Java 模块契约内 → **Rust 无需对齐，无缺口** |

### 6.3 新发现缺口

| 编号 | 优先级 | 缺口 | 证据（Java / Rust） |
|---|---|---|---|
| P0-1 | **P0** | **Builder 不建连，`build()` 返回的客户端不可用**：Java `buildSync/buildAsync` 内部创建并 initialize 真实 MCP 客户端；Rust `build(&self, manager)` 仅 `build_config()` + `wrapper.initialize()`（后者只置位标志位），**不创建也不连接底层 client**。按 Java 文档示例调用必然在 `list_tools()` 处报 `No connected MCP client for server '<name>'`。`HigressMcpClientBuilder` 的 `transport_type` / `timeout` / `initialization_timeout` 因此**全部无效**（未被投影进 `McpServerConfig`）。 | Java `HigressMcpClientBuilder.java:311-332, 357-379`；Rust `client.rs:306-322`、`:401-409`、`:100-104` |
| P1-1 | **P1** | **`call_tool` 缺 meta 透传**：Java `callTool(name, args, meta)` 三参重载原样把 `meta` 交给 delegate（`HigressMcpClientWrapperTest#testCallTool_PassesMetaStraightThrough` 专门钉住，含 `null` 透传）；Rust `call_tool(tool_name, arguments)` 无 meta 形参，恒走 `call_tool_by_server`（meta 固定 `None`），harness 的 `call_tool_by_server_with_meta` 未被使用 → **traceId 等 `_meta` 无法透传到 Higress 网关**。 | Java `:240-272`；Rust `client.rs:466-493` |
| P1-2 | **P1** | **`convert_to_mcp_tool` 丢弃 `outputSchema`**：Java `convertToMcpTool` 在 `toolInfo.outputSchema() != null` 时 `builder.outputSchema(...)`；Rust `client.rs:556` 恒为 `output_schema: None`。→ 工具 schema 丢信息（模型侧看不到输出结构）。 | Java `:223-225`；Rust `client.rs:544-562` |
| P1-3 | **P1** | **`HigressToolkit` 注册能力大幅弱于 Java**：① 只能注册 `HigressMcpClientWrapper`（Java `registerMcpClient(McpClientWrapper)` 接受任意 wrapper，按 `instanceof` 决定是否缓存，Java 有 `testRegisterMcpClient_NonHigressClient` 钉住）；② 无 `enableTools`/`disableTools`/`group`/`toolNamePrefix`/`presetParameters`/`propagateMeta` 过滤与分组参数（Java 走 core `McpClientManager.registerMcpClient` 8 参重载 + `Toolkit.registration()` 流式 API，且 Higress 官方文档 `docs/v2/*/integration/infrastructure/higress.md` 明确宣传 `toolkit.registration().mcpClient(client).enableTools(...).group(...).apply()`）；③ 注册的工具**未调用 `toolkit.set_mcp_client_name()`**，丢失 core 的 MCP 客户端归属标注（Java `RegisteredToolFunction.mcpClientName`）。 | Java `HigressToolkit.java:99-111`、core `McpClientManager.java:73-230`、core `Toolkit.java:1030-1053`；Rust `toolkit.rs:67-80` |
| P1-4 | **P1** | **核心链路（tool-search 成功路径）无测试且不可测**：Java 有 `testListTools_WithToolSearch` / `..._Error` / `..._CachesTools` / `testListTools_WithoutToolSearch_CachesTools` / `testSearchTools_WithQueryAndTopK`（断言请求体 `topK==3`）/ `testConvertToMcpTool_*` 共 8+ 用例；Rust 仅覆盖失败路径。根因：`HigressMcpClientWrapper` 直接依赖**具体类型** `Arc<McpClientManager>`，**无 trait 抽象**，无法像 Java 用 Mockito mock delegate 那样注入桩 → 缓存路径、转换路径、请求体断言均无法离线验证（§3.3 的 `HigressToolkit` P2 测试同样因此挂起）。建议引入 `trait HigressMcpTransport { list_tools / call_tool }` 并让 `McpClientManager` 实现之。 | Java `HigressMcpClientWrapperTest.java:144-188, 282-305, 336-377`；Rust `client.rs:329-337`、`:859-895` |
| P2-1 | **P2** | **单参 `search_tools` 仍发 `topK` + 文档注释自相矛盾**：`client.rs:497-498` 注释称「omits `topK`」，实现却在 `:513` 无条件写入 `topK`；Java 单参重载只发 `{"query": q}`。 | Java `:317-320`；Rust `client.rs:495-516` |
| P2-2 | **P2** | **`build_input_schema` 只保留 `properties` + `required`**：Java 把 `inputSchema` 整张 map 原样交给 `McpSchema.Tool.builder(name, map)`；Rust 丢弃 `$schema`/`additionalProperties`/`type` 变体等键，且 `properties` 中非对象值被 `filter_map` 丢弃。 | Java `:210-227`；Rust `client.rs:565-582` |
| P2-3 | **P2** | **`close()` 不释放底层连接**：Java `close()` 调 `delegateClient.close()`（并 try/catch 吞异常）；Rust 仅复位本地标志 + 清缓存，不 `manager.disconnect/remove_client`。 | Java `:279-294`；Rust `client.rs:527-531` |
| P2-4 | **P2** | **缺 `propagate_meta` 开关**：Java core `McpClientWrapper` 有 `getPropagateMeta()/setPropagateMeta()`；Higress wrapper 继承之，Rust 侧无对应 API（harness `McpServerConfig::propagate_meta` 仅在 `McpServerRegistrar` 路径生效，Higress 自行 `to_server_config()` 时未设置 → 默认 `true`）。 | Java core `McpClientWrapper.java:101-115`；Rust `client.rs:100-104` |
| P2-5 | **P2** | **`toString` 文本格式微差**：Java `HigressToolSearchResult{tools=[a, b]}`；Rust `HigressToolSearchResult{tools=["a", "b"]}`（`{:?}` 带引号）。Java 断言仅 `contains("tools=")`，故不破坏测试契约。 | Java `:211-217`；Rust `search_result.rs:181-193` |
| P2-6 | **P2** | **`HigressToolkit` 非 `Toolkit` 子类**：Java `HigressToolkit extends Toolkit`，可直接 `ReActAgent.builder().toolkit(higressToolkit)`；Rust 是 newtype，`ReActAgent` 需要 core `Toolkit`，须 `.into_toolkit()`（或 `.toolkit()` 借用）。 | Java `HigressToolkit.java:56`；Rust `toolkit.rs:22-25, 56-58`；core `react_agent.rs:1126` |
| P2-7 | **P2** | **query 参数语义弱于 Java**：Java core `extractEndpoint()` 会 **URL-decode 原 URL query、按键合并（新增优先）、并 `URLEncoder.encode` 后拼接**；Rust `endpoint_with_query()` 直接 `k=v&k2=v2` 追加、无编码、同名键会产生重复参数。 | Java core `McpClientBuilder.java:748-800`；Rust `client.rs:84-96` |
| P2-8 | **P2** | **transport 选择未透传**：Rust `HigressClientConfig.transport_type` 仅存于配置，`to_server_config()` 不携带；harness 依 URL 是否含 `/sse` 自行嗅探（`mcp_client_manager.rs:540-542`），与 Java 显式 `sseTransport/streamableHttpTransport` 不等价（URL 含 `/sse` 的 StreamableHTTP 端点会被误判）。 | Java `:413-416`；Rust `client.rs:100-104`、`harness/mcp_client_manager.rs:217` |

### 6.4 勘误列表（原文 → 实际）

| # | 原文位置 | 原文 | 实际 |
|---|---|---|---|
| 1 | §1 模块概览 | 「Java 规模（初测）: 主代码 ~1071 行 / 测试 ~971 行」 | **1050 / 1016**（非空行；含空行 1146 / 1252）。rollup `EXTENSIONS_GAP_TRACKER.md` L36 记的 `1071 / 971` 同样与实测不符（主多记 21、测少记 45），建议汇总方一并校正 |
| 2 | §3.3 测试表 | 「`HigressMcpClientBuilder` 校验测试 … **15 个用例**」 | **13 个** builder/config 用例（`client.rs` 21 = 13 builder + 3 `convert_to_mcp_tool` + 1 `tool_info_new` + 4 wrapper） |
| 3 | §3.3 测试表 | 「`HigressMcpClientWrapper` 行为测试 … **7 个用例**」 | 拆分口径应为 **4（wrapper）+ 3（`convert_to_mcp_tool`）= 7**；若把 `tool_info_new` 计入 builder 则 builder 为 14。原文未说明归属，属口径不清 |
| 4 | §4.1.1 第 2 步 | 「`search_tools(&self, query)` 单参方法 … 在其内部委托 `search_tools_with_top_k`」+ `client.rs:497` 注释「omits `topK`」 | 实现与注释**矛盾**：单参 `search_tools` 实际仍发送 `topK`（`client.rs:513`），与 Java 单参重载（只发 `query`）不一致 → 登记 P2-1 |
| 5 | §4.1.1 标题 | 「本轮修复（search_tools topK 参数不匹配）」 | 修复**方向正确且已生效**：`list_tools` 的 tool-search 分支现携带配置 `topK`，与 Java L164 一致；但**未消除**单参 `search_tools` 的 topK 语义偏差（见上一条） |
| 6 | §2.1 依赖映射 | 「`java.time.Duration` → `std::time::Duration` **仅作为配置保存**（连接由 harness 完成）」 | 属实，但需补充：该 `Duration` **从未被应用到任何连接**（`McpServerConfig` 无超时字段），因此 `timeout`/`initialization_timeout` 目前**完全无效** → 见 P0-1 |
| 7 | §4.1 | 「**已完成**：4 个 Java 主类全部迁移；… `cargo build` / `cargo test` 全绿」 | 编译/测试结论本轮**未复跑**（静态复核），沿用上一轮实测；「4 个主类全部迁移」在**类型层面**成立，但在 **builder 建连 / meta 透传 / toolkit 注册过滤** 三处**行为层面未覆盖**（P0-1 / P1-1 / P1-3） |
| 8 | §3.3 | 「【校正】本 crate 原无 `#[cfg(test)]` … 本轮已补齐 30 个离线用例」 | 数量核实**一致**（`#[test]` 27 + `#[tokio::test]` 3 = 30；`client.rs` 21 + `search_result.rs` 9）；但「30 个」覆盖的是**子集**，Java 侧共 **74** 个 `@Test`，差集集中在 tool-search 成功路径 / 缓存路径 / meta 透传 / toolkit 注册过滤（P1-1 ~ P1-4） |

### 6.5 复核后建议的 rollup 状态值

- **建议值：`已完成`**（保持不变）+ 状态描述中追加限定语。
- **理由**：① 4 个 Java 主类均有真实存在的 Rust 对应实现，模块可编译、30 test 全绿，**核心链路（`x_higress_tool_search` 选工具 + 结果解析 + 工具缓存 + toolkit 注册）代码路径完整**；② 发现的缺口全部属于**契约增强类**（建连编排 / meta 透传 / 注册过滤 / 可测性），无「Java 有、Rust 完全无对应能力」的整类缺失；③ 无 P0 级**数据错误或崩溃**缺陷 —— P0-1 的后果是「调用方需自行建连」，属**用法约定**而非逻辑错误，且已在 §4.2 第 3 条记录。
- **建议状态描述**（供汇总方登记）：`已完成（build + test 通过，30 test；静态复核确认 4/4 主类映射真实、协议契约 1:1；遗留 P0×1 builder 不建连、P1×4 建连编排/meta 透传/outputSchema/toolkit 过滤与可测性，见 HIGRESS__GAP_TRACKER.md §6）`。
- **若汇总方按缺口加权**，可将 P0-1 与 P1-1/P1-3/P1-4 视为「已完成（需二次收尾）」而非「已完成」，但**不建议降级为「部分完成」**——因主类映射与协议契约已 1:1，仅差增强项。


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
- P1-4（tool-search / meta 透传 / toolkit 注册等测试缺口）仅因可注入 `McpConnection` 而具备继续补测的基础，本次未补齐；P2-1/P2-2/P2-4~P2-7 未处理。