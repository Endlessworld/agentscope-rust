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

## 2026-09 Docker 沙箱后端（CLI 实现）踩坑
- **Java 的 `WorkspaceSpecApplier` 不能照搬**：Java 用宿主 `Files.writeString(Path.of("/workspace/..."))` 写容器路径，在 Windows/无该路径的宿主上会直接失败（Java 端隐含 Linux 宿主的假设）。Rust 实现改为经 `docker exec` 物化（`mkdir -p` + `printf %s <base64> | base64 -d > file`），避免宿主路径依赖，同时保证内容编码安全。
- **状态字段扩展必须带 `#[serde(default)]`**：`DockerSandboxState` 新增字段后，旧 JSON（无这些字段）仍需可加载；`container_owned` 需 `default = "fn"` 才能缺省为 `true`（`#[serde(default)]` 会得到 `false`，语义变成“非自管容器”→ shutdown 不再清理）。写了一个 legacy-JSON 反序列化测试固定该行为。
- **`container_id` 从 `String` 改 `Option<String>`**：容器要 `docker run` 之后才有 id，映射 Java 的 nullable containerId。
- **命令超时与管道死锁**：所有 docker CLI 调用都复用 `filesystem/sandbox.rs` 的套路——先 `spawn()`、再并发 drain stdout/stderr、超时 `kill_process_tree(pid)`（Windows `taskkill /F /T`）。`hydrate_workspace` 要往 stdin 写 tar，必须一边写一边 drain stdout/stderr，否则 tar 输出填满管道即死锁。
- **`ExecResult.exit_code = -1` 表示超时**：与 crate 既有约定（`ExecuteResponse`）一致，不要新造错误类型。
- **E: 盘紧张会伪装成编译错误**：`cargo test --workspace` 报大量 `could not compile` + `error writing dependencies ... os error 112（磁盘空间不足）`。解法：`rm -rf target/debug/incremental`（释放约 1.6GB）后重跑；不要误判为代码错误。
  `session_id` 只用于 `x-opencode-session` 请求头。

## 2026-09 worker 缺口报告必须抽样复核（幻觉案例）
- **案例**：`worker_output/worker_result_d90adbd7.txt`（HarnessAgent Builder 对比）基线伪造——
  声称 Java `HarnessAgentBuilderSupport` 有 62 setter、build() 2358-2987 行，Rust builder 有 68 方法
  （`parse_agent_map` / `heartbeat_config` / `activityTrackerConfig` / `default_model`）。
  实际该 Java 类只是第 71 行的 package-private 小助手；Rust `HarnessAgentBuilder` 仅 51 字段 / 53 `pub fn`；
  报告点名的 16 个字段（`defaultToolProperties`/`toolPropertyOverride`/`disabledTools`/`allowedTools`/
  `additionalEnvironment`/`dbConfig`/`codexExecutionMode`/`maxAgentContextTokens`/`fileContextMaxFiles`/
  `perFileMaxTokens`/`extraFileAgentNames`/`logLevelOverrides`/`__MYSELF__`/`heartbeatConfig`/
  `activityTrackerConfig`/`parse_agent_map`）在 **Java 与 Rust 两个仓库中 grep 均 0 命中**。
- **陷阱**：行号可以看起来吻合（Rust `build()` 实际就在 753 行，与报告一致）→ 行号吻合 **不能** 作为可信度证据；
  真正的判定标准是「字段名/方法名能否 grep 到」。
- **做法**：对账此类报告时先做存在性 grep，再采信结论；不可复核的报告直接作废、重新推导，
  不要在其基础上"补一半"。对账 Java 源时用 `grep`/`ls`（read_file 拒绝 workspace 外路径）。

## 2026-09-17：对账 codex（E:\local-github\codex）大型仓库的侦察经验

