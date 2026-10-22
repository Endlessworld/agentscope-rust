# 多端点（chat/completions · responses · messages）改造方案

> 目标：让 `models.json` 能描述每个模型实际使用的 API 端点协议，
> 使 opencode zen / zen-go 等"同一供应商下不同模型走不同端点"的场景可正常工作，
> 同时兼容其它供应商（Anthropic 原生、Azure OpenAI、Ollama 等）。

> **实施后变更（2026-09-21，`d86a578`）**：本方案落地时把三个 Model 实现放在 `crates/agentscope-core/src/model/`；此后已**整体迁至 `crates/agentscope-extensions-model/src/`**（`openai/mod.rs`、`responses/mod.rs`、`anthropic_async/mod.rs`），core 仅保留 `Model` trait / `ModelRegistry` / provider SPI。下文出现的 `model/openai.rs` / `model/responses.rs` / `model/anthropic.rs` 路径请按此换算；`async-openai` / `async-anthropic` / `reqwest13` 依赖也随之下沉到 `agentscope-extensions-model/Cargo.toml`。

## 1. 背景与问题

当前模型链路只有一条：

```
models.json(provider.base_url + api_key + models[])
  → ModelConfig::resolve()/resolve_model()
  → build_model() 固定 new OpenAiChatModel
  → async-openai chat().create_stream_byot()  →  POST {base_url}/chat/completions
```

`OpenAiChatModel` 硬编码走 `/chat/completions`，无法请求 `/v1/messages`、`/v1/responses`。
而 opencode zen / zen-go 下：

| 端点后缀 | 协议 | 典型模型 |
| --- | --- | --- |
| `/chat/completions` | OpenAI Chat | glm-*、kimi-*、deepseek-*、mimo-*、hy*、longcat-* |
| `/responses` | OpenAI Responses | grok-4.6、gpt-5.6-luna、muse-spark-* |
| `/messages` | Anthropic Messages | minimax-*、qwen3.6/3.7/3.8-* |

用 `chat/completions` 客户端请求另两类端点必然失败（404/400）。

## 2. 关键事实核实（async-openai v0.41.3 实际能力）

必须在动手前澄清，避免按错误假设设计：

- **`/chat/completions`**：已用，`async_openai::types::chat::CreateChatCompletionRequest` + `create_stream_byot`。
- **`/responses`：原生支持**。`async_openai` 0.41.3 提供 `responses` / `response-types` feature，
  `src/responses/responses_.rs` 的 `create` / `create_stream` 路径为 `{api_base}/responses`，
  返回 `ResponseStream`（`ResponseStreamEvent` 事件枚举）。
  → 只需在 `agentscope-core/Cargo.toml` 打开 `responses`、`response-types` feature，无需自研 HTTP。
- **`/messages`：async-openai 不支持**，但**可用现成 SDK `async-anthropic` 0.6.0**（MIT，见 §2.1），
  无需自研 reqwest+SSE。
- `OpenAIConfig::url()` 为 `format!("{api_base}{path}")`（`config.rs:219`），
  所以 base_url 只写到 `/v1`，端点后缀由各协议客户端内部拼接。

### 2.1 `async-anthropic` 0.6.0 能力核实（已下载源码确认）

来源：`https://static.crates.io/crates/async-anthropic/async-anthropic-0.6.0.crate`（MIT，2026-08）。

| 需求 | 结论 | 依据 |
| --- | --- | --- |
| 自定义 base_url | ✅ | `Client` 字段 `base_url: String`，`Client::builder().base_url(..)` |
| 自定义 api_key | ✅ | `Client::from_api_key(..)` / builder `api_key` |
| 自定义请求头（x-opencode-session） | ✅ | builder 可注入 `http_client: reqwest::Client`（预置 header） |
| 流式 | ✅ | `messages().create_stream(req) -> CreateMessagesResponseStream` |
| 工具调用 | ✅ | `CreateMessagesRequest.tools: Option<Vec<serde_json::Map>>`、`tool_choice`、
  `MessageContent::{ToolUse, ToolResult}`、`Message::tool_uses()` |
| 请求路径 | ✅ 匹配 | `messages.rs` 硬编码 `/v1/messages`；`format_url` = `{base_url}/{path}` |

关键源码（`src/client.rs`）：

```rust
const BASE_URL: &str = "https://api.anthropic.com";

#[derive(Clone, Debug, Builder)]
#[builder(setter(into, strip_option))]
pub struct Client {
    http_client: reqwest::Client,   // 可注入带自定义 header 的 client
    base_url: String,
    api_key: secrecy::SecretString,
    version: String,                // 默认 "2023-06-01"
    beta: Option<String>,
    backoff: ExponentialBackoff,
}

fn format_url(&self, path: &str) -> String {
    format!("{}/{}", self.base_url.trim_end_matches('/'), path.trim_start_matches('/'))
}
```

