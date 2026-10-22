# 模型目录（models.dev）驱动的模型能力与上下文全链路方案

> 状态：**Phase 0–6 已完成；HTTP 解压（zstd/gzip/brotli/deflate + system-proxy）已启用**
> 依据：2026-10-22 对 `https://models.dev/models.json?type=all` 与 `https://models.dev/api.json` 的实测，
> 以及对当前仓库模型调用链的静态复核。
> 关联：`docs/MULTI_ENDPOINT_PLAN.md`、`docs/extensions/MODEL__GAP_TRACKER.md`。

---

## 0. 一页结论

1. 本次 `Prepared model input exceeds budget: estimated=60549, limit=58624` 的根因不是模型或网关，
   而是 **同一条链上有 7 个互不相同的硬编码真值来源**，其中 `deepseek* → 64k` 把输入预算压到了 58,624。
2. `docs/MODEL_CATALOG_PLAN.md` 的目标是让 **模型窗口 / 输出上限 / 能力开关 / 使用场景** 只从
   **一个可缓存的在线目录** 派生，本地配置只做覆盖，内置常量只做最后兜底。
3. 默认窗口统一为 **256,000**（未知模型）；删除所有 `64_000` / `64000`。
4. 目录每天刷新一次；**请求永远读内存快照**，刷新失败继续用旧缓存，绝不阻塞或放大模型调用延迟。
5. **ACP 侧同一份真值实时下发**：模型卡片（窗口/输出/模态/特性）、思考等级、输入类型限制
   全部随所选模型变化，通过 `configOptions` + `_meta` + `ConfigOptionUpdate` 推给客户端，
   不等下一轮 prompt、不等重启。
6. 分 7 个阶段落地；Phase 0 只改常量即可先让线上错误消失，Phase 1 起再引入目录。

---

## 1. 问题：7 个互相独立的真值来源

| # | 来源 | 文件 | 当前值 | 被谁使用 | 问题 |
|---|---|---|---|---|---|
| 1 | 上下文窗口表 | `agentscope-core/src/model/context_windows.rs:22-25` | `deepseek*` = 64,000，未知 = 128,000 | 8 个模型适配器的 `context_window_size()` | 与真实模型严重不符（实测见 §2） |
| 2 | 编译器兜底 | `agentscope-harness/src/agent/context/builder.rs:535` | `64_000` | 窗口未知时的输入预算 | 又一份 64k |
| 3 | ACP 压缩触发 | `agentscope-extensions-acp/src/coding_agent.rs:47` | 256,000 | `CompactionConfig.trigger_tokens` | 与模型窗口无关 |
| 4 | ACP 主链路压缩 | `agentscope-extensions-acp/src/acp_harness_agent.rs:967-979` | 256,000 | 会话压缩 | 小窗口下 **永远晚于预算报错** |
| 5 | 用量上报兜底 | `agentscope-extensions-acp/src/session.rs:1204-1208` | 128,000 | ACP `UsageUpdate.size` | 客户端看到错误窗口 |
| 6 | 请求能力表 | `agentscope-extensions-model/src/common/capabilities.rs:79-134` | `deepseek` → 只接受 `max_tokens` 等 | OpenAI 兼容请求构建 | 纯前缀猜测，无法表达 `temperature=false`、`structured_output` 等 |
| 7 | 名字启发式 | `agentscope-extensions-model/src/openai/mod.rs:99-101` | 名字含 `deepseek` → 回传 `reasoning_content` | reasoning passback | 同类问题，目录里已有明确字段 |

> Phase 0 已修复上表 #1/#2/#5 的 `64k/128k` 常量；表中保留的是修复前的问题描述。
> #3/#4 的 256k 仍待 Phase 5 改为按模型窗口动态计算，#6/#7 待 Phase 3 接入目录。

> ACP 侧另有 10 处独立缺口（固定思考等级、image/audio=false、只取文本块等），
> 单独列在 **§7.0**；它们和上表共享同一个根因：客户端看到的能力不是所选模型的真实能力。

### 1.1 本次错误的完整推导

`ContextPipeline::input_limit()`（`builder.rs:509-542`）：

```
window       = 64,000                     // 来源 #1
output       = min(4096, 64000/8) = 4,096 // reserved_output_tokens 未配置
safety       = max(1, 64000/50)  = 1,280
input_limit  = 64000 - 4096 - 1280 = 58,624
estimated    = 60,549                     // ApproximateEstimator：12 + chars/2.5/条，16 + schema/2.5/个工具
```

压垮它的不是模型本身，而是：

- **压缩从未触发**：`trigger_tokens=256,000` > `input_limit=58,624`，所以在预算报错之前，
  压缩中间件（`on_agent`）根本不会运行（`compaction.rs:602-607`）。
- **可丢弃物料已经丢完**：`builder.rs:257-297` 只允许丢 `required=false && placement != System` 的
  memory / knowledge / 其他；System、工具 schema、历史消息都不能丢。
- **60,549 < 64,000**：请求其实塞得进 64k 上下文，只是塞不进"扣掉输出预留和安全边际"的 58,624。

---

## 2. 在线目录实测（2026-10-22）

### 2.1 两个入口

| 入口 | 大小 | 结构 | 关键内容 |
|---|---|---|---|
| `https://models.dev/models.json?type=all` | 508,801 B | 扁平对象，610 条，key = `provider/model` | 模型基础字段、`limit{context,output}`、modalities、benchmarks |
| `https://models.dev/api.json` | 6,053,148 B | 按 provider 分组，232 providers / 9,142 models | 额外提供 `api`、`env`、`npm`、`doc`、`cost`、`status`、`structured_output`、`reasoning_options`、`interleaved`、`canonical_model_id` |

响应头（models.json）：

```
etag: W/"e5fef5cb5166f55628391b78b44575e6"
cache-control: public, max-age=0, must-revalidate
content-encoding: zstd
server: cloudflare
```

→ **支持 `If-None-Match` 条件请求**，每日刷新可以做 304 短路；`content-encoding: zstd` 由
reqwest 的 `zstd` feature 自动协商并解压。

> 数据只做本地缓存与界面展示，不随二进制分发；若未来要内置离线快照，先确认
> models.dev 的数据许可与归属声明。

### 2.2 `models.json?type=all` 字段

```
id, name, description, family, attachment, reasoning, tool_call, temperature,
knowledge, release_date, last_updated, modalities{input,output}, open_weights,
limit{context,output}, license, weights[], benchmarks[], links[]
```

`type` 字段只出现在非聊天模型上：`image(24) / embedding(8) / reranking(3) / transcription(5) /
video(10) / speech(10) / realtime(3) / decision(8)`；**没有 `type` 即聊天模型**。

### 2.3 `api.json` 的增量字段（推荐作为增强源）

- `provider.api`：该 provider 的真实 base URL → **用于把本地 `base_url` 对齐到目录 provider**。
- `provider.env`：认证环境变量名（不用于读取密钥，只用于提示/匹配）。
- `model.status`：`deprecated` 共 **1,173** 条、`beta` **73**、`alpha` **1** → 选择列表要标注，避免新会话默认选中 deprecated；alpha/beta 可显示标签但允许选择。
- `model.structured_output`：**6,838** 条 → 直接驱动 Native/Prompt 结构化输出。
- `model.reasoning_options`：`toggle(1,479)`、`effort[...]`、`budget_tokens(641)` → 校验合法取值。
  **形状是多态的**：既可能是单个对象（`{"type":"toggle"}`），也可能是对象数组
  （`[{"type":"effort","values":[...]},{"type":"budget_tokens"}]`）；统一归一化为 `Vec`。
  `effort.values` 实测有 `none/medium/xhigh` 等非连续值，必须按目录原值下发，不能假设
  只有 low/medium/high 三档。
- `model.interleaved.field`：**1,395** 条（多为 `reasoning_content`）→ 替代名字启发式。
- `model.cost`：`input/output/cache_read` → 可做成本守卫与免费模型标记。
- `model.canonical_model_id`：把网关别名映射到规范模型。

### 2.4 数据坑（必须在归一化时处理）

| 坑 | 实测 | 处理 |
|---|---|---|
| `limit.context = 0` | 14 条（图像/语音/转录类） | `0` 表示 N/A，不是"未知"，不得参与预算 |
| `limit.output > limit.context` | 如 `grok-4.6` 500k/500k、`minimax/MiniMax-M3` 1M/512k | `output_eff = min(output, context)` |
| 同模型多 provider 上限不同 | `opencode/deepseek-v4.1-flash-free` = **200k/32k**，`deepseek/deepseek-v4.1-flash` = **1M/384k** | **provider 命中优先**，不得被全局条目抬高 |
| 网关私有变体不在目录 | `mimo-v2.5-free`、`muse-spark-1.3-contributor-free` 等 | 归一化 + 默认 256k + 日志 |
| 模型 id 大小写不一致 | `minimax-m3` ↔ `MiniMax-M3` | 大小写不敏感索引 |
| `reasoning_options` 形状多态 | 单对象 `{"type":"toggle"}` 或数组；`effort.values` 可有 `none/medium/xhigh` | 统一为 `Vec`；按原值下发，不硬编码三档 |
| `modalities` / `limit` 缺失 | 部分网关条目没有字段 | 默认 `["text"]`/`0`（未知），不推断多模态 |

### 2.5 本地 32 个模型的命中率实测

以 `docs/models.json` 中的 32 个模型为例（`Go` + `Zen` 两个 provider）：

| 匹配方式 | 命中 | 例子 |
|---|---|---|
| `provider/model` 精确 | 0（本地 provider 是别名 `Go`/`Zen`） | — |
| 裸 id 唯一精确 | **23** | `glm-5.3 → zhipuai/glm-5.3`、`deepseek-v4.1-flash → deepseek/deepseek-v4.1-flash` |
| 归一化（小写、去 `-free`）+ 唯一 | **5** | `mimo-v2.5-free → xiaomi/mimo-v2.5`、`minimax-m3 → minimax/MiniMax-M3` |
| 前缀 + 候选共识 | **2** | `nemotron-3-ultra-free → nvidia/nemotron-3-ultra-550b-a55b` |
| 歧义/缺失 → 默认 256k | **2** | `minimax-m2.5`（只有 `MiniMax-M2.5-highspeed`） |

→ 覆盖率约 **94%**；剩余 6% 落到默认 256k，且**不会错误抬高**。

### 2.6 provider 别名识别实测

只按"模型集合重合度"会把 `Go` 判成 `llmgateway`（25/28 = 0.89），而真正的
`opencode-go` 只有 20/28 = 0.71。加上 `base_url` 主机过滤（`opencode.ai`）后：

```
Go  + host=opencode.ai  → opencode-go (20/28)  ✔
Zen + host=opencode.ai  → opencode        (6/6) ✔
```

**结论：provider 识别必须"主机优先、模型集合消歧"，不能只靠模型名字。**

---

## 3. 目标与非目标

### 目标

1. 删除所有 `64k` 硬编码；未知模型默认 **256k**。
2. 模型窗口、输出上限、特性支持、使用场景全部由 models.dev 目录派生。
3. 目录每日自动刷新，请求只读缓存（内存快照），离线可用。
4. 预算、压缩、驱逐、请求参数、用量上报使用**同一份窗口真值**。
5. 目录不可用/模型缺失/数据异常时，行为可预测且有日志。

### 非目标

- 不改变 wire protocol 的选择逻辑（仍由 `models.json` 的 `completions/messages/responses` 决定）。
- 不引入新的计费/配额系统；`cost` 只用于展示与可选守卫。
- 不在本方案内做 tokenizer 精确化（作为 Phase 5 的可选增强）。
- 不把 API key、请求内容或会话数据发给 models.dev。

---

## 4. 总体架构

