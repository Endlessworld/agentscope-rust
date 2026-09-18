# 思考等级下发策略（**取代** `2026-09-18-thinking-budget-fix.md` 中「其余模型一律不传」的结论）

## 背景
`/v1/chat/completions` 请求体带 `thinking_budget` → 严格网关（Console Go / opencode zen）400：
`invalid request body: json: unknown field "thinking_budget"`。
注入点在 `crates/agentscope-harness/src/acp/session.rs::thought_level_params`
（经 `GenerateOptions.additional_body_params` 合并进 body，见 `agentscope-core/src/model/openai.rs::stream`）。

## 用户决策
「但是 `reasoning_effort` 还是可以传的，先试一下，只传 `reasoning_effort`，不传 enable_thinking + thinking_budget」
→ 不做按厂商分流，**所有模型统一只下发 `reasoning_effort`**，由实测验证网关是否接受。

## 现行实现（工作区未提交改动）
`fn thought_level_params(level: &str) -> GenerateOptions`（已移除 `model_name` 参数）：
- `auto` / 空 → `GenerateOptions::default()`（什么都不传，保留 provider 默认）
- 任意显式等级 → `GenerateOptions::builder().reasoning_effort(effort)`
  - `disabled` / `none` → `minimal`
  - `max` → `high`（无 `xhigh`）
  - 其余（low/medium/high）原样透传
- **永不**写 `enable_thinking` / `thinking_budget` 到 body

调用方 `apply_session_config` 已随之删掉「读取内嵌 ReAct agent 的 model_name」那段（不再需要）。

## 测试（已跑绿）
`cargo test -p agentscope-harness --lib acp::session::tests`
- `auto_sends_nothing`
- `explicit_levels_send_only_reasoning_effort`（断言 additional_body_params 恒为空）

## 相关事实
- chat/completions 路径唯一注入点是 `additional_body_params`；`openai.rs::build_request` 从不写
  `thinking_budget`。`reasoning_effort: Option<..>` 为 None 时不序列化。
- `GenerateOptions.thinking_budget` 字段保留，仅供 **DashScope 原生 API**
  （`extensions-model/src/dashscope/converter.rs`）使用，勿删。
- 同步改动的文档/注释：`docs/MIGRATION_GAP_TRACKER.md`、`acp/config.rs::DEFAULT_THOUGHT_LEVEL`。

## 结果 ✅ 已实测通过（2026-09-18）
用户确认「已确认可正常请求」：严格网关（Console Go / zen）接受只带 `reasoning_effort`、
不含 `enable_thinking`/`thinking_budget` 的 `/chat/completions` body，400 已消失。
**不要把这两个 vendor 参数加回 body。**

## 回退预案（未触发，保留备查）
若厂商网关也拒绝 `reasoning_effort`：恢复按模型名分流（仅 o1/o3/o4/gpt-5* 传），
或改为 `additional_body_params` + 配置开关。
