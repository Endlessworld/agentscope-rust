# 经验教训（踩过的坑）

## 2026-08-28 ACP 客户端不渲染思考/工具调用
- **现象**：`acp_harness_agent` 的思考过程与工具调用在真实 ACP 客户端不渲染；CLI 示例能打印工具调用但思考也没出来。
- **根因**：ACP 事件层（`acp/event.rs`）发出的 `AgentMessageChunk`/`AgentThoughtChunk` 未带 `message_id`，
  真实客户端靠 `message_id` 把同一 assistant 消息的 chunk 归组，缺失时思考/工具调用无法关联渲染；
  且工具调用硬编码 `ToolKind::Edit` + `title=tool_name` + 创建时不带 `raw_input`，客户端无法生成正确的工具卡片。
- **关键认知**：参考 agent（`agent-client-protocol-test/src/testy.rs`）每个 chunk 都 `.message_id(...)`，
  工具调用带人性化 title + 正确 kind + raw_input。这是 ACP 客户端渲染的必要条件。
- **解法**：`session.rs` 每轮生成稳定 `message_id` 透传到所有 chunk 与 `ToolEventMapper`；
  `event.rs` 新增 `tool_kind_for`（按名推断 kind）与 `human_title`，`ToolEventMapper` 在 ToolCallStart 附带 raw_input。

## 2026-08-28 ACP 流式输出累积问题
- **现象**：ACP 流式输出中，多个工具调用之间的思考/文本积累到下一次工具调用才推送到前端。
- **根因**：`OpenAiChatModel::stream()` 的 `StreamAccumulator` 把整个 SSE 流所有 chunk 累积到缓冲
  （text/tool_calls），直到流结束才 `into_response()` 产出一个完整 `ChatResponse`。导致：
  1. text 增量丢失 → `TextBlockDelta` 每轮只触发一次（整段），非逐 token。
  2. `thinking` 字段在 `accumulate` 中从不赋值 → `ThinkingBlockDelta` 从不触发。
- **关键认知**：async-openai v0.41.3 的 `ChatCompletionStreamResponseDelta` 没有 `reasoning_content`
  字段，serde 默认忽略未知字段，思考内容被静默丢弃。
- **解法**：项目已启用 async-openai 的 `byot` feature，可调用 `create_stream_byot::<CustomChunk>()`
  自定义 chunk 反序列化类型（`R: DeserializeOwned`），从而捕获 `reasoning_content` 并逐 chunk 产出
  增量 `ChatResponse`。
- **注意**：`CompletionUsage` 在 `async_openai::types::chat` 模块（经 `pub use chat_::*` 导出），
  不在 `async_openai::types` 根。

## 2026-09 ACP $/cancel_request 实现（SDK cancellation 机制）
- **现象**：`AcpCodingAgent` 的 `$/cancel_request` 通知 handler 未注册（GAP_TRACKER 标 ⚠️），需实现取消。
- **关键认知 1**：**无需注册 `on_receive_notification(CancelRequestNotification)`**——SDK 会在收到 `$/cancel_request` 时
  自动更新对应 `Responder` 的 cancellation marker（`Responder::cancellation()` → `RequestCancellation`，Clone）。
  正确做法是 handler 内取 marker 并监听 `cancellation.cancelled()`。
- **关键认知 2（踩坑）**：**长 handler 会阻塞 dispatch loop**，导致 `$/cancel_request` 无法被处理、marker 永远不更新。
  必须把长任务（如 `run_prompt`）用 `cx.spawn(...)` 移出 dispatch loop，handler 立即返回 `Ok(())`。
  首次实现因 handler 内 `await run_prompt`（never-ending model）导致 e2e 测试 5 分钟超时卡死。
- **关键认知 3**：SDK 提供 `Channel::duplex()`（内存双向通道，实现 `ConnectTo<R>` for all roles），
  可做**进程内端到端测试**（无需子进程/真实 model）：agent 端 `run_with_transport(channel_a)`，
  client 端 `Client.builder().connect_with(channel_b, ...)`。
- **测试诀窍**：`NeverEndingModel`（`futures::stream::pending()`）让 `run_prompt` 永不结束，
  client 侧 `prompt_request.cancel()` 后断言 `StopReason::Cancelled`。
- **注意**：`Responder::respond(self, ...)` 消费 self，需连同 `responder` 一起 move 进 spawn 任务；
  `cx.spawn` 要求 `Result<(), Error>` 输出。

## 2026-08-28 opencode zen 网关思考字段是 `reasoning` 而非 `reasoning_content`
- **现象**：release 版 CLI 连接 `acp_harness_agent` 时，界面始终无思考过程输出（只有正文）。
- **根因**：`openai.rs` 的 `OpenAiStreamDelta` 只解析 `reasoning_content` 字段，但 opencode zen 网关
  （OPENAI_BASE_URL=https://opencode.ai/zen/go/v1）返回的是 `reasoning` 字段（以及 `reasoning_details`），
  思考内容被 serde 静默丢弃 → 无 `ThinkingBlock` → 无 `AgentThoughtChunk`。
- **关键认知**：不同网关思考字段名不同——DeepSeek/O1 用 `reasoning_content`，opencode zen 用 `reasoning`。
  需同时兼容两者。
- **解法**：`OpenAiStreamDelta` 增加 `reasoning` 字段，新增 `reasoning_text()` 方法先取 `reasoning_content`
  再取 `reasoning`，`accumulate` 改用该方法。
- **调试方法**：写端到端 stdio 测试（`tests/acp_stdio_thought_test.rs`）拉起真实 `acp_harness_agent` 子进程，
  用 `on_receive_notification` 收集所有通知并断言 `AgentThoughtChunk`。
- **坑**：改完 `agentscope-core` 后必须重新 `cargo build --example acp_harness_agent`，
  否则 example exe 仍链接旧库，测试/CLI 拉起的还是旧行为。
## 2026-09 Windows进程树管理修复（sandbox.rs）
- **现象**：Windows上执行shell命令（如`git --version`）时，即使超时很短，工具也报告超时。
- **根因**：`tokio::process::Child::drop()`只杀死直接子进程（`pwsh.exe`），但其孙进程（`git.exe`、`git-credential-manager.exe`等）成为孤儿进程，持有管道句柄不释放→`wait()`永远挂起。
- **关键认知**：**Windows进程树管理需要`taskkill /F /T`**：`taskkill /T`会递归杀死整个进程树，避免孤儿进程。
- **管道死锁修复**：必须在`wait()`之前获取stdout/stderr（通过`tokio::spawn`并发读取），否则如果子进程写满OS管道缓冲区（~64KB），`wait()`永远不会返回。
- **解法**：改用`spawn()`+`wait()`替代`cmd.output()`，超时时调用`kill_process_tree(pid)`，并行读取stdout/stderr避免死锁。
- **跨平台处理**：Windows用`taskkill /F /T /PID`，Unix用`kill -TERM --`发送SIGTERM到进程组。
- **测试要点**：即使简单命令如`git --version`，在Windows上也可能因管道死锁而挂起，需要验证超时机制。

## 2026-09 ACP权限确认机制（PermissionBehavior::Ask）
- **现象**：当工具调用需要用户确认时（`PermissionBehavior::Ask`），之前会返回错误，无法支持人机协作。
- **关键认知1**：**权限引擎需要运行时可替换**：用`Mutex<PermissionEngine>`替代直接持有，允许在不重建agent的情况下切换权限上下文。
- **关键认知2**：**确认处理需要异步回调**：定义`ConfirmHandler`类型（返回`Pin<Box<dyn Future>>`），支持ACP客户端异步交互。
- **关键认知3**：**持久化规则需要原子写入**：使用临时文件+重命名模式，避免并发读写导致文件损坏。
- **解法**：
  1. `ReActAgent`新增`set_permission_context`、`set_confirm_handler`、`add_permission_rule`方法
  2. 实现`AcpClientHandle`封装ACP客户端连接，发送`session/request_permission`请求
  3. 创建`PermissionRuleStore`持久化"always allow/always reject"用户选择
  4. 模式映射：plan→Explore（只读）、accept_edits→AcceptEdits（文件编辑自动批准，shell需确认）、yolo→Bypass
- **测试要点**：需要验证权限模式切换、Ask批准/拒绝、handler运行时切换、持久化规则跨会话保留


## 2026-09 MCP 工具桥接（tokio 运行时内不可 block_on）
- **现象**：`McpClientManager::call_tool` 是 async，但 core `McpTool` 的 handler 是同步闭包。
  最初在 handler 内用 `tokio::runtime::Handle::current().block_on(...)` 桥接，运行时报
  "Cannot start a runtime from within a runtime" panic（`Tool::call` 本身在 tokio 运行时内执行）。
- **关键认知**：**在 tokio 运行时内无法 `block_on`**（单线程/current-thread runtime 尤其如此）。
  正确做法是让 `Tool::call`（async）直接 await 异步 handler，而不是用同步 handler 包一层再 block。