```
                    ┌──────────────────────────────┐
 每天 1 次 / 冷启动  │  CatalogRefresher            │
 ──────────────────▶│  GET models.json + api.json  │
                    │  (ETag each) → merge         │
                    │  validate → normalize        │
                    │  atomic write → swap snapshot│
                    └──────────────┬───────────────┘
                                   │ Arc<CatalogSnapshot>
                                   ▼
  models.json ──▶ ModelConfig ──▶ ModelCatalog.resolve(ctx) ──▶ ModelDescriptor
  (provider/model/protocol/override)        │
                                            ├─ context_window / max_output
                                            ├─ capabilities (temperature/tool/structured/...)
                                            ├─ modalities / reasoning options / status
                                            └─ use_cases（派生标签）
                                   │
                                   ▼
  ModelCreationContext ──▶ 各 provider create() ──▶ Model 实例
                                   │                    │
                                   │                    ├─ context_window_size()
                                   │                    ├─ max_output_tokens()
                                   │                    └─ descriptor()
                                   ▼
  ContextPipeline ── input_limit = window - reserved - safety
  Compaction       ── trigger = f(input_limit)
  Request builder  ── 按 descriptor 裁剪/放行参数
  ControlPlane     ── 上报真实窗口与用量
  ACP              ── configOptions（模型卡片/思考等级/输入限制）
                     + ConfigOptionUpdate + UsageUpdate（实时）
                     + prompt 前逐块校验（不支持的输入不发起模型调用）
```

模块放置：**`agentscope-core/src/model/catalog/`**。理由：窗口与 `Model` trait 在 core；
`agentscope-extensions-model` 已依赖 core，反向不成立；core 已有 `reqwest`/`tokio`/`serde`。

```
agentscope-core/src/model/catalog/
  mod.rs        // ModelCatalog 句柄（global/ephemeral）、CatalogEvent、公开 API
  config.rs     // CatalogConfig + 环境变量
  types.rs      // CatalogSnapshot / ModelDescriptor / ModelKind / UseCase / Cost
  resolve.rs    // provider 识别 + 模型匹配 + 置信度
  source.rs     // models.dev 两种 JSON 的解析与合并
  store.rs      // 磁盘缓存、TTL、ETag、原子写
  refresh.rs    // HTTP fetcher、单飞、退避、changed_keys diff
  wrap.rs       // CatalogAwareModel，覆盖窗口/输出/descriptor
```

---

## 5. Catalog 模块设计

### 5.1 数据模型

```rust
pub const DEFAULT_CONTEXT_WINDOW: usize = 256_000;
pub const DEFAULT_RESERVED_OUTPUT: usize = 8_192;
pub const CATALOG_SCHEMA_VERSION: u32 = 1;

pub struct CatalogSnapshot {
    pub schema_version: u32,
    pub generation: u64,                   // 内容变化才 +1；304 不变
    pub sources: Vec<SourceInfo>,          // url + etag + fetched_at + bytes
    pub entries: HashMap<String, Arc<ModelDescriptor>>,      // "provider/model"
    pub by_bare: HashMap<String, Vec<Arc<ModelDescriptor>>>, // 小写裸 id
    pub by_provider: HashMap<String, Vec<Arc<ModelDescriptor>>>,
    pub provider_api: HashMap<String, String>,               // provider -> api host
    pub fetched_at: DateTime<Utc>,
}

/// 廉价句柄（内部 Arc）。生产用 global()，测试用 ephemeral()，
/// 避免把全局单例写死进 ACP / adapter。
pub struct ModelCatalog(Arc<CatalogInner>);

pub struct CatalogEvent {
    pub generation: u64,
    pub changed_keys: Arc<HashSet<String>>, // 新增/删除/字段变化
}

pub struct ModelDescriptor {
    pub key: String,                 // "opencode-go/deepseek-v4.1-flash"
    pub provider: String,
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub family: Option<String>,
    pub kind: ModelKind,             // Chat | Embedding | Image | Speech | ...
    pub context_window: usize,       // 0 = N/A
    pub max_output: usize,           // 已 min(context)
    pub input_modalities: ModalitySet,
    pub output_modalities: ModalitySet,
    pub attachment: bool,
    pub reasoning: bool,
    pub reasoning_options: Vec<ReasoningOption>, // Toggle | Effort(values) | Budget
    pub interleaved_field: Option<String>,       // 如 reasoning_content
    pub tool_call: bool,
    pub structured_output: Option<bool>,
    pub temperature: bool,
    pub status: ModelStatus,         // Active | Beta | Alpha | Deprecated
    pub open_weights: bool,
    pub cost: Option<Cost>,
    pub canonical: Option<String>,
    pub use_cases: UseCaseSet,       // 派生，见 §8.4
    pub fingerprint: u64,            // 关键字段 hash，目录 diff 用
}
```

`ModelCatalog` 句柄可克隆、可注入：`AcpCodingAgent`、`ModelRegistry`、测试 fixture 都持有
同一个句柄；`global()` 只是进程默认实例，不是唯一入口。`fingerprint` 只覆盖会影响窗口/
能力/卡片/请求参数的字段（key、limits、modalities、capabilities、reasoning、cost、status），
用于生成 `CatalogEvent.changed_keys`。

`ModelDescriptor::effective_context()`：`context_window == 0` 时返回 `None`（N/A），
调用方决定兜底；聊天模型缺条目时返回 `DEFAULT_CONTEXT_WINDOW`。

```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CatalogConfidence {
    Catalog,    // provider 精确 / canonical / 裸 id 唯一
    Normalized, // 归一化 / 前缀共识
    Default,    // 未命中，使用 256k
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ModelStatus {
    #[default]
    Active,
    Beta,
    Alpha,
    Deprecated,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningOption {
    Toggle,
    Effort { values: Vec<String> },
    BudgetTokens,
}
```

`reasoning_options` 字段用自定义 `deserialize_with` 兼容“单对象 / 对象数组”，统一成 `Vec`。

### 5.2 解析优先级（从高到低）

| 级别 | 来源 | 说明 |
|---|---|---|
| 1 | 运行时显式 | `options.contextWindowSize` / `with_context_window_size()` / `with_max_output_tokens()` |
| 2 | 本地 `models.json` 覆盖 | provider / model 级 `context_window`、`max_output_tokens`、`capabilities` |
| 3 | 目录 provider 精确 | 本地 provider 已识别 + `provider/model` 命中（**最高置信度**） |
| 4 | 目录裸 id 精确 | 全库唯一命中（`-free` 等变体优先于 canonical） |
| 5 | 目录 canonical | `canonical_model_id` 命中；**只在没有 exact 时使用**，不得覆盖 free 变体 |
| 6 | 归一化唯一 / 前缀共识 | 见 §5.2.2；不得抬高窗口 |
| 7 | 内置兜底 | 已删除 64k；只保留 `DEFAULT_CONTEXT_WINDOW = 256_000` |

#### 5.2.1 provider 识别

```
1) 若本地 provider 配置了 catalog_provider → 直接用（最高置信度）
2) 取本地 base_url 的 host，匹配目录 provider.api 的 host
   （允许路径不同：zen/go/v1 与 inference/go/openai/v1 视为同一 provider）
3) 在 host 匹配集合内，按"本地模型集合 ∩ 目录模型集合"取最大覆盖率，阈值 ≥ 0.5
4) 否则不绑定 provider，逐模型走裸 id 匹配
5) 结果缓存到 catalog.provider_map.v1.json，key = hash(base_url + 排序后的本地模型 id 集合
   + 目录 generation)；base_url/模型集合/目录变化都重建
```

#### 5.2.2 模型匹配

```
normalize(id) = lowercase, 去网关后缀(-free/-preview/-exp/日期), 去已知 provider 前缀
候选集 = 目录全量
1) provider 已识别: 该 provider 内 exact(id)（**不 normalize**）
2) 全库 exact(id)（大小写不敏感，**不 normalize**）
3) provider 已识别: 该 provider 内 exact(normalize(id))
4) 全库 exact(normalize(id))
5) canonical_model_id == id（只在 1-4 都 miss 后）
6) 全库唯一后缀匹配（ends_with("/"+normalize(id))）
7) 前缀匹配：所有候选的 context/output 完全一致才接受（共识规则）
8) 否则 miss → 默认 256k，记录 confidence=low
```

**共识规则**：候选之间窗口不一致时宁可 miss，也不猜。这样 `minimax-m2.5` 不会误用
`MiniMax-M2.5-highspeed` 的窗口。

**路径型模型 id**：模型 id 本身可能含 `/`（OpenRouter 的 `amazon/nova-2-lite-v1`）。
`provider/model_id` 用**第一个** `/` 分割，剩余整体是 model id；不要用 `rsplit('/')`
（`acp_harness_agent.rs:522` 现有实现会取错）。`normalize` 只去掉已知本地 provider 别名
（如 `Go/`），不要把 OpenRouter 自带的 `amazon/` 当 provider 前缀。

**`-free` 不是装饰**：`opencode/deepseek-v4.1-flash-free` 是 200k/32k，付费版是
1M/384k。必须先做 provider 精确 / 裸 id 精确匹配；`-free`/`-preview` 的归一化只在
miss 之后参与，且候选窗口不一致时按共识规则放弃，绝不能把 free 归一到付费版。
若必须去掉 `-free` 才能命中，且候选 `context > 256k`，直接视为 miss（free 变体不应
借用大窗口）；候选 ≤256k 时可接受并标记 `Normalized`。

#### 5.2.3 解析结果

```rust
pub struct Resolution {
    pub descriptor: Option<Arc<ModelDescriptor>>,
    pub kind: MatchKind,          // ExactProvider | HostId | BareUnique | Canonical | Normalized | PrefixConsensus | Default
    pub confidence: f32,          // 1.0 / 0.9 / 0.8 / 0.6 / 0.4 / 0.0
    pub provider: Option<String>,
}
```

每次解析输出一行 debug 日志（model、命中 key、置信度、窗口、来源），便于排障。

`Resolution.confidence` 是内部排序分数；对外（ACP 卡片/manifest）用 `CatalogConfidence`。
映射：`≥0.9 → Catalog`、`0.5..0.9 → Normalized`、`<0.5 / None → Default`。

### 5.3 缓存与每日刷新

