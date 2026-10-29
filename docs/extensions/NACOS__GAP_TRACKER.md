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
| Java 规模 | 主代码 11 个 `.java`（另 4 个 `pom.xml`）2095 行 / 92.3KB；测试 8 文件 2452 行 / 122.0KB；Java `@Test` 合计 120 |
| Rust crate | `crates/agentscope-extensions-nacos`（11 个 `.rs` + `Cargo.toml`，3050 行） |
| 迁移波次 | C |
| 当前状态 | **总判：进行中**（仍有 P1：真实 NacosAiService 未实现）—— **进行中**（三个子域主体已迁移，**2026-10-28 补齐 P0/P1 语义缺口**：security scheme 按 type 分派、supportedInterfaces 组装/拆分、prompt 推送语义、skill 资源 key/必填校验/zip 结构校验、A2A 类型复用 protocol crate、doOverwrite 去重前插、endpoint 去重。离线单测 **70 passed / 0 failed**，见 §7；**2026-10-29 复核**：§2/§3 所列六项缺口（supportedInterfaces 三元组、capabilities 强制语义、prompt 推送、skill 资源 key/必填/zip、SecurityScheme 五 type、A2A 平行类型）均已在 §7 关闭（依据 §7-1⋯§7-6 + `cargo test` 70 passed）；真实剩余见 §3.3 / §4.2） |

## 2. Java 清单与依赖映射

> Java 模块 = Nacos 配置/注册中心侧，与存储侧（JDBC/Redis/MongoDB…）不同：不实现 `DistributedStore`。经 codegraph 探查，harness `crates/agentscope-harness/src/agent/` 下**不存在** a2a/prompt 相关抽象（无 `a2a/` 目录，无 `AgentCard`/`AgentRegistry`/`AgentCardResolver`/prompt 仓库或 listener trait），故 a2a/prompt 两域使用本 crate 本地 trait 作为「协议侧未就绪」的适配层；skill 域**复用** harness `SkillRepository`。

### 2.1 nacos-a2a（A2A 注册与发现）

| Java 类 / 包 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `a2a.model.AgentCard`（Nacos 版子集） | Nacos 侧 AgentCard DTO | `a2a::model::NacosAgentCard` | 已完成 |
| `a2a.model.{AgentCapabilities,AgentSkill,AgentInterface,AgentProvider}` | Nacos 版子规格 | `a2a::model::{NacosAgentCapabilities,NacosAgentSkill,NacosAgentInterface,NacosAgentProvider}`（Nacos 侧）+ **A2A 侧复用** `agentscope_extensions_a2a_client::model::{AgentCapabilities,AgentSkill,AgentInterface,AgentProvider}`（本 crate 旧名 `A2aAgent*` 保留为 `pub use` 别名） | 已完成（2026-10-28 去平行类型）|
| Nacos `AgentEndpoint`（`registerAgentEndpoint` 载荷） | 注册到 Nacos 的单个 endpoint（transport/address/port/path/supportTls/**version**/protocol/query） | `a2a::model::NacosAgentEndpoint` + `from_transport(props, version)` | **本轮补齐**（此前直接注册 `NacosA2aTransportProperties`，丢失 `version` 字段且字段名为 `host` 而非 `address`） |
| `utils.AgentCardConverterUtil` | Nacos ↔ A2A AgentCard 双向转换（含 null/空集合语义） | `a2a::converter::NacosAgentCardConverter` + `convert_to_*` + `supported_interfaces`/`nacos_supported_interfaces`/`apply_supported_interfaces` | **已完成 2026-10-28**（supportedInterfaces 列表↔三元组组装/拆分已补；`capabilities.extendedAgentCard` ↔ `supportsAuthenticatedExtendedCard` 双向；`stateTransitionHistory` 写 Nacos 强制 `false`、读时不回填，见 §7-1） |
| `utils.SecuritySchemeConverterUtil`（209 行，包级私有） | Nacos `Map<String,SecurityScheme>` ↔ A2A `Map<String,SecurityScheme>` 按 `type` 分派 apiKey/http/oauth2/openIdConnect/mutualTLS，含 OAuth flows 与 scopes、`needsQuoting` 式 null 剔除 | `a2a::security_scheme::{convert_to_a2a_security_schemes,convert_to_nacos_security_schemes}` | **已完成 2026-10-28**（按 type 分派 + OAuth flows/scopes/pkceRequired 归一 + null 剔除；未知 type 读取时丢弃、写入时折叠为 `{}`，与 Java 一致，见 §7-2） |
| `registry.NacosA2aRegistry` | 向 Nacos 发布 AgentCard + Endpoint | `a2a::registry::NacosA2aRegistry` | 已完成（依赖 `NacosAiService` trait；endpoint 现带 card version） |
| `registry.NacosA2aRegistryProperties` | 注册选项（setAsLatest/enabledRegisterEndpoint/overwritePreferredTransport/transports） | `a2a::registry::NacosA2aRegistryProperties` + `Builder` + `builder_from` | 已完成 |
| `registry.NacosA2aRegistryTransportProperties` | 单个 transport endpoint 配置 | `a2a::registry::NacosA2aTransportProperties` + `Builder` | 已完成（`build()` 校验 `transport` 非空；`effective_protocol()` 复刻 `handleTlsIfNeeded`） |
| `registry.NacosA2aTransportPropertiesEnvParser` | 解析 `NACOS_A2A_AGENT_{TRANSPORT}_{ATTR}`；非法 port 抛 `INVALID_PARAM` | `a2a::registry::NacosA2aTransportPropertiesEnvParser` | **本轮补齐**：`parse`/`from_env` 改为返回 `AiServiceResult`，非法 port 返回 `Err(AiServiceError::INVALID_PARAM, "Invalid `port` value for transport `JSONRPC`: no_number")`，对齐 Java `NacosRuntimeException(NacosException.INVALID_PARAM, ..)`（此前静默置 0） |
| `registry.constants.Constants` | 协议/前缀/属性枚举 | `a2a::registry::constants`（含 `TransportPropertiesAttribute`） | 已完成（6 个属性 envKey/propertyKey 逐一对齐；`TRANSPORT_DEFAULT_PROTOCOL` 对应 `AiConstants.A2a.A2A_ENDPOINT_DEFAULT_PROTOCOL`） |
| `registry.NacosAgentRegistry` | `AgentRegistry` 实现；合并 deploy/env transports、覆写 preferredTransport、生成 URL | `a2a::registry::NacosAgentRegistry` + `TransportProperties` + `AgentRegistry` trait | **已完成 2026-10-28**（`build_transport_properties`/`generate_new_url`/`handleTlsIfNeeded`/`overwriteAttributes` 逐行对齐；`do_overwrite` 现为 `removeIf(equals)` 去重 + `insert(0, ..)` 前插，见 §7-3） |
| `discovery.NacosAgentCardResolver` | 从 Nacos 订阅并缓存 AgentCard（`AgentCardResolver` 实现） | `a2a::discovery::NacosAgentCardResolver` + `AgentCardResolver` trait | 已完成（`async_trait`；内存缓存 + 订阅回调更新，含推送更新单测；订阅去重 `agentCardUpdaters` 由 `card_listeners` map 等价覆盖） |
| `io.agentscope.core.a2a.agent.card.AgentCardResolver` / `...server.registry.AgentRegistry` / `io.a2a.spec.AgentCard` | 协议侧接口与 A2A 规范模型 | **A2A 模型复用** `agentscope_extensions_a2a_client::model`（`AgentCard`/`AgentSkill`/`AgentCapabilities`/`AgentProvider`/`AgentInterface`）；`A2aAgentCard` 仅为携带 `documentationUrl` 的薄包装（`Deref` 转发）；`AgentResolver`/`AgentRegistry` 仍为本 crate 精简 trait | **已完成 2026-10-28（部分）**：模型已复用；`AgentRegistry` 确实缺位（保留本地）；`AgentCardResolver` 保持 async（适配本 crate 的 async `NacosAiService`）；protocol `AgentCard` 缺 `documentationUrl` 记入 §4.2-5 |

