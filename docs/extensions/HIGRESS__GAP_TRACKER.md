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
| Java 规模（静态复核实测） | 主代码 **1050 行** / 测试 **1016 行**（非空行；含空行分别 1146 / 1252）；rollup 记的 1071 / 971 与实测不符，见 history/HIGRESS__MIGRATION_HISTORY.md 勘误 |
| Rust crate | `crates/agentscope-extensions-higress` |
| 迁移波次 | C |
| 当前状态 | **已完成**（P1/P2 清零；2026-10-31 实测 60 passed / 0 failed，另 1 doc-test）—— P1-1 ~ P1-4 已关闭；P2-4/P2-5/P2-6/P2-7 已落地，P2-2 定性为「设计决定（不迁移）」，P2-8 转跨模块需求。见 history/HIGRESS__MIGRATION_HISTORY.md |

> 2026-10-31 全量对齐复核：逐类+逐文件核对结论见 §3.5（7 项缺口/待裁定，编号 HIGRESS-01..07）与 §4.3（D01..D04 / X01..X10）；存量 §3.1–§3.4 未改动。当前状态不变（无一票否决级 P0）。

## 2. Java 清单与依赖映射
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `HigressMcpClientBuilder` | 流式构建 Higress MCP 客户端（SSE / StreamableHTTP 两种 transport、headers、queryParams、timeout、toolSearch）| `HigressMcpClientBuilder`（src/client.rs）| 构建 `HigressClientConfig` + `HigressMcpClientWrapper`；`build`/`build_async` 对齐 `buildSync`/`buildAsync`；**`build(&self, manager)` 已真建连**：内部 `wrapper.initialize()` 经 `HigressMcpTransport::connect_server`（生产实现 = harness `McpClientManager::create_client` + `connect`）创建并 connect 底层 client，受 `initialization_timeout` 约束，失败原样返回且保持 `initialized=false`（既有测试 `initialize_connects_and_records_full_config` / `initialize_propagates_connection_failure` / `initialize_applies_initialization_timeout`）—— 见 history/HIGRESS__MIGRATION_HISTORY.md 原 P0-1 **已于 2026-10-28 修复** |
| `HigressMcpClientWrapper` | Higress MCP 客户端包装：委托底层 MCP 客户端；`x_higress_tool_search` 语义化选工具；工具缓存 | `HigressMcpClientWrapper`（src/client.rs）| 经窄 trait `HigressMcpTransport`（`connect_server`/`find_client_id_by_server`/`list_tools`/`call_tool_by_server`/`disconnect_by_server`）委托，生产实现为 harness `McpClientManager`，非自建 MCP 连接；`call_tool(name,args)`（等价 Java 传 `null` meta）与 **`call_tool_with_meta(name,args,meta)`（meta 原样透传）** 并存 —— 见 history/HIGRESS__MIGRATION_HISTORY.md P1-1 已完成 |
| `HigressToolSearchResult`（含 `ToolInfo`）| 解析 `x_higress_tool_search` 结果：优先 `structuredContent`，回退 `content` 文本 JSON；`ToolInfo{name,description,title,inputSchema,outputSchema}` | `HigressToolSearchResult` + `HigressToolInfo`（src/search_result.rs）| 解析 `rust_mcp_sdk::schema::CallToolResult` |
| `HigressToolkit` | 继承 core `Toolkit`，注册 Higress MCP 客户端并缓存 | `HigressToolkit`（src/toolkit.rs）| newtype 包装 core `agentscope_core::tool::Toolkit`；`register_mcp_client(Arc<dyn McpToolSource>)` 接受**任意** MCP 客户端、仅 Higress 实例入缓存（等价 `instanceof`）；`registration()` 提供 `enable_tools`/`disable_tools`/`group`/`tool_name_prefix`/`preset_parameters`/`propagate_meta`，注册时经 core `ToolRegistration::mcp_client` 落 `set_mcp_client_name` —— 见 history/HIGRESS__MIGRATION_HISTORY.md P1-3 已完成 |

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
| `HigressMcpClientWrapper`（委托 + tool search）| `HigressMcpClientWrapper` | 已完成 | 委托缝 = `HigressMcpTransport`（harness `McpClientManager` 实现）；`list_tools`/`call_tool`/`call_tool_with_meta`/`search_tools`/`close` —— P1-1/P1-2/P1-4 已关闭（见 history/HIGRESS__MIGRATION_HISTORY.md） |
| `HigressToolkit`（注册 + 缓存）| `HigressToolkit` | 已完成 | 复用 core `Toolkit` + `ToolRegistration`；`registration()` 全选项 —— P1-3 已关闭（见 history/HIGRESS__MIGRATION_HISTORY.md） |

