# 修复：v1/chat/completions 请求体带 `thinking_budget` 导致严格网关 400

## 现象
个别模型（Console Go / opencode zen 网关）报错：
`HTTP 400 ... invalid request body: json: unknown field "thinking_budget"`

## 根因
`crates/agentscope-harness/src/acp/session.rs::thought_level_params` 中，对**非 OpenAI** 模型
（deepseek/glm/kimi/qwen/…）在 thought_level != auto 时通过
`GenerateOptions::additional_body_param` 注入 `enable_thinking` + `thinking_budget`；
`agentscope-core/src/model/openai.rs::stream()` 会把这些
`additional_body_params` 合并进 `/chat/completions` 的 JSON body。
宽松网关忽略未知字段，严格网关直接 400。

## 修复（工作区未提交改动）
`thought_level_params` 现在只做两件事：
- `auto`/空 → `GenerateOptions::default()`（不传任何思考参数）
- OpenAI `o1/o3/o4/gpt-5*` → `reasoning_effort`（官方字段，`max` 钳到 `high`）
- **其余模型一律 `GenerateOptions::default()`** —— 即「默认不传」

配套回归测试（session.rs `mod tests`，3 个，已跑绿）：
`auto_sends_no_thinking_params` / `openai_reasoning_models_use_reasoning_effort` /
`vendor_models_send_no_thinking_body_params`。

## 注意
- chat/completions 路径只有 `additional_body_params` 一个注入点；`openai.rs::build_request`
  本身从不写 `thinking_budget`。空 map ⇒ 请求体里不会出现该字段。
- `GenerateOptions.thinking_budget` 字段仍保留，仅 **DashScope 原生 API**
  (`extensions-model/src/dashscope/converter.rs`) 使用（Qwen 支持该参数，勿删）。
- 文档同步：`docs/MIGRATION_GAP_TRACKER.md`「动态模型切换 + per-session headers」行
  已由「enable_thinking 直达请求体」改为「仅 OpenAI o/gpt-5 映射 reasoning_effort，其余不传」。

## 验证命令
`cargo test -p agentscope-harness --lib acp::session::tests`