### 2.2 nacos-prompt（Prompt 监听）

| Java 类 / 包 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `prompt.NacosPromptListener` | 经 `AiService.subscribePrompt` 获取并缓存；`{{var}}`/`${var}` 渲染；version/label/default 支持 | `prompt::NacosPromptListener` | 已完成（方法面/优先级/EMPTY_SENTINEL/default 回退均对齐；推送回调已对齐 Java，见 §7-4） |
| `Prompt`（`getTemplate`/`getVersion`/`render(Map)`） | prompt 载荷 + 渲染 | `service::NacosPrompt` + `service::render_template` | 已完成（`EMPTY_SENTINEL` 语义、未知占位符保留、default 渲染） |
| `AbstractNacosPromptListener#onEvent` | 推送回调更新缓存（仅丢 null key / null 载荷） | `service::PromptListener` 类型别名 + `prompt::apply_prompt_push` + `InMemoryNacosAiService::fire_prompt_update` | **已完成 2026-10-28**（Java `NacosPromptListener.java:50-64`：key 为 null/空 → 丢弃；`prompt != null && getTemplate() != null` → 写入，**空串模板照写**；Rust 现同语义：`apply_prompt_push` 仅拦空 key，空模板入库，见 §7-4） |
| `NacosPromptListener#getPrompt(key, version, label, args, default)` | 读取入口（缓存命中不重复订阅） | `prompt::NacosPromptListener::{get_prompt, get_prompt_with_args, get_prompt_with_default, get_prompt_full}` | 已完成（含 version/label 透传、缓存命中、default 回退单测） |

### 2.3 nacos-skill（技能仓库）

| Java 类 / 包 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `skill.NacosSkillRepository` | `AgentSkillRepository` 实现：从 Nacos 下载 skill ZIP，解析 SKILL.md 构建 `AgentSkill`；只读；version/label 优先级；`knownSkillNames` 枚举 | `skill::NacosSkillRepository` | **进行中**（**实现 harness `SkillRepository`**：`repo_id`/`source`/`is_writeable`/`repo_kind`/`load` + `write_skill`/`delete_skill` 只读拒绝 ✅；version/label 三级优先级与 `downloadSkillZipBytes` 分派 ✅；但 `getAllSkillNames` 无对应（harness 无「只取名字」方法），`getRepositoryInfo` 无对应结构，见 §6-缺口 S-3） |
| `SkillUtil.createFromZip` / `MarkdownSkillParser` | 从 zip 构建 skill（含资源条目、二进制 base64 回退） | `skill::frontmatter::create_from_zip` + `AgentSkill::builder` | **已完成 2026-10-28**（zip 结构校验、资源 key 相对根目录、必填 name/description/正文校验均对齐 Java；见 §7-5） |
| `NacosSkillRepository#adaptNacosSkillZipForYamlFrontmatter` / `normalizeFoldedFlatYaml` | 折叠 Nacos 导出缩进 YAML 续行并重打包 zip | `skill::frontmatter::{adapt_nacos_skill_zip, adapt_skill_md_frontmatter, normalize_folded_flat_yaml}` | 已完成（纯函数；含 `needsQuoting` 引号/转义复刻） |
| `io.agentscope.core.skill.repository.AgentSkillRepository` / `AgentSkill` | 技能仓库接口/模型 | **harness `agentscope_harness::agent::skill::{SkillRepository, AgentSkill, SkillRepo}`（复用，不重建）** | 已完成 |