### 3.2 依赖与拆分说明
- **`rust-mcp-sdk`**：本 crate 直接依赖 `rust-mcp-sdk`（版本与 harness 一致，走 crates.io 远程仓库）以使用 `rust_mcp_sdk::schema` 的 MCP 类型。**未修改根 `Cargo.toml` 的 `[workspace.dependencies]`**，版本在本 crate `Cargo.toml` 内声明。若汇总方希望统一为 workspace 依赖，见 §4.2。
- **未重建平行类型**：MCP 客户端生命周期复用 harness `McpClientManager`；工具治理复用 core `Toolkit`/`Tool`/`McpTool`。

### 3.3 测试
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| `HigressToolSearchResult` 解析测试 | `HigressToolSearchResultTest` | 已完成 | 9 个用例（`search_result.rs`）：structuredContent 优先、content 回退、空 content、`isError`、非法 JSON、缺 name 默认空串、null schema 丢弃、Display |
| `HigressMcpClientBuilder` 校验测试 | `HigressMcpClientBuilderTest` | 已完成 | **实测 13 个** builder/config 用例（`client.rs`）：clientName 空白拒绝、缺 endpoint、两种 transport、header/queryParam 排序与合并、`toolSearch` 空 query 拒绝、`topK<=0` 拒绝、`topK` 保留、`endpoint_with_query`、`to_server_config` 投影、超时 |
| `HigressMcpClientWrapper` 行为测试 | `HigressMcpClientWrapperTest` | 已完成 | **实测 38 个**（`client.rs`）：builder/config 21 个 + wrapper 行为 17 个。wrapper 覆盖 `with_options` 状态、`initialize` 幂等/超时/传输不符/失败传播、`close` 重置、未连接时 `list_tools`/`search_tools` 报错，以及 **P1-4 新增**：tool-search 成功路径（含请求体 `query`+`topK` 断言）、缓存命中（第二次不再调用网关）、无 tool-search 时全量缓存、tool-search 失败上抛、text-content 回退、`call_tool_with_meta` 透传与 `None` 透传、单参 `search_tools` 不含 `topK`、`convert_to_mcp_tool` 三类 input schema + output schema（含非 object 降级） |
| `HigressToolkit` 测试 | `HigressToolkitTest` | 已完成 | **实测 10 个**（`toolkit.rs`）：全量注册 + `initialize` 先行 + `mcp_client_name` 标记、非 Higress 客户端不缓存、Higress 客户端入库、`enableTools`/`disableTools` 优先级、前缀/分组/preset（含 schema 隐藏 preset 键）、meta 透传、`propagate_meta(false)`、per-tool override（含未知工具 fail-loud）、缺 client 报错 |
| 【校正】本 crate 原无 `#[cfg(test)]` | — | 已修复 | 已补齐离线用例；2026-10-28 实测 `#[test]` 30 + `#[tokio::test]` 27 = **57**，另有 1 个 doc test（`registration()` 示例，`no_run` 编译通过）；无 `#[ignore]`、无 `#[should_panic]` |

## 3.5 2026-10-31 全量对齐复核清单（唯一编号）