- **解法**：core `McpTool` 新增 `async_handler: Option<McpAsyncHandler>` 字段 + `with_async_handler()`；
  `call()` 优先 await async handler。类型别名 `McpAsyncHandler = Box<dyn Fn(&McpTool, JsonMap) -> Pin<Box<dyn Future<Output=Result<ToolResultBlock>> + Send>> + Send + Sync>`。
  嵌套 `Pin<Box<dyn Future + Send>>` 与外层 `+ Send + Sync` 易混淆，用类型别名最清晰。
- **测试要点**：真实 stdio MCP server 可用自包含 Node 脚本（纯 JSON-RPC，`readline` + `process.stdout`），
  无外部依赖即可做 e2e；`McpServerConfig.command` 用 `node <script>`（node 在 PATH），
  注意 `connect()` 里 `command.split_whitespace()` 对含空格路径会破坏（用 PATH 中的可执行名规避）。

## 2026-09 MCP SSE/HTTP 传输接入（rust-mcp-sdk 的 sse/streamable-http feature 是坏的）
- **现象**：给 harness 的 `rust-mcp-sdk` 加 `sse`/`streamable-http` feature 后编译失败，
  `mcp_http.rs` 里 `handle_sse_message`（`#[cfg(feature="sse")]`）引用 `internal_handle_sse_message`
  （`#[cfg(feature="server")]`），未开 `server` 时 E0599。
- **根因**：rust-mcp-sdk 1.0.1 的 `sse`/`streamable-http` feature 会额外编译 server 侧 `mcp_http`
  模块（`lib.rs` 里 `#[cfg(any(sse, streamable-http, auth))] pub mod mcp_http`），而该模块自身
  feature 门控不一致（`server` 未开时引用了不存在的方法）。
- **解法**：**绕开 sdk 的 sse/streamable-http feature**，直接依赖 `rust-mcp-transport`
  （sdk 同款 1.0.0）并开 `sse`/`streamable-http`；客户端传输 `ClientSseTransport`/`ClientStreamableTransport`
  在 transport crate 里工作正常。sdk 仅保留 `client`/`stdio`/`macros`。
- **关键认知**：
  1. `ClientSseTransport::new(&str, ClientSseTransportOptions)`；`ClientStreamableTransport::new(&StreamableTransportOptions, session_id: Option<_>, standalone: bool)`。
  2. `create_client::<T>` 是泛型，三种 transport 是不同具体类型 → 必须在每个分支内各自 `create_client`，不能先构造统一类型再传。
  3. `StreamableTransportOptions`/`RequestOptions` 的 `custom_headers` 是 `Option<HashMap<String,String>>`。
  4. ACP `McpServer` 枚举 `#[non_exhaustive]`，match 需 `_other` 通配分支。
  5. URL 含 `/sse` 判定为 legacy SSE，否则 Streamable HTTP（`is_sse_url` 辅助函数 + 单测）。

## 2026-09 ACP 会话恢复语义（acp_harness_agent 仅进程内恢复）
- **结论**：`AcpCodingAgent.sessions` 是内存 `HashMap`；`AcpAgentSession` 持有 `Arc<HarnessAgent>`，
  对话历史只在嵌 `ReActAgent` 的 `AgentState.context` 里。**进程退出即全丢**，重启后同 sessionId
  只能新建空会话（`load/resume` 未命中 → `create_session`），对未知 id 的 `prompt` 返回 `Refusal`。
- **进程内可恢复**：断线重连用 `session/load`/`session/resume` 命中注册表即复用原 agent；
  prompt handler 持 Clone，跑完必须 `register_session` 写回（`config`/`agent`/`mcp_manager` 为 Arc 才不丢）。
  取消（`$/cancel_request` / `close`）→ `StopReason::Cancelled`，本轮未完成消息不入 AgentState。
  `max_sessions` 超限按 `session-<epoch_ms>` 淘汰最旧 → 历史静默丢失。
- **缺口**：`load_session` 不回放历史 `session/update`（协议要求），命中时出现"模型记得、UI 不显示"；
  `load/resume` 不校验 sessionId 归属。
- **跨重启能力已有但未接线**：core `JsonFileAgentStateStore` + harness `StateCheckpointMiddleware`
  （首次 run 前恢复、每次 run 后保存，key=`agent_state`），由 `HarnessAgentBuilder::state_store()
  + session_id()/user_id()` 装配（harness_agent.rs:1102-1121）。example `build_agent()` 未设置，
  `session_id` 只用于 `x-opencode-session` 请求头。
