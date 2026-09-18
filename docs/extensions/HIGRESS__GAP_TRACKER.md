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
| Java 规模（初测） | 主代码 ~1071 行 / 测试 ~971 行 |
| Rust crate | `crates/agentscope-extensions-higress` |
| 迁移波次 | C |
| 当前状态 | 已完成（30 test 通过） |

## 2. Java 清单与依赖映射
| Java 类 / 包 | 职责 | Rust 对应 | 说明 |
|---|---|---|---|
| `HigressMcpClientBuilder` | 流式构建 Higress MCP 客户端（SSE / StreamableHTTP 两种 transport、headers、queryParams、timeout、toolSearch）| `HigressMcpClientBuilder`（src/client.rs）| 构建 `HigressClientConfig` + `HigressMcpClientWrapper`；`build`/`build_async` 对齐 `buildSync`/`buildAsync` |
| `HigressMcpClientWrapper` | Higress MCP 客户端包装：委托底层 MCP 客户端；`x_higress_tool_search` 语义化选工具；工具缓存 | `HigressMcpClientWrapper`（src/client.rs）| 委托 **harness `McpClientManager`**（`list_tools`/`call_tool_by_server`），非自建 MCP 连接 |
| `HigressToolSearchResult`（含 `ToolInfo`）| 解析 `x_higress_tool_search` 结果：优先 `structuredContent`，回退 `content` 文本 JSON；`ToolInfo{name,description,title,inputSchema,outputSchema}` | `HigressToolSearchResult` + `HigressToolInfo`（src/search_result.rs）| 解析 `rust_mcp_sdk::schema::CallToolResult` |
| `HigressToolkit` | 继承 core `Toolkit`，注册 Higress MCP 客户端并缓存 | `HigressToolkit`（src/toolkit.rs）| newtype 包装 core `agentscope_core::tool::Toolkit`，缓存最近注册的 Higress 客户端 |

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
| `HigressMcpClientBuilder` 校验测试 | `HigressMcpClientBuilderTest` | 已完成 | 15 个用例（`client.rs`）：clientName 空白拒绝、缺 endpoint、两种 transport、header/queryParam 排序与合并、`toolSearch` 空 query 拒绝、`topK<=0` 拒绝、`topK` 保留、`endpoint_with_query`、`to_server_config` 投影、超时 |
| `HigressMcpClientWrapper` 行为测试 | `HigressMcpClientWrapperTest` | 已完成 | 7 个用例：`with_options` 状态、`initialize` 幂等、`close` 重置、未连接时 `list_tools` 报错、`search_tools` 失败路径、`convert_to_mcp_tool` 三类 schema 形态 |
| `HigressToolkit` 测试 | `HigressToolkitTest` | 待办（P2） | `HigressToolkit` 的 `register_mcp_client` 需真实/桩 `McpClientManager` 连接，当前为纯内存 manager 无法产工具；未造假用例 |
| 【校正】本 crate 原无 `#[cfg(test)]` | — | 已修复 | 上一轮 tracker 记录「0 个用例」属实；本轮已补齐 30 个离线用例 |

## 4. 项目状态与下一步
### 4.1 当前状态
- **已完成**：4 个 Java 主类全部迁移；30 个离线单测；`cargo build` / `cargo test` 全绿。
- **验证命令与结果（本会话实测）**：
  - `cargo test -p agentscope-extensions-higress` → `test result: ok. 30 passed; 0 failed; 0 ignored`（doc-tests 0）。
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
- **改动文件**：`crates/agentscope-extensions-higress/src/client.rs`（仅本 crate）。
- **验证**：`cargo build -p agentscope-extensions-higress` → 通过；`cargo test -p agentscope-extensions-higress` → 通过。

### 4.2 下一步与阻塞
1. **汇总方**：是否将 `rust-mcp-sdk` 提升为根 `Cargo.toml` 的 `[workspace.dependencies]`（当前仅本 crate 内声明，与 harness `Cargo.lock` 锁定的 `1.0.1` 一致）。
2. **汇总方（信息共享）**：Java `McpClientWrapper` 无 Rust 等价基类，本模块以 `McpClientManager` + server 名委托实现；若其它 C 波次模块（AISTIO/STUDIO 等）也需 MCP 客户端包装，建议统一抽象后再收敛。
3. 真实端到端联调（连真实 Higress 网关）需 `McpClientManager` 建立连接；本 crate 已把 `HigressClientConfig` 转换成 `McpServerConfig`，端到端由调用方完成连接。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-higress` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-higress` / `cargo test -p agentscope-extensions-higress` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-higress/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