> 复核基线：Java `main` @ `1524b3191`（`agentscope-extensions-higress`，主代码 4 类 / 1050 行）；Rust `crates/agentscope-extensions-higress`（4 个 .rs / ≈3.1k 行，其中测试占大半）。本表为本轮逐文件核对结论，**存量 §3.1–§3.4 历史清单保持原样未重新编号**。
> 编号规则：`HIGRESS-NN`（状态取值 `待办` / `进行中` / `已完成` / `设计决定（不迁移）` / `待裁定`）；§4.3 差异另有 `HIGRESS-DNN` / `HIGRESS-XNN` 系列。编号本模块内唯一、单调递增，不复用历史编号。
> **2026-10-31 裁定（汇总方已确认）**：本文件的 `待裁定` 项已全部裁定，状态列更新为 `待办` / `设计决定（不迁移）` / `已完成`；裁定结论、依据与执行批次见 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` §3.3。
> 正文其它位置出现的旧「待裁定 N 项」为复核时点快照，一律以 §3.5/§3.6 表格的状态列为准。
> 测试数口径：沿用既有实测 **60 passed / 0 failed + 1 doc-test**（`client.rs` 41 / `search_result.rs` 9 / `toolkit.rs` 10，本轮按属性计数复核一致）；本轮未运行 cargo。

### 3.5.0 四个 Java 类逐一映射（无漏项）

| Java 类 | Rust 对应 | 结论 |
|---|---|---|
| `HigressMcpClientBuilder`（SSE/StreamableHTTP、headers、queryParams、timeout、initializationTimeout、toolSearch）| `client.rs#HigressMcpClientBuilder` + `HigressClientConfig`（`client.rs:57`）+ `TransportType`（`client.rs:38`）| **已迁移**；校验顺序/默认值/`topK<=0` 拒绝/endpoint 必填全部对齐。差异见 HIGRESS-06/07 与 X01/X04 |
| `HigressMcpClientWrapper`（委托 + `x_higress_tool_search` + 工具缓存 + close）| `client.rs#HigressMcpClientWrapper`（`client.rs:503`）+ `HigressMcpTransport`（`client.rs:184`）| **已迁移**；`list_tools`/`call_tool`/`call_tool_with_meta`/`search_tools`/`search_tools_with_top_k`/`get_cached_tool`/`close` 齐全。缺口见 HIGRESS-02/04 |
| `HigressToolSearchResult`（含 `ToolInfo`）| `search_result.rs#HigressToolSearchResult`（`parse` @ `search_result.rs:76`）+ `HigressToolInfo` | **已迁移**；structuredContent 优先 → text 回退 → `errorMessage`，Display 文案一致 |
| `HigressToolkit`（`registerMcpClient` 覆写 + `getHigressMcpClient`）| `toolkit.rs#HigressToolkit`（`toolkit.rs:105`）+ `McpToolSource`（`toolkit.rs:38`）+ `McpClientRegistration` | **已迁移并扩展**；仅 Higress 客户端入缓存的 `instanceof` 语义由 `as_higress` 等价实现。核心链路另有 HIGRESS-01/03/05 |

### 3.5.1 缺口与待裁定

