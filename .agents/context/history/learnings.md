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