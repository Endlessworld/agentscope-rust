# NACOS 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **NACOS** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-nacos\`
> **Java 子模块**：nacos-a2a, nacos-prompt, nacos-skill
> **Rust 目标**：`crates/agentscope-extensions-nacos/`（波次 C）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-nacos` |
| Java 子模块 | nacos-a2a, nacos-prompt, nacos-skill |
| Java 规模 | 主代码 18 文件 ~68KB / 测试 8 文件 ~137KB |
| Rust crate | `crates/agentscope-extensions-nacos` |
| 迁移波次 | C |
| 当前状态 | **已完成**（build + test 全绿；三个子域均迁移，含 43 条离线单测） |

## 2. Java 清单与依赖映射

> Java 模块 = Nacos 配置/注册中心侧，与存储侧（JDBC/Redis/MongoDB…）不同：不实现 `DistributedStore`。经 codegraph 探查，harness `crates/agentscope-harness/src/agent/` 下**不存在** a2a/prompt 相关抽象（无 `a2a/` 目录，无 `AgentCard`/`AgentRegistry`/`AgentCardResolver`/prompt 仓库或 listener trait），故 a2a/prompt 两域使用本 crate 本地 trait 作为「协议侧未就绪」的适配层；skill 域**复用** harness `SkillRepository`。

### 2.1 nacos-a2a（A2A 注册与发现）

| Java 类 / 包 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `a2a.model.AgentCard`（Nacos 版子集） | Nacos 侧 AgentCard DTO | `a2a::model::NacosAgentCard` | 已完成 |
| `a2a.model.{AgentCapabilities,AgentSkill,AgentInterface,AgentProvider}` | Nacos 版子规格 | `a2a::model::{NacosAgentCapabilities,NacosAgentSkill,NacosAgentInterface,NacosAgentProvider}` | 已完成（`SecurityScheme` 保留为 opaque JSON） |
| Nacos `AgentEndpoint`（`registerAgentEndpoint` 载荷） | 注册到 Nacos 的单个 endpoint（transport/address/port/path/supportTls/**version**/protocol/query） | `a2a::model::NacosAgentEndpoint` + `from_transport(props, version)` | **本轮补齐**（此前直接注册 `NacosA2aTransportProperties`，丢失 `version` 字段且字段名为 `host` 而非 `address`） |
| `utils.AgentCardConverterUtil` | Nacos ↔ A2A AgentCard 双向转换（含 null/空集合语义） | `a2a::converter::NacosAgentCardConverter` + `convert_to_*` 自由函数 | 已完成 |
| `registry.NacosA2aRegistry` | 向 Nacos 发布 AgentCard + Endpoint | `a2a::registry::NacosA2aRegistry` | 已完成（依赖 `NacosAiService` trait；endpoint 现带 card version） |
| `registry.NacosA2aRegistryProperties` | 注册选项（setAsLatest/enabledRegisterEndpoint/overwritePreferredTransport/transports） | `a2a::registry::NacosA2aRegistryProperties` + `Builder` + `builder_from` | 已完成 |
| `registry.NacosA2aRegistryTransportProperties` | 单个 transport endpoint 配置 | `a2a::registry::NacosA2aTransportProperties` + `Builder` | 已完成（`build()` 校验 `transport` 非空；`effective_protocol()` 复刻 `handleTlsIfNeeded`） |
| `registry.NacosA2aTransportPropertiesEnvParser` | 解析 `NACOS_A2A_AGENT_{TRANSPORT}_{ATTR}`；非法 port 抛 `INVALID_PARAM` | `a2a::registry::NacosA2aTransportPropertiesEnvParser` | **本轮补齐**：`parse`/`from_env` 改为返回 `AiServiceResult`，非法 port 返回 `Err(AiServiceError::INVALID_PARAM, "Invalid `port` value for transport `JSONRPC`: no_number")`，对齐 Java `NacosRuntimeException(NacosException.INVALID_PARAM, ..)`（此前静默置 0） |
| `registry.NacosAgentRegistry` | `AgentRegistry` 实现；合并 deploy/env transports、覆写 preferredTransport、生成 URL | `a2a::registry::NacosAgentRegistry` + `TransportProperties` + `AgentRegistry` trait | 已完成（`build_transport_properties` 现透传 env 解析错误） |
| `registry.constants.Constants` | 协议/前缀/属性枚举 | `a2a::registry::constants`（含 `TransportPropertiesAttribute`） | 已完成 |
| `discovery.NacosAgentCardResolver` | 从 Nacos 订阅并缓存 AgentCard（`AgentCardResolver` 实现） | `a2a::discovery::NacosAgentCardResolver` + `AgentCardResolver` trait | 已完成（`async_trait`；内存缓存 + 订阅回调更新，含推送更新单测） |
| `io.agentscope.core.a2a.agent.card.AgentCardResolver` / `...server.registry.AgentRegistry` / `io.a2a.spec.AgentCard` | 协议侧接口与 A2A 规范模型 | 本 crate 精简同构 trait/模型 | 设计决定（协议 crate a2a 未就绪，见 §4.2-1） |

### 2.2 nacos-prompt（Prompt 监听）

| Java 类 / 包 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `prompt.NacosPromptListener` | 经 `AiService.subscribePrompt` 获取并缓存；`{{var}}`/`${var}` 渲染；version/label/default 支持 | `prompt::NacosPromptListener` | 已完成 |
| `Prompt`（`getTemplate`/`getVersion`/`render(Map)`） | prompt 载荷 + 渲染 | `service::NacosPrompt` + `service::render_template` | 已完成（`EMPTY_SENTINEL` 语义、未知占位符保留、default 渲染） |
| `AbstractNacosPromptListener#onEvent` | 推送回调更新缓存（忽略空 key/template） | `service::PromptListener` 类型别名 + `InMemoryNacosAiService::fire_prompt_update` | 已完成 |
| `NacosPromptListener#getPrompt(key, version, label, args, default)` | 读取入口（缓存命中不重复订阅） | `prompt::NacosPromptListener::{get_prompt, get_prompt_with_args, get_prompt_with_default, get_prompt_full}` | 已完成（含 version/label 透传、缓存命中、default 回退单测） |