| 编号 | 优先级 | 缺口项（可执行描述） | Java 参考（文件#符号） | Rust 现状（文件:行 / rg 零命中） | 执行状态 | 下一动作 |
|---|---|---|---|---|---|---|
| HIGRESS-01 | P1 | 模型可见参数 schema 丢失**根级** JSON Schema 关键字：`$defs`/`definitions`/`additionalProperties`/`$schema`/`description` 全被裁掉，布尔子 schema（`{"p": true}`）属性直接消失。Java 先 `parameters.putAll(inputSchema)` 再只删除 preset 键，故全部关键字原样到达模型 | `HigressMcpClientWrapper.java#convertToMcpTool`；core `McpTool.java#convertMcpSchemaToParameters`（`putAll` 保留全量）；调用点 `McpClientManager.java:425` | `client.rs:1033` `build_input_schema`（只取 `properties`/`required`，`filter_map` 丢非对象属性）；`client.rs:936` `convert_to_mcp_tool`；`toolkit.rs:481` `tool_parameters`（重建 `{type,properties,required}`）；`client.rs:1094` 同源路径 | 待办 | `HigressToolInfo` 保留原始 schema `Value`（或在 `convert_to_mcp_tool` 旁路），`tool_parameters` 改为「原 schema 全量拷贝 → 删 preset 键 → 补齐 type/properties/required」；core `McpTool::new` 收 `Value`，无需改 MCP 类型。验收：新增 2 测试（根级 `additionalProperties:false` + `$defs`；布尔子 schema），断言 `tool.parameters_schema()` 保留这些键 |
| HIGRESS-02 | P1 | **连接级** `propagateMeta` 开关在调用路径上不生效：`set_propagate_meta` 只写 `AtomicBool`，`initialize` 已初始化即早退（不再重建 `server_config`），`call_tool_with_meta` 把 `meta` 原样透传；core `McpTool` 只 AND 了 per-tool 开关，harness 调用路径也不读 `config.propagate_meta` | `McpTool.java:281`（`propagateMeta && clientWrapper.isPropagateMeta()`，每次调用现读 volatile）；`McpClientWrapper.java:101/115` | `client.rs:623`（仅 store）；`client.rs:639`/`647`（早退）；`client.rs:680`（唯一投影点）；`client.rs:831`（原样透传）；harness `mcp_client_manager.rs:398-430` 不读 config，`rg 'propagate_meta'` 仅 63/74/98/485/495 命中且均非调用路径；core `tool/mcp.rs:193` 只判 per-tool | 待办 | `call_tool_with_meta`（含 `search_tools*` 派生路径）在 `!self.propagate_meta()` 时把 `meta` 置 `None`；或 wrapper 记录 `client_id` 后调 harness `set_propagate_meta`。验收：`set_propagate_meta(false)` 后携带 `Some(meta)` 调用 → Recorder 收到 `None`（新测试） |
| HIGRESS-03 | P1 | 工具结果非文本内容丢失：`render_call_result` 只保留 `TextContent`，`ImageContent`/`AudioContent`/`EmbeddedResource` 被 `{:?}` Debug 成字符串；Java 走 `McpContentConverter` 产出 `ImageBlock`/`AudioBlock`/内联资源文本 | `McpTool.java:286` → `McpContentConverter.java:60/110`（Text/Image/Audio/EmbeddedResource 四分支） | `toolkit.rs:509` `render_call_result`；跨仓复核：`rg 'ImageContent\|AudioContent\|EmbeddedResource' crates/agentscope-harness/src` 零命中，`rg 'fn .*call_tool_result' crates/agentscope-core/src` 零命中（无等价转换器） | 待办（跨模块） | core 增 `convert_call_tool_result`（对齐 `McpContentConverter`），Higress 改调。验收：图片结果 → `ContentBlock::Image`，文本资源 → `TextBlock`，音频 → `ContentBlock::Audio` |
| HIGRESS-04 | P2 | 同名连接无条件复用、不校验 endpoint/headers：`build()` 用新 endpoint 时静默绑定旧连接（配置变更被吞） | `HigressMcpClientBuilder.java#buildSync/#buildAsync`（每次新建 delegate 并 initialize） | `client.rs:660-673`（`find_client_id_by_server(&self.name)` 命中即 `initialized=true` 返回，`client.rs:666`） | 待办 | 复用前比对 `server_config.url`/`headers`；不一致则报错或另建连接（可加 `reuse_existing: bool` 开关）。验收：同名不同 endpoint 的第二个 wrapper `initialize` 返回错误 |
| HIGRESS-05 | P2 | 非 tool-search 路径丢失 display title 与 outputSchema（tool-search 路径有）：注册出的工具 `title=None`，确认弹窗只能用工具名 | `HigressMcpClientWrapper.java#convertToMcpTool`（title/description/outputSchema）；core `McpTool.java#resolveDisplayTitle`（`title` 或 `annotations.title`） | `client.rs:1094` `mcp_tool_info_to_tool`（`title: None, output_schema: None`）；根因 harness `McpToolInfo`（`mcp_client_manager.rs:38-47`）仅有 name/description/parameters/server | 待办（跨模块） | harness `McpToolInfo` 增 `title`/`output_schema` 后透传。验收：自定义 transport 返回带 title 的 `McpToolInfo` → 注册后 `Tool::title()` 非空 |
| HIGRESS-06 | P2 | 传输方式无法显式指定：harness 靠 URL 含 `/sse` 嗅探，SSE 端点不带 `/sse` 时不可用；现仅以 `validate_transport` fail-loud 兜底 | `HigressMcpClientBuilder.java#createMcpClientBuilder`（`sseTransport` / `streamableHttpTransport` 显式分支） | `client.rs:719` `validate_transport` 只报冲突不纠偏；harness `mcp_client_manager.rs:533` `is_sse_url` | 待办 | 与 §4.2 第 5 条（原 P2-8）同源；待汇总方决定是否给 `McpServerConfig` 增 `transport` 字段，在此之前保持 fail-loud 【裁定 2026-10-31：A：McpServerConfig 加 transport: Option<McpTransport>（None=保持嗅探）】 |
| HIGRESS-07 | P2 | MCP 客户端抽象归属未定：core 无 `McpClientWrapper` 等价基类，本 crate 自建 `McpToolSource`（仅服务本模块），其它 C 波次模块各自另建 | Java core `McpClientWrapper`（abstract：`listTools`/`callTool` 双参+三参/`getCachedTool`/`close`） | `toolkit.rs:38` `pub trait McpToolSource`；`rg 'McpClientWrapper' crates/agentscope-core` 零命中（命中仅在 `crates/agentscope-extensions-higress/*` 与 harness 注释） | 待办 | 与 §4.2 第 2 条同源；决定后上提 core/harness 并收敛 【裁定 2026-10-31：A：McpToolSource trait 上提 harness，McpClientManager 提供 adapter】 |

### 3.5.2 本轮复核确认已对齐（抽样，不求穷举）

| 编号 | 能力 | Java 参考 | Rust 证据（文件:行 + 测试名） | 执行状态 |
|---|---|---|---|---|
| HIGRESS-08 | 工具检索参数：`topK` 默认 10、单参 `searchTools(query)` **不发** `topK`、两参发 `topK`、`topK<=0` 拒绝、`listTools` 走两参 | `HigressMcpClientWrapper.java#searchTools(String)` / `#searchTools(String,int)` / `#listTools`；`HigressMcpClientBuilder.java#toolSearch(String,int)` | `client.rs:32`（默认 10）、`client.rs:869`（只发 query）、`client.rs:881-889`（发 topK）、`client.rs:450` 校验；测试 `search_tools_single_argument_omits_top_k`、`tool_search_lists_converted_tools_and_sends_query_with_top_k`、`tool_search_with_top_k_rejects_zero` | 已完成 |
| HIGRESS-09 | 超时默认值（120s 请求 / 30s 初始化）与每请求超时施加 | `HigressMcpClientBuilder.java`（`DEFAULT_TIMEOUT`/`DEFAULT_INIT_TIMEOUT`、`mcpClientBuilder.timeout`） | `client.rs:28`/`30`；`client.rs:742` `with_request_timeout`；`client.rs:688` 初始化超时；测试 `timeout_setters_are_respected`、`initialize_applies_initialization_timeout` | 已完成 |
| HIGRESS-10 | 鉴权/自定义头与 query 参数落到真实连接（`McpServerConfig.headers` → harness `custom_headers`） | `HigressMcpClientBuilder.java#header/#headers/#queryParam/#queryParams` → `McpClientBuilder.headers/queryParams` | `client.rs:149` `to_server_config`；harness `mcp_client_manager.rs:179-237`（`custom_headers` 进 SSE/StreamableHTTP 选项）；测试 `to_server_config_projects_name_url_and_headers` | 已完成 |
| HIGRESS-11 | endpoint 自带 query 与配置 queryParams 合并、重复键后者胜、`URLEncoder`/`URLDecoder` 语义（空格 `+`、`%XX`） | core `McpClientBuilder#extractEndpoint`（`URLDecoder`/`URLEncoder`，UTF_8） | `client.rs:96` `endpoint_with_query`、`client.rs:965` `decode_query_component`、`client.rs:1008` `encode_query_component`；测试 `endpoint_with_query_merges_duplicate_keys_and_encodes_like_java` | 已完成 |
| HIGRESS-12 | 搜索结果解析：structuredContent 优先 → text JSON 回退 → `isError`/空 content/坏 JSON 降级为 `errorMessage`，Display 文案一致 | `HigressToolSearchResult.java#parse/#parseFromStructuredContent/#parseFromContent/#toString` | `search_result.rs:76`/`125`/`145` + `Display`；9 个测试（`search_result.rs` 全绿） | 已完成 |
| HIGRESS-13 | 工具缓存与关闭清理（`cachedTools` + `close` 复位） | `HigressMcpClientWrapper.java#listTools/#close`；base `McpClientWrapper#getCachedTool` | `client.rs:925` `cache_tools`、`client.rs:896` `get_cached_tool`、`client.rs:904` `close`；测试 `tool_search_results_are_cached_and_served_without_a_second_call`、`list_tools_without_tool_search_caches_every_tool`、`close_disconnects_and_resets_state` | 已完成 |
| HIGRESS-14 | 注册链路：`initialize` → `listTools` → `enableTools`/`disableTools` 过滤（enable 优先）→ group/preset → 打 `mcp_client` 标记 | `HigressToolkit.java#registerMcpClient`；core `Toolkit.ToolRegistration#apply` + `McpClientManager#shouldRegisterTool` | `toolkit.rs:456` `should_register_tool`、`toolkit.rs:314` `apply`；测试 `register_mcp_client_registers_every_tool`、`registration_filters_with_enable_and_disable_tools`、`registration_applies_prefix_group_and_presets` | 已完成 |
| HIGRESS-15 | 仅 Higress 客户端入缓存（`instanceof` 等价），且注册成功后才缓存 | `HigressToolkit.java#cacheHigressClient` | `toolkit.rs:73` `as_higress`、`toolkit.rs` `apply` 尾部缓存；测试 `non_higress_client_is_not_cached`、`higress_client_is_cached_after_registration`、`apply_without_client_fails` | 已完成 |
| HIGRESS-16 | meta 三参透传：原样交给网关，`None` → 不发 `_meta`；per-tool `propagateMeta` 覆盖 + 未知工具 fail-loud | `HigressMcpClientWrapper.java#callTool(String,Map,Map)`；core `McpTool#buildMetaMap` + `McpClientManager` override 校验 | `client.rs:831`、`toolkit.rs:441` `resolve_propagate_meta`；测试 `call_tool_with_meta_passes_meta_straight_through`、`call_tool_without_meta_passes_none`、`propagate_meta_false_omits_metadata`、`propagate_meta_per_tool_override_rejects_unknown_tool` | 已完成 |
| HIGRESS-17 | 分页与重试：两侧均无（Java `HigressMcpClientWrapper.listTools` 单页委托，模块内 4 类无 retry/backoff；Rust harness `list_tools` 无游标） | `HigressMcpClientWrapper.java#listTools`；`McpClientWrapper.java:132`（抽象单页） | `rg 'next_cursor\|nextCursor' crates/agentscope-harness/src/agent/mcp_client_manager.rs` 零命中；`rg '(?i)retry\|backoff'` 在 Java higress 主代码与本 crate `src` 均零命中 | 已完成（行为一致，无待办） |