磁盘布局（默认 `$HOME/.agentscope/model-catalog/`，Windows 为
`%USERPROFILE%\.agentscope\model-catalog\`）：

```
sources/models.raw.json      // models.json?type=all 原始响应
sources/models.meta.json     // url, etag, last_modified, fetched_at, checked_at, bytes, sha256, schema_version
sources/api.raw.json         // api.json 原始响应（增强源）
sources/api.meta.json        // 同上
catalog.v1.json              // 归一化快照（只保留需要的字段，紧凑 JSON）
catalog.provider_map.v1.json // 本地 provider 别名 -> 目录 provider
```

刷新策略：

| 场景 | 行为 |
|---|---|
| 冷启动、无缓存 | 同步读取失败 → 立即用默认 256k 启动；后台做**一次** 15s 上限的拉取 |
| 有缓存、未过期 | 直接使用，不联网 |
| 有缓存、已过期 | **先服务旧快照**，后台单飞刷新 |
| ETag 命中 304 | 只更新 `checked_at`，不重写数据 |
| 200 | 校验 → 归一化 → `tmp + rename` 原子替换 → 交换 `Arc` |
| 网络失败 | 保留旧快照；退避 1h→2h→4h…上限 24h；不影响请求 |
| 目录损坏/字段非法 | 整份丢弃该源；双源时保留可用的那一份；回退旧快照 |

单飞与并发：

- 进程内用 `AtomicBool` + `tokio::sync::Mutex` 保证同一时刻只有一个刷新任务。
- 多进程用"原子 rename + 后写覆盖"（内容相同，无害）；不引入文件锁依赖。
- 读路径只做 `Arc` 克隆，不持锁、不 await。

TTL：

```
default_ttl = 24h + jitter(0..30min)   // 避免多实例同时打 models.dev
```

刷新触发：

1. 进程启动时检查 `fetched_at`；
2. `tokio::time::interval` 后台循环（有 runtime 才启动，`Handle::try_current()`）；
3. 解析 miss 时的按需刷新（去抖：同一进程 1 小时内最多 1 次）。

配置：

```bash
AGENTSCOPE_MODEL_CATALOG=auto|off|refresh   # 默认 auto
AGENTSCOPE_MODEL_CATALOG_URL=https://models.dev/models.json?type=all
AGENTSCOPE_MODEL_CATALOG_ENRICH_URL=https://models.dev/api.json   # 可选，强烈建议
AGENTSCOPE_MODEL_CATALOG_TTL=24h
AGENTSCOPE_MODEL_CATALOG_TIMEOUT=10s
AGENTSCOPE_MODEL_CATALOG_DIR=~/.agentscope/model-catalog
```

> 建议默认同时拉取 `api.json` 作为增强源：只有它提供 `provider.api`（主机匹配）、`cost`、
> `structured_output`、`status`、`reasoning_options`、`interleaved`。`models.json?type=all`
> 作为基础源保留，两个源任一可用即可工作。

#### 5.3.1 HTTP 与跨平台细节

**压缩**：workspace `reqwest 0.13` 已启用
`["json", "stream", "rustls", "gzip", "brotli", "deflate", "zstd", "system-proxy"]`。
models.dev 实测返回 `content-encoding: zstd`，现在由 reqwest 自动协商并解压。
`fetches_real_models_dev_payload` 会在 `--ignored` 模式下做真实压缩请求验证。
若解压失败，只丢弃该源并保留旧快照，不能把乱码写进缓存。

**ETag / 缓存头**：

- models.dev 返回弱 ETag（`W/"..."`）。`If-None-Match` 必须原样发送，不要剥掉 `W/`。
- `cache-control: public, max-age=0, must-revalidate` 意味着 HTTP 缓存不会帮你省请求；
  24h TTL 完全由应用层控制。
- 两个源各自保存 `etag`/`last_modified`，各自处理 304；一个 304 不影响另一个 200。
- 304 只更新 `checked_at`，不增加 `generation`，不重写 `catalog.v1.json`。

**请求参数**：

| 项 | 值 |
|---|---|
| 方法 | `GET`，匿名，无 Cookie |
| UA | `agentscope/<version>` |
| 连接超时 | 3s |
| 总超时 | 15s（`api.json` 6 MB；不要 30s 阻塞退出） |
| 响应上限 | `models.json` 1 MiB；`api.json` 8 MiB |
| Content-Type | 必须是 JSON；HTML（ captive portal）直接判失败 |
| 重定向 | 最多 3 次 |
| 代理 | 启用 `system-proxy` 读取 `HTTPS_PROXY`/`HTTP_PROXY`；企业 MITM 可用 `off` |

**路径与原子写**：

- 解析顺序：`AGENTSCOPE_MODEL_CATALOG_DIR` → `AGENTSCOPE_HOME/model-catalog` →
  `$HOME`（Unix）/ `%USERPROFILE%`（Windows）下的 `.agentscope/model-catalog` →
  系统临时目录。环境变量里的 `~` 需要手动展开（shell 不会替你展开）。
- 写临时文件 → `fsync` → `rename` 覆盖；Unix 再 `fsync` 父目录。
- Windows 上杀毒软件/索引器可能短暂锁住目标文件：`rename` 失败重试 3 次（间隔 50ms），
  仍失败则保留旧快照并记录 warning。
- 缓存文件权限：Unix `0600`；Windows 依赖用户目录 ACL。缓存里不含密钥。

**时钟**：

- 跨进程用 `SystemTime` 持久化 `fetched_at`；进程内调度用 `Instant` 防系统时间跳变。
- `fetched_at` 比当前时间超前 >5min 视为异常，按“已过期”处理。
- TTL jitter 用进程启动时的随机数，避免整点惊群。

#### 5.3.2 启动与内存预算

目标：**请求热路径不解析大 JSON、不联网、不等待**。

| 阶段 | 预算 | 做法 |
|---|---|---|
| 读 `catalog.v1.json` | ≤50ms / ≤8 MiB | 紧凑快照，启动时同步读 |
| 无快照，读 `models.raw.json` | ≤20ms / 500 KiB | 先提供基础窗口/模态 |
| 解析 `api.raw.json` | 后台 `spawn_blocking` | 6 MB / 9k 模型，不阻塞首个请求 |
| 单次 resolve | ≤1ms（缓存命中） | 见 §5.6 |
| 刷新下载 | 后台，15s 上限 | 失败保留旧快照 |

- `api.json` 解析后立刻裁剪为 `CompactModel`（只留 provider/model/limit/modalities/
  capabilities/reasoning/cost/status），丢弃 `benchmarks`、`links`、`weights` 等大字段。
- 冷启动无缓存：默认 256k 直接服务，后台拉取；需要“首次就准”的 CLI 可设
  `AGENTSCOPE_MODEL_CATALOG_COLD_BLOCK_MS=1000`（默认 0，绝不默认阻塞）。
- 解析任务用 `spawn_blocking`，避免在 async worker 上做 6 MB `serde_json`。
- `catalog.v1.json` 带 `schema_version` + `generation`；版本不匹配则从 raw 重建，
  raw 也不可用时忽略缓存。
- 内存目标：常驻 ≤20 MiB（9k 模型 + provider map + resolve cache）。

### 5.4 本地配置扩展（向后兼容）

现有 `~/.agentscope/models.json` 不变；新增可选字段：

```jsonc
{
  "providers": {
    "Go": {
      "base_url": "https://opencode.ai/zen/go/v1",
      "api_key": "sk-...",
      "catalog_provider": "opencode-go",     // 可选：跳过自动识别
      "models": { "completions": ["deepseek-v4.1-flash"] },
      "model_overrides": {                    // 可选：按模型覆盖
        "deepseek-v4.1-flash": {
          "context_window": 1000000,
          "max_output_tokens": 384000,
          "capabilities": { "image_input": true, "structured_output": true }
        }
      }
    }
  }
}
```

覆盖字段永远优先于目录；覆盖非法（0、负数、output > context）时拒绝加载并告警。

可选环境变量（全部有默认值）：

| 变量 | 默认 | 说明 |
|---|---|---|
| `AGENTSCOPE_MODEL_CATALOG` | `on` | `off` 完全禁用目录，行为等同 Phase 0 |
| `AGENTSCOPE_MODEL_CATALOG_DIR` | `$HOME/.agentscope/model-catalog` | 缓存目录 |
| `AGENTSCOPE_MODEL_CATALOG_URL` | `https://models.dev/models.json?type=all` | 基础源（可换镜像） |
| `AGENTSCOPE_MODEL_CATALOG_API_URL` | `https://models.dev/api.json` | 增强源 |
| `AGENTSCOPE_MODEL_CATALOG_API` | `on` | `off` 只读基础源，省 6 MB |
| `AGENTSCOPE_MODEL_CATALOG_TTL_HOURS` | `24` | 应用层 TTL |
| `AGENTSCOPE_MODEL_CATALOG_COLD_BLOCK_MS` | `0` | 冷启动最多阻塞多久；默认不阻塞 |
| `AGENTSCOPE_ACP_MODEL_FLAT` | `off` | ACP 模型不分组 |
| `AGENTSCOPE_ACP_MODEL_META` | `on` | ACP `_meta` 开关（description 仍保留） |

### 5.5 失败、降级与安全

| 风险 | 处理 |
|---|---|
| 目录返回 HTML/超大响应 | `Content-Type: application/json` 校验 + 8 MiB 上限 + 15s 总超时 |
| 数值异常 | `context ∈ [1_024, 10_000_000]`、`output ∈ [0, context]`，越界丢弃该条 |
| 恶意/错误 provider 匹配 | 只在 host 匹配集合内做覆盖度消歧；置信度 < 0.5 不采用 |
| 密钥泄漏 | 只发匿名 GET，不转发任何本地 header/key |
| 离线/内网 | `AGENTSCOPE_MODEL_CATALOG=off` 或缓存可用；始终不阻塞主流程 |
| 429 / 5xx | 指数退避（1m/5m/15m，上限 1h），尊重 `Retry-After`；不立即重试 |
| 目录升级 | 归一化快照带 `schema_version`；不一致时从 raw 重建，raw 不可用则忽略缓存 |
| zstd 解压失败 | 丢弃该源，保留旧快照；日志提示检查 `reqwest` 解压特性 |
| Windows 文件被占用 | `rename` 重试 3 次；仍失败保留旧快照，不影响请求 |
| 6 MB `api.json` 解析 | `spawn_blocking` + 紧凑裁剪；不阻塞 async worker / 首个请求 |
| 系统时钟跳变 | `fetched_at` 超前 >5min 视为过期；进程内用 `Instant` |
| 代理 / 企业 TLS | 支持 `HTTPS_PROXY`；MITM 环境用 `AGENTSCOPE_MODEL_CATALOG=off` |

### 5.6 解析缓存与观测

- `resolve` 结果缓存：`RwLock<HashMap<(u64, String, String), Arc<ModelDescriptor>>>`，
  key = `(generation, provider_key, model_id)`；generation 变化时整体清空；上限 1024，
  超出直接清空（目录刷新频率低，不需要 LRU 依赖）。
- `provider_map` 只在快照交换时重建，不在请求路径做 provider 消歧。
- 日志（每个模型每个 generation 只打一次）：
  - `[model-catalog] provider=Go -> opencode-go confidence=0.93 host=opencode.ai`
  - `[model-catalog] model=deepseek-v4.1-flash -> opencode-go/... ctx=1000000 out=384000 source=catalog`
  - `[model-catalog] model=minimax-m2.5 -> default ctx=256000 reason=ambiguous`
- `ContextManifest` 增加 `catalogGeneration`，便于把一次请求与目录版本对上。
- 可选计数（tracing field）：`catalog.resolve.hit/miss/fallback`、
  `catalog.refresh.304/200/error`、`catalog.push.sent/failed`。

---

## 6. 全链路改造清单

### 6.1 常量与窗口

| 文件 | 现状 | 改法 |
|---|---|---|
| `agentscope-core/src/model/context_windows.rs` | deepseek 64k；默认 128k | 删除 64k 条目；`DEFAULT_CONTEXT_WINDOW = 256_000`；函数改为"目录优先 + 表兜底" |
| `agentscope-harness/src/agent/context/builder.rs:535` | 兜底 64000 | 改用 `DEFAULT_CONTEXT_WINDOW` |
| `agentscope-extensions-acp/src/session.rs:1204` | 兜底 128000 | 改用 `DEFAULT_CONTEXT_WINDOW` |
| `agentscope-extensions-acp/src/coding_agent.rs:47` | 256k 常量 | 删除；改用动态触发（§8.2） |
| `agentscope-extensions-acp/src/acp_harness_agent.rs:967` | 固定 256k | 改为"按模型窗口动态" |

### 6.2 模型构造与能力

| 文件 | 改法 |
|---|---|
| `agentscope-core/src/model/model.rs` | `Model` 增加 `max_output_tokens() -> usize`（默认 0）、`descriptor() -> Option<&ModelDescriptor>`（默认 None），保持向后兼容 |
| `agentscope-core/src/model/provider.rs` | `ModelCreationContext` 增加 `catalog: Option<Arc<ModelDescriptor>>`；`resolve_with` 创建前解析一次 |
| `openai/mod.rs`、`responses/mod.rs`、`anthropic{, _async}/mod.rs`、`gemini/`、`ollama/`、`dashscope/`、`openai_official/` | `create()/from_context()` 解析目录；实例保存 descriptor；`context_window_size()`/`max_output_tokens()` 返回目录值 |
| `common/capabilities.rs` | `resolve_capabilities(model)` → `resolve_capabilities_with(model, Option<&ModelDescriptor>)`；目录字段优先，前缀表仅兜底 |
| `openai/mod.rs:99` | `requires_reasoning_passback` 改为读 `descriptor.interleaved_field.is_some()`，名字启发式仅兜底 |
| `ModelCapabilities`（core，窗口/结构化） | 与 `common::ModelCapabilities`（请求参数）命名冲突 → 前者保留，后者改名 `RequestCapabilities`（或直接并入 `ModelDescriptor`） |
| `acp_harness_agent.rs:522` | `rsplit('/')` 会把 `Provider/amazon/nova...` 的 id 截成 `nova...`；改为 `split_once('/')`，剩余整体作为 model id |

### 6.3 请求构建

| 行为 | 目录字段 | 落点 |
|---|---|---|
| 丢弃 `temperature/top_p` | `temperature == false` | `openai/mod.rs:445-467`、`responses`、`dashscope`、`ollama` |
| `max_tokens` vs `max_completion_tokens` | 目录无此字段 → 保留现有前缀表兜底 | `openai/mod.rs:461-468` |
| `max_tokens` 夹紧 | `limit.output` | 所有 adapter |
| 原生结构化输出 | `structured_output` | `react_agent.rs:801-803`、各 adapter |
| 工具调用 | `tool_call == false` | 注册工具前告警/禁用原生工具 |
| reasoning effort 校验 | `reasoning_options` | `openai/mod.rs:489`、`responses` |
| 图片输入 | `modalities.input` 含 `image` + `attachment` | **Chat Completions 目前会静默丢弃图片**（`openai/mod.rs:523-527`），需补 `image_url` part：`Source::Base64` → `data:{mime};base64,{data}`，`Source::Url` → 原 URL |
| PDF/音频/视频 | `modalities.input` | Gemini/DashScope 已有能力，按 descriptor 放行 |
| 非聊天模型 | `type ∈ {embedding,image,...}` | 不进入 Chat 模型选择，不构建 chat 请求 |

### 6.4 上下文预算 / 压缩 / 驱逐

| 行为 | 现状 | 改法 |
|---|---|---|
| 输入预算 | `window - min(4096,window/8) - window/50` | `window - reserved_output - safety`，`reserved_output` 由 `max_output` 夹紧（§8.1） |
| 预估器 | 两个独立实现，均为 `chars/2.5` | 统一到 `catalog::estimate_input()`；编译器和压缩共用 |
| 压缩触发 | 固定 256k | `trigger = 0.9 × input_limit`（§8.2） |
| 压缩 keep | 固定 12 条 / 2k~8k | 由 `trigger` 派生，限制在 `[2k, 32k]` |
| 摘要调用预算 | 超限直接失败 | `Purpose::Summary` 时按最旧优先截断到 `0.8 × input_limit`，保证压缩本身不会失败 |
| 工具结果驱逐 | 固定 80k 字符 | `min(80_000, window × 0.1 tokens × 2.5)`，仍可配置覆盖 |
| 模型切换 | 压缩配置在 build 时固定 | 触发/keep 在运行时按当前 model 计算 |
| 用量上报 | 128k 兜底 | `DEFAULT_CONTEXT_WINDOW`；`used` 优先用最近一次 `input_tokens` |
| 预算错误 | 只报 `estimated`/`limit` | 附 `model`、`catalogKey`、`window`、`reserved_output`、`safety`，明确是本地预算而非网关错误 |
| 多模态估算 | 只按文本 `chars/2.5` | 图片/音频按固定 token 估算（如 1k/图）或 provider 预算，避免低估导致 400 |
| 压缩递归 | 摘要调用也走预算 | `Purpose::Summary` 标记，禁止摘要调用再次触发压缩 |
| 压缩失败 | 直接失败 | 最旧优先截断 + 工具结果驱逐；仍失败返回可读错误，不重试到超时 |
| 摘要模型 | 重新选模型 | 用当前会话模型；模型不可用时回退纯截断 |
| 模型切换失败 | 只发 advisory，配置已改 | 回滚 `config.model` 到 `applied_model`，并推 `ConfigOptionUpdate` 纠正客户端 |

### 6.5 可观测与协议

- `ContextManifest` 增加：`catalogKey`、`catalogSource`、`catalogConfidence`、`contextWindow`、
  `maxOutputTokens`、`reservedOutputTokens`。
- 启动日志：`[model-catalog] source=... age=...h entries=... providers=...`。
- 每次解析：`[model-catalog] model=... -> key=... confidence=... ctx=... out=...`（debug）。
- ACP：按 §7 实时下发模型卡片、思考等级、输入限制；`UsageUpdate.size` 用真实窗口；
  目录刷新后主动推送，不等下一轮 prompt。
- ControlPlane：`ContextSnapshot.max_tokens` 用真实窗口（`agent_scope_adapter.rs:300-312`）。
- 可选：`cost.free == true` 的模型在选择列表打 `free` 标签。

---

## 7. ACP 模块：按所选模型实时下发能力

> 目标：`session/new` / `session/load` / `session/resume` / `session/set_config_option`
> 以及目录刷新时，客户端拿到的 **模型列表、思考等级、输入类型限制** 必须来自
> 当前所选模型的真实目录配置，而不是固定常量、前缀猜测或静态能力位。

### 7.0 现状审计

| # | 能力 | 现状 | 文件 | 后果 |
|---|---|---|---|---|
| A1 | 模型列表 | `ModelChoice { id, name }`，name 仅 `provider/model` | `config.rs:28-33`、`acp_harness_agent.rs:582-613` | 客户端看不到上下文/输出/模态，无法做本地预算或附件门控 |
| A2 | 模型选项 | 只构造 `value + name` | `config.rs:78-89` | 没有 `description` / `_meta` / `category` |
| A3 | 思考等级 | 固定 `auto/disabled/low/medium/high/max`，对所有模型 | `config.rs:52-75` | 非推理模型也显示；不支持的等级会 400；`max→high`、`disabled→minimal` 有损 |
| A4 | 输入能力 | 全局 `image(false)` / `audio(false)` | `coding_agent.rs:193-198` | 视觉模型也无法从客户端收到图片 |
| A5 | 输入转换 | 只提取 `ContentBlock::Text`，图片/音频/资源被静默丢弃 | `session.rs:916-926` | 用户发图后模型看不到，且无提示 |
| A6 | 切换时机 | `set_model` 只改配置，下一轮 prompt 才换模型 | `session.rs:524-583` | 切换后客户端立即看不到新窗口/新等级 |
| A7 | 用量窗口 | `UsageUpdate.size` 取 live model，兜底 128k | `session.rs:1199-1212` | 切换模型后窗口滞后；未知模型 128k |
| A8 | 跨 provider | `all_models` 按裸 id 去重 | `acp_harness_agent.rs:582-613` | 同名模型跨 provider 时下拉项丢失/解析不稳定 |
| A9 | 推送内容 | 配置变更只重发原选项（内容未变） | `coding_agent.rs:466-477` | 客户端收到"更新"但没有任何新信息 |
| A10 | 目录刷新 | 无广播 | — | 每日刷新后活跃会话仍用旧窗口，直到重启 |
| A11 | 初始选中 | `SessionConfig::default().model = "default"`，不在 `models` 列表 | `session.rs:33-48`、`acp_harness_agent.rs:1091-1106` | 客户端初始无选中模型；思考等级也无法按真实模型派生 |
| A12 | 会话恢复 | 已用 sidecar 持久化 `{model, mode, thought_level}` | `session.rs`、`journal` | 重启后可恢复；旧 sidecar 非法值回退默认 |

> A12 已实现：sidecar 位于 `<cwd>/.agentscope/acp-sessions/<session>.json`；load 时校验
> model/mode/thought_level，缺失或非法值回退默认。

### 7.1 ACP 协议可用的下发面（schema 1.9.1）

| 通道 | 位置 | 用途 |
|---|---|---|
| `NewSessionResponse.configOptions` / `LoadSessionResponse` / `ResumeSessionResponse` | `v1/agent.rs:878 / 1055 / 1225 / 1389` | 初始全量模型卡片 + 思考等级 |
| `SessionConfigSelectOption.description` | `v1/agent.rs:2120` | **所有客户端都能显示**的一行摘要 |
| `SessionConfigSelectOption.meta` | 同上（`Meta = Map<String, Value>`，`v1/ext.rs:14`） | 机器可读的完整模型卡片 |
| `SessionConfigOption.description/category/meta` | `v1/agent.rs:2346` | 选项级说明 + `Model`/`Mode`/`ThoughtLevel` 分类 |
| `SessionUpdate::ConfigOptionUpdate` | `v1/client.rs:497` | 变更后实时推送（无需等下一轮） |
| `SessionUpdate::UsageUpdate` | `v1/agent.rs` | `size` = 当前模型真实窗口 |
| `AgentCapabilities.promptCapabilities` | `v1/agent.rs:4465` | 连接级输入能力（静态；只能取并集） |
| `SessionUpdate::AgentMessageChunk` | — | 不支持的输入/等级的可读错误 |
| `SessionConfigOptionsCapabilities.boolean` | `v1/client.rs:2403` | 布尔扩展能力（本方案不使用） |

**协议约束**：ACP 没有"模型信息查询"方法，也没有 per-model capabilities 推送；
`configOptions` 是唯一标准通道，`_meta` 是唯一结构化扩展通道。因此模型卡片必须随
`configOptions` 一起下发，`_meta` key 必须命名空间化（`agentscope.dev/model`）；
`promptCapabilities` 是连接级静态字段，不能随模型变化，per-model 限制只能靠
`_meta` + prompt 校验。

### 7.2 模型卡片（ModelChoice → SelectOption）

```rust
pub struct ModelChoice {
    pub id: String,                  // 下拉值：唯一；冲突时用 "Provider/model"
    pub name: String,                // 展示：裸模型 id
    pub provider: String,            // 分组：本地 provider 别名
    pub catalog_key: Option<String>, // 解析结果；None 时按 id 现查
    // 仅用于目录 provider 匹配，不序列化、不进 _meta
    #[serde(skip)]
    pub(crate) base_url: Option<String>,
}
// ModelCard 不冻结在 ModelChoice 里：config_options() 每次用
// ModelCatalog::global() + input_limit() 现算，24h 刷新后无需重启。
// base_url 只用于目录 provider 匹配；序列化、日志、_meta 都不包含它。
// 冷启动目录未就绪时，刷新后仍可据此补全 provider 映射。
pub struct ModelCard {
    pub schema: u32,                 // 1
    pub catalog_key: Option<String>,
    pub confidence: CatalogConfidence, // catalog | normalized | default
    pub context_window: usize,
    pub max_input_tokens: usize,   // 与编译器同一个 input_limit 公式
    pub max_output_tokens: usize,
    pub input_modalities: Vec<String>,
    pub output_modalities: Vec<String>,
    pub attachment: bool,
    pub tool_call: bool,
    pub reasoning: bool,
    pub structured_output: Option<bool>, // api.json 才有；None = 未知
    pub temperature: bool,
    pub reasoning_levels: Vec<ThoughtLevel>,
    pub use_cases: Vec<String>,
    pub status: ModelStatus,
    pub cost: Option<Cost>,
}
```

构造 `SessionConfigSelectOption`：

```rust
let description = format!(
    "{} ctx · {} out · {}{}{}{}",
    human_tokens(card.context_window),
    human_tokens(card.max_output_tokens),
    card.input_modalities.join("+"),
    if card.tool_call { " · tools" } else { "" },
    if card.reasoning { " · reasoning" } else { "" },
    status_label(card.status),
);
let meta = json!({ "agentscope.dev/model": card })
    .as_object().cloned().unwrap_or_default();
SessionConfigSelectOption::new(choice.id, choice.name)
    .description(description)
    .meta(meta)
```

`_meta` 负载（机器可读）：

```json
{"agentscope.dev/model": {
  "schema": 1,
  "catalogKey": "opencode-go/deepseek-v4.1-flash",
  "confidence": "catalog",
  "provider": "opencode-go",
  "model": "deepseek-v4.1-flash",
  "contextWindow": 1000000,
  "maxInputTokens": 975808,
  "maxOutputTokens": 384000,
  "inputModalities": ["text", "image"],
  "outputModalities": ["text"],
  "attachment": true,
  "capabilities": {
    "toolCall": true, "reasoning": true, "temperature": true,
    "structuredOutput": true, "interleavedReasoning": "reasoning_content"
  },
  "reasoningLevels": ["auto", "low", "high", "max"],
  "useCases": ["coding", "agentic", "long-context"],
  "status": "active",
  "cost": {"input": 0.3, "output": 1.2, "cacheRead": 0.006}
}}
```

`maxInputTokens` 是**默认输入预算**（按默认预留 8k 输出 + 安全边际计算），
不是 `contextWindow - maxOutputTokens`；`maxOutputTokens` 是模型能力上限。实际请求会按
`GenerateOptions.max_tokens` 动态夹紧。客户端不要用 `contextWindow - maxOutputTokens`
自行推导预算。

`_meta` 序列化用 camelCase、跳过 `None`；`status`/`confidence` 用小写字符串。

模型列表规则：

- 按 provider 分组（`SessionConfigSelectOptions::Grouped`），组名 = 本地 provider 别名。
- `id` 默认用裸 id；同一裸 id 出现在多个 provider 时改用 `Provider/model`
  （`ModelConfig::resolve_model` 已支持该形式）。
- **去重键必须是 `(provider, bare_id)`**：先统计每个裸 id 的 provider 数，排序后遍历；
  只有 `count > 1` 时才加前缀。现有 `seen.insert(id)` 会丢掉同名模型的第二个 provider。
- `Provider/model` 用**第一个** `/` 分割，剩余整体是 model id（OpenRouter 的 id 本身含 `/`）；
  `set_model` 按 `ModelChoice.id` 精确匹配，不要拆字符串后再猜 provider。
- 排序：provider 字典序 → 模型字典序；`free` / `deprecated` 只做标签，不隐藏。
- 只列本地 `models.json` 中配置的模型（没有凭据的目录模型不可用）。
- `models` 为空时不下发 `model` 选项（或下发一个 `disabled` 的 `default`），不要
  `currentValue` 指向不存在项；thought_level 同理只保留 `auto`。
- 目录未命中也必须下发卡片：`catalogKey=null`、`confidence="default"`、
  `contextWindow=DEFAULT_CONTEXT_WINDOW`、`maxOutputTokens=DEFAULT_RESERVED_OUTPUT`、
  `inputModalities=["text"]`，description 标注 `unknown model · default 256k`。
  不要省略选项，否则客户端无法区分“未知”和“不可用”。
- 会话创建时把 `config.model` 归一化为真实 id（`agent.model().model_name()` 或
  `default_model`），同步 `applied_model`；**不要**把 `"default"` 当作 currentValue 下发。
  找不到时回退列表第一项，并记录 warning。
- 模型选项本身：`category = SessionConfigOptionCategory::Model`，
  `description = "模型能力、上下文与输入类型随选择实时更新"`。
- `max_input_tokens` 按默认策略 + 空 `GenerateOptions` 计算（与编译器同一公式）；若请求显式提高 `max_tokens`，
  实际可用输入会略低，客户端应以 `UsageUpdate`/manifest 为准。
- `ModelChoice` 新增字段时用 `#[non_exhaustive]` + builder/setter，避免下游结构体字面量破坏。

### 7.3 思考等级（reasoning options）

由 `descriptor.reasoning` + `reasoning_options` 派生，**不再全局固定**：

| 目录状态 | 下发的选项 | 请求映射 |
|---|---|---|
| 无目录 / 未知模型 | 只下发 `auto`（或不下发） | 不发参数，使用模型默认 |
| `reasoning=false` | **不下发** `thought_level`（从 configOptions 移除） | — |
| `reasoning=true`，无 `reasoning_options` | 只下发 `auto`（或不下发） | 不发参数，使用模型默认 |
| `effort: [none,low,medium,high,xhigh,max]` | `auto` + 目录原值（按强度排序、去重） | `reasoning_effort = <value>` |
| `effort` 不含 `none` | 不提供 `disabled` | — |
| `toggle` | `auto` + `on`/`off`（仅当适配器支持关闭）；否则只 `auto` | `on` → 模型默认；`off` → 目录支持时 `reasoning_effort:"none"` |
| `toggle` + `effort` 同时存在 | `auto` + effort 原值；可选 `off`（仅适配器支持关闭） | effort 原样；`off` → 关闭推理 |
| `budget_tokens` | `auto` + `budget:1024/4096/8192/16384/32768`（仅 Anthropic 等支持 budget 的适配器） | `thinking_budget = <n>` |

规则：

1. **删除有损映射**：不再 `max → high`、`disabled → minimal`；下发的就是目录里的合法值。
2. 当前值不在新模型列表里 → 自动回退 `auto`，并在同一次 `ConfigOptionUpdate` 中推送。
3. `set_config_option("thought_level", v)` 校验 `v ∈ 当前模型列表`；非法值拒绝（保留旧值）并回推当前选项。
4. `config_options()` 构建前先 `normalize_thought_level`：即使配置里是上一个模型的旧值，
   下发的 `currentValue` 也必须合法（回退 `auto`）。
5. `SessionConfigOption.category = ThoughtLevel`；`description` 写明来源（如 `catalog: effort low/high/max`）。
6. 请求落点仍是 `GenerateOptions.reasoning_effort` / `thinking_budget`；严格网关不认识的字段不发送。
7. 凡是下发 `thought_level`，`auto` 必须存在；非推理模型整个选项不下发（避免“假可选”）。
8. `toggle` 与 `effort` 同时存在时以 `effort` 为主；`toggle` 只决定能否额外提供 `off`，
   不要再单列 `on`（否则同一状态有两个选项）。

### 7.4 输入类型限制

ACP `ContentBlock` 与目录字段的映射：

| ACP block | 目录条件 | 允许时 | 不允许时 |
|---|---|---|---|
| `Text` | 始终 | `TextBlock` | — |
| `Image` | `modalities.input` 含 `image` 且 `attachment` | `ImageBlock`（base64/url） | 拒绝 + 提示切换视觉模型 |
| `Audio` | 含 `audio` 且适配器支持 | `AudioBlock` | 拒绝（当前 OpenAI Chat 适配器不支持音频输入） |
| `ResourceLink` | 始终（链接本身） | 保留为链接/文本引用 | — |
| `Resource`（`text/*`） | `embedded_context=true` | 内联为 `TextBlock` | 拒绝 |
| `Resource`（`application/pdf`） | `embedded_context=true` 且目录含 `pdf` | 作为文件/文本块 | 拒绝 |
| `Resource`（`video/*`） | 含 `video` | ACP 无视频块 → 拒绝并说明 | 拒绝 |
| 其他 | — | 不降级，直接拒绝（不要把二进制当文本） | 拒绝 |

`ImageContent` 自带 base64 `data` + `mimeType`（`uri` 可选），直接映射到
`Source::Base64`，不要为了补 URL 再下载一次；`AudioContent` 同理。

资源上限（防止客户端发超大附件把进程打爆）：

| 项 | 上限 | 超限行为 |
|---|---|---|
| 单张图片 base64 | 10 MB | Refusal + 可读提示 |
| 单请求图片数 | 8 | Refusal |
| 单个 PDF/Resource | 20 MB | 不支持 file part 的 adapter 直接拒绝 |
| 单条 prompt 总附件 | 30 MB | Refusal |

下发与执行：

1. 模型卡片的 `description` + `_meta.inputModalities` 让客户端在**发送前**禁用附件入口。
2. `AgentCapabilities.prompt_capabilities`：`image` 取所有已配置模型 `image` 的并集
   （无视觉模型则为 `false`；Phase 3 完成图片链路后设为 `true`）；`audio` 只有在音频链路
   落地后才设 `true`；`embedded_context` 保持 `true`。该字段静态，per-model 限制靠
   `_meta` + prompt 校验。
3. `session/prompt` 在提交前逐块校验（替换 `session.rs:916-926` 的纯文本提取）：
   - **顺序**：先 `apply_session_config` 得到实际生效的 `applied_model`，再按它的
     descriptor 校验；切换失败回退旧模型时，按旧模型限制校验并提示。
   - 不支持的块 → 立即 `AgentMessageChunk("Model X does not accept image input; switch to a vision-capable model (e.g. …)")`
     + `StopReason::Refusal`，**不发起模型调用**。
   - 支持 → `Msg::user_blocks(blocks)`，通过新增的 `Session::submit_msg` 进入运行时
     （当前 `submit` 只接受 `String`）。
4. 纯文本请求保持现有路径，避免影响本地 CLI/测试。
5. 图片真正到达 adapter 仍依赖 Phase 3 的 Chat Completions 多模态改造；两边必须同一 Phase 验收。

### 7.5 实时推送时序

```
session/new / load / resume
  └─ configOptions = [model(grouped, cards), mode, thought_level(derived)]
       + Response._meta["agentscope.dev/model"] = 当前模型卡片

session/set_config_option(model, X)
  ├─ 立即解析 X 的 descriptor（不建模型）
  ├─ 重算 model 选项 + thought_level 选项
  ├─ 若当前 thought_level 非法 → 重置 auto
  ├─ ConfigOptionUpdate(new options)
  ├─ UsageUpdate(used=last_used, size=X.context)
  └─ Response.configOptions = new options

catalog refresh (background, generation++)
  └─ for each live session with client:
       ├─ 若 selected_key ∈ changed_keys（或 selected_key=None）→ ConfigOptionUpdate
       └─ UsageUpdate(size = new context)
```

- `thought_level` / `mode` 变更只推 `ConfigOptionUpdate`；只有 `model` 变更才推
  `UsageUpdate`（窗口可能变化）。

实现要点：

```rust
// core/catalog（CatalogEvent 定义见 §5.1）
pub fn generation(&self) -> u64;
pub fn subscribe(&self) -> tokio::sync::watch::Receiver<Arc<CatalogEvent>>;

// acp main
spawn_catalog_update_forwarder(agent.session_registry(), agent.catalog());

// session
pub fn refresh_model_state(&self) -> Vec<SessionNotification> {
    let desc = self.selected_descriptor();       // catalog, 不建模型
    self.normalize_thought_level(desc.as_deref());
    vec![
        self.config_option_update(desc.as_deref()),
        self.usage_notification_with_size(
            desc.map(|d| d.context_window).unwrap_or(DEFAULT_CONTEXT_WINDOW),
        ),
    ]
}
```

- `AcpClientHandle` 增加 `notify(SessionNotification)`（当前只有 request/elicit/connect）。
- 目录刷新任务只做 `send_notification`（非阻塞）；拿不到 client handle 的会话跳过，
  下一次请求自然拿到新数据。
- 同一 generation 只推一次；304 不增加 generation。
- `changed_keys` 由快照 diff 生成：`ModelDescriptor.fingerprint: u64` 对 key/limits/
  modalities/capabilities/reasoning/cost/status 做 hash；首次加载或强制全量时等于全部 key。
- 只推 `selected_key ∈ changed_keys` 的会话；`selected_key=None`（冷启动未解析）保守推送，
  刷新后可能补全。`changed_keys` 为空时什么都不推。
- `selected_key` 在新快照里不存在（provider 映射变化/条目删除）时也要推送，并重新解析；
  否则旧 key 会让会话永远跳过更新。
- 推送失败只记日志，不影响会话。
- 推送顺序：先 options 后 usage（客户端先拿到新窗口，再渲染用量）。
- `UsageUpdate` 可选带 `cost`：按目录 `cost` 与本轮 usage 估算，未知模型不填；客户端
  只能当估算值展示，不能用于计费。

### 7.6 与其它层的接线