### 2.4 依赖映射（Java → Rust）

| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| `com.alibaba.nacos.api.ai.AiService` | Nacos AI 客户端（subscribePrompt / releaseAgentCard / registerAgentEndpoint / downloadSkillZip*） | `service::NacosAiService` trait（async） | Nacos 无官方 Rust AI SDK → 抽象为 trait；`InMemoryNacosAiService` 为测试/嵌入式实现；真实 HTTP 实现待裁定（§4.2-3） |
| `AiFactory.createAiService(Properties)` | 建 Nacos 客户端 | 由调用方注入 `Arc<dyn NacosAiService>` | 依赖倒置 |
| `com.alibaba.nacos.api.exception.NacosException` | 错误码（`NOT_FOUND`/`INVALID_PARAM`） | `service::AiServiceError`（`is_not_found`；常量 `NOT_FOUND=404`/`INVALID_PARAM=400`） | 已完成 |
| `com.alibaba.nacos.common.utils.{StringUtils,JacksonUtils}` | 字符串/JSON | std + `serde_json` | |
| `io.a2a.spec.AgentCard` 等 | A2A 规范模型 | **复用** `agentscope_extensions_a2a_client::model::{AgentCard, AgentSkill, AgentCapabilities, AgentProvider, AgentInterface}`（新增 path 依赖 `agentscope-extensions-a2a-client`）| 2026-10-28 完成；`SecurityScheme` 在 protocol 中为 `= Value`，故本 crate 用 `a2a::security_scheme` 做 type 分派 |
| `java.util.zip.*` / `java.util.Base64` | zip 读写 / 二进制资源 | `zip = 0.6`、`base64`（本 crate `[dependencies]`） | 未改根 `[workspace.dependencies]`（`base64` 走 workspace） |
| `ConcurrentHashMap` | 本地缓存 | `std::sync::Mutex<HashMap<..>>` | 单进程嵌入式场景足够 |

## 3. 详细缺口清单

### 3.1 骨架与依赖（P0）

| 缺口项 | Java 参考 | 优先级 | 执行状态 | 交接信息/说明 |
|---|---|---|---|---|
| crate 骨架 + 依赖声明 | — | P0 | 已完成 | 本 crate `[dependencies]` 增 `zip = 0.6`；`base64` 复用 workspace；未改根 `workspace.dependencies` |
| nacos-a2a 模型 + 转换 | `a2a.model.*`, `utils.AgentCardConverterUtil` | P0 | 已完成 | §2.1；字段级镜像 + interfaces 三元组组装/拆分 + capabilities 双向强制语义（§7-1） |
| nacos-a2a security scheme 转换 | `utils.SecuritySchemeConverterUtil` | P1 | 已完成 | §7-2；`a2a::security_scheme` 按 type 分派，含 OAuth flows/scopes/pkceRequired |
| nacos-a2a registry + properties + env parser | `registry.*` | P0 | 已完成 | §2.1；env parser 非法 port 现返回 `INVALID_PARAM`（对齐 Java） |
| nacos-a2a discovery resolver | `discovery.NacosAgentCardResolver` | P0 | 已完成 | §2.1；缓存 + 推送更新单测 |
| nacos-prompt listener | `prompt.NacosPromptListener` | P0 | 已完成 | §2.2；推送回调语义已对齐（空 key 丢弃、空模板照写，§7-4） |
| nacos-skill repository + zip/frontmatter | `skill.NacosSkillRepository` | P0 | 已完成 | §2.3；资源 key / 必填校验 / zip 结构校验均对齐（§7-5） |
| endpoint 载荷保真（`version`/`address`） | Nacos `AgentEndpoint` | P1 | 已完成 | 新增 `a2a::model::NacosAgentEndpoint` + `from_transport`，由 `register_endpoints` 使用 |

### 3.2 设计决定（不迁移 / 延后）

| 项 | 优先级 | 决定 | 说明 |
|---|---|---|---|
| 真实 Nacos HTTP 客户端 | P1 | 设计决定（延后） | **2026-10-29 复核**：仍仅 trait + `InMemoryNacosAiService`，`reqwest` 实现未落地；无成熟 Rust Nacos AI SDK；`NacosAiService` trait 为注入缝隙，与 Java `AiService` 注入点对齐；HTTP 实现归属待裁定（§4.2-3） |
| 与 protocol crate 的 A2A 模型统一 | P1 | 已完成（2026-10-28） | 已复用 `agentscope_extensions_a2a_client::model` 的 `AgentSkill`/`AgentCapabilities`/`AgentProvider`/`AgentInterface`；`A2aAgentCard` 降为携带 `documentationUrl` 的薄包装（`Deref`）；`AgentRegistry` 仍本地（protocol 无）。protocol 若补 `documentation_url` 则可退化为 `pub use`（§4.2-5） |
| `AgentCardResolver` 同步→异步 | P0 | 设计决定 | Java 版阻塞（Nacos 客户端阻塞）；Rust 侧 `NacosAiService` 为 async，故 trait 用 `async_trait` |
| `NacosA2aRegistry` 多 endpoint 批量注册 | P2 | 设计决定 | Java 在 1 个 endpoint 时用单值、≥2 时用 `Set` 去重批量；Rust 侧统一逐个 `register_agent_endpoint`（调用次数差异可接受；但 `Set` 的去重语义不可忽略——见 §3.3 同名条目） |
| `NacosSkillRepository` 的 `load()` 同步 trait | P2 | 设计决定 | harness `SkillRepository::load` 为同步 → 内部 `block_on` 驱动 async 下载（对齐 Java 阻塞 `getAllSkills`）；有 runtime 时走 `block_in_place`，测试无 runtime 时走临时 current-thread runtime |