### 2.3 nacos-skill（技能仓库）

| Java 类 / 包 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `skill.NacosSkillRepository` | `AgentSkillRepository` 实现：从 Nacos 下载 skill ZIP，解析 SKILL.md 构建 `AgentSkill`；只读；version/label 优先级；`knownSkillNames` 枚举 | `skill::NacosSkillRepository` | 已完成（**实现 harness `SkillRepository`**：`repo_id`/`source`/`is_writeable`/`repo_kind`/`load` + `write_skill`/`delete_skill` 只读拒绝） |
| `SkillUtil.createFromZip` / `MarkdownSkillParser` | 从 zip 构建 skill（含资源条目、二进制 base64 回退） | `skill::frontmatter::create_from_zip` + `AgentSkill::builder` | 已完成（harness 无 zip 解析 → 本 crate 引入 `zip` 依赖） |
| `NacosSkillRepository#adaptNacosSkillZipForYamlFrontmatter` / `normalizeFoldedFlatYaml` | 折叠 Nacos 导出缩进 YAML 续行并重打包 zip | `skill::frontmatter::{adapt_nacos_skill_zip, adapt_skill_md_frontmatter, normalize_folded_flat_yaml}` | 已完成（纯函数；含 `needsQuoting` 引号/转义复刻） |
| `io.agentscope.core.skill.repository.AgentSkillRepository` / `AgentSkill` | 技能仓库接口/模型 | **harness `agentscope_harness::agent::skill::{SkillRepository, AgentSkill, SkillRepo}`（复用，不重建）** | 已完成 |

### 2.4 依赖映射（Java → Rust）

| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| `com.alibaba.nacos.api.ai.AiService` | Nacos AI 客户端（subscribePrompt / releaseAgentCard / registerAgentEndpoint / downloadSkillZip*） | `service::NacosAiService` trait（async） | Nacos 无官方 Rust AI SDK → 抽象为 trait；`InMemoryNacosAiService` 为测试/嵌入式实现；真实 HTTP 实现待裁定（§4.2-3） |
| `AiFactory.createAiService(Properties)` | 建 Nacos 客户端 | 由调用方注入 `Arc<dyn NacosAiService>` | 依赖倒置 |
| `com.alibaba.nacos.api.exception.NacosException` | 错误码（`NOT_FOUND`/`INVALID_PARAM`） | `service::AiServiceError`（`is_not_found`；常量 `NOT_FOUND=404`/`INVALID_PARAM=400`） | 已完成 |
| `com.alibaba.nacos.common.utils.{StringUtils,JacksonUtils}` | 字符串/JSON | std + `serde_json` | |
| `io.a2a.spec.AgentCard` 等 | A2A 规范模型 | `a2a::model::{A2aAgentCard, ...}`（本 crate 本地模型） | 协议 crate a2a 未就绪 → §4.2-1 |
| `java.util.zip.*` / `java.util.Base64` | zip 读写 / 二进制资源 | `zip = 0.6`、`base64`（本 crate `[dependencies]`） | 未改根 `[workspace.dependencies]`（`base64` 走 workspace） |
| `ConcurrentHashMap` | 本地缓存 | `std::sync::Mutex<HashMap<..>>` | 单进程嵌入式场景足够 |