| 文件 | 改动 |
|---|---|
| `config.rs` | `ModelChoice` 扩展 provider/catalog_key；`model_option` 支持 description/meta/group/category；`thought_level_option` 由 descriptor 生成；新增 `thought_levels_for(desc)`；`thought_level_params(level, desc)` |
| `session.rs` | 创建时归一化 `config.model`/`applied_model`；`config_options()` 接受 descriptor；`set_model` 校验并立即刷新；`usage_notification` 用所选模型；`run_prompt` 校验并转换 blocks；新增 `selected_descriptor`；`model_builder` 改为接受 `ModelChoice`；`client`/`last_input_tokens` 改为跨克隆共享 |
| `coding_agent.rs` | `set_config_option` 对非法值返回错误；`capabilities()` 的 image/audio 按模型并集；启动目录广播任务 |
| `acp_harness_agent.rs` | `all_models()` 用目录生成 `catalog_key`/provider；卡片仍由 `config_options()` 现算；冲突 id 用 `Provider/model`；`model_builder` 用 choice 的 provider/name/base_url 构建 |
| `support.rs` | 可选：`supports_config_options()`（标准 select 不需要，布尔扩展才需要） |
| `acp_agent_cli.rs` | 渲染模型 description；`ConfigOptionUpdate` 显示新模型/等级/窗口；附件不支持时提示 |
| `harness/session/session.rs` | 新增 `submit_msg(Msg)` / `submit_blocks(Vec<ContentBlock>)` |

`model_builder` 签名改为 `Fn(&ModelChoice) -> Result<Arc<dyn Model>, String>`；
`apply_session_config` 先按 `config.model`（select id）找到 `ModelChoice`，再构建。
不要把 `Provider/model` 直接当模型 id 传给现有 builder。

`selected_descriptor()` 先按 `catalog_key` 查；miss 时用 `base_url + name` 重新解析
（目录刷新后 provider 映射可能变化），结果按 generation 缓存，不回写 `ModelChoice`。

`AcpCodingAgentBuilder` 增加 `.catalog(ModelCatalog)`（默认 `global()`）；`AcpAgentSession`
持有句柄，`config_options()` 用 `self.catalog` 而不是全局单例，测试可注入 `ephemeral()`。

### 7.7 ACP 测试

| 用例 | 断言 |
|---|---|
| `session/new` | model 选项含 description + `_meta["agentscope.dev/model"]`；thinking 选项与模型匹配 |
| `session/new` 初始值 | `currentValue` 是列表中的真实模型 id（不是 `"default"`），thought_level 合法 |
| 切到非推理模型 | `thought_level` 从 configOptions 消失；若原值 high → 自动回退 auto |
| 切到 1M 模型 | `UsageUpdate.size=1,000,000` 立即到达（不等 prompt） |
| 切到 200k free 模型 | size=200,000；description 含 200k/32k |
| 非法等级 | `set_config_option` 返回错误，旧值保留，选项回推 |
| 图片 → 非视觉模型 | 返回 Refusal + 可读消息；fake model 未被调用 |
| 图片 → 视觉模型 | fake model 收到含 `ImageBlock` 的 `Msg` |
| 目录刷新 | fixture 改窗口 → 活跃会话收到 ConfigOptionUpdate + UsageUpdate |
| 304 | 不推送（generation 不变） |
| 跨 provider 同名模型 | 两个选项都存在，value 为 `Provider/model`，解析到各自 base_url |

### 7.8 协议落地细节

#### 7.8.1 精确字段与构造

`NewSessionResponse` / `LoadSessionResponse` / `ResumeSessionResponse` /
`SetSessionConfigOptionResponse` 都同时带 `config_options` 与 `_meta`（schema 1.9.1）。
构造时统一使用 `SessionConfigOption::select`，不要手写 `SessionConfigKind`：

```rust
let model = SessionConfigOption::select(
        "model",
        "Model",
        current_model_id,
        SessionConfigSelectOptions::Grouped(groups), // Vec<SessionConfigSelectGroup>
    )
    .description("Model capabilities, context, and input types update immediately")
    .category(SessionConfigOptionCategory::Model)
    .meta(model_meta);

let thought = SessionConfigOption::select(
        "thought_level",
        "Thinking",
        current_level,
        levels, // Vec<SessionConfigSelectOption>
    )
    .description("Derived from the selected model's catalog reasoning options")
    .category(SessionConfigOptionCategory::ThoughtLevel);
```

- 选项 id 永久固定为 `model` / `mode` / `thought_level`，避免客户端保存的偏好失效。
- `SessionConfigSelectGroup::new(group_id, group_name, options)`；
  `SessionConfigSelectOption::new(value, name).description(...).meta(...)`。
- 响应级 `_meta` 放当前模型快照（读一次即可，不必遍历选项）：
  `response.meta(meta!{ "agentscope.dev/model": card })`。
- 通知统一走 `SessionNotification`，`send_notification` 是同步的：

```rust
cx.send_notification(SessionNotification::new(
    session_id.clone(),
    SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(options)),
))?;
cx.send_notification(SessionNotification::new(
    session_id,
    SessionUpdate::UsageUpdate(UsageUpdate::new(used, size)),
))?;
```

#### 7.8.2 值类型与错误语义

`SetSessionConfigOptionRequest.value` 是带 `type` 判别器的枚举：

```rust
pub enum SessionConfigOptionValue {
    Boolean { value: bool },
    ValueId { value: SessionConfigValueId }, // type 缺省时的默认分支
}
```

| 请求 | 行为 |
|---|---|
| `model` 不在 `self.models` | `respond_with_error(Error::invalid_params().data(...))`，不修改配置 |
| `thought_level` 不在当前模型派生列表 | 同上；旧值保留，并回推当前 options 让 UI 复位 |
| `mode` 不在 `self.modes` | 同上 |
| 选择型选项收到 `Boolean` | `invalid_params`（类型不匹配） |
| 未知 `config_id` | `invalid_params`，不要像现在这样静默 `_ => {}` |
| 未知 session | `invalid_params` + data（ACP 无专门的 session-not-found 错误码） |
| 成功 | 修改配置 → 重算 options → `ConfigOptionUpdate` + `UsageUpdate` → 响应全量 options |
| 值未变化 | 直接响应全量 options，不推送通知 |

原则：**响应里的 `config_options` 是权威状态**；出错时客户端应保持旧值，agent 不产生副作用。
错误路径可选地再发一次 `ConfigOptionUpdate`（内容为旧状态），用于纠正客户端的乐观 UI。

#### 7.8.3 并发与共享状态

`AcpAgentSession` 是 `Clone`，registry 保存的是克隆；当前只有 `config` 是
`Arc<Mutex<...>>` 共享的。广播需要 `client` 与“最近用量”，把它们改成共享
（`applied_model` 仍由 prompt 路径私有，广播不依赖它）：

```rust
client: Arc<Mutex<Option<Arc<AcpClientHandle>>>>, // set_client(&self) 即可
last_input_tokens: Arc<AtomicU64>,                 // 最近一次模型调用的 input tokens
```

`last_input_tokens` 由 prompt 路径在每次模型调用后 `store`，控制面只 `load`。

这样 `refresh_model_state(&self)` 可以在任意克隆上工作，不需要 `&mut self`：
```rust
pub fn refresh_model_state(&self) -> Vec<SessionNotification> {
    let desc = self.selected_descriptor();          // catalog 查询，不建模型
    self.normalize_thought_level(desc.as_deref());  // 锁 config，必要时回退 auto
    vec![
        self.config_option_update(desc.as_deref()),
        self.usage_notification_with_size(desc.map(|d| d.context_window).unwrap_or(DEFAULT_CONTEXT_WINDOW)),
    ]
}
```

实现时先 `let cfg = self.config.lock().unwrap().clone();`，释放锁后再做目录查询与字符串
拼接，避免 `std::sync::Mutex` 重入死锁（`selected_descriptor`/`config_option_update`
内部不要再锁同一把锁）。

目录广播任务：

1. 在 `sessions` 锁内只收集 `Vec<(AcpClientHandle, Vec<SessionNotification>)>`；
2. 释放 registry 锁后再逐条 `send_notification`（同步、非阻塞）；
3. 失败只 `tracing::warn!`，不重试、不阻塞；
4. 全程不 `await`，避免和 prompt 任务形成锁顺序问题。

prompt 路径仍保留 `apply_session_config` 在**下一轮开始时**真正替换模型；
控制面推送只用目录快照，不触发模型构建，因此切换是即时的且不会产生额外网络请求。

`UsageUpdate.used` 取 `min(last_input_tokens, size.saturating_sub(1))`：切换模型后
历史 token 数只是近似值（tokenizer 可能不同），不能让 `used > size`。

**用量窗口必须区分“本轮实际模型”和“当前选中模型”**：

- prompt 运行中的 `UsageUpdate.size` 用 `applied_model` 的 descriptor；
- `set_config_option` / 目录刷新触发的即时 `UsageUpdate.size` 用 `selected` descriptor。
- 否则用户在 prompt 运行中切模型，会把本轮用量标到新模型的窗口上。

#### 7.8.4 兼容与降级

- `category` 是 UX 提示，客户端可能忽略；不要用它做正确性判断。
- `description` 是通用兜底；`_meta` 是增强通道。客户端丢弃 `_meta` 时仍能看到
  `1M ctx · 384k out · text+image · tools · reasoning`。
- 分组选项是核心能力；若某些客户端渲染异常，用 `AGENTSCOPE_ACP_MODEL_FLAT=1` 退化为平铺。
- `AGENTSCOPE_ACP_MODEL_META=off` 可关闭 `_meta`（保留 description），用于排查客户端问题。
- 模型 value id 默认用裸 id；同名冲突时才用 `Provider/model`。恢复旧会话时把裸 id
  归一化到当前列表中的唯一项，找不到则回退默认模型并提示。
- 本仓库固定 `agent-client-protocol-schema 1.9.1`，使用 `configOptions`；
  不要回退到旧版已移除的 `models` 字段。
- 即使客户端不处理 `ConfigOptionUpdate`，`session/new` / `load` / `resume` 与每次
  `set_config_option` 响应都带全量 options，状态最终一致。

#### 7.8.5 安全与隐私

- `_meta` 只放目录数据与计算出的预算：**不得**包含 `base_url`、`api_key`、请求头、
  环境变量名、内部网关地址。
- `catalogKey` 是公开数据；`provider` 用本地别名（如 `Go`）时可能暴露配置命名，
  但绝不包含凭据。需要更保守时只下发目录 provider id。
- 模型卡片与 `description` 不包含任何用户输入、会话内容或用量明细。
- 目录请求仍然匿名，不携带任何密钥或提示内容（与 §5.5 一致）。

#### 7.8.6 测试脚手架

- 传输层用 `Channel::duplex()`，agent 侧 `AcpCodingAgent::builder().model_factory(...)`，
  客户端侧 `Client.builder().connect_with(...)`，与 `tests/acp_session_resume_test.rs` 一致。
- 假模型记录 `Msg.content`：

```rust
seen.lock().unwrap().push(messages.last().unwrap().content.clone());
```

- 客户端用 `on_receive_notification` 收集 `ConfigOptionUpdate` / `UsageUpdate`，
  断言顺序为「先 options，后 usage」。
- 目录 fixture：`AGENTSCOPE_MODEL_CATALOG_DIR=<temp>` 写入 `models.json` + `api.json`；
  测试用 `refresh_now()`（`#[cfg(test)]` 或 feature）触发刷新。
- 目录句柄：测试优先 `ModelCatalog::ephemeral()` 注入独立实例；必须用 `global()` 的
  用例加 `serial_test`，避免并行测试互相覆盖 generation。
