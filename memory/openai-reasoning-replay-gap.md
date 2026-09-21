# 更正与补充：ReActAgent 400 有两个独立缺陷（2026-09-20）

本文更正同目录 `openai-empty-assistant-message-fix.md` 中**不准确**的一句：
> 「修复：始终发送 content，空文本回退 ""。**对齐 Java** OpenAIMessageConverter.convertAssistantMessage」

该表述不准确。Java 的 `convertAssistantMessage` 做了**两件事**，Rust 只做了第一件：

## 缺陷 1：空壳 assistant 消息（content 缺失）——工作区已改，未 commit
- 位置：`crates/agentscope-core/src/model/openai.rs` `to_request_messages` 的 `MsgRole::Assistant`。
- 根因：`extract_text_content`（`formatter/base_formatter.rs:16`，`:24` 跳过 `ThinkingBlock`）返回 `""` 时旧代码把 `content` 置 `None` → `{"role":"assistant"}`。
- 触发链：`react_agent.rs:121 build_final_message`（有 thinking push Thinking，文本空不 push Text）→ `:819` 入历史 → `:800` 下轮取历史。
- 修复：`content = Some(Text(""))` 兜底（对齐 Java 的第一件事）。

## 缺陷 2：reasoning_content 从不回传（Java 的第二件事，Rust 完全缺失）
DeepSeek 官方文档 https://api-docs.deepseek.com/zh-cn/guides/thinking_mode 硬要求：
- **携带 tools 的请求**：历史轮次 `reasoning_content` **必须完整回传**（即使该轮未实际调用工具），**否则返回 400**。
- 未携带 tools：无需回传，传了也被忽略。
- 思考模式不支持 `temperature`/`presence_penalty`/`frequency_penalty`（不报错但不生效）；`top_p` 仅思考模式生效，下限 0.95。
- OpenAI 格式开关 `{"thinking":{"type":"enabled/disabled"}}`（经 `extra_body`）；强度 `reasoning_effort` = low/high/max（默认开且 high）。

Rust 数据链「在、但最后一跳断」：
- ✅ 解析 `reasoning_content`：`core/src/model/openai.rs:505`（`OpenAiStreamDelta`，兼容 `reasoning` 别名）
- ✅ 存为 `ThinkingBlock`：`react_agent.rs:110` + `:123-125`
- ❌ 转请求：`openai.rs:339` 走 `extract_text_content` → Thinking 被丢
- ❌ 请求体字段：async-openai 0.41.3 `ChatCompletionRequestAssistantMessage` **无 `reasoning_content`**（仅 content/refusal/name/audio/tool_calls/function_call）；全仓库请求侧零命中

Java 参考实现：
- `extensions-model-openai/formatter/OpenAIMessageConverter.java:318-336`：content 空填 `""` **且** `builder.reasoningContent(thinking)`；`:338-352` 从 `ThinkingBlock.METADATA_REASONING_DETAILS` 恢复 `reasoning_details`（OpenRouter/Gemini）。
- `compat/deepseek/DeepSeekFormatter.java:96-113 applyDeepSeekFixes`：**段级裁剪**——当前轮（`i>=lastUserIndex`）与「含 tool_calls 的段」保留 reasoning，其余删除（`:172-201 fixMessage`）；`DeepSeekMultiAgentFormatter` 复用。
- `dto/OpenAIMessage.java:84` `@JsonProperty("reasoning_content")`（`@JsonAlias("reasoning")`）+ `reasoning_details`。

落地障碍（若实施）：
1. async-openai 无字段 → 只能走已有 Value 通道（`openai.rs:135-155` `serde_json::to_value` + merge `extra_body` + `create_stream_byot`）。**不能用 `additional_body_params`**（顶层参数，进不了单条 message）。
2. `to_request_messages` **非 1:1**（一条 Tool Msg 带 N 个 result → N 条消息，`openai.rs:369-381`），不能按下标事后回填 `body["messages"][i]`，须在构造 assistant 消息时就带上。
3. 需策略层（有 tools 才回放 / 段级裁剪），不能无条件回放。

## 待裁定
① 策略放 core 内联 vs 引入 formatter seam（§2.2 已定 core 拥有 openai、extensions 不重复实现）② 是否无条件回放 ③ `reasoning_details` 是否一并迁移。

## 相关并存矛盾
`core/formatter/base_formatter.rs:24` 丢弃 thinking vs `extensions-model/src/anthropic/converter.rs:211-216` 把 thinking 降级为纯文本塞 content——两处口径不一致，且都不是 `reasoning_content` 字段语义，需统一。

## 无法确证的点（勿当结论）
错误串 `Invalid assistant message: content or tool_calls must be set` 是 vLLM 类严格网关措辞；DeepSeek 文档只说「返回 400」未给原文。因此「当前 400 由缺陷 1 还是 2 触发」须看真实请求体才能定论；两者都会 400。