## 4. 项目状态与下一步
### 4.1 当前状态
- **已完成**：4 个 Java 主类全部迁移；**57 个离线单测 + 1 个 doc test**（2026-10-28 实测属性数：`#[test]` 30 + `#[tokio::test]` 27 = 57，分布 `client.rs` 38 / `search_result.rs` 9 / `toolkit.rs` 10）。
- **验证命令与结果（2026-10-28 实测）**：
  - `cargo build -p agentscope-extensions-higress` → 通过，本 crate **0 warning**（原 `text_content` dead-code 已随 P1-4 重构删除）。
  - `cargo test -p agentscope-extensions-higress` → `test result: ok. 57 passed; 0 failed; 0 ignored`（lib）+ `test result: ok. 1 passed; 0 failed`（doc-tests）。
- **已知问题**：无（harness 侧既有 warning 与本 crate 无关）。

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
- **【静态复核补注】**：第 2 步的**文档注释与实现不符**——`client.rs:497-498` 声称「Mirrors the Java single-argument overload `searchTools(String query)`, which omits `topK`」，但实现无条件写入 `topK`（`client.rs:513`）。Java 单参重载只发 `{"query": q}`（`HigressMcpClientWrapper.java:318`）。**结论：编译阻塞缺陷已修复（`list_tools` 路径与 Java L164 一致 ✅），但单参 `search_tools` 的 topK 语义仍与 Java 有偏差**，登记为 history/HIGRESS__MIGRATION_HISTORY.md 的缺口 P2-1 + 勘误第 3 条。
- **改动文件**：`crates/agentscope-extensions-higress/src/client.rs`（仅本 crate）。
- **验证**：`cargo build -p agentscope-extensions-higress` → 通过；`cargo test -p agentscope-extensions-higress` → 通过。
- **【静态复核确认】**：`client.rs:424-426` 现为 `self.search_tools_with_top_k(&query, self.tool_search_top_k)`，`search_tools_with_top_k` 签名为 `(query: &str, top_k: usize)`（`client.rs:506-510`），实参 2 个、形参 2 个 → **原 `E0061` 编译错误确已消除** ✅。
- **【2026-10-28 闭环】**：单参 `search_tools` 已改为只发 `{"query": q}`（不再附带 `topK`），注释与实现一致；`list_tools` 的 tool-search 分支仍走两参重载。P2-1 关闭，见 history/HIGRESS__MIGRATION_HISTORY.md。