- 刷新用例：建会话 → 切模型 → 改 fixture 窗口 → `refresh_now()` → 超时内收到两条通知。
- 304 用例：fixture server 返 304 → `generation` 不变 → 无通知。
- 图片用例：`ContentBlock::Image(ImageContent::new(base64, "image/png"))` →
  视觉模型看到 `CoreBlock::Image`；非视觉模型 `stop_reason == Refusal` 且假模型 `seen` 为空。

#### 7.8.7 待定决策（当前默认值）

| 决策 | 当前默认 | 备选 |
|---|---|---|
| 模型 value id | 裸 id；冲突时 `Provider/model` | 始终 `Provider/model` |
| `promptCapabilities.image` | Phase 3 后按已配置模型并集设为 `true` | 维持 `false` 直到全链路多模态完成 |
| 非法 `thought_level` | 拒绝 + 保留旧值 | 夹紧到最近合法值 |
| 切模型后旧 level 非法 | 自动回退 `auto` | 保留旧值（可能 400） |
| `toggle` 型模型 | 默认只给 `auto`，除非适配器明确支持关闭 | `auto`/`on`/`off` |
| `audio` 输入 | 暂不 advertise，prompt 拒绝 | advertise + 逐块拒绝 |
| 模型分组 | 按 provider 分组 | 平铺 |
| 会话恢复的模型/等级 | sidecar `{model,mode,thought_level}` | 不持久化 |
| `_meta` 命名空间 | `agentscope.dev/model` | `agentscope/model` |

这些默认值都可以按客户端反馈调整；除「非法值拒绝」外，其余都不影响协议正确性。

Phase 4 内部建议拆成 3 个可独立评审的 commit：

1. **数据面**：`ModelChoice` + `ModelCard` + `config_options()` 的 description/meta/group/category
   （纯新增，不改现有选择逻辑）。
2. **控制面**：`thought_level` 派生、非法值校验、切模型立即推送 `ConfigOptionUpdate` + `UsageUpdate`。
3. **输入面**：`promptCapabilities` 并集、prompt 逐块校验、`submit_msg`、目录广播、CLI 渲染。

---

## 8. 关键计算策略

### 8.1 输入预算与输出预留

```rust
fn input_limit(desc: Option<&ModelDescriptor>, policy: &ContextPolicy, opts: &GenerateOptions) -> usize {
    let window = desc.and_then(|d| d.effective_context()).unwrap_or(DEFAULT_CONTEXT_WINDOW);

    // 1) 实际允许生成的 token 数
    let cap = desc.map(|d| d.max_output).filter(|n| *n > 0).unwrap_or(u32::MAX as usize);
    let requested = opts.max_tokens.or(opts.max_completion_tokens)
        .map(|n| n as usize)
        .unwrap_or_else(|| policy.default_reserved_output); // 默认 8_192
    let mut reserved = requested.min(cap);

    // 2) 安全边际：2%，上限 16k
    let safety = policy.safety_margin_tokens.max((window / 50).min(16_000));

    // 3) 不允许预留把输入饿死：至少留 16k 输入
    let min_input = 16_000.min(window / 2);
    if window.saturating_sub(reserved + safety) < min_input {
        reserved = window.saturating_sub(safety + min_input);
    }
    window.saturating_sub(reserved + safety)
}
```

数值对比（同一份 60,549 estimated 请求）：

| 场景 | window | reserved | safety | input_limit | 结果 |
|---|---|---|---|---|---|
| 现状（64k 表） | 64,000 | 4,096 | 1,280 | 58,624 | ❌ |
| Phase 0（默认 256k） | 256,000 | 4,096 | 5,120 | 246,784 | ✅ |
| 目录：`opencode/deepseek-v4.1-flash-free` | 200,000 | 4,096 | 4,000 | 191,904 | ✅ |
| 目录：`opencode-go/deepseek-v4.1-flash` | 1,000,000 | 4,096 | 20,000 | 975,904 | ✅ |
| 新策略：1M 模型，默认 8k 输出 | 1,000,000 | 8,192 | 16,000 | 975,808 | ✅ |

> 原则：**输出预留是"本次允许生成多少"，不是 `limit.output` 的照抄**。否则 1M 模型预留 384k 会浪费 38% 上下文。

### 8.2 压缩触发

```rust
// CompactionConfig 新增 trigger_ratio（默认 0.9），保留 trigger_tokens 兼容
let trigger = if cfg.trigger_tokens > 0 {
    cfg.trigger_tokens
} else {
    (input_limit as f64 * cfg.trigger_ratio) as usize
};
let keep = (trigger / 10).clamp(2_000, 32_000);
```

- 触发必须**严格小于** `input_limit`，给摘要请求和下一轮留空间。
- 压缩仍在 `on_agent`（模型调用前）执行，但阈值与当前模型窗口绑定，不再依赖 256k 常量。
- 摘要请求走 `ContextPurpose::Summary`，只保留 System，并在超限时截断最旧历史，不返回 `budget_exceeded`。

### 8.3 能力合并顺序

```
最终能力 = 本地覆盖 > 目录 provider 精确条目 > 目录 canonical 条目 > 前缀表兜底 > 默认值
```

- `temperature`：目录 `false` 时强制丢弃采样参数（比 `gpt-5` 前缀规则更准）。
- `tool_call`：`false` 时禁用原生工具并记录一次性告警；不静默把工具 schema 发出去。
- `structured_output`：有值时直接决定 ReactAgent 走 Native 还是 Prompt。
- `interleaved.field`：存在则开启 reasoning passback（DeepSeek 等）。
- `status=deprecated`：允许继续用（用户可能显式选择），但选择列表标注且不作为默认。

### 8.4 使用场景（派生标签）

| 目录字段 | 派生标签 | 框架用途 |
|---|---|---|
| 无 `type` | `chat` | 可进入聊天模型选择 |
| `reasoning = true` / family 含 thinking | `reasoning` | 保留 thinking，允许 reasoning effort |
| `tool_call = true` | `agentic` | 注册工具 |
| `modalities.input` 含 image/pdf/audio | `vision` / `document` / `audio` | 放行对应 ContentBlock |
| `context ≥ 500k` | `long-context` | 展示；压缩阈值更高 |
| `cost` 全 0 | `free` | 展示；可作默认候选 |
| `status = deprecated` | `deprecated` | 列表标注，禁止默认 |
| `status = alpha` / `beta` | `alpha` / `beta` | 显示标签，允许选择 |
| description/family 含 coder/coding/agent | `coding` | 展示；用于自动选型 |

示例（实测目录值）：

| 模型 | 上下文 | 输出 | 特性 | 派生场景 |
|---|---|---|---|---|
| `opencode-go/deepseek-v4.1-flash` | 1,000,000 | 384,000 | reasoning + tools + image + interleaved | coding, agentic, long-context |
| `opencode/deepseek-v4.1-flash-free` | 200,000 | 32,000 | reasoning + tools + image + interleaved | free, coding, agentic |
| `zhipuai/glm-5.3` | 1,000,000 | 131,072 | reasoning + tools, 无图片 | coding, long-context |
| `minimax/MiniMax-M3` | 1,048,576 | 512,000 | reasoning + tools + image/video | long-context, multimodal |
| `openai/gpt-5.6-luna` | 1,050,000 | 128,000 | `temperature=false` + tools + image/pdf | reasoning, vision, long-context |

---

## 9. 分阶段实施

依赖关系：

```
Phase 0 ─▶ Phase 1 ─▶ Phase 2 ─┬─▶ Phase 3 ─▶ Phase 4
                               └─▶ Phase 5 ─▶ Phase 6
```

- Phase 4（ACP）依赖 Phase 2 的窗口与 `input_limit()`，以及 Phase 3 的图片链路。
- Phase 5 只替换压缩/驱逐/预估的调用方，不再改窗口来源。

### Phase 0 — 常量热修（已完成，2026-10-22）

| 文件 | 改动 |
|---|---|
| `agentscope-core/src/model/context_windows.rs` | 删除 `deepseek-reasoner` / `deepseek-chat` / `deepseek` 三条 `64_000`；`DEFAULT_CONTEXT_WINDOW = 256_000`；文件头注释同步 |
| `agentscope-core/src/model/mod.rs` | 重导出 `DEFAULT_CONTEXT_WINDOW`，供 harness / ACP 复用 |
| `agentscope-harness/src/agent/context/builder.rs` | 窗口未知时的 `64_000` 兜底 → `DEFAULT_CONTEXT_WINDOW as i64` |
| `agentscope-extensions-acp/src/session.rs` | `UsageUpdate.size` 的 `unwrap_or(128_000)` → `unwrap_or(DEFAULT_CONTEXT_WINDOW as u64)` |
| 相关单测 | `deepseek-chat` 断言 `64_000` → `256_000`；未知模型断言跟随 `DEFAULT_CONTEXT_WINDOW` |

- 不在 Phase 0 改 `coding_agent.rs:47` / `acp_harness_agent.rs` 的 256k：
  它们本来就是 256k，Phase 5 再改成动态触发。
- **验收结果**：
  - `rg "64_000|64000" crates -g "*.rs"` 无命中。
  - `cargo test -p agentscope-core context_windows`：3 passed。
  - `cargo test -p agentscope-harness agent::context`：46 passed。
  - `cargo test -p agentscope-extensions-acp`：46 unit + 7 integration passed。
  - 未知模型输入预算 = `256,000 - 4,096 - 5,120 = 246,784`（Phase 0 仍用旧策略）；
    Phase 2 引入 `DEFAULT_RESERVED_OUTPUT = 8,192` 与 16k 安全上限后变为 `231,808`。

### Phase 1 — Catalog 模块（已完成，2026-10-22）

- 新增 `catalog/{mod,config,types,source,resolve,store,refresh,wrap}.rs` + 单元测试。
- 支持双源（`models.json?type=all` + 可选 `api.json`）、归一化、合并、`provider_api` host 索引。
- 磁盘缓存（`catalog.v1.json` + `sources/*.raw.json/meta.json`）、TTL、ETag/304、原子写、
  单飞、`CatalogEvent { generation, changed_keys }`、`fingerprint` diff。
- HTTP 已启用 `gzip`/`brotli`/`deflate`/`zstd` + `system-proxy`；`Content-Type` 校验、
  15s 超时、1 MiB/8 MiB 上限。
- **验收结果**：
  - 22 个 catalog 单元测试通过（`cargo test -p agentscope-core catalog`）。
  - 真实 payload 解析（`--ignored` 诊断测试）：`models.json` >500 条、`api.json` >8,000 条，
    `opencode-go/deepseek-v4.1-flash` = 1,000,000/384,000。
  - 真实压缩 HTTP 拉取测试 `fetches_real_models_dev_payload` 通过。
  - 解析失败/API 源失败保留旧快照；304 不重写 normalized snapshot。

### Phase 2 — 窗口接入（已完成，2026-10-23）

- `Model` trait 增加 `max_output_tokens()` / `descriptor()`；`ModelCreationContext` 增加
  `catalog: Option<Arc<ModelDescriptor>>`。
- 新增 `CatalogAwareModel`：不侵入每个 adapter 的请求实现，只在构造后覆盖
  `context_window_size()` / `max_output_tokens()` / `descriptor()`。
- `ModelRegistry::resolve*` 与 ACP `build_model_for_protocol` 都会自动包一层。
- OpenAI Chat / Responses / Anthropic / Anthropic async / Gemini / Ollama / DashScope /
  OpenAI Official 都在构造时尝试解析目录 descriptor；显式 `with_context_window_size`
  仍可覆盖。
- `descriptor_for()` 使用 `provider_key + base_url host + path token + 本地模型集合` 消歧；
  `-free` 精确命中优先，归一化只做 fallback。
- `input_limit()` 已提供（`ModelDescriptor::input_limit`），Phase 5 已替换调用方。
- **验收**：`opencode-go/deepseek-v4.1-flash`→1M、`-free`→200k、未知→256k；全 workspace
  编译与现有测试全绿。

### Phase 3 — 能力与请求参数（已完成，2026-10-23）