### 3.3 剩余缺口（增强项，非阻塞）

| 缺口项 | 优先级 | 状态 | 说明 |
|---|---|---|---|
| 真实 Nacos HTTP 客户端（`reqwest` 版 `NacosAiService`） | P1 | 待办（跨模块） | 见 §4.2-3；**2026-10-29 复核**：未变（仍无 `reqwest` 实现） |
| 与 protocol crate A2A 模型/接口合并（复用 `AgentCard`/`AgentCardResolver`，仅保留 `AgentRegistry` 本地补位） | P1 | 已完成（模型部分） | 见 §7-6；`TransportProperties` 仍本地（protocol 同名类型在 `a2a-server`，会拖入 axum 且 host/port 为 Option，见 §4.2-6） |
| A2A security scheme 结构化转换（apiKey/http/oauth2/openIdConnect/mutualTLS + OAuth flows） | P1 | 已完成 | 见 §7-2 |
| `do_overwrite` 的接口去重与前插（Java `removeIf(equals)` + `add(0, ..)`） | P2 | 已完成 | 见 §7-3；`do_overwrite_dedups_and_prepends_the_interface` |
| `NacosAgentRegistry` 批量 endpoint 去重（Java `Set` 语义） | P2 | 已完成 | 见 §7-3；`duplicate_endpoints_are_registered_once` |
| `AgentCardResolver` 缓存失效/TTL 策略 | P2 | 待办 | Java 亦仅靠订阅推送更新、无 TTL；当前保持一致 |

## 4. 项目状态与下一步

### 4.1 当前状态

- **状态：已完成**。三个子模块 nacos-a2a / nacos-prompt / nacos-skill 均已迁移并含离线单测。
- **文件清单**（均在本 crate 内，未触碰其它 crate）：
  - `crates/agentscope-extensions-nacos/Cargo.toml`
  - `src/lib.rs`（导出 `a2a`/`prompt`/`service`/`skill` + `NacosAiService`/`AiServiceError`/`AiServiceResult`/`InMemoryNacosAiService`）
  - `src/service.rs`（`NacosAiService` trait、`NacosPrompt` + `render_template`、`AiServiceError`、`PromptListener`/`AgentCardListener`、`InMemoryNacosAiService`，含 `fire_prompt_update`/`fire_agent_card_update`/`released_cards`/`registered_endpoints`/`prompt_subscriptions`）
  - `src/a2a/mod.rs`、`src/a2a/model.rs`（`A2aAgentCard` 薄包装 + `NacosAgentCard` 双模型 + `NacosAgentEndpoint`；A2A 子类型 `pub use` 自 protocol crate）、`src/a2a/converter.rs`（含 interfaces 组装/拆分）、`src/a2a/security_scheme.rs`（**新增 2026-10-28**）、`src/a2a/discovery.rs`、`src/a2a/registry/mod.rs`、`src/a2a/registry/constants.rs`
  - `src/prompt.rs`（`NacosPromptListener`）
  - `src/skill/mod.rs`（`NacosSkillRepository` 实现 harness `SkillRepository`）、`src/skill/frontmatter.rs`（ZIP 解析/重打包 + Nacos YAML 折叠）
- **复用 harness/core 既有抽象**：
  - nacos-skill **复用** `agentscope_harness::agent::skill::{SkillRepository, AgentSkill, SkillRepo}`（未新建平行仓库类型）。
  - nacos-a2a / nacos-prompt：codegraph 探查确认 harness **无** a2a/prompt 抽象（既无 `AgentCardResolver`/`AgentRegistry`，也无 prompt 仓库/listener trait），故本 crate 定义精简 trait 作为适配层——非「重复发明」，而是协议侧缺位下的本地缝隙，见 §4.2-1。
- **验证命令与结果**（2026-09-18，本会话）：
  - `cargo build -p agentscope-extensions-nacos` → **通过**（无 error；仅其它 crate 既有 warning）。
  - `cargo test  -p agentscope-extensions-nacos` → **通过：43 passed; 0 failed**（基线 27 → 本轮 +16）。
- **验证命令与结果**（2026-10-28，本会话）：
  - `cargo build -p agentscope-extensions-nacos` → **exit 0**（本 crate 0 warning）。
  - `cargo test -p agentscope-extensions-nacos` → **70 passed; 0 failed; 0 ignored**（43 → +27，含 10 条反向验证用例）。
  - `cargo clippy -p agentscope-extensions-nacos --all-targets` → 仅 2 条**历史** warning（`service.rs:183` type_complexity、`registry/mod.rs:742` bool_assert_comparison，均非本轮新增）。