- **仓库规模决定搜索策略**：codex 是 ≈110 crate 的 monorepo。全仓 `rg -l -i <pattern>`（无路径限定）**会超时**（实测 60s 超时后被转为交互式 shell，需 KillShell）。正确做法：先按目录限定（`rg ... core/src tools/src`），或用 `Get-ChildItem -Recurse -Depth 2 -Filter Cargo.toml | Select-String` 这类有界搜索。
- **尺寸统计比 grep 更快**：先 `Get-ChildItem -Directory` 拿 crate 清单，再对关键目录做「文件数 + 行数」统计，几次调用即可建立量级认知，避免逐文件读。
- **pwsh 输出中文会乱码**（终端编码），但 `[System.IO.File]::ReadAllText(path)` + 正则统计不受影响——统计类检查优先用这种方式。
- **codex 的工具名不在 ToolName 枚举里**：`protocol/src/tool_name.rs` 只是 `{namespace, name}` 结构体；真正工具名要 grep `ToolName::plain(...)` / `ToolName::namespaced(...)`（在 handler 里注册）。
- **写大文档的可靠手法**：`write_file` 只写 ≤500 字符骨架 + 末尾唯一哨兵 `<!--END-->`，之后每次 `smart_edit` 用 `search_replace` 把哨兵替换为「新章节 + 哨兵」，可无上限续写且不丢定位；最后一次替换不带哨兵即收尾。写完后用 pwsh 把 LF 归一为 CRLF。
- **smart_edit 的两个坑**：`insert_at_line` 会在「疑似字符串/块注释」的行上误报失败（markdown 里的引号可触发）；此时改用 `search_replace`，或把追加内容写成临时文件再用 .NET 追加。
- **`insert_at_line` 不会自动处理行号偏移**：同一次调用里多个 `insert_at_line` 是顺序执行且不做偏移补偿，后一个若按「原文件行号」填写就会越过文件长度直接失败（而同一批后面的编辑会被跳过）。追加式写文件优先用 `search_replace` 锁住文件尾部唯一文本。

## 2026-09-07：给 Rust harness 接 Java 式 subagent 物化链的经验

- **`HarnessAgent` 必须先实现 `CallableAgent`**，否则 `SubagentFactory::create -> Arc<dyn CallableAgent>` 无处可返——Java 里 HarnessAgent 本身就是 Agent，Rust 它是纯包裹器（内嵌 `Option<ReActAgent>`），需要显式 `impl Agent/CallableAgent` 委托。
- **子 agent 继承的父 toolkit 必须是「已注册完所有工具」的快照**：Rust 的 `build()` 原先把 subagent 段放在工具注册之前，直接拷贝会得到空 toolkit。接线的关键动作之一是把该段搬到工具全部注册之后，并在注册 `agent_spawn` **之前**做快照（leaf 不该继承 spawn 工具）。
- **`load_from_directory(dir, main_workspace)` 第二个参数是 `Option<&str>`**（不是 `&str`），且 dynamic middleware 自己会扫描 `subagents/`；static 模式必须由 builder 自己扫，否则声明永远物化不出来。

## 2026-09-18：会话续聊 HTTP 400（role 'tool'）根因 = reasoning 中间件的改写从未到达模型

- **症状**：ACP `acp_harness_agent` 会话中断/收尾后继续对话报
  `Messages with role 'tool' must be a response to a preceding message with 'tool_calls'`。
- **两个叠加缺陷**（只修一个都不够）：
  1. `react_agent::run_reasoning` 在中间件链**执行之前**就把 `ModelCallInput` 冻结了，
     `reasoning_core` 闭包参数写作 `_r` 并被忽略 → 所有 `on_reasoning` 改写（不仅
     `ToolCallPairSanitizerMiddleware`，还有 task_reminder / inbox / subagents /
     dynamic_subagents / teams / tool_result_eviction）**全部是死代码**。
  2. `extract_pending_tool_calls` 只看“最后一条 assistant 消息”的 tool_use，
     **不检查是否已被后续 TOOL 消息应答** → `summarizing()`（max_iters 收尾）给已应答的
     call 再写一条 TOOL 结果，持久化出“同一 tool_call_id 应答两次”的历史；下一次请求必 400。
