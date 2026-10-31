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
| 当前状态 | **总判：已完成**（P1 清零，P2 于 2026-10-31 收口——P2-4/P2-5/P2-6/P2-7 已落地，P2-2 定性为「设计决定（不迁移）」，P2-8 转跨模块需求；见 history/HIGRESS__MIGRATION_HISTORY.md）—— 已完成（57 lib test + 1 doc test；P1-1 ~ P1-4 于 2026-10-28 关闭） |

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

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-higress` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-higress` / `cargo test -p agentscope-extensions-higress` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-higress/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. 执行历史（已归档）

- 执行记录 / 静态复核 / 勘误 / 反向验证明细：[`history/HIGRESS__MIGRATION_HISTORY.md`](./history/HIGRESS__MIGRATION_HISTORY.md)
- 本文件只保留现行状态（§0–§5）；历史条目一律写进上述归档文件，不再回填本文。