- **已知问题**：无编译/测试阻塞。**2026-10-29 复核**：protocol crate A2A 模型统一已完成（§7-6）；**`documentationUrl`、`TransportProperties`、`zip` 三项跨模块请求均已关闭**（§4.2-5 / §4.2-6 / §4.2-2）。**真实剩余**：①真实 `NacosAiService`（`reqwest`）未实现（仅 trait + `InMemoryNacosAiService`，归属待裁定，§4.2-3）；②`getAllSkillNames`/`getRepositoryInfo` 无对应（harness `SkillRepository` 缺方法，S-3）；③`SkillRepository::load` 同步 trait 内 `block_on`（§4.2-4）；④`AgentCardResolver` 无 TTL（§3.3）。

### 4.2 下一步与阻塞（跨模块需求）

1. ~~**【跨模块依赖】protocol crate 的 `a2a` 子模块不可编译**~~ → **已勘误并关闭（见 §6.4-1）**：protocol 的 A2A 模型可编译，本 crate 已于 2026-10-28 复用（`agentscope-extensions-a2a-client` 为新增 path 依赖）。仅 `AgentRegistry` 仍是本 crate 本地 trait。
2. ~~**【汇总方/根依赖】`zip = 0.6` 现为 crate 内独立版本声明**~~ → **已完成（2026-10-29）**：`zip = "0.6"` 已登记根 `[workspace.dependencies]`（`default-features = false, features = ["deflate"]`），本 crate 改为 `zip = { workspace = true }`。
3. **【跨模块】真实 Nacos AI 客户端实现未提供**：`NacosAiService` 仅给出 trait + 内存实现；真实 HTTP 实现是否另立 crate / 归入 protocol 侧，待汇总方裁定。
4. **【可选】nacos-skill 的 `load()` 为同步 trait 方法**：内部用 `block_on` 驱动 async 下载。若 harness 未来为 `SkillRepository` 增加 async 变体，应切换以避免 `block_in_place` 限制。
5. ~~**【跨模块】protocol `AgentCard` 缺 `documentation_url`**~~ → **已完成（2026-10-29）**：`agentscope_extensions_a2a_client::model::AgentCard` 已新增 `documentation_url`（serde `documentationUrl`，往返用例 `agent_card_serializes_documentation_url`）；本 crate 的 `A2aAgentCard` 已删除本地字段，仅保留 `#[serde(flatten)] inner: AgentCard` 包装（`Deref`/`DerefMut` 转发字段），未改用 `pub type`（保留 `new`/getter 便利方法）。
6. ~~**【跨模块】`TransportProperties` 未复用**~~ → **已完成（2026-10-29）**：该类型已从 `agentscope-extensions-a2a-server::card` 下沉到 `agentscope-extensions-a2a-client::transport_properties`（server 侧改为 re-export，公共路径 `a2a::card::TransportProperties` 不变）。本 crate **仍保留自己的必填版** `a2a::registry::TransportProperties`（`host: String` / `port: i32`），因其来自 `AgentRegistry::register` 的必填语义，与共享版 `Option` 形态不同，不属重复实现。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-nacos` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-nacos` / `cargo test -p agentscope-extensions-nacos` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-nacos/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. 静态复核记录（2026-10-22）

> **复核方式**：纯静态（逐类打开 Java 源码 + 逐符号核对 Rust 实现），本节为 **2026-10-22 快照**，缺口已在 2026-10-28 关闭（以 §7 为准）。
> **口径**：行数为**非空行**；Rust 测试数为 `#[test]`/`#[tokio::test]` 属性计数。

### 6.1 实测统计

| 项 | Java | Rust |
|---|---|---|
| 规模 | 主 11 个 `.java`（另 4 个 `pom.xml`）**2095 行** / 92.3KB；测试 8 文件 **2452 行** / 122.0KB | 11 个 `.rs` + `Cargo.toml`，**2750 行** |
| 测试 | **120 个 `@Test`** | **43** 个（与 tracker 记载一致） |
| 子模块 | nacos-a2a / nacos-prompt / nacos-skill | 同名目录 `a2a/` / `prompt.rs` / `skill/` |

> rollup 原记 `1873 / 2378` 已过期，应为 **2095 / 2452**。

### 6.2 逐子域对齐结论

| 子域 | 结论 | 依据 |
|---|---|---|
| nacos-a2a 模型 | **已完成** | `NacosAgentEndpoint`（含 `version`/`address`）与 env parser 错误语义已补齐；2026-10-28 补齐 `supportedInterfaces` 组装/拆分、`extendedAgentCard` 与 `stateTransitionHistory=false` 强制语义（§7-1） |
| nacos-a2a 转换 | **已完成** | 字段级镜像 + `SecuritySchemeConverterUtil` 等价的 `a2a::security_scheme`（按 type 分派，§7-2） |
| nacos-a2a registry | **已完成** | `generate_new_url`/`handleTlsIfNeeded`/`overwriteAttributes` 逐行对齐；`do_overwrite` 已补 `removeIf(equals)` + `add(0, ..)`，endpoint 已补 `Set` 去重（§7-3） |
| nacos-a2a discovery | **已完成** | `async_trait` + 内存缓存 + 订阅回调更新，订阅去重等价覆盖 |
| nacos-prompt | **已完成** | 方法面/优先级/`EMPTY_SENTINEL`/default 回退均对齐；推送回调已对齐 Java（只丢空 key，空模板照写，§7-4） |
| nacos-skill | **已完成** | harness `SkillRepository` 只读语义、version/label 三级优先级对齐；资源 key（相对 root）/必填字段/zip 结构校验均对齐（§7-5） |

