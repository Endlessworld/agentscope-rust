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
| 当前状态 | **已完成**（P1/P2 清零；2026-10-31 实测 92 passed / 0 failed（默认与 `--features http-client` 同））—— 真实 `NacosAiService`（`src/http.rs` + `NacosHttpTransport` seam + `ReqwestNacosTransport`，含 `poll_once`/`spawn_polling`）已落地；P0/P1 语义缺口与 TTL/`invalidate`、A2A capability extensions 保留（Java `5da592b4a`）全部关闭；仅真实 Nacos 服务联调需环境（非代码缺口）。详见 history/NACOS__MIGRATION_HISTORY.md |

## 2. Java 清单与依赖映射

> Java 模块 = Nacos 配置/注册中心侧，与存储侧（JDBC/Redis/MongoDB…）不同：不实现 `DistributedStore`。经 codegraph 探查，harness `crates/agentscope-harness/src/agent/` 下**不存在** a2a/prompt 相关抽象（无 `a2a/` 目录，无 `AgentCard`/`AgentRegistry`/`AgentCardResolver`/prompt 仓库或 listener trait），故 a2a/prompt 两域使用本 crate 本地 trait 作为「协议侧未就绪」的适配层；skill 域**复用** harness `SkillRepository`。

### 2.1 nacos-a2a（A2A 注册与发现）

| Java 类 / 包 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `a2a.model.AgentCard`（Nacos 版子集） | Nacos 侧 AgentCard DTO | `a2a::model::NacosAgentCard` | 已完成 |
| `a2a.model.{AgentCapabilities,AgentSkill,AgentInterface,AgentProvider}` | Nacos 版子规格 | `a2a::model::{NacosAgentCapabilities,NacosAgentSkill,NacosAgentInterface,NacosAgentProvider}`（Nacos 侧）+ **A2A 侧复用** `agentscope_extensions_a2a_client::model::{AgentCapabilities,AgentSkill,AgentInterface,AgentProvider}`（本 crate 旧名 `A2aAgent*` 保留为 `pub use` 别名） | 已完成（2026-10-28 去平行类型）|
| Nacos `AgentEndpoint`（`registerAgentEndpoint` 载荷） | 注册到 Nacos 的单个 endpoint（transport/address/port/path/supportTls/**version**/protocol/query） | `a2a::model::NacosAgentEndpoint` + `from_transport(props, version)` | **本轮补齐**（此前直接注册 `NacosA2aTransportProperties`，丢失 `version` 字段且字段名为 `host` 而非 `address`） |
| `utils.AgentCardConverterUtil` | Nacos ↔ A2A AgentCard 双向转换（含 null/空集合语义） | `a2a::converter::NacosAgentCardConverter` + `convert_to_*` + `supported_interfaces`/`nacos_supported_interfaces`/`apply_supported_interfaces` | **已完成 2026-10-28**（supportedInterfaces 列表↔三元组组装/拆分已补；`capabilities.extendedAgentCard` ↔ `supportsAuthenticatedExtendedCard` 双向；`stateTransitionHistory` 写 Nacos 强制 `false`、读时不回填，见 history/NACOS__MIGRATION_HISTORY.md） |
| `utils.SecuritySchemeConverterUtil`（209 行，包级私有） | Nacos `Map<String,SecurityScheme>` ↔ A2A `Map<String,SecurityScheme>` 按 `type` 分派 apiKey/http/oauth2/openIdConnect/mutualTLS，含 OAuth flows 与 scopes、`needsQuoting` 式 null 剔除 | `a2a::security_scheme::{convert_to_a2a_security_schemes,convert_to_nacos_security_schemes}` | **已完成 2026-10-28**（按 type 分派 + OAuth flows/scopes/pkceRequired 归一 + null 剔除；未知 type 读取时丢弃、写入时折叠为 `{}`，与 Java 一致，见 history/NACOS__MIGRATION_HISTORY.md） |
| `registry.NacosA2aRegistry` | 向 Nacos 发布 AgentCard + Endpoint | `a2a::registry::NacosA2aRegistry` | 已完成（依赖 `NacosAiService` trait；endpoint 现带 card version） |
| `registry.NacosA2aRegistryProperties` | 注册选项（setAsLatest/enabledRegisterEndpoint/overwritePreferredTransport/transports） | `a2a::registry::NacosA2aRegistryProperties` + `Builder` + `builder_from` | 已完成 |
| `registry.NacosA2aRegistryTransportProperties` | 单个 transport endpoint 配置 | `a2a::registry::NacosA2aTransportProperties` + `Builder` | 已完成（`build()` 校验 `transport` 非空；`effective_protocol()` 复刻 `handleTlsIfNeeded`） |
| `registry.NacosA2aTransportPropertiesEnvParser` | 解析 `NACOS_A2A_AGENT_{TRANSPORT}_{ATTR}`；非法 port 抛 `INVALID_PARAM` | `a2a::registry::NacosA2aTransportPropertiesEnvParser` | **本轮补齐**：`parse`/`from_env` 改为返回 `AiServiceResult`，非法 port 返回 `Err(AiServiceError::INVALID_PARAM, "Invalid `port` value for transport `JSONRPC`: no_number")`，对齐 Java `NacosRuntimeException(NacosException.INVALID_PARAM, ..)`（此前静默置 0） |
| `registry.constants.Constants` | 协议/前缀/属性枚举 | `a2a::registry::constants`（含 `TransportPropertiesAttribute`） | 已完成（6 个属性 envKey/propertyKey 逐一对齐；`TRANSPORT_DEFAULT_PROTOCOL` 对应 `AiConstants.A2a.A2A_ENDPOINT_DEFAULT_PROTOCOL`） |
| `registry.NacosAgentRegistry` | `AgentRegistry` 实现；合并 deploy/env transports、覆写 preferredTransport、生成 URL | `a2a::registry::NacosAgentRegistry` + `TransportProperties` + `AgentRegistry` trait | **已完成 2026-10-28**（`build_transport_properties`/`generate_new_url`/`handleTlsIfNeeded`/`overwriteAttributes` 逐行对齐；`do_overwrite` 现为 `removeIf(equals)` 去重 + `insert(0, ..)` 前插，见 history/NACOS__MIGRATION_HISTORY.md） |
| `discovery.NacosAgentCardResolver` | 从 Nacos 订阅并缓存 AgentCard（`AgentCardResolver` 实现） | `a2a::discovery::NacosAgentCardResolver` + `AgentCardResolver` trait | 已完成（`async_trait`；内存缓存 + 订阅回调更新，含推送更新单测；订阅去重 `agentCardUpdaters` 由 `card_listeners` map 等价覆盖） |
| `io.agentscope.core.a2a.agent.card.AgentCardResolver` / `...server.registry.AgentRegistry` / `io.a2a.spec.AgentCard` | 协议侧接口与 A2A 规范模型 | **A2A 模型复用** `agentscope_extensions_a2a_client::model`（`AgentCard`/`AgentSkill`/`AgentCapabilities`/`AgentProvider`/`AgentInterface`）；`A2aAgentCard` 仅为携带 `documentationUrl` 的薄包装（`Deref` 转发）；`AgentResolver`/`AgentRegistry` 仍为本 crate 精简 trait | **已完成 2026-10-28（部分）**：模型已复用；`AgentRegistry` 确实缺位（保留本地）；`AgentCardResolver` 保持 async（适配本 crate 的 async `NacosAiService`）；protocol `AgentCard` 缺 `documentationUrl` 记入 §4.2-5 |