### 4.2 下一步与阻塞
1. ~~**汇总方（待裁定项）**：是否将 `rust-mcp-sdk` 提升为根 `[workspace.dependencies]`~~ → **已完成（2026-10-29）**：根 `Cargo.toml` 已新增 `rust-mcp-sdk = { version = "1.0.1", default-features = false }`，harness 与本 crate 均改为 `rust-mcp-sdk = { workspace = true, ... }`，版本不再需要人工对齐。
2. **汇总方（信息共享 / 待裁定项）**：Java `McpClientWrapper` 无 Rust 等价基类，本模块以 `McpClientManager` + server 名委托实现；若其它 C 波次模块（CONTROLPLANE/STUDIO 等）也需 MCP 客户端包装，建议统一抽象后再收敛。
3. 真实端到端联调（连真实 Higress 网关）需 `McpClientManager` 建立连接；本 crate 已把 `HigressClientConfig` 转换成 `McpServerConfig`。**【2026-10-28 已修复】**`build(&self, manager)` 现在会调用 `wrapper.initialize()`，经 `HigressMcpTransport::connect_server` 真实创建并 connect 底层 client（`transport_type`/`timeout`/`initialization_timeout` 全部投影进 `McpConnectRequest`），失败向上返回且 `initialized` 保持 false；对应测试 `initialize_connects_and_records_full_config`（断言 manager 里能查到 client id、transport/timeout 全量透传）、`initialize_propagates_connection_failure`、`initialize_applies_initialization_timeout`、`initialize_is_idempotent_and_only_connects_once`。原「`build()` 不建连」的 P0-1 已关闭。
4. **【2026-10-28 已完成】** 已引入 `trait HigressMcpTransport`（`list_tools` / `call_tool_by_server(…, meta)` 等），由 `McpClientManager` 实现；`HigressMcpClientWrapper::with_transport` 允许注入替身，P1-4 与 `HigressToolkit` 测试因此可离线覆盖（见 history/HIGRESS__MIGRATION_HISTORY.md）。
5. **P2 收口（2026-10-31）**：P2-4 / P2-5 / P2-6 / P2-7 已闭环；P2-2 定性「设计决定（不迁移）」；**P2-8 仍为跨模块需求**（harness `McpServerConfig` 无 `transport` 字段，`mcp_client_manager.rs:533` 的 `is_sse_url` 靠 URL 含 `/sse` 嗅探，本 crate 无法在不改 harness 的前提下显式指定传输），已记入回执「跨模块需求」，待汇总方裁定。其余旧条目（① ② ③ ④ ⑤ ⑥ ⑦）见 history/HIGRESS__MIGRATION_HISTORY.md 逐条状态。另：`rust-mcp-sdk` 已上提 workspace（见第 1 条）；MCP 客户端包装抽象是否跨 C 波次统一仍为第 2 条的**待裁定项**。

### 4.3 与 Java 的已知差异（有意为之，非缺口）

#### 4.3.1 设计决定（不迁移）

| 编号 | 项 | Java 参考 | 不迁移理由（源码级依据） | 影响面 |
|---|---|---|---|---|
| HIGRESS-D01 | core `McpClientWrapper`/`McpClientBuilder` 抽象基类 + reactor `Mono` 响应式返回 | `HigressMcpClientWrapper extends McpClientWrapper`；`buildAsync()` 返回 `Mono<HigressMcpClientWrapper>` | Rust core 无 MCP 客户端基类（`rg 'McpClientWrapper' crates/agentscope-core` 零命中），客户端生命周期归 harness `McpClientManager`；编排层统一为 `async fn -> Result<T>`（MIGRATION_PLAN §4.3）。若重建平行基类会与 harness 形成双实现 | 客户端 API 形状不同（`Result` 而非 `Mono`）；调用方改 `.await`，无行为差异 |
| HIGRESS-D02 | `buildSync()` 阻塞入口 | `HigressMcpClientBuilder.java#buildSync`（内部 `initialize().block()`） | Rust 侧不引入 blocking 运行时：`rg 'block_on' crates/agentscope-extensions-higress` 零命中，`build`/`build_async`（`client.rs:483`/`491`）为同一 async 实现；阻塞由宿主在自有 runtime 上 `block_on` | 宿主若需同步 API，自行包一层；无功能缺口 |
| HIGRESS-D03 | `HigressToolSearchResult.parse(null)` 判空分支（返回 `error(CallToolResult is null)`） | `HigressToolSearchResult.java#parse`（首行 `if (callToolResult == null)`） | Rust 签名 `parse(&CallToolResult)`（`search_result.rs:76`），空值在类型层面不可表示，故该分支消失（非漏迁） | 无：调用方拿不到 `None` 入参 |
| HIGRESS-D04 | `HigressToolkit extends Toolkit` 继承 core Toolkit 全部 API | `HigressToolkit.java`（类继承） | Rust 无继承，采用 newtype：`toolkit: Toolkit` + `toolkit()`/`into_toolkit()` 兜底（`toolkit.rs:105`/`133`/`137`）；未代理的父类方法经 `.toolkit()` 直达，避免手抄整套 Toolkit 表面 | 需经 `.toolkit()` 访问未代理的父类方法；无行为差异 |

#### 4.3.2 设计决定与已知差异