### 6.3 缺口清单

| ID | 缺口 | Java 参考 | 优先级 | 状态 |
|---|---|---|---|---|
| **A2A-1** | `SecuritySchemeConverterUtil`（209 行，包级私有）无对应实现——Rust `security_schemes` 仅为 opaque `serde_json::Map` 原样透传 | `utils.SecuritySchemeConverterUtil` | **P1** | **已完成**（§7-2） |
| **A2A-2** | `supportedInterfaces` 列表↔三元组组装/拆分缺失；`capabilities.extendedAgentCard`、`stateTransitionHistory=false` 强制语义缺失 | `utils.AgentCardConverterUtil` | **P0** | **已完成**（§7-1） |
| **A2A-3** | 应复用 protocol 的 `AgentCard`/`AgentSkill`/…，仅 `AgentRegistry` 本地补位 | `io.agentscope.core.a2a.agent.card.*` | **P1**（跨模块） | **已完成**（§7-6，`A2aAgentCard` 保留薄包装，见 §4.2-5） |
| **A2A-4** | `do_overwrite` 缺 Java 的 `removeIf(equals)` 去重 + `add(0, ..)` 前插 | `registry.NacosAgentRegistry` | P2 | **已完成**（§7-3） |
| **A2A-5** | `NacosAgentRegistry` 批量 endpoint 无 `Set` 去重 | `registry.NacosA2aRegistry` | P2 | **已完成**（§7-3） |
| **P-1** | prompt 推送回调语义相反（空模板被丢弃、未判空 key） | `NacosPromptListener#onEvent` | **P1** | **已完成**（§7-4） |
| **S-1** | skill 资源 key 语义与 Java 不一致（全路径 vs 相对根目录，且未排除 SKILL.md） | `SkillUtil.createFromZip` | **P0** | **已完成**（§7-5） |
| **S-2** | skill 必填字段校验、zip 结构校验缺失 | `SkillUtil.createFromZip` / `createFrom` | **P1** | **已完成**（§7-5） |
| **S-3** | `getAllSkillNames` 无对应（harness 无“只取名字”方法）、`getRepositoryInfo` 无对应结构 | `NacosSkillRepository` | P2 | 待办（harness `SkillRepository` 无对应方法；`location()` 已提供 `getRepositoryInfo().location` 等价值） |

### 6.4 被证伪的既有结论（勘误）

| # | 原文 | 实际 |
|---|---|---|
| 1 | 「protocol crate 的 `a2a` **不可编译**，故本 crate 自建 `A2aAgentCard`/`AgentCardResolver`/`AgentRegistry`」 | **前提已证伪**：`crates/agentscope-extensions-protocol/src/a2a/{card,client,message,model,server}.rs` 均存在，`src/lib.rs` 已 `pub mod a2a`；`card.rs` 已有 `AgentCardResolver` trait，`model.rs` 已有 `AgentCard`/`AgentSkill`/`AgentCapabilities`/`AgentProvider`/`AgentInterface`/`TransportProperties`。**唯一真缺位的是 `AgentRegistry`** → 本 crate 的平行类型属**重复实现债务** |
| 2 | Java 规模「主 18 文件 ~68KB / 测试 8 文件 ~137KB」 | 主 **11 个 `.java` + 4 个 `pom.xml`，2095 行 / 92.3KB**；测试 8 文件 **2452 行 / 122.0KB** |
| 3 | 「`NacosA2aRegistry` 单/批量注册：去重差异忽略」 | 去重语义**不可忽略**——Rust 逐个 `register_agent_endpoint` 且不去重，Nacos 侧会收到重复 endpoint |
| 4 | 「`io.a2a.spec.AgentCard` → 本 crate 本地模型」 | 应改为**复用** protocol 的 `AgentCard`（见勘误 1） |

### 6.5 对 rollup 的建议

- 状态由 `已完成` 下调为 **`进行中`**：三子域主体已迁移、43 个离线单测静态计数一致，但存在 P0×2（A2A-2 / S-1）与 P1×5，且「自建 A2A 类型」的前提已失效。
- rollup 行数应改为 **2095 / 2452**，测试数由 `43` 改为 **`70`**（2026-10-28）。
- ~~需汇总方裁定：是否把 `A2aAgentCard` 等改为复用 `agentscope-extensions-protocol`~~ → **已执行（§7-6）**，仅剩 `documentation_url`（§4.2-5）与 `TransportProperties`（§4.2-6）两项跨模块请求。

## 7. 2026-10-28 执行记录（本轮：A2A-1/2/3/4/5、P-1、S-1/S-2）

> 口径：只改本 crate 与本文文件；Java 参考 `D:\IdeaProjects\agentscope-java` HEAD `16cb024ae`（行号为该文件实际行号）。
> 验证：`cargo build -p agentscope-extensions-nacos` → exit 0；`cargo test -p agentscope-extensions-nacos` → **70 passed / 0 failed / 0 ignored**（基线 43）。

### 7-1 第 2 项：`supportedInterfaces` 三元组组装/拆分 + 两个强制语义（A2A-2，P0）