→ 所以 base_url 仍只写到 `/v1`（如 `https://opencode.ai/zen/go/v1`），SDK 内部拼 `/v1/messages`，
与 §3.3 的 base_url 约定一致。

**注意事项**：
- `async-anthropic` 依赖 `reqwest 0.12`；本 workspace 也是 `reqwest 0.12`（`rustls-tls`），
  但 `Cargo.lock` 里 async-openai 0.41 已拉入 `reqwest 0.13`。两个 reqwest 大版本会**共存**（各自独立编译），
  可接受，但需在 `cargo build` 时确认无 trait/类型冲突。
- 默认 feature `rustls`；与 workspace 的 rustls 策略一致，直接 `default-features = false, features = ["rustls"]`。
- `beta` 头可选（如需要 `anthropic-beta` 特性时用 builder 设置）。

## 3. 配置结构改造（回答"是否该改 models 结构"）

### 3.1 结论

**直接按协议分组，不做数组/对象双格式兼容**（已确认）。

- `models` 字段**统一**为对象：`{ "completions": [...], "messages": [...], "responses": [...] }`。
- 三个 key 均 `#[serde(default)]`，缺省组可省略；**不做** `untagged` 兼容旧数组。
- 代码里用枚举 `ApiProtocol` 承载三个 key，而不是散落的字符串。
- 旧配置（数组写法）将解析失败——需一次性迁移（本仓库 `docs/models.json` 与用户 `~/.agentscope/models.json` 同步更新）。

不建议改成 `[{"id":..., "endpoint":...}]` 的逐模型对象数组——
当前所有模型端点都等于标准后缀（`/chat/completions`、`/messages`、`/responses`），
逐模型对象只带来冗余，属于 YAGNI；留作后续可选扩展即可。

### 3.2 目标结构

```jsonc
{
  "default_provider": "Go",
  "default_model": "deepseek-v4.1-flash",
  "providers": {
    "Zen": {
      "base_url": "https://opencode.ai/zen/v1",
      "api_key": "public",
      "models": { "completions": [ "mimo-v2.5-free", "..." ] }
    },
    "Go": {
      "base_url": "https://opencode.ai/zen/go/v1",
      "api_key": "sk-",
      "models": {
        "completions": [ "glm-5.3", "glm-5.2", "glm-5.1", "glm-5.3-flash",
                         "kimi-k3", "kimi-k2.7-code", "kimi-k2.6", "longcat-2.0",
                         "deepseek-v4-pro", "deepseek-v4-flash", "deepseek-v4.1-flash",
                         "deepseek-v4-flash-vision-exp", "mimo-v2.5", "mimo-v2.5-pro",
                         "hy4-preview", "hy3" ],
        "messages":    [ "minimax-m3", "minimax-m2.7", "minimax-m2.5",
                         "qwen3.8-max", "qwen3.8-flash", "qwen3.7-max",
                         "qwen3.7-plus", "qwen3.6-plus" ],
        "responses":   [ "grok-4.6", "gpt-5.6-luna",
                         "muse-spark-1.3-contributor", "muse-spark-1.2-contributor" ]
      }
    }
  }
}
```

要点：
- 三个 key 与端点后缀一一对应，直观、易手写；缺省组省略（`#[serde(default)]`）。
- base_url 仍只到 `/v1`，后缀由协议客户端决定，与现有约定一致。
- 某模型若同时支持多协议，可在多个组重复列出（下拉列表按 id 去重即可）。
- 其它供应商：Anthropic 原生 → 全部放 `messages`；纯 OpenAI 网关 → 全部放 `completions`。

### 3.3 为什么不做成 base_url 里带端点后缀

有人会想直接把 `base_url` 写成 `.../v1/responses`。**不推荐**：
供应商切换、子 agent 覆盖、`OPENAI_BASE_URL` 环境变量覆盖都基于 base_url，
把协议塞进 URL 会让"换供应商/换模型"逻辑耦合且易错。协议是独立维度，应独立表达。

## 4. 代码改造方案

### 4.1 agentscope-core：引入协议维度

**(a) `model/provider.rs` 新增协议枚举并挂到 `ModelCreationContext`**

```rust
/// API 端点协议。决定使用哪个 Model 实现与 URL 后缀。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApiProtocol {
    #[default]
    ChatCompletions, // POST {base}/chat/completions
    Responses,       // POST {base}/responses
    Messages,        // POST {base}/messages  (Anthropic)
}
```