- 新增 `resolve_capabilities_with(model, Option<&ModelDescriptor>)`；`temperature=false`
  时关闭 sampling/penalties，`tool_call=false` 时关闭 parallel tool calls。
- OpenAI Chat 与 Responses 的请求构建使用 catalog-aware capabilities。
- Chat Completions 已把 `ContentBlock::Image` 转成 `image_url`（Base64 → data URL，URL 原样），
  并拒绝向 `input_modalities` 不含 image 的模型发送图片。
- `max_tokens` / `max_completion_tokens` 统一由 `clamp_max_tokens` 夹紧到目录 `max_output`；
  OpenAI / Responses / Anthropic / Anthropic async / Gemini / Ollama / DashScope /
  OpenAI Official 均已接入。
- `OpenAiChatModel` 的 reasoning passback 优先读 `descriptor.interleaved_field`，名字启发式仅兜底。
- **验收**：golden JSON 测试全绿；非视觉模型收到图片会返回可读错误。

### Phase 4 — ACP 能力实时下发（部分完成，2026-10-22）

- `ModelChoice` 增加 `provider` / `catalog_key` / `base_url`；`all_models()` 按
  `(provider, bare_id)` 去重，跨 provider 同名模型才用 `Provider/model`。
- `model_option()` 按 provider 分组，每个选项带 catalog `description` 与
  `_meta["agentscope.dev/model"]`（窗口/输出/模态/能力/思考等级/cost/status）。
- `thought_level` 由 `reasoning_options` 派生；`set_config_option` 校验模型与等级，
  非法值返回 `invalid_params`；未知 session/config 不再返回空 options。
- `session/new` 初始化时把 `config.model` 对齐真实 `agent.model()`，不再下发 `"default"`。
- `thought_level_params` 删除 `max→high`、`disabled→minimal` 有损映射。
- `session/prompt` 逐块转换：
  - 文本 / `ResourceLink` / 内嵌文本资源 → `TextBlock`
  - 图片 → `ImageBlock`（Base64 data URL），超过 10 MB 或超过 8 张直接 Refusal
  - PDF → `DataBlock`（仅当目录 `attachment=true`）
  - 音频 → `AudioBlock`（仅当目录含 audio）
  - 不支持的类型立即 `AgentMessageChunk` + `Refusal`，**不发起模型调用**
- 新增 `Session::submit_msg(Msg)` / `submit_with_key_msg`，ACP 用多模态 `Msg` 进入运行时。
- `promptCapabilities.image` 改为已配置模型的并集。
- `AcpAgentSession.client` 改为跨克隆共享；新增后台 `spawn_catalog_forwarder`：
  目录 `CatalogEvent` 变化时，向 `selected_catalog_key` 受影响的活跃会话推送
  `ConfigOptionUpdate` + `UsageUpdate`。
- A12 已实现：`{model, mode, thought_level}` 写入
  `<cwd>/.agentscope/acp-sessions/<session>.json`，`session/load` 时校验并恢复。
- **待完成**：`UsageUpdate.cost`；ACP `session/load|resume` 的 `_meta` 回放目前仍按
  `config_options()` 现算。
- **验收**：现有 46 unit + 7 integration 全绿；非视觉模型收到图片会得到可读 Refusal；
  目录刷新会向活跃会话推送新窗口。

### Phase 5 — 上下文 / 压缩 / 驱逐统一（已完成，2026-10-23）

- `ContextPipeline::input_limit` 读取 `Model::max_output_tokens()`，默认输出预留 8,192，
  安全边际 16k；预算错误附带 model/catalogKey/window/reserved/safety。
- 预估器升级为 `mixed-text-2.5chars-media-sized-v2`：图片按 base64 体积估算，
  音频 2,048、视频 4,096、Data 2,048，不再所有媒体一律 2,048。
- ACP 压缩触发 = `90% × model.context_window_size()`（未知模型仍 256k）；
  `CompactionConfig::scaled_for_window` 缩放 reserved + prune 预算。
- `ToolResultEvictionConfig::scaled_for_window` 按窗口把 80k 阈值缩放到 20k–200k。
- `ContextManifest` 增加 `contextWindow` / `catalogGeneration` / `catalogKey`。
- **验收**：harness context 46 tests 全绿；预算错误现在能区分本地预算与网关错误。

### Phase 6 — 可观测 / 文档 / 开关（已完成，2026-10-23）

- `ContextManifest` 已带 `contextWindow` / `catalogGeneration` / `catalogKey`。
- `ContextSnapshot`（ControlPlane）已带 `catalogKey` / `catalogGeneration`；`acp_agent_cli`
  的 `ConfigOptionUpdate` 会显示当前模型/等级的 description。
- `ModelCatalog::resolve` 在 cache miss 时输出 model/provider/base_url/generation/
  kind/confidence/catalogKey/contextWindow 日志；refresh 失败带源名。
- `AGENTSCOPE_MODEL_CATALOG=off` 可完全回退到 Phase 0 行为。
- **验收**：日志和两个 UI 都能回答“这个窗口从哪来、置信度多少、缓存多旧”。

---

## 10. 测试计划

| 类型 | 内容 |
|---|---|
| 单元（fixture） | 解析 2 种 JSON 形状；字段缺失/0/超界；合并优先级 |
| 单元（resolver） | 精确 provider、host 匹配、裸 id 唯一、canonical、归一化、前缀共识、歧义→默认 |
| 单元（cache） | TTL、304、损坏缓存、超大响应、原子替换、schema 版本不匹配 |
| 单元（budget） | 64k/200k/256k/1M 的 `input_limit`；`output > context` 夹紧；最小输入保护 |
| 单元（compaction） | `trigger_ratio` 在 200k/1M 下的触发点；Summary 截断 |
| 集成 | 用 fixture 目录启动 harness：原 60,549 请求通过；模型切换后窗口/触发同步 |
| ACP | 模型卡片 description/`_meta`；思考等级派生与回退；切模型立即推 `UsageUpdate`；图片拦截/放行 |
| ACP 广播 | fixture 刷新 generation → 活跃会话收到 `ConfigOptionUpdate` + `UsageUpdate`；304 不推 |
| 回归 | `deepseek-v4.1-flash-free` 解析到 200k，而不是 canonical 1M |
| 离线 | 无网络/超时/500/HTML 响应下启动与请求均正常 |
| HTTP | zstd/gzip 解压；弱 ETag 304；`Content-Type` 非 JSON；超时；8 MiB 上限 |
| 并发 | 同一时刻多次刷新只发一个请求；并发 resolve 不重复消歧 |
| 时钟 | `fetched_at` 超前 5min → 视为过期；进程内 `Instant` 不受跳变影响 |
| 性能 | 冷启动读快照 <50ms；热路径 O(1) 查找；`api.json` 解析在 `spawn_blocking` |

测试不得访问公网：所有网络用例走 `MemoryCatalogFetcher` 或 fixture 文件。
测试默认 `AGENTSCOPE_MODEL_CATALOG=off`；需要目录的用例显式注入 `ephemeral()` + fixture。

---

## 11. 风险与权衡

| 风险 | 影响 | 缓解 |
|---|---|---|
| models.dev 把窗口标大 | 请求被 provider 400 | provider 精确条目优先；`deepseek-v4.1-flash-free` 类网关条目优先于 canonical |
| 目录缺网关私有模型 | 退回 256k，可能提前压缩 | 归一化 + 本地覆盖；256k 对绝大多数模型安全 |
| 目录字段语义变化 | 能力误判 | `schema_version` + 严格校验 + 未知字段忽略 + 本地覆盖 |
| 每日刷新带来启动抖动 | 首次请求变慢 | 只读快照；`api.json` 后台解析；无缓存直接 256k；`COLD_BLOCK_MS` 默认 0 |
| 压缩响应解压失败 | zstd 解析失败/乱码 | 已启用 reqwest 解压特性；失败只丢弃该源并保留旧快照 |
| Windows 文件被占用 | 原子写失败 | `rename` 重试 3 次；仍失败保留旧快照，不影响请求 |
| `api.json` 6 MB | 启动内存/CPU 峰值 | 紧凑裁剪 + `spawn_blocking`；常驻目标 ≤20 MiB |
| 企业代理 / MITM | 刷新失败 | 支持 `HTTPS_PROXY`；MITM 用 `off` 或本地快照 |
| 多进程写缓存 | 文件损坏 | `tmp + rename`；读取失败回退旧快照 |
| 能力收紧导致行为变化 | 例如不再发 temperature | 先只对目录明确 `false` 的模型生效；提供 `off` 开关 |
| 统一预估器 | 历史 token 统计变化 | 保留 method 字符串与 manifest，便于对比 |
| 客户端忽略 `_meta` | 看不到结构化能力 | `description` 兜底；关键限制同时放进人类可读文本 |
| `promptCapabilities` 是连接级静态 | 无法按模型变化 | 取已配置模型并集 + prompt 逐块校验；`_meta` 标注 per-model 限制 |
| 目录刷新推送抖动 | 客户端频繁重绘 | `generation` 去重；同一 generation 只推一次；304 不推 |
| ACP schema 演进 | `_meta` 语义漂移 | 固定 1.9.1；`_meta` 内带 `schema: 1`；未知字段忽略 |
| 模型切换后 level 非法 | 请求 400 | 切换时立即校验并回退 `auto`，同一次更新推送 |

---

## 12. 验收清单

- [ ] `src/` 下 `rg "64_000|64000"` 无命中。
- [ ] 未知模型 `context_window_size()` = 256,000。
- [ ] `deepseek-v4.1-flash` → 1,000,000；`deepseek-v4.1-flash-free` → 200,000。
- [ ] 原失败请求 `estimated=60549` 通过，且 manifest 记录 catalogKey/confidence。
- [ ] 200k 模型压缩在 ~90% 触发；1M 模型不被 256k 提前压缩。
- [ ] 断网启动 <50ms，使用旧缓存并打印 age；24h 后自动刷新，304 不重写。
- [x] zstd 压缩响应能正确解压；弱 ETag `W/"..."` 原样用于 `If-None-Match`。
- [ ] Linux/macOS/Windows 都能定位缓存目录；Windows 原子写失败有重试且不损坏旧快照。
- [ ] 无缓存冷启动首个请求 <50ms（直接 256k），`api.json` 解析不阻塞请求。
- [ ] `AGENTSCOPE_MODEL_CATALOG=off` 时行为与 Phase 0 完全一致。
- [ ] 请求热路径无新增网络 I/O；目录解析为内存查找。
- [ ] 不向 models.dev 发送任何密钥、会话或提示内容；`_meta` 不含 base_url/api_key。
- [ ] ACP `session/new` 返回的模型选项带 description + `_meta["agentscope.dev/model"]`。
- [ ] 切换到非推理模型后 `thought_level` 消失/回退 `auto`，并立即推送。
- [ ] 切换模型后 `UsageUpdate.size` 立即等于新模型窗口（不等下一轮 prompt）。
- [ ] 图片发给非视觉模型时返回可读错误且不发起模型调用；视觉模型能收到 `ImageBlock`。
- [ ] 目录刷新后活跃会话收到 `ConfigOptionUpdate` + `UsageUpdate`；304 不推送。

---

## 13. 附：实现顺序建议

```
Phase 0  → 先止血（独立 PR，1 个 commit）
Phase 1  → catalog 模块 + fixture（不接线，纯新增）
Phase 2  → 窗口接线（行为可观测，风险低）
Phase 3  → 请求参数/多模态（需要 golden 测试保护）
Phase 4  → ACP 模型卡片/思考等级/输入限制实时下发（§7）
Phase 5  → 上下文/压缩统一（最需要回归）
Phase 6  → 可观测与文档
```

每个 Phase 都可独立合并；Phase 0 与 Phase 2 之后应各做一次真实网关冒烟
（`opencode-go/deepseek-v4.1-flash`、`opencode/deepseek-v4.1-flash-free`、未知模型各一次）。
