# 参考实现调研：flux（GrayCodeAI）— reasoning_content 回传（2026-09-20）

## 仓库
- `https://github.com/GrayCodeAI/flux.git`，Go 1.26+，MIT，"Universal LLM Provider Runtime"（驱动 rho coding agent 的 provider 引擎）。
- 已 clone 到 `E:\local-github\agentscope-rust\flux`（3.2MB / 609 文件，untracked 且**未被 .gitignore 忽略**，`git check-ignore` 退出码 1）。
- 相关目录：`client/adapters/`（provider 适配）、`client/core/`（归一化流）、`client/thinking_policy.go`、`llm/`。

## 核心发现：provider 级开关 `RequiresReasoningPassback`
`client/adapters/compat.go:16-22`（`OpenAICompatConfig` 字段）+ `:124-128`：
```go
DeepSeekCompat = OpenAICompatConfig{
    MaxTokensField: "max_tokens", SupportsUsageInStreaming: true,
    RequiresReasoningPassback: true, ThinkingFormat: "deepseek", DefaultDisableThinking: true,
}
```
注释原文即我们的 400：
> DeepSeek requires the assistant's reasoning_content to be passed back whenever that turn performed a tool call, otherwise the API returns HTTP 400. When no tool call happened the field is ignored by the provider, so forwarding is always safe for compliant providers.

实际转发 `client/adapters/openai.go:215-220`（**无条件、无分段裁剪**）：
```go
if compat != nil && compat.RequiresReasoningPassback &&
    m.Role == "assistant" && strings.TrimSpace(m.Thinking) != "" {
    msg["reasoning_content"] = m.Thinking
}
```
配套：`openai.go:139`（非流式响应 DTO `ReasoningContent`）、`client/core/stream.go:301`（流式 delta `reasoning_content`）、`openai.go:526`（`result.Thinking = ch.Message.ReasoningContent`）。
测试：`client/reasoning_thinking_test.go:174-250` 双向断言（DeepSeek 必转发 / 非 DeepSeek 必不转发）。

## 对 agentscope-rust 的裁定价值
1. **「是否回放」应是 provider 级选项**（回答此前待裁定②③），而非全局一刀切。
2. **两条并存通道**（解决了 core 丢弃 vs anthropic 回放的矛盾）：
   - `reasoning_content` 字段通道 → 仅 provider 专属（DeepSeek），目的是避 400；
   - thinking 降级为文本通道 → `ThinkingFormat` / `RequiresThinkingAsText`（`compat.go:14-15`），各 provider 各自格式。
3. **flux 比 Java 更简单**：不按段裁剪（Java `DeepSeekFormatter.applyDeepSeekFixes` 的分段策略），只判「provider 需要 + thinking 非空」就转发，理由是「无 tool call 时 provider 忽略，转发总是安全」。→ 支持**无条件回放（限定 DeepSeek）**，不值得引入分段复杂度。
4. `ThinkingFormat` 枚举（`client/thinking_policy.go:22-31`）覆盖 wire 差异：`zai`/`longcat`/`kimi`/`deepseek`/`xiaomi` → `thinking={"type":"enabled"|"disabled"}`；`minimax` → `adaptive`；`agnes` → `chat_template_kwargs.enable_thinking`；`qwen` → 顶层 `enable_thinking`；`openrouter` → `reasoning={enabled:bool}`。`openai.go:413-445` 实际构造。
5. flux 的 `FluxMessage.Thinking` 是**扁平 string**，非结构化 block；与 `extensions-model/anthropic/converter.rs:211-216` 的「降级为文本」同构。
6. **附带发现（我们未处理）**：`client/core/stream.go:322-345` `thinkSplitter` —— 部分 DeepSeek/Qwen/Ollama 路由把思维链**内联在 `delta.content` 的 `<think>...</think>` 标签**里，需按 chunk 边界精确拆分（跨 chunk 的半个标签要缓冲），否则思维链泄漏进正文。Rust 侧无此逻辑。

## 对照表（Rust 现状 vs flux）
| 能力 | agentscope-rust | flux |
|---|---|---|
| 解析 `reasoning_content` | ✅ `core/src/model/openai.rs:505` | ✅ `stream.go:301` |
| 存 thinking | ✅ `ThinkingBlock` | ✅ `FluxMessage.Thinking` |
| 回传字段 | ❌ 无 | ✅ `openai.go:219` |
| provider 开关 | ❌ 无 | ✅ `OpenAICompatConfig` |
| 段级裁剪 | ❌ | ❌（有意不做） |
| 空 content 兜底 | ✅（工作区已改） | 天然有字段 |
| `<think>` 内联拆分 | ❌ | ✅ `thinkSplitter` |