## 3. 详细缺口清单

### 3.1 骨架与依赖（P0）

| 缺口项 | Java 参考 | 优先级 | 执行状态 | 交接信息/说明 |
|---|---|---|---|---|
| crate 骨架 + 依赖声明 | — | P0 | 已完成 | 本 crate `[dependencies]` 增 `zip = 0.6`；`base64` 复用 workspace；未改根 `workspace.dependencies` |
| nacos-a2a 模型 + 转换 | `a2a.model.*`, `utils.AgentCardConverterUtil` | P0 | 已完成 | §2.1；含 null/空集合/opaque securitySchemes 往返单测 |
| nacos-a2a registry + properties + env parser | `registry.*` | P0 | 已完成 | §2.1；env parser 非法 port 现返回 `INVALID_PARAM`（对齐 Java） |
| nacos-a2a discovery resolver | `discovery.NacosAgentCardResolver` | P0 | 已完成 | §2.1；缓存 + 推送更新单测 |
| nacos-prompt listener | `prompt.NacosPromptListener` | P0 | 已完成 | §2.2；渲染/default/缓存命中/version-label 透传单测 |
| nacos-skill repository + zip/frontmatter | `skill.NacosSkillRepository` | P0 | 已完成 | §2.3；harness `SkillRepository` 只读语义 + frontmatter 折叠单测 |
| endpoint 载荷保真（`version`/`address`） | Nacos `AgentEndpoint` | P1 | 已完成 | 新增 `a2a::model::NacosAgentEndpoint` + `from_transport`，由 `register_endpoints` 使用 |

### 3.2 设计决定（不迁移 / 延后）

| 项 | 优先级 | 决定 | 说明 |
|---|---|---|---|
| 真实 Nacos HTTP 客户端 | P1 | 设计决定（延后） | 无成熟 Rust Nacos AI SDK；`NacosAiService` trait 为注入缝隙，与 Java `AiService` 注入点对齐；HTTP 实现归属待裁定（§4.2-3） |
| 与 protocol crate 的 A2A 模型统一 | P1 | 设计决定（延后） | protocol crate 的 `a2a` 当前不可编译；本 crate 用本地 `A2aAgentCard`/`AgentCardResolver`/`AgentRegistry`，待 PROTOCOL 落地后合并复用（§4.2-1） |
| `AgentCardResolver` 同步→异步 | P0 | 设计决定 | Java 版阻塞（Nacos 客户端阻塞）；Rust 侧 `NacosAiService` 为 async，故 trait 用 `async_trait` |
| `NacosA2aRegistry` 多 endpoint 批量注册 | P2 | 设计决定 | Java 在 1 个 endpoint 时用单值、≥2 时用 `Set` 去重批量；Rust 侧统一逐个 `register_agent_endpoint`（语义等价，去重差异忽略） |
| `NacosSkillRepository` 的 `load()` 同步 trait | P2 | 设计决定 | harness `SkillRepository::load` 为同步 → 内部 `block_on` 驱动 async 下载（对齐 Java 阻塞 `getAllSkills`）；有 runtime 时走 `block_in_place`，测试无 runtime 时走临时 current-thread runtime |

### 3.3 剩余缺口（增强项，非阻塞）