- **定位手法（可复用）**：不要去猜消息序列——直接解析 `.agentscope/state/<user>/<session>/agent_state.json`，
  把 `context[]` 的 role + tool_use.id / tool_result.id 打成一行一条，悬空/重复立刻肉眼可见。
  本次第 15/16 两条 TOOL 回答同一个 id，即缺陷 2 的铁证。
- **验证手法**：写“记录模型”（RecordingModel 把收到的 messages 存下来）+ 断言
  「同一 tool_call_id 在发给 provider 的消息里只能被应答一次」。先让测试在旧代码上失败
  （实测 `left: 2, right: 1`），再改代码。
- **教训**：洋葱式 middleware 链的 core 闭包**必须消费上游传入的 input**；一旦
  “提前求值 + 忽略参数”，中间件会静默失效（无编译错误、无告警、单测也可能全绿，
  因为中间件单测通常直接调用 `mw.on_reasoning(...)`）。给中间件写测试要端到端（真跑 agent + 记录模型）。

## 2026-09-19：async-openai 0.41.3 实际支持的端点（核实结论）

- **`/chat/completions`**：已用（`chat-completion` feature，`chat().create_stream_byot`）。
- **`/responses`：原生支持**。feature `responses` + `response-types`；
  `src/responses/responses_.rs` 的 `create`/`create_stream` 路径为 `{api_base}/responses`，
  返回 `ResponseStream`（`ResponseStreamEvent`）。**无需自研 HTTP**。
- **`/messages`：不支持**。crate 内无 Anthropic Messages 协议实现 → 必须 `reqwest` + 手写 SSE 解析。
- `OpenAIConfig::url()` = `format!("{api_base}{path}")`（`config.rs:219`），所以 base_url 只写到 `/v1`，
  端点后缀由各协议客户端内部拼。
- **教训**：设计前先翻依赖源码（`~/.cargo/registry/src/.../async-openai-0.41.3`）确认能力，
  不要凭“只支持 chat/completions”的旧印象下结论——会多写一个不必要的自研 Responses 客户端。

## 2026-09-19：`async-anthropic` 0.6.0 能力（已下载 crate 源码核实）

- 来源 `https://static.crates.io/crates/async-anthropic/async-anthropic-0.6.0.crate`（MIT，2026-08）。
- `Client` 是 `derive_builder` 结构体，字段：`http_client: reqwest::Client` / `base_url` / `api_key: SecretString` /
  `version`（默认 `2023-06-01`）/ `beta: Option<String>` / `backoff`。→ **可自定义 base_url + 注入带自定义 header 的 reqwest client**。
- `messages().create_stream(req) -> CreateMessagesResponseStream`；`create(req)` 走非流式。
- 工具调用完整：`CreateMessagesRequest.tools: Option<Vec<serde_json::Map>>`、`tool_choice`、
  `MessageContent::{ToolUse, ToolResult}`、`Message::tool_uses()`、`ToolChoice::{Auto,Any,Tool(name)}`。
- `format_url` = `{base_url}/{path}`，`messages.rs` 硬编码 `/v1/messages` → base_url 只写到 `/v1` 即可。
- 流式事件枚举 `MessagesStreamEvent`：`message_start`/`content_block_start`/`content_block_delta`/
  `content_block_stop`/`message_delta`/`message_stop`。
- **依赖冲突提醒**：async-anthropic 用 `reqwest 0.12`，async-openai 0.41 用 `reqwest 0.13` → 双版本共存。
- **教训**：选第三方 SDK 前先下载 crate 源码看 `client.rs`/`types.rs`，比翻 docs.rs（文档覆盖率仅 18%）可靠得多。

## 2026-09-19：多端点改造落地要点（responses / anthropic）

- **byot 宏**：`#[crate::byot]` 会生成 `xxx_byot` 泛型版本，但**原方法仍然保留**。所以
  `client.responses().create_stream(req)` 可直接用，无需 `create_stream_byot`（后者仅在需要自定义
  SSE chunk 类型时才用，如 openai.rs 为了拿 `reasoning_content`）。