- **Java**：`AgentCardConverterUtil.java:92-122`（`convertToA2aAgentInterfaces`：url + `preferredTransport`（缺省 `"JSONRPC"`）首位，后续 `additionalInterfaces` 按 `!result.contains(each)` 去重，`protocolVersion` 缺省用 `AgentInterface.CURRENT_PROTOCOL_VERSION`）、`：124-135`（`extendedAgentCard = Boolean.TRUE.equals(supportsAuthenticatedExtendedCard)`，只读 streaming/pushNotifications）、`:161-193`（A2A→Nacos：首接口→`protocolVersion`/`url`/`preferredTransport`；`additionalInterfaces = mapList(interfaces, …)` **含首接口**）、`:194-202`（`setStateTransitionHistory(false)` 强制）。
- **Rust**：`a2a/converter.rs` 新增 `supported_interfaces(card)`（A2A 卡）、`nacos_supported_interfaces(card)`、`apply_supported_interfaces(card, &[…])`（拆分：首元素→`url`/`preferredTransport`，其余→`additionalInterfaces`）。
  - Nacos→A2A：`capabilities.streaming/push_notifications` 物化为 `Some(unwrap_or(false))`；`state_transition_history` **不回填**（A2A 1.0 无此字段）；`supports_authenticated_extended_card = Some(unwrap_or(false))`（对应 `capabilities.extendedAgentCard`）。
  - A2A→Nacos：`state_transition_history = Some(false)` 强制；`additional_interfaces` 含全部接口；无接口时 `url`/`preferred_transport`/`protocol_version` 均为 `None`。
- **已知模型差异**：protocol `AgentCard` 是 A2A 0.3 形态（`url`/`preferredTransport`/`additionalInterfaces` 为独立字段，且 `url` 非 Option），故：空 `url` 视为无首接口（Java 只跳 `null`）；`protocolVersion` 由卡片携带而非每接口。
- **测试**：`a2a_to_nacos_lists_every_interface`、`nacos_interfaces_default_transport_and_dedup`、`nacos_card_without_url_has_no_preferred_interface`、`apply_supported_interfaces_puts_first_as_preferred`、`nacos_capabilities_become_definite_booleans`、`state_transition_history_is_forced_false_on_nacos`、`round_trips_card_fields`。
- **反向验证**：去掉 `assemble_interfaces` 的去重 → `nacos_interfaces_default_transport_and_dedup` + `round_trips_card_fields` FAILED；`state_transition_history` 改回 `None` → `state_transition_history_is_forced_false_on_nacos` FAILED。

### 7-2 第 1 项：`SecuritySchemeConverterUtil`（A2A-1，P1）

- **Java**：`SecuritySchemeConverterUtil.java:46-61`（读：null 入参→null；不可描述的 scheme 丢弃）、`:63-90`（按 `String.valueOf(get("type"))` 分派 apiKey/http/oauth2/openIdConnect/mutualTLS，default→null）、`:92-99`（`in` 必须匹 `APIKeySecurityScheme.Location.asString()`，否则整条丢弃）、`:101-127`（OAuth flows 只留 authorizationCode/clientCredentials/deviceCode，scopes 缺省 `Map.of()`，`pkceRequired = Boolean.TRUE.equals(...)`）、`:129-167`（写：未知子类→空 map）、`:169-203`（写 flows 字段顺序 + `putIfPresent`）。
- **Rust**：新增 `crates/agentscope-extensions-nacos/src/a2a/security_scheme.rs`：`convert_to_a2a_security_schemes` / `convert_to_nacos_security_schemes` + `convert_to_a2a_security_scheme` / `convert_to_nacos_security_scheme` / `convert_to_a2a_oauth_flows` / `convert_to_nacos_oauth_flows` / `scopes` / `put_string` / `put_value`。
  - **未知 type 行为**：读（Nacos→A2A）**丢弃**；写（A2A→Nacos）折叠为 `{}`（等价 Java `new SecurityScheme()` 空 map）。
  - 已接入 `converter.rs` 双向；`security`（requirements）仍原样透传（Java 亦然）。
- **测试**（8 条）：`reads_every_supported_type`、`unknown_type_and_bad_api_key_location_are_dropped`、`oauth_flows_keep_only_the_typed_flows`、`oauth_flows_are_normalised`、`writes_the_a2a_03_shape`、`round_trips_every_scheme_kind`、`oauth2_without_flows_gets_an_empty_flows_object`、`absent_schemes_stay_absent`。
- **反向验证**：`_ => return None` 改为保留未知 type → `unknown_type_and_bad_api_key_location_are_dropped` FAILED。

### 7-3 第 6 项：`do_overwrite` 去重前插 + endpoint 去重（A2A-4/A2A-5，P2）

- **Java**：`NacosAgentRegistry.java:201-221`（`removeIf(agentInterface::equals)` + `add(0, agentInterface)`，首接口即 preferred）、`:170-193`（覆写入口）；`NacosA2aRegistry.java:105-133`（≥2 时 `collect(Collectors.toSet())` 去重再注册）。
- **Rust**：`registry/mod.rs::do_overwrite` 改为 `supported_interfaces` → `retain(!= new)` → `insert(0, new)` → `apply_supported_interfaces`（不再 `push` 追加）；`register_endpoints` 先 `Vec::contains` 去重再逐个注册。
- **测试**：`do_overwrite_dedups_and_prepends_the_interface`、`duplicate_endpoints_are_registered_once`。
- **反向验证**：`insert(0)` 改 `push` → `do_overwrite_dedups_and_prepends_the_interface` FAILED；删 `contains` 判断 → `duplicate_endpoints_are_registered_once` FAILED。