### 2.2 nacos-prompt（Prompt 监听）

| Java 类 / 包 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `prompt.NacosPromptListener` | 经 `AiService.subscribePrompt` 获取并缓存；`{{var}}`/`${var}` 渲染；version/label/default 支持 | `prompt::NacosPromptListener` | 已完成（方法面/优先级/EMPTY_SENTINEL/default 回退均对齐；推送回调已对齐 Java，见 history/NACOS__MIGRATION_HISTORY.md） |
| `Prompt`（`getTemplate`/`getVersion`/`render(Map)`） | prompt 载荷 + 渲染 | `service::NacosPrompt` + `service::render_template` | 已完成（`EMPTY_SENTINEL` 语义、未知占位符保留、default 渲染） |
| `AbstractNacosPromptListener#onEvent` | 推送回调更新缓存（仅丢 null key / null 载荷） | `service::PromptListener` 类型别名 + `prompt::apply_prompt_push` + `InMemoryNacosAiService::fire_prompt_update` | **已完成 2026-10-28**（Java `NacosPromptListener.java:50-64`：key 为 null/空 → 丢弃；`prompt != null && getTemplate() != null` → 写入，**空串模板照写**；Rust 现同语义：`apply_prompt_push` 仅拦空 key，空模板入库，见 history/NACOS__MIGRATION_HISTORY.md） |
| `NacosPromptListener#getPrompt(key, version, label, args, default)` | 读取入口（缓存命中不重复订阅） | `prompt::NacosPromptListener::{get_prompt, get_prompt_with_args, get_prompt_with_default, get_prompt_full}` | 已完成（含 version/label 透传、缓存命中、default 回退单测） |