在 `ModelCreationContext` 增加 `pub protocol: ApiProtocol`（`Default` 即 `ChatCompletions`）
并提供 builder 方法 `protocol()`。（已存在的 `endpoint_path` 保留，仅作为高级覆盖，
日常不需要手填路径。）

**(b) 依赖**

`agentscope-core/Cargo.toml`：

```toml
async-openai = { version = "0.41", default-features = false,
  features = ["chat-completion", "chat-completion-types", "byot",
              "responses", "response-types", "middleware", "rustls"] }
async-anthropic = { version = "0.6", default-features = false, features = ["rustls"] }
```

`async-anthropic` 依赖 `reqwest 0.12`（与 workspace 一致）；async-openai 0.41 用 `reqwest 0.13`，
两者共存（各自独立编译）——首轮 `cargo build` 需确认无冲突。

**(c) 三个 Model 实现**

| 协议 | 文件 | 实现基础 | 请求 | 事件映射 |
| --- | --- | --- | --- | --- |
| ChatCompletions | `extensions-model/src/openai/mod.rs`（原 `core/src/model/openai.rs`） | async-openai chat | `/chat/completions` | 现有 `StreamIncrementer` |
| Responses | `extensions-model/src/responses/mod.rs`（原 `core/src/model/responses.rs`） | async-openai `responses` feature | `/responses` | `ResponseStreamEvent` → `ChatResponse` |
| Messages | `extensions-model/src/anthropic_async/mod.rs`（原 `core/src/model/anthropic.rs`） | **`async-anthropic` 0.6.0** | `/v1/messages` | `MessagesStreamEvent` → `ChatResponse` |

三者都实现同一个 `Model` trait，`stream()` 签名不变，所以对上层（ReActAgent、ACP）**完全透明**。

**Responses 映射要点**：
- 入参：`Msg` 列表 → `CreateResponse.input`（system 消息进 `instructions`），
  `ToolSchema` → `CreateResponse.tools`（function 类型），`stream: true`，
  `temperature/top_p/max_output_tokens` 按选项填。
- 出参：`response.output_text.delta` → `TextBlock`；
  `response.reasoning_summary_text.delta`（若有）→ `ThinkingBlock`；
  `response.function_call_arguments.delta` / `output_item.added`（function_call）→ `ToolUseBlock`；
  `response.completed` → 取 usage 收尾。

**Messages（Anthropic）映射要点**（基于 `async-anthropic` 0.6.0，**含工具调用**）：
- 客户端构造：`Client::builder().base_url(base_url).api_key(key).http_client(custom).build()`；
  `custom` 为预置了 `x-opencode-session` 等附加头的 `reqwest::Client`（保留 prompt-cache 语义）。
- 请求体：`CreateMessagesRequestBuilder`：`model`、`max_tokens`（必填，缺省用 `DEFAULT_MAX_TOKENS=2048`）、
  `system`、`messages`、`tools`（`Vec<serde_json::Map>`，由 `ToolSchema` 转 `{name,description,input_schema}`）、
  `tool_choice`、`stream: true`、`temperature`、`top_p`。
- 消息转换：`Msg` → `Message{role, content}`；assistant 的 tool_use → `MessageContent::ToolUse`，
  TOOL 结果 → `MessageContent::ToolResult{tool_use_id, content}`。
- 流式事件（`MessagesStreamEvent`）：`MessageStart`（usage）、`ContentBlockStart`（`tool_use` id/name）、
  `ContentBlockDelta`（`text_delta` / `thinking_delta` / `input_json_delta`）、
  `MessageDelta`（`stop_reason`、output usage）、`MessageStop`。
- `stop_reason`：`tool_use`→`ToolCalls`，`end_turn`→`Stop`，`max_tokens`→`Length`。
- 工具 JSON 分片（`input_json_delta`）需按 index 累加后一次解析，与现有 OpenAI `StreamIncrementer` 同构。

**(d) Provider 注册**

- `OpenAiProvider`（现有，`openai`，仅 ChatCompletions）；
- 新增 `OpenAiResponsesProvider`（id 如 `openai-responses`）；
- 新增 `AnthropicMessagesProvider`（id 如 `anthropic`）。

更省事的做法：在 `openai.rs` 提供统一工厂
`pub fn build_model(ctx: &ModelCreationContext) -> Result<Arc<dyn Model>>`，
内部 `match ctx.protocol` 分派到三个实现；`default_registry()` 一并注册三个 Provider。
harness 侧真正需要的就是这个分派函数。