- **async-openai responses 关键类型**（`types::responses`）：`CreateResponse` 全 pub + Default；
  `InputParam::Items(Vec<InputItem>)`；`InputItem::{EasyMessage, Item}`；`Item::{FunctionCall, FunctionCallOutput}`；
  `Tool::Function(FunctionTool{parameters: Option<Value>})`；`ToolChoiceParam::Mode(ToolChoiceOptions)`；
  事件 `ResponseStreamEvent::{ResponseOutputTextDelta, ResponseReasoningTextDelta,
  ResponseFunctionCallArgumentsDelta/Done, ResponseOutputItemAdded, ResponseCompleted, ResponseError}`；
  `Response { id, usage: Option<ResponseUsage>, status: Status, output }`。
- **async-anthropic 关键陷阱**：
  1. `ClientBuilder` 的 setter 是 `&mut self`（derive_builder 默认），不能 `Client::builder().a().b()` 链式
     —— 临时值会被 drop。必须先 `let mut b = Client::builder(); b.x();`。
  2. `messages().create_stream()` 直接返回 stream（不是 `Result<Stream>`）。
  3. `ContentBlockDelta` 只有 `TextDelta`/`InputJsonDelta`，**无 thinking_delta** → Anthropic 链路不产 ThinkingBlock。
  4. `MessageStart` 有 7 个字段（含 `stop_sequence`/`usage`），测试构造时易漏。
  5. system 是**顶层字段**不是 message；tool_result 必须放在 **user** 消息里。
- **reqwest 双版本共存验证通过**：async-anthropic 用 0.12、async-openai 0.41 用 0.13，同时编译无冲突。
- **环境坑**：本机 `cargo test --workspace` 会因页面文件不足报 `os error 1455`（mmap rlib 失败），
  必须加 `-j 2`。这类报错看起来像代码错误，实际是内存/虚拟内存不足。

## 2026-09-19：acp_harness_agent 模型配置被静默覆写（BOM / 解析失败）

- **现象**：`~/.agentscope/models.json` 已存在且被手工修改，每次运行仍被重置为内置默认。
- **根因**：`ModelConfig::load_or_create()` 把“文件不存在”与“解析失败”混为一谈——
  `try_load()` 任一失败（读失败/`serde_json` 解析失败）都返回 `None`，随后无条件 `save()` 写默认值，
  把用户文件覆盖掉。Windows 编辑器（记事本等）给 UTF-8 加 BOM（`EF BB BF`），`serde_json::from_str`
  直接报错 → 稳定复现“每次覆写”。
- **修复**：① 仅当 `path.exists()==false` 时才写默认配置；已存在但解析失败则保持文件原样、
  打印告警并本次回退内置默认。② `try_load` 读取后 `strip_prefix('\u{feff}')` 容忍 UTF-8 BOM。
- **验证**（隔离 `HOME` 跑 `acp_harness_agent.exe`）：正常修改✅加载、BOM 修改✅加载、非法 JSON✅保留原文件+告警。
- **教训**：“首次生成默认配置”类逻辑，写盘必须 gated 在**文件不存在**上，绝不能以解析成功为前提；
  Windows 下所有读 JSON 配置的入口都应容忍 BOM。
- **追加（宽松解析）**：`try_load` 先严格解析，失败再经 `sanitize_json` 剥离 `//`、`/* */` 注释与尾随逗号后重试；
  任意换行/缩进 serde_json 本就支持，无需特殊处理。手写扫描器必须字符串感知（track `in_string`/`escaped`），
  否则 URL `https://…` 里的 `//` 会被误删。未引入 `json5` 依赖（零新增依赖）。
- **验证**：带注释+尾逗号的配置 → `model='LENIENT-MODEL'`、`base_url='https://opencode.ai/zen/v1'` 均正确；
  真语法错误仍保留原文件 + 告警。真值验证手法：用 `Start-Process -RedirectStandardInput`
  向 ACP agent 发 `initialize`+`session/new`，触发 `build_agent` 打印解析后的 base_url，比只看“无报错”可靠。