### 2.3 nacos-skill（技能仓库）

| Java 类 / 包 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `skill.NacosSkillRepository` | `AgentSkillRepository` 实现：从 Nacos 下载 skill ZIP，解析 SKILL.md 构建 `AgentSkill`；只读；version/label 优先级；`knownSkillNames` 枚举 | `skill::NacosSkillRepository` | **进行中**（**实现 harness `SkillRepository`**：`repo_id`/`source`/`is_writeable`/`repo_kind`/`load` + `write_skill`/`delete_skill` 只读拒绝 ✅；version/label 三级优先级与 `downloadSkillZipBytes` 分派 ✅；但 `getAllSkillNames` 无对应（harness 无「只取名字」方法），`getRepositoryInfo` 无对应结构，见 history/NACOS__MIGRATION_HISTORY.md-缺口 S-3） |
| `SkillUtil.createFromZip` / `MarkdownSkillParser` | 从 zip 构建 skill（含资源条目、二进制 base64 回退） | `skill::frontmatter::create_from_zip` + `AgentSkill::builder` | **已完成 2026-10-28**（zip 结构校验、资源 key 相对根目录、必填 name/description/正文校验均对齐 Java；见 history/NACOS__MIGRATION_HISTORY.md） |
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
| nacos-a2a 模型 + 转换 | `a2a.model.*`, `utils.AgentCardConverterUtil` | P0 | 已完成 | §2.1；字段级镜像 + interfaces 三元组组装/拆分 + capabilities 双向强制语义（见 history/NACOS__MIGRATION_HISTORY.md） |
| nacos-a2a security scheme 转换 | `utils.SecuritySchemeConverterUtil` | P1 | 已完成 | 见 history/NACOS__MIGRATION_HISTORY.md；`a2a::security_scheme` 按 type 分派，含 OAuth flows/scopes/pkceRequired |
| nacos-a2a registry + properties + env parser | `registry.*` | P0 | 已完成 | §2.1；env parser 非法 port 现返回 `INVALID_PARAM`（对齐 Java） |
| nacos-a2a discovery resolver | `discovery.NacosAgentCardResolver` | P0 | 已完成 | §2.1；缓存 + 推送更新单测 |
| nacos-prompt listener | `prompt.NacosPromptListener` | P0 | 已完成 | §2.2；推送回调语义已对齐（空 key 丢弃、空模板照写，见 history/NACOS__MIGRATION_HISTORY.md） |
| nacos-skill repository + zip/frontmatter | `skill.NacosSkillRepository` | P0 | 已完成 | §2.3；资源 key / 必填校验 / zip 结构校验均对齐（见 history/NACOS__MIGRATION_HISTORY.md） |
| endpoint 载荷保真（`version`/`address`） | Nacos `AgentEndpoint` | P1 | 已完成 | 新增 `a2a::model::NacosAgentEndpoint` + `from_transport`，由 `register_endpoints` 使用 |

### 3.2 设计决定（不迁移 / 延后）

| 项 | 优先级 | 决定 | 说明 |
|---|---|---|---|
| 真实 Nacos HTTP 客户端 | P1 | **已完成（2026-10-31，见 history/NACOS__MIGRATION_HISTORY.md）** | `src/http.rs::NacosHttpAiService` 实现整个 `NacosAiService`（`/v1/ai/agent`、`/v1/ai/agent/endpoint`、`/v1/ai/prompt`、`/v1/ai/skill`），经 `NacosHttpTransport` 缝隙注入；真实 `reqwest` 客户端 `ReqwestNacosTransport` 在 `http-client` feature 后（默认构建不加依赖）。离线用例走假 transport（见 history/NACOS__MIGRATION_HISTORY.md）。原「延后」裁定作废 |
| 与 protocol crate 的 A2A 模型统一 | P1 | 已完成（2026-10-28） | 已复用 `agentscope_extensions_a2a_client::model` 的 `AgentSkill`/`AgentCapabilities`/`AgentProvider`/`AgentInterface`；`A2aAgentCard` 降为携带 `documentationUrl` 的薄包装（`Deref`）；`AgentRegistry` 仍本地（protocol 无）。protocol 若补 `documentation_url` 则可退化为 `pub use`（§4.2-5） |
| `AgentCardResolver` 同步→异步 | P0 | 设计决定 | Java 版阻塞（Nacos 客户端阻塞）；Rust 侧 `NacosAiService` 为 async，故 trait 用 `async_trait` |
| `NacosA2aRegistry` 多 endpoint 批量注册 | P2 | 设计决定 | Java 在 1 个 endpoint 时用单值、≥2 时用 `Set` 去重批量；Rust 侧统一逐个 `register_agent_endpoint`（调用次数差异可接受；但 `Set` 的去重语义不可忽略——见 §3.3 同名条目） |
| `NacosSkillRepository` 的 `load()` 同步 trait | P2 | 已完成（2026-10-31 修正） | harness `SkillRepository::load` 为同步 → 内部 `block_on` 驱动 async 下载；**原实现只判断「有无 runtime」就直接 `block_in_place`，在 current-thread runtime（`#[tokio::test]` 默认）会 panic** → 改为先判 `Handle::runtime_flavor()`，仅多线程 runtime 用 `block_in_place`，否则用临时 current-thread runtime。用例 `skill::load_from_a_current_thread_runtime_does_not_panic`（见 history/NACOS__MIGRATION_HISTORY.md） |