### 7-4 第 3 项：prompt 推送回调语义（P-1，P1）

- **Java**：`NacosPromptListener.java:42-64`——event null→return；`key == null || key.isEmpty()` → 丢弃（warn）；`prompt != null && prompt.getTemplate() != null` → `prompts.put(key, prompt)`（**空串模板照写**）；否则 warn。
- **Rust**：新增 `prompt::apply_prompt_push(&mut HashMap, key, &NacosPrompt) -> bool`：仅 `key.is_empty()` 时丢弃（`prompt` 为引用，不存在 null 载荷）；**空模板直接入库**。`subscribe_and_load` 的闭包改为调它（并改为毒锁容错）。
- **测试**：`empty_template_push_is_stored_as_is`、`push_with_empty_key_is_dropped`（原有 `cache_updates_on_push` 仍绿）。
- **反向验证**：`if key.is_empty()` 改 `if key.is_empty() || prompt.template.is_empty()` → 上述 2 条 FAILED。

### 7-5 第 4 项：skill 资源 key / 必填校验 / zip 结构（S-1/S-2，P0+P1）

- **Java**：`SkillUtil.java:290-355`（`createFromZip`：跳目录；`normalizeZipEntryName`；`separatorIndex <= 0` → “under a single root directory”；不同 root → 报错；`SKILL.md` 必须在根目录直接下（`endsWith("/SKILL.md") && != root/SKILL.md`）；**资源 key = 相对 root 的路径**（`:343-344`），`SKILL.md` 不入 resources；缺失 → “SKILL.md not found in zip content.”）、`:357-373`（相对路径/禁止 `..`/反斜杠归一）、`:90-112`（必填 name/description：必须是非空字符串，否则 `IllegalArgumentException`；正文为空也报错）、`:387-399`（`stringifyRequiredMetadata`：非 String 或 blank → null）。
- **Rust**：重写 `skill/frontmatter.rs::create_from_zip(zip_bytes, source)`（**去掉了 `fallback_name` 形参**，Java 无此回退，缺 name 直接报错）；新增 `build_skill` / `required_metadata` / `parse_skill_md`（返回 metadata + 正文）/ `normalize_entry_name -> Result`；错误文案逐字对齐 Java。二进制资源仍 `base64:` 前缀（Java 按 charset 有损解码，Rust 保留原字节，已在注释说明）。
- **测试**：`parses_skill_from_zip`（key `extra.txt`、SKILL.md 不入 resources、metadata 含 name/description）、`resource_paths_are_relative_to_the_root`、`backslash_entry_names_are_normalized`、`entries_without_a_single_root_are_rejected`、`nested_skill_md_is_rejected`、`parent_directory_segments_are_rejected`、`missing_name_is_rejected`、`missing_description_is_rejected`、`empty_body_is_rejected`、`empty_zip_is_rejected`、`binary_resources_are_base64_encoded`。
- **反向验证**：资源 key 改回全路径 → 4 条 skill 测试 FAILED；`required_metadata` 去错误分支 → `missing_name_is_rejected`/`missing_description_is_rejected` FAILED。

### 7-6 第 5 项：复用 protocol crate 的 A2A 类型（A2A-3，P1）

- **新增依赖**（仅本 crate `Cargo.toml`）：`agentscope-extensions-a2a-client = { path = "../agentscope-extensions-a2a-client" }`（未改根 `Cargo.toml`；`Cargo.lock` 自动新增 1 条依赖边）。
- **已删除的平行类型**（`a2a/model.rs`）：`A2aAgentCapabilities` / `A2aAgentProvider` / `A2aAgentInterface` / `A2aAgentSkill` → 改为 `pub use agentscope_extensions_a2a_client::model::{AgentCapabilities as A2aAgentCapabilities, …}`（公开 API 名字不变）。
- **`A2aAgentCard`**：降为 `#[serde(flatten)] inner: AgentCard` + `documentation_url` 的薄包装，`Deref`/`DerefMut` 转发字段（`card.url` 等调用点无需改）；只因 protocol `AgentCard` 无 `documentationUrl`（§4.2-5）。
- **未复用**：`AgentRegistry`（protocol 确无）、`AgentCardResolver`（本 crate 为 async 适配 `NacosAiService`）、`TransportProperties`（§4.2-6）。
- **测试**：`a2a_card_serializes_documentation_url_flat`（flatten 序列化/反序列化往返）、`provider_without_url_round_trips_as_none`、`card_json_field_names_match_nacos`。

### 7-7 未做 / 顺带发现

- **S-3 未做**：`getAllSkillNames`/`getRepositoryInfo` 无对应（harness `SkillRepository` 无对应方法；`NacosSkillRepository::location()` 已提供 `getRepositoryInfo().location` 等价值）。
- 既有测试中编码了错语义的 3 条已**收紧**到 Java 语义（非放宽）：`parse_frontmatter_uses_fallback_name` → `missing_name_is_rejected`；`resource_paths_are_normalized_to_forward_slashes` → `resource_paths_are_relative_to_the_root`；`empty_capabilities_round_trip_as_defaults` → `nacos_capabilities_become_definite_booleans`；`security_schemes_kept_as_opaque_json` → `security_schemes_are_converted_by_type`。
- `NacosAgentCard.url` 由 `String` 改为 `Option<String>`（对齐 Nacos/Java 可空 url）；调用点 `registry` 测试已同步。