| 编号 | 项 | Rust 实现 | 与 Java 的差异 | 为何保留（收益/依据） |
|---|---|---|---|---|
| HIGRESS-X01 | 请求超时的施加位置 | `with_request_timeout`（`client.rs:742`）包裹 `find`/`list_tools`/`call_tool`/`disconnect`；`McpConnectRequest.timeout`（`client.rs:164`）仅用于日志与诊断 | Java 把 `timeout` 交给底层 SDK 的 HTTP client | 在 wrapper 层统一施压，超时错误文案带 client 名与操作名，便于定位；`McpServerConfig` 无 timeout 字段，无法下沉 |
| HIGRESS-X02 | 初始化幂等与并发安全 | 双检 + `tokio::sync::Mutex` `init_guard`（`client.rs:638-650`） | Java 只判 `isInitialized()`，并发 `initialize()` 可重复建连 | 避免并发调用各建一个 client（`initialize_is_idempotent_and_only_connects_once`） |
| HIGRESS-X03 | 同名连接复用与关闭策略 | 已连接则复用（`client.rs:660-673`）；`close` 仅在 `is_initialized()` 时 disconnect（`client.rs:906`） | Java 每个 wrapper 独占 delegate，`close` 无条件关闭并吞异常 | 共享 `McpClientManager` 场景下避免重复建连/误断他人连接（风险见 HIGRESS-04） |
| HIGRESS-X04 | `propagate_meta` 暴露层级 | builder（`client.rs:412`）、config（`client.rs:82`）、wrapper（`client.rs:613`/`623`）三级暴露 | Java `HigressMcpClientBuilder` 未暴露（继承 core `McpClientBuilder.propagateMeta`） | 本 crate 不重建 core builder，需显式入口才能把该开关投影进 `McpServerConfig`（生效性见 HIGRESS-02） |
| HIGRESS-X05 | 过滤/前缀/preset/meta 的落点 | `should_register_tool`（`toolkit.rs:456`）、`tool_name_prefix`、`preset_parameters`、`resolve_propagate_meta`（`toolkit.rs:441`）在 crate 内实现 | Java 由 core `Toolkit.ToolRegistration`/`McpClientManager` 提供 | Rust core `ToolRegistration` 无 `enableTools`/`toolNamePrefix`；后续宜上提 core（属跨模块改进项，非本模块缺陷） |
| HIGRESS-X06 | 测试/扩展用窄 port | `McpToolSource`（`toolkit.rs:38`）+ `HigressMcpTransport`（`client.rs:184`），生产实现分别为 `HigressMcpClientWrapper` 与 `McpClientManager` | Java 用基类 + Mockito mock | 只覆盖实际用到的 5 个操作，离线可测（本轮实测 60 用例），不复制 MCP 抽象 |
| HIGRESS-X07 | 错误类型与文案 | 校验失败一律 `Error::Other`（`client.rs:297` 等）；文案 `clientName must not be null or empty` | Java 区分 `IllegalArgumentException`（endpoint/query/clientName）与 `IllegalStateException`（transport），文案 `Client name cannot be null or empty` | core `Error` 无 `InvalidArgument`/`InvalidState` 变体（`core/src/error.rs:8-101`），`Error::Other` 是唯一贴合项；若需分类应由 core 补变体（跨模块） |
| HIGRESS-X08 | schema 兜底与空 `required` | `build_input_schema` 跳过非对象属性（`client.rs:1033`）；`tool_parameters` 在 `required` 为空时省略该键（`toolkit.rs:481`） | Java 遇非 Map 会抛 `ClassCastException` 并回退文本解析；且恒写 `required: []` | 更鲁棒（不因单个畸形属性整体失败），空 `required` 对模型语义等价（关键字丢失另见 HIGRESS-01） |
| HIGRESS-X09 | 工具结果解码往返 | harness 返回 `serde_json::Value`，wrapper 反序列化回 `CallToolResult`（`client.rs:850-852`），失败即 `Error::Other(Failed to decode tool call result)` | Java 直接持有 typed `McpSchema.CallToolResult` | harness transport 缝以 JSON `Value` 为边界；显式报错优于静默丢字段 |
| HIGRESS-X10 | `top_k` 类型 | `usize`（`client.rs:881`），负数在编译期不可表达，0 运行时拒绝 | Java `int`，负数靠 `topK <= 0` 校验 | 更强的类型约束，行为等价 |

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-higress` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-higress` / `cargo test -p agentscope-extensions-higress` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-higress/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. 执行历史（已归档）

- 执行记录 / 静态复核 / 勘误 / 反向验证明细：[`history/HIGRESS__MIGRATION_HISTORY.md`](./history/HIGRESS__MIGRATION_HISTORY.md)
- 本文件只保留现行状态（§0–§5）；历史条目一律写进上述归档文件，不再回填本文。