### 3.3 剩余缺口（增强项，非阻塞）

| 缺口项 | 优先级 | 状态 | 说明 |
|---|---|---|---|
| 真实 Nacos HTTP 客户端（`reqwest` 版 `NacosAiService`） | P1 | **已完成（2026-10-31）** | 见 history/NACOS__MIGRATION_HISTORY.md（`NacosHttpAiService` + `NacosHttpTransport` + `ReqwestNacosTransport`） |
| 与 protocol crate A2A 模型/接口合并（复用 `AgentCard`/`AgentCardResolver`，仅保留 `AgentRegistry` 本地补位） | P1 | 已完成（模型部分） | 见 history/NACOS__MIGRATION_HISTORY.md；`TransportProperties` 仍本地（protocol 同名类型在 `a2a-server`，会拖入 axum 且 host/port 为 Option，见 §4.2-6） |
| A2A security scheme 结构化转换（apiKey/http/oauth2/openIdConnect/mutualTLS + OAuth flows） | P1 | 已完成 | 见 history/NACOS__MIGRATION_HISTORY.md |
| `do_overwrite` 的接口去重与前插（Java `removeIf(equals)` + `add(0, ..)`） | P2 | 已完成 | 见 history/NACOS__MIGRATION_HISTORY.md；`do_overwrite_dedups_and_prepends_the_interface` |
| `NacosAgentRegistry` 批量 endpoint 去重（Java `Set` 语义） | P2 | 已完成 | 见 history/NACOS__MIGRATION_HISTORY.md；`duplicate_endpoints_are_registered_once` |
| `AgentCardResolver` 缓存失效/TTL 策略 | P2 | **已完成（2026-10-31）** | 新增 `NacosAgentCardResolver::with_ttl(Duration)` / `invalidate(name)`；默认 `None` 保持 Java 的永不过期行为。用例 `a_zero_ttl_refetches_the_card`、`a_cached_card_never_expires_without_a_ttl`、`invalidate_drops_a_cached_card`（见 history/NACOS__MIGRATION_HISTORY.md） |

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
- **已知问题**：无编译/测试阻塞。**2026-10-29 复核**：protocol crate A2A 模型统一已完成（见 history/NACOS__MIGRATION_HISTORY.md）；**`documentationUrl`、`TransportProperties`、`zip` 三项跨模块请求均已关闭**（§4.2-5 / §4.2-6 / §4.2-2）。**真实剩余**：①真实 `NacosAiService`（`reqwest`）未实现（仅 trait + `InMemoryNacosAiService`，归属待裁定，§4.2-3）；②`getAllSkillNames`/`getRepositoryInfo` 无对应（harness `SkillRepository` 缺方法，S-3）；③`SkillRepository::load` 同步 trait 内 `block_on`（§4.2-4）；④`AgentCardResolver` 无 TTL（§3.3）。

### 4.2 下一步与阻塞（跨模块需求）

1. ~~**【跨模块依赖】protocol crate 的 `a2a` 子模块不可编译**~~ → **已勘误并关闭（见 history/NACOS__MIGRATION_HISTORY.md）**：protocol 的 A2A 模型可编译，本 crate 已于 2026-10-28 复用（`agentscope-extensions-a2a-client` 为新增 path 依赖）。仅 `AgentRegistry` 仍是本 crate 本地 trait。
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

## 6. 执行历史（已归档）

- 执行记录 / 静态复核 / 勘误 / 反向验证明细：[`history/NACOS__MIGRATION_HISTORY.md`](./history/NACOS__MIGRATION_HISTORY.md)
- 本文件只保留现行状态（§0–§5）；历史条目一律写进上述归档文件，不再回填本文。