| 缺口项 | 优先级 | 状态 | 说明 |
|---|---|---|---|
| 真实 Nacos HTTP 客户端（`reqwest` 版 `NacosAiService`） | P1 | 待办（跨模块） | 见 §4.2-3 |
| 与 protocol crate A2A 模型/接口合并 | P1 | 待办（跨模块） | 见 §4.2-1 |
| `NacosAgentRegistry` 批量 endpoint 去重（Java `Set` 语义） | P2 | 待办 | 当前逐个注册；如需 Nacos `Set` 语义可在 crate 内补齐 |
| `AgentCardResolver` 缓存失效/TTL 策略 | P2 | 待办 | Java 亦仅靠订阅推送更新、无 TTL；当前保持一致 |

## 4. 项目状态与下一步

### 4.1 当前状态

- **状态：已完成**。三个子模块 nacos-a2a / nacos-prompt / nacos-skill 均已迁移并含离线单测。
- **文件清单**（均在本 crate 内，未触碰其它 crate）：
  - `crates/agentscope-extensions-nacos/Cargo.toml`
  - `src/lib.rs`（导出 `a2a`/`prompt`/`service`/`skill` + `NacosAiService`/`AiServiceError`/`AiServiceResult`/`InMemoryNacosAiService`）
  - `src/service.rs`（`NacosAiService` trait、`NacosPrompt` + `render_template`、`AiServiceError`、`PromptListener`/`AgentCardListener`、`InMemoryNacosAiService`，含 `fire_prompt_update`/`fire_agent_card_update`/`released_cards`/`registered_endpoints`/`prompt_subscriptions`）
  - `src/a2a/mod.rs`、`src/a2a/model.rs`（`A2aAgentCard`/`NacosAgentCard` 双模型 + `NacosAgentEndpoint`）、`src/a2a/converter.rs`、`src/a2a/discovery.rs`、`src/a2a/registry/mod.rs`、`src/a2a/registry/constants.rs`
  - `src/prompt.rs`（`NacosPromptListener`）
  - `src/skill/mod.rs`（`NacosSkillRepository` 实现 harness `SkillRepository`）、`src/skill/frontmatter.rs`（ZIP 解析/重打包 + Nacos YAML 折叠）
- **复用 harness/core 既有抽象**：
  - nacos-skill **复用** `agentscope_harness::agent::skill::{SkillRepository, AgentSkill, SkillRepo}`（未新建平行仓库类型）。
  - nacos-a2a / nacos-prompt：codegraph 探查确认 harness **无** a2a/prompt 抽象（既无 `AgentCardResolver`/`AgentRegistry`，也无 prompt 仓库/listener trait），故本 crate 定义精简 trait 作为适配层——非「重复发明」，而是协议侧缺位下的本地缝隙，见 §4.2-1。
- **验证命令与结果**（2026-09-18，本会话）：
  - `cargo build -p agentscope-extensions-nacos` → **通过**（无 error；仅其它 crate 既有 warning）。
  - `cargo test  -p agentscope-extensions-nacos` → **通过：43 passed; 0 failed**（基线 27 → 本轮 +16）。
- **已知问题**：无。真实 Nacos 客户端与 protocol crate A2A 模型统一列为待裁定（§4.2）。

### 4.2 下一步与阻塞（跨模块需求）

1. **【跨模块依赖】protocol crate 的 `a2a` 子模块不可编译**：`crates/agentscope-extensions-protocol/src/a2a/mod.rs` 声明 `card/message/client/server` 等子模块，但目录下仅有 `model.rs`，且 `src/lib.rs` 仅含文档注释（无 `mod` 声明）。因「不改其它 crate」，本 crate 自建 `A2aAgentCard` 及 `AgentCardResolver`/`AgentRegistry` trait。待 PROTOCOL 落地后应合并为复用（避免重复发明——符合汇总方「避免重建平行类型」的约束）。**请汇总方裁定合并时机**。
2. **【汇总方/根依赖】`zip = 0.6` 现为 crate 内独立版本声明**（`base64` 复用 workspace）。若需统一版本，请汇总方登记根 `[workspace.dependencies]`。
3. **【跨模块】真实 Nacos AI 客户端实现未提供**：`NacosAiService` 仅给出 trait + 内存实现；真实 HTTP 实现是否另立 crate / 归入 protocol 侧，待汇总方裁定。
4. **【可选】nacos-skill 的 `load()` 为同步 trait 方法**：内部用 `block_on` 驱动 async 下载。若 harness 未来为 `SkillRepository` 增加 async 变体，应切换以避免 `block_in_place` 限制。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-nacos` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-nacos` / `cargo test -p agentscope-extensions-nacos` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-nacos/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。