### 4.2 agentscope-harness：配置解析 + 分派

`src/acp_harness_agent.rs`：

```rust
/// 按端点协议分组的模型列表（新格式，唯一支持格式）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelsSpec {
    #[serde(default)] pub completions: Vec<String>,
    #[serde(default)] pub messages: Vec<String>,
    #[serde(default)] pub responses: Vec<String>,
}
```

- `ModelProviderConfig.models` 由 `Vec<String>` 改为 `ModelsSpec`。
- `ModelsSpec::iter()` 返回 `(model_id, ApiProtocol)` 迭代器（遍历三组）。
- `all_models()`：遍历三组，按 id 去重生成 `ModelChoice`（下拉不受影响）。
- `resolve_model(model_id)` 返回值由 `(base_url, api_key)` 改为
  `ResolvedModel { base_url, api_key, protocol }`。
- `resolve()` 同样返回 protocol。
- `build_model()` 改为返回 `Arc<dyn Model>`，并按 protocol 调用 4.1 的统一工厂
  （不再硬编码 `OpenAiChatModel::from_context`）。
  `model_resolver` 闭包与初始模型共用同一分派逻辑。
- `ModelConfig::default()`：默认配置改成分组写法（默认模型 `deepseek-v4.1-flash` → `completions`）。
- **迁移**：`docs/models.json` 与用户 `~/.agentscope/models.json` 需改为新格式；
  旧数组配置会解析失败（无兼容层，已确认）。

### 4.3 src/acp_agent_cli.rs

`list_models()` 目前把 `models` 当数组读（`.as_array()`），需改为遍历
`completions/messages/responses` 三组并打印协议标签，例如 `- grok-4.6 (responses)`。

## 5. 分阶段实施与验证

| 阶段 | 内容 | 验证 |
| --- | --- | --- |
| P0 | 4.2 配置结构（ModelsSpec 对象）+ resolve 返回 protocol，协议先只实现 ChatCompletions 分派 | 新 models.json 解析；`cargo test -p agentscope-harness` |
| P1 | 4.1(b)(c) Responses：开 feature，新增 `model/responses.rs` 与 Provider | `CODING_MODEL_ID=grok-4.6` 实跑命中 `/responses` |
| P2 | 4.1(c) Messages：引入 `async-anthropic`，新增 `model/anthropic.rs` 与 Provider（含工具调用） | `CODING_MODEL_ID=minimax-m3` 实跑命中 `/messages`，含工具调用 |
| P3 | 4.3 CLI 展示 + 文档/默认配置更新 | `/models` 正确列出三组；`README` 与 `models.json` 样例同步 |

每阶段 `cargo build --workspace` + `cargo clippy --workspace --all-targets` 通过。

### 验证用例（建议写成单测）

1. `models:{"completions":["a","b"]}` → 两条均 `ChatCompletions`。
2. `models:{"messages":["m"],"responses":["r"]}` → 分别解析为 `Messages`/`Responses`。
3. `resolve_model("grok-4.6")` → protocol == `Responses`，base_url == Go 的 base_url。
4. `all_models()` 覆盖三组且无重复。
5. 旧数组写法 → 解析失败（预期，验证无兼容层）。

## 6. 影响面与工作量

- 新增（落地时位于 core，2026-09-21 已迁至 `crates/agentscope-extensions-model/src/`）：`responses/mod.rs`（async-openai）、`anthropic_async/mod.rs`（async-anthropic）；同期 `openai/mod.rs` 亦随迁（接口名不变）。
- 修改：`model/provider.rs`（+枚举/字段）、`model/mod.rs`（导出）、
  `agentscope-extensions-model/Cargo.toml`（+async-openai features、+async-anthropic 依赖；2026-09-21 前为 `agentscope-core/Cargo.toml`）、
  `src/acp_harness_agent.rs`、`src/acp_agent_cli.rs`、`docs/models.json`、`README.md`。
- 风险：
  - `async-anthropic`（reqwest 0.12）与 async-openai 0.41（reqwest 0.13）双版本共存，需编译验证。
  - Anthropic 工具调用流式 JSON 分片（`input_json_delta`）需按 index 累加解析，需重点测。
  - async-openai `responses` feature 与现有 `byot` 组合需确认无冲突（编译验证）。
  - 无兼容层：旧数组 `models` 配置会直接解析失败，需同步迁移用户配置文件。
  - 三元组（base_url/api_key/protocol）需在 `resolve`、`resolve_model`、`build_model` 三处保持一致，
    避免漏传 protocol 退化为 chat/completions。
