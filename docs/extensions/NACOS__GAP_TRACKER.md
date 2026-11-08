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
| 当前状态 | **已完成**（P1/P2 清零；2026-10-31 实测 92 passed / 0 failed（默认与 `--features http-client` 同））—— 真实 `NacosAiService`（`src/http.rs` + `NacosHttpTransport` seam + `ReqwestNacosTransport`，含 `poll_once`/`spawn_polling`）已落地；P0/P1 语义缺口与 TTL/`invalidate`、A2A capability extensions 保留（Java `5da592b4a`）全部关闭；仅真实 Nacos 服务联调需环境（非代码缺口）。详见 history/NACOS__MIGRATION_HISTORY.md。**2026-10-31 本轮**：NACOS-01/02/03/04/11/15/16 已落地（协议侧 `AgentRegistry`/`AgentCardResolver` 互操作、accessToken 登录与提前刷新、namespace 全路径覆盖、错误分类枚举、envelope 业务码校验），待主智能体统一编译与测试；汇总 tracker 行待回填。**2026-10-31 Round 2**：NACOS-05/06/07 已落地（`endpointType` 实参、请求超时 + 重试/指数退避、心跳重注册），实测 **115 passed / 0 failed（`--features http-client`）**、**113 passed / 0 failed（默认构建）**，`cargo build` EXITCODE=0。**2026-11-01 Round 4**：NACOS-08/09/10/12/13/14 已关闭（非法 transport 上抛、IPv6 host 加方括号、`skill_exists` 诊断出口、退订/`close`、`polling` 端到端与便捷装配、`location` 注释更正），实测 **121 passed / 0 failed（`--features http-client`）**、**120 passed / 0 failed（默认构建）**，两种配置 `cargo build` 均 EXITCODE=0 |

> **2026-10-31 全量对齐复核**：逐文件复核 Java 11 个主类后新增 §3.5（`NACOS-01..23`）与 §4.3（`D01..D05` / `X01..X09`）。结论修正：原「P1/P2 清零」仅对**模块内**能力成立，跨 crate 互操作（`AgentRegistry`/`AgentCardResolver`）与真实 Nacos 客户端行为（鉴权刷新、namespace、错误码、超时重试）仍有 4 项 P0 / 4 项 P1 待办。存量 §3.1–§3.3 状态列与历史进程未改动。

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
| `io.agentscope.core.a2a.agent.card.AgentCardResolver` / `...server.registry.AgentRegistry` / `io.a2a.spec.AgentCard` | 协议侧接口与 A2A 规范模型 | **A2A 模型复用** `agentscope_extensions_a2a_client::model`（`AgentCard`/`AgentSkill`/`AgentCapabilities`/`AgentProvider`/`AgentInterface`）；`A2aAgentCard` 仅为携带 `documentationUrl` 的薄包装（`Deref` 转发）；`AgentResolver`/`AgentRegistry` 仍为本 crate 精简 trait | **已完成 2026-10-31**：两条协议 trait 均已实现——`NacosAgentRegistry` 实现 `agentscope_extensions_a2a_server::registry::AgentRegistry`（NACOS-01）、`NacosAgentCardResolver` 实现 `agentscope_extensions_a2a_client::card::AgentCardResolver`（NACOS-02）；本 crate 的精简 trait 保留为无协议依赖的内部 API（`InMemory`/测试路径使用） |

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
| 与 protocol crate A2A 模型/接口合并（复用 `AgentCard`/`AgentCardResolver`，仅保留 `AgentRegistry` 本地补位） | P1 | **已完成（2026-10-31）** | 模型 + 两条协议 trait 均已对齐：`NacosAgentRegistry` 实现 `a2a_server::registry::AgentRegistry`、`NacosAgentCardResolver` 实现 `a2a_client::card::AgentCardResolver`（见 §3.5 NACOS-01/02）；本 crate 的精简 trait 仅保留为内部/测试 API。`TransportProperties` 互转见 §3.5 NACOS-01 |
| A2A security scheme 结构化转换（apiKey/http/oauth2/openIdConnect/mutualTLS + OAuth flows） | P1 | 已完成 | 见 history/NACOS__MIGRATION_HISTORY.md |
| `do_overwrite` 的接口去重与前插（Java `removeIf(equals)` + `add(0, ..)`） | P2 | 已完成 | 见 history/NACOS__MIGRATION_HISTORY.md；`do_overwrite_dedups_and_prepends_the_interface` |
| `NacosAgentRegistry` 批量 endpoint 去重（Java `Set` 语义） | P2 | 已完成 | 见 history/NACOS__MIGRATION_HISTORY.md；`duplicate_endpoints_are_registered_once` |
| `AgentCardResolver` 缓存失效/TTL 策略 | P2 | **已完成（2026-10-31）** | 新增 `NacosAgentCardResolver::with_ttl(Duration)` / `invalidate(name)`；默认 `None` 保持 Java 的永不过期行为。用例 `a_zero_ttl_refetches_the_card`、`a_cached_card_never_expires_without_a_ttl`、`invalidate_drops_a_cached_card`（见 history/NACOS__MIGRATION_HISTORY.md） |

## 3.5 2026-10-31 全量对齐复核清单（唯一编号）

> 复核基线：Java `main` @ `1524b3191`；Rust 工作树 HEAD 见仓库。本表为本轮逐文件核对结论，**存量 §3.1–§3.4 历史清单保持原样未重新编号**。
> 编号规则：`NACOS-NN`（缺口，状态列取值 `待办` / `进行中` / `已完成` / `设计决定（不迁移）` / `待裁定`）；§4.3 差异另有 `NACOS-D<NN>` / `NACOS-X<NN>` 系列。编号本模块内唯一、单调递增，不复用历史编号。
> **2026-10-31 裁定（汇总方已确认）**：本文件的 `待裁定` 项已全部裁定，状态列更新为 `待办` / `设计决定（不迁移）` / `已完成`；裁定结论、依据与执行批次见 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` §3.3。
> 正文其它位置出现的旧「待裁定 N 项」为复核时点快照，一律以 §3.5/§3.6 表格的状态列为准。
> 复核范围：Java 三子模块 11 个主类（2095 行）/ 8 个测试类（120 个 `@Test`）↔ Rust crate 14 个 `.rs`；测试数沿用既有实测 **92 passed / 0 failed（`--features http-client`，默认构建同）**，本轮未运行 cargo。
> **2026-10-31 第二轮（仅代码/文档编辑，未运行 cargo/rustc）**：NACOS-01、NACOS-02、NACOS-03、NACOS-04、NACOS-11、NACOS-15、NACOS-16 落地，逐行状态与行内 `【2026-10-31 落地】` 说明为准；编译与测试由主智能体统一执行。

### 3.5.1 缺口与待裁定

| 编号 | 优先级 | 缺口项（可执行描述） | Java 参考（文件#符号） | Rust 现状（文件:行 / rg 零命中） | 执行状态 | 下一动作 |
|---|---|---|---|---|---|---|
| NACOS-01 | P0 | `NacosAgentRegistry` 未实现**协议侧**注册接口，无法接入 `AgentRegistryService`（A2A server 的「服务就绪即注册」链路），Nacos 注册能力对 Rust 运行时实际不可达 | `...nacos-a2a/.../registry/NacosAgentRegistry.java#register`（`implements io.agentscope.core.a2a.server.registry.AgentRegistry`） | `src/a2a/registry/mod.rs:463` 自定义**异步** trait `AgentRegistry`（入参 `&A2aAgentCard` + `Vec<TransportProperties>`）；协议侧已有 `crates/agentscope-extensions-a2a-server/src/registry.rs:25` 的**同步** `AgentRegistry`（入参 `&AgentCard` + `&[TransportProperties]`、错误 `AgentRegistryError`）；`rg 'a2a_server::registry::AgentRegistry'` 零命中（另核 `AgentRegistryService`/`registry_name` 亦无实现点） | 已完成 | **2026-10-31 落地**：`impl agentscope_extensions_a2a_server::registry::AgentRegistry for NacosAgentRegistry`（`src/a2a/registry/mod.rs:544`），协议 `&[TransportProperties]` 经 `protocol_transport_to_local`（`:579`）转成本 crate 必填版（缺 host 直接报错，不静默注册空地址），async `register` 用 `futures::executor::block_on` 驱动；新增 path 依赖 `agentscope-extensions-a2a-server`（会拖入 axum/reqwest，注册链路必需，已记 §4.2-7）。用例 `the_protocol_registry_service_registers_the_agent_into_nacos`（`AgentRegistryService::new(vec![Arc::new(nacos_registry)])` 后 `released_cards()`/`registered_endpoints()` 非空，缺 host 计入 `RegistrationReport.failed`）。原下一动作：在 `NacosAgentRegistry` 上补 `impl agentscope_extensions_a2a_server::registry::AgentRegistry`：把 `&[TransportProperties]`（`a2a-client/src/transport_properties.rs:19`，`Option<u16>` + `extra`）转换成本 crate 必填版，内部用 `futures::executor::block_on` 驱动 async `register_agent`；验收：`AgentRegistryService::new(vec![Arc::new(nacos_registry)])` 注册后 `InMemoryNacosAiService.released_cards()` 非空，且失败被计入 `RegistrationReport.failed` |
| NACOS-02 | P0 | `NacosAgentCardResolver` 未实现**协议侧** `AgentCardResolver`，Nacos 发现到的 card 无法交给 A2A client agent → 发现能力无消费方 | `...nacos-a2a/.../discovery/NacosAgentCardResolver.java#getAgentCard`（`implements io.agentscope.core.a2a.agent.card.AgentCardResolver`） | `src/a2a/discovery.rs:28` 自定义 trait（返回 `Result<A2aAgentCard, String>`）；协议侧 `crates/agentscope-extensions-a2a-client/src/card.rs:27` 的 `AgentCardResolver`（装箱 Future、返回 `Result<AgentCard, A2aError>`、`resolver_name()`）；`rg 'card::AgentCardResolver'` 零命中；全仓 `rg 'NacosAgentCardResolver'`（除本 crate）亦零命中 | 已完成 | **2026-10-31 落地**：`impl agentscope_extensions_a2a_client::card::AgentCardResolver for NacosAgentCardResolver`（`src/a2a/discovery.rs:163`），`resolver_name()` 返回 `NacosAgentCardResolver`；缓存/TTL/推送簿记复用，逻辑抽为 `resolve_detailed()`（`:94`，保留 `AiServiceError` 分类），`to_protocol_error` 把 NOT_FOUND 映射为 `A2aError::CardUnavailable`（带 agent 名 + resolver 名）、HTTP 状态映射 `A2aError::Http`。用例 `the_protocol_resolver_feeds_the_a2a_client`（`A2aClient::new(agent, Arc<dyn 协议 resolver>).agent_card()` 取到 card）、`a_missing_card_is_reported_as_card_unavailable`。原下一动作：为 `NacosAgentCardResolver` 补协议 trait 实现（或在协议 trait 上做 `impl` 适配层，内部复用现有缓存/TTL）；`A2aError` 需可承载 NOT_FOUND 语义；验收：以 `FixedAgentCardResolver` 同构方式被 A2A client 消费的用例，且 `resolver_name()` 返回 `NacosAgentCardResolver` |
| NACOS-03 | P0 | 无 accessToken **获取与刷新**：只能注入静态 token，Nacos 开启鉴权后 token 过期即全链路 401/403（生产阻断） | `AiFactory.createAiService(Properties)`（`NacosAgentCardResolver.java:70`、`NacosAgentRegistry.java#builder(Properties)`）由 Nacos SDK 依 `username/password` 或 `accessToken` 自动登录并按 `tokenTtl` 刷新 | `src/http.rs:120` 仅静态 `access_token: Option<String>`，`src/http.rs:280` 无条件拼 `accessToken=`（另两处命中均为 `:637`/`:679` 测试断言）；`rg -n '/auth/login|v1/auth' src` 零命中；`rg -n 'username|password' src` 仅命中 `src/a2a/security_scheme.rs:121` 的注释（OAuth password flow 说明）→ 无任何登录/刷新代码路径 | 已完成 | **2026-10-31 落地**：`NacosHttpConfig` 增 `username`/`password`/`auth_path`/`token_refresh_ratio`（`src/http.rs:126`）+ `with_credentials`；`NacosHttpAiService::login`（`src/http.rs:472`，`POST /v1/auth/login?username&password`，读 `accessToken`/`tokenTtl`，缺 `tokenTtl` 回退 `DEFAULT_TOKEN_TTL`=5h）；`access_token()`（`:437`）缓存 token 并在 `refresh_deadline(ttl, 0.9)`（`:226`，90% 提前刷新）到期后重登；`send()`（`:406`）遇 401/403 重登一次并重放原请求，登录失败 `warn` 后回退静态 `access_token`。用例 `credentials_log_in_once_and_the_token_is_reused`、`a_rejected_token_is_renewed_once_and_the_request_replayed`、`an_expired_token_is_refreshed_before_the_request`、`the_refresh_deadline_is_ninety_percent_of_the_ttl`、`a_failed_login_falls_back_to_the_static_token`（原下一动作：增加 `NacosAuth` seam：`login`（`/v1/auth/login`）+ `tokenTtl` + 提前刷新（建议 90% 处），`NacosHttpConfig` 增 `username`/`password`，token 读取失败回退静态值；验收：fake transport 先返回过期 token/401、再返回新 token，断言自动重登一次并重放原请求）|
| NACOS-04 | P0 | `namespaceId` 只随**写**请求下发，读路径（card / prompt / skill zip）落到默认命名空间 → 非 `public` 命名空间里读到错数据或 404 | Java 由 `Properties(namespace)` 构造 `AiService`，**所有**调用自动带 namespace（`NacosSkillRepository.java:146` 以 namespace 决定读取范围，`:147-148` 仅作 source/location 展示） | `src/http.rs:365`、`src/http.rs:389` 仅 release/register 带 `namespaceId`；`fetch_agent_card`（`src/http.rs:330`）、`fetch_prompt`（`src/http.rs:349`）、`download_skill`（`src/http.rs:475`）的 query 无 namespaceId；`NacosSkillRepository.namespace_id`（`src/skill/mod.rs:64`）只参与 `source`/`location` 字符串 | 已完成 | **2026-10-31 落地**：`build_url`（`src/http.rs:384`）对**所有**请求统一追加 `namespaceId`（空则省略），读路径 `fetch_agent_card`/`fetch_prompt`/`download_skill` 与写路径同规则；`NacosHttpConfig::new(base_url, namespace)` 使 namespace 成为配置级必填项（见 NACOS-16），`for_namespace()`（`:293`）提供方法级 override。用例 `read_paths_carry_the_configured_namespace`（card/prompt/skill 三条 URL 均含 `namespaceId=dev`）、`an_empty_namespace_is_omitted_from_the_query`、`a_scoped_service_addresses_another_namespace`（原下一动作：三处读请求 query 追加 `namespaceId`，空则省略，与写路径同规则）|
| NACOS-05 | P1 | `releaseAgentCard` 缺 `endpointType` 实参（Java 必传 `A2A_ENDPOINT_TYPE_SERVICE`），注册载荷与 Java 不一致 | `...registry/NacosA2aRegistry.java:88-92`（`releaseAgentCard(card, AiConstants.A2a.A2A_ENDPOINT_TYPE_SERVICE, setAsLatest)`） | `src/service.rs:111` trait 方法签名为 `(card, set_as_latest)`；`src/http.rs:355-375` POST query 仅 `setAsLatest`/`namespaceId`；`rg 'endpointType|endpoint_type' src` 仅命中 `service.rs:111` 的注释 | 已完成 | **2026-10-31 Round 2 落地**：trait 改为 `release_agent_card(card, endpoint_type, set_as_latest)`（`src/service.rs`，新增常量 `A2A_ENDPOINT_TYPE_SERVICE = "service"`）；HTTP 侧 query 首参下发 `endpointType=`（`src/http.rs`）；`NacosA2aRegistry` 固定传 service（`src/a2a/registry/mod.rs`）；`InMemoryNacosAiService` 记录 `ReleasedAgentCard{name,endpoint_type,set_as_latest}` + `released_cards_detailed()`，原 `released_cards()` 保留。用例 `the_release_carries_the_endpoint_type`、`release_agent_card_posts_the_card_and_set_as_latest`、`registers_card_and_endpoints`。原下一动作：trait 增 `endpoint_type: &str`（默认 `service`），HTTP 侧下发 `endpointType=`；验收：用例断言 release URL 含 `endpointType=service`（值以联调确认为准，保留可配置） |
| NACOS-06 | P1 | 无请求超时、无重试/退避；轮询失败仅 `warn` 后等下一 tick | Java 客户端超时/重试由 Nacos SDK 统一管理（模块内自研代码零命中：`rg -ni 'retry|timeout' <java-root>` = 0） | `src/http/reqwest_transport.rs:18-31` 用 `reqwest::Client::new()`（未设 `timeout`/`connect_timeout`），`execute`（`:44-56`）单次发送；`spawn_polling`（`:62-75`）固定 `interval` + `tracing::warn`，无退避上限、无抖动 | 已完成 | **2026-10-31 Round 2 落地**：新增 `RetryPolicy{max_attempts,initial_backoff,max_backoff,multiplier}` + `NacosHttpConfig::retry`（默认 3 次 / 200ms 起、×2、上限 2s）+ `with_retry`；重试在 `NacosHttpAiService::send`（单次发送拆为 `send_once`），仅 `AiServiceError::is_retryable()`（Transport/429/5xx）重试，最后一次结果原样上抛；`ReqwestNacosTransport::with_timeout(Duration)`（默认 30s，逐请求 `RequestBuilder::timeout`）+ `without_timeout`/`timeout()`；`spawn_polling` 失败按 `polling_backoff` 指数退避（上限 60s，成功即复位）。用例 `a_retryable_failure_is_retried_until_it_succeeds`、`terminal_failures_are_not_retried_and_the_budget_is_finite`、`a_transport_timeout_is_retryable`、`the_retry_policy_backs_off_exponentially_and_stops_at_the_budget`、`a_timeout_is_applied_by_default_and_can_be_overridden`、`polling_backs_off_exponentially_until_the_cap`。原下一动作：`ReqwestNacosTransport` 增 `with_timeout(Duration)` / `with_retry(max_attempts, backoff)`；`spawn_polling` 失败按指数退避（对齐 core `ExecutionConfig::model_defaults()` 的 `[2,4,8,16,32,60]s`）；验收：fake transport 连续两次 500 后 200，断言重试次数与最终成功、以及超时用例返回可重试错误 |
| NACOS-07 | P1 | 端点注册一次性 POST，无心跳/周期重注册：进程重启或服务端摘除后不会自愈 | 模块内无心跳代码（`rg -i 'heartbeat|health' <java-root>` 零命中）；Java 由 Nacos SDK 长连接心跳维持实例存活（模块自身不写心跳） | `src/http.rs:377-402` 单次 `register_agent_endpoint`；`src/a2a/registry/mod.rs:390-430` 单次 `register_endpoints`；`rg -i 'heartbeat|health|re_register|reregister' src` 零命中 | 已完成 | **2026-10-31 Round 2 落地**：`NacosA2aRegistry::heartbeat(card, props)` 无「已存在」探测地重放 release+endpoint（载荷与首次注册逐字一致）+ `spawn_heartbeat(card, props, interval)`（句柄可 abort，失败按 `heartbeat_backoff` 指数退避、上限 60s，成功复位）。用例 `a_heartbeat_replays_the_registration_payload`、`the_heartbeat_loop_ticks_until_it_is_aborted`（5ms 间隔 60ms 内 ≥2 tick）、`the_heartbeat_backoff_grows_and_is_capped`；服务端是否强制心跳仍需真实 Nacos 联调确认。原下一动作：增 `NacosA2aRegistry::spawn_heartbeat(interval)`（周期重放 release+register 载荷，失败退避，句柄可 abort）；或明确文档化「由宿主负责重新注册」；验收：用例断言第二次载荷与首次一致且间隔可注入。**服务端是否强制心跳需真实 Nacos 联调确认（非代码缺口）** |
| NACOS-08 | P2 | 非法 transport（`transport` 为空）被**静默丢弃**，配置错误不可见 | `NacosAgentRegistry.java#parseTransportFromTransportProperties` 调 builder，`build()` 抛 `IllegalArgumentException`（未捕获 → `register` 失败） | `src/a2a/registry/mod.rs:532-548` 用 `filter_map(... .build().ok())` 静默跳过 | 已完成 | **2026-11-01 落地**：`build_transport_properties`（`src/a2a/registry/mod.rs:674`）改为 `map(.. build()..)` + `collect::<Result<_, String>>()?`，空 `transport` 直接让 `NacosAgentRegistry::register` 返回 `Err`（消息形如 `Invalid A2A transport ...: A2A Endpoint` + ``transport`` can not be empty），且 release/endpoint 均未发出（env 合并路径不变）。用例 `an_invalid_transport_fails_the_registration`（`register` 返 Err、`released_cards()` 与 `registered_endpoints()` 皆空）。原下一动作：改为 `collect::<Result<Vec<_>, _>>()` 并上抛 `Err` |
| NACOS-09 | P2 | `generateNewUrl` 未归一化 host：IPv6 字面量产出非法 URL（`http://::1:8080`） | `NacosAgentRegistry.java#generateNewUrl` 经 `NetworkUtils.formatHostForUrl(host)` 处理 | `src/a2a/registry/mod.rs:676-693` 直接 `format!(protocol://host:port)`，无方括号/无 host 校验 | 已完成 | **2026-11-01 落地**：`generate_new_url`（`src/a2a/registry/mod.rs:839`）先过 `format_host_for_url`（`:863`），裸 IPv6 字面量加方括号、已带方括号原样保留（对齐 `NetworkUtils.formatHostForUrl`）。用例 `generate_new_url_brackets_ipv6_hosts`（`::1`→`http://[::1]:8080`、`[::1]` 不变、`127.0.0.1` 不变）。原下一动作：host 含 `:` 且未带方括号时包 `[..]` |
| NACOS-10 | P2 | `skill_exists` 对非 NOT_FOUND 错误静默返回 false，丢失诊断 | `NacosSkillRepository.java#skillExists`（NOT_FOUND → false；其它 → `log.warn` 后 false） | `src/skill/mod.rs:225-236` 两个分支都返回 `false`，非 not_found 分支无日志、无错误出口 | 已完成 | **2026-11-01 落地**：`skill_exists`（`src/skill/mod.rs:229`）改为委托新方法 `skill_exists_detailed`（`:247`），非 `NOT_FOUND` 分支 `tracing::warn!` 后仍返回 `false`（Java `log.warn`）；`skill_exists_detailed` 把 `NOT_FOUND`/空名归 `Ok(false)`、其余错误原样 `Err`。用例 `a_non_not_found_failure_is_not_swallowed`（`NacosHttpAiService` + 恒 500 的假 transport → `skill_exists=false`、`skill_exists_detailed` 返 `Err(code()=500, !is_not_found())`），`skill_exists_and_missing` 增补 detailed 两断言。注：warn 用代码路径保证，crate 内无 log capture 设施故未对日志文本断言。原下一动作：非 not_found 分支 `tracing::warn!`，并另提供 `skill_exists_detailed()` |
| NACOS-11 | P1 | Nacos **业务错误码未识别**：HTTP 200 + envelope `code != 0` 会被当成功（写路径静默成功）或被误报为解析错误（读路径丢 code） | Java 客户端对 envelope 非 0 code 抛 `NacosException(code, message)`，`NacosA2aRegistry.java#tryReleaseAgentCard` 依赖该异常上抛失败 | `src/http.rs:455-473` 只判 `response.is_success()`；`error_from_response`（`:516-536`）仅在非 2xx 调用；`unwrap_envelope`（`:498-514`）只剥 `data`、不校验 `code` → `release_agent_card`/`register_agent_endpoint` 在服务端拒绝时返回 `Ok(())`，`get_*` 则报 `invalid agent card payload`（code 丢失） | 已完成 | **2026-10-31 落地**：`unwrap_envelope`（`src/http.rs:701`）改为返回 `AiServiceResult<Value>`，`code != 0` 直接产出 `AiServiceError::Business{code,message}`；`get_json` 与新增的 `check_write_response`（`:729`，release / register-endpoint 共用）统一走该校验，纯文本 ACK（`ok`）仍视为成功；非 2xx 且带 Nacos 报文时保留 code，否则归 `Http`。用例 `a_business_error_code_in_a_success_response_is_an_error`（200+`code=300` 写路径 `Err(300)`、200+`code=404` 读路径 `is_not_found()`）（原下一动作：`get_json` 与两个写路径统一先校验 envelope，`code==0` 为成功，否则 `AiServiceError{code,message}`）|
| NACOS-12 | P2 | 订阅无退订/关闭：listener 表只增不减，`poll_once` 永久拉取已弃用的 key | 模块内 Java 亦未显式退订（`NacosPromptListener` 无 unsubscribe），生命周期由 `AiService` 客户端销毁承担 | `src/http.rs:196-203` `Subscriptions` 只有 insert/读取；`rg 'unsubscribe|remove_listener|fn close|fn shutdown' src` 零命中 | 已完成 | **2026-11-01 落地**：`NacosHttpAiService` 新增 `unsubscribe_prompt`（`src/http.rs:512`）/ `unsubscribe_agent_card`（`:522`，与 `subscribe_agent_card` 对称命名，条目原文写作 `unsubscribe_card`）/ `close`（`:533`，清空卡片+prompt 订阅表，服务仍可再次 `subscribe_*`），均只剪本地订阅表（Nacos 无退订 API，Java 靠销毁 `AiService` 客户端）。用例 `unsubscribing_and_close_stop_polling_the_key`：订阅 card+prompt（2 次请求）→ 退订 prompt 后 `poll_once()` 返 0（card 不变）且第 3 条请求只含 `agentName`、**不含 `promptKey`** → 再退订 card 后 `poll_once()` 返 0 且不再发请求（队列已空，若发请求会报错）→ 重新订阅 card（第 4 次请求）后 `close()` → `poll_once()` 返 0。原下一动作：增 `unsubscribe_prompt`/`unsubscribe_card` 与 `NacosHttpAiService::close()` |
| NACOS-13 | P2 | 「HTTP 轮询 → 订阅者」缺端到端用例；`spawn_polling` 全仓无调用方（仅 re-export），订阅刷新实际未接线 | Java push 由 SDK 长连接直达 `AbstractNacosPromptListener#onEvent`（`NacosPromptListener.java:50-64`） | `spawn_polling` 仅 `src/lib.rs:32` re-export 与 `src/http/reqwest_transport.rs:62` 定义，`rg 'spawn_polling' --glob '*.rs'` 无第三处；`src/http.rs:725-780` 只覆盖 `poll_once` 与 listener 各自行为 | 已完成 | **2026-11-01 落地**：新增 `NacosHttpAiService::polling(interval)`（`src/http.rs:547`，`tokio::spawn` + 失败指数退避、成功复位，句柄可 abort），并把 `MAX_POLLING_BACKOFF`/`polling_backoff` 从 `src/http/reqwest_transport.rs` 移到 `src/http.rs:355`/`:363`（**不再受 `http-client` 门控**；`http/reqwest_transport` 是私有模块，二者此前也无公共可达路径，故未留 re-export；原 `spawn_polling` 变薄包装转调该方法，其公共路径 `agentscope_extensions_nacos::spawn_polling` 不变）。用例 ①`polling_refreshes_the_prompt_listener_and_the_card_resolver`：`NacosAgentCardResolver` + `NacosPromptListener` 建在同一 `NacosHttpAiService` 上 → 首轮订阅取旧值 → fake transport 第二轮返回新 card/new template → `poll_once()` 返 2 且二者从缓存读到新值（`http://agent:9999` / `hi ada`），总请求数仍为 4；②`polling_pushes_changes_to_subscribers_until_aborted`：5ms 间隔、50ms 内订阅者收到变更后 abort。原下一动作：补用例 + 便捷装配 |
| NACOS-14 | P2 | `NacosSkillRepositoryInfo.location` 的文档注释写成 `nacos://<namespace>`，与实现 `namespace:<ns>` 不符（实现正确、注释错，易误导调用方） | `NacosSkillRepository.java:88`（`LOCATION_PREFIX = namespace:`）、`:148` | `src/skill/mod.rs:57-58` 注释 vs `src/skill/mod.rs:115`（实测 `namespace:public`，既有用例 `repo_metadata_matches_java` 已断言正确值） | 已完成 | **2026-11-01 落地**：仅改注释，`NacosSkillRepositoryInfo.location` 的文档改为「`namespace:<id>`，mirroring Java `LOCATION_PREFIX`」（`src/skill/mod.rs:57`），与实现 `format!("{LOCATION_PREFIX}{namespace_id}")`（`:115`）及既有断言 `repo_metadata_matches_java`（期望 `namespace:public`）一致；无代码/行为改动。原下一动作：只改注释 |
| NACOS-15 | P2 | 待裁定：`AiServiceError` 是否升级为分类枚举（Transport / Http / Business / Serde），以支持重试判定与 fallback 分支 | `NacosException` 以 `errCode` 分类，调用方按码分支（`NacosSkillRepository.java#getSkill` 只特判 `NOT_FOUND`） | `src/service.rs:24-52` 单一 `{code, message}`；传输失败与解析失败都归 `code=500`（`src/http.rs:296`、`:311`） | 已完成 | **2026-10-31 落地**：`AiServiceError` 改为分类枚举 `Transport{message}` / `Http{status,message}` / `Business{code,message}` / `Serde{message}`（`src/service.rs:30`），保留 `new(code,message)`、`not_found`、`NOT_FOUND`/`INVALID_PARAM` 构造与 `code()`/`message()` 访问器（`.code`→`.code()`、`.message`→`.message()` 已全量改完），新增 `is_retryable()`（Transport、429/5xx 为 true；Business/Serde 为 false）；生产侧已按分类产出：传输失败 `Transport`、解析失败 `Serde`、envelope code `Business`、裸状态 `Http`。用例 `the_error_kind_drives_the_retry_decision`。（原记录：与 NACOS-06 / NACOS-11 一并定：若做重试，必须先有「可重试」信号；裁决后统一改 `service.rs` 与全部调用点（`rg 'AiServiceError::new' src` 全部复核） 【裁定 2026-10-31：A：升级为分类枚举 Transport/Http/Business/Serde，保留 code/message 构造】 |
| NACOS-16 | P2 | 待裁定：namespace 语义归属——按配置（一 service 一 namespace，`NacosHttpConfig.namespace`）还是按调用（trait 方法逐次带 `namespaceId`） | Java 每个 `AiService` 绑定一份 `Properties(namespace)`；`NacosSkillRepository` 的 `namespaceId` 只用于 source/location，下载仍走注入的 `AiService` | 现状同 Java（`src/skill/mod.rs:64`），但 HTTP 实现必须显式带参才能落到正确命名空间 → 与 NACOS-04 强相关 | 已完成 | **2026-10-31 落地（裁定 A）**：`NacosHttpConfig::namespace` 由 `Option<String>` 改为必填 `String`（构造函数第二参，`src/http.rs:126`/`:164`），`build_url` 对所有请求统一下发 `namespaceId`（空串表示服务端默认命名空间、参数省略）；方法级 override 以 `NacosHttpAiService::for_namespace()`（`:293`）提供——共享 transport 与登录缓存、独立订阅簿记，等价「同一 service 调用另一命名空间」，trait 签名不变（`NacosSkillRepository` 无需改）。用例 `a_scoped_service_addresses_another_namespace`、`an_empty_namespace_is_omitted_from_the_query`，并顺带关闭 NACOS-04。（原记录：若选配置级：`NacosHttpConfig.namespace` 变必填并覆盖所有请求；若选参数级：trait 全方法加 `namespace` 参数）|

### 3.5.2 本轮复核确认已对齐（抽样，不求穷举）

| 编号 | 能力 | Java 参考 | Rust 证据（文件:行 + 测试名） | 执行状态 |
|---|---|---|---|---|
| NACOS-17 | Nacos ↔ A2A 卡片双向转换（含 `capabilities.extensions`、supportedInterfaces 组装/拆分、securityRequirements、`documentationUrl`、`stateTransitionHistory` 写入恒 false） | `utils/AgentCardConverterUtil.java#convertToA2aAgentCard` / `#convertToNacosAgentCard` | `src/a2a/converter.rs:37`、`:72`、`:118`（interfaces）、`:203`（extensions）、`:349`；用例 `round_trips_*` 系列（`converter.rs:396-692`，17 个） | 已完成 |
| NACOS-18 | 安全方案按 `type` 分派（apiKey/http/oauth2/openIdConnect/mutualTLS，含 OAuth flows/scopes、null 剔除） | `utils/SecuritySchemeConverterUtil.java`（209 行） | `src/a2a/security_scheme.rs:76`（A2A 侧）、`:178`（Nacos 侧）；8 个用例（apiKey in=header/query、oauth2 flows、mtls、未知 type 折叠） | 已完成 |
| NACOS-19 | registry properties / env parser（非法 `port` → `INVALID_PARAM`）/ `overwritePreferredTransport` 去重 + 前插 / `effective_protocol` 的 TLS 升级 | `NacosA2aRegistryProperties.java`、`NacosA2aTransportPropertiesEnvParser.java#convertToProperties`、`NacosAgentRegistry.java#doOverwrite`/`#handleTlsIfNeeded` | `src/a2a/registry/mod.rs:120-320`（properties+parser）、`:80`（effective_protocol）、`:694-712`（do_overwrite）；用例 `env_parser_rejects_invalid_port`、`do_overwrite_dedups_and_prepends_the_interface`、`duplicate_endpoints_are_registered_once` | 已完成 |
| NACOS-20 | AgentCard 订阅 + 缓存 + push 更新（含并发合并：listener 写入优先） | `discovery/NacosAgentCardResolver.java#getAgentCard`/`#getAndSubscribe` | `src/a2a/discovery.rs:96-146`；用例 `resolves_and_caches_agent_card`、`caches_are_per_resolver_instance`（push 更新后返回新值） | 已完成（Rust 额外 TTL，见 NACOS-X02） |
| NACOS-21 | prompt 读取语义：`EMPTY_SENTINEL`、空模板、default 渲染 `{{}}`/`${}`、version+label 透传、缓存命中不重复订阅、push 仅丢空 key | `prompt/NacosPromptListener.java:107-136`、`#onEvent`（`:50-64`） | `src/prompt.rs:65-135`、`apply_prompt_push`（`:136`）；用例 `returns_rendered_prompt`、`falls_back_to_default_when_missing`、`default_is_rendered_with_args` | 已完成 |
| NACOS-22 | skill 只读仓库：zip 结构校验、Nacos 折叠 YAML 重打包、资源 key 相对根、version→label→default 分派、`knownSkillNames` 列举、`getRepositoryInfo` 形态 | `skill/NacosSkillRepository.java#getSkill`/`#downloadSkillZipBytes`/`#adaptNacosSkillZipForYamlFrontmatter`/`#getRepositoryInfo` | `src/skill/mod.rs:203`、`:206-224`、`:173`（info，`Display` 与 Java `toString` 逐字一致）、`src/skill/frontmatter.rs`；用例 `get_skill_downloads_and_parses`、`load_returns_known_skills`、`repo_metadata_matches_java` | 已完成（仓库抽象差异见 NACOS-X06） |
| NACOS-23 | 真实 HTTP `NacosAiService` + 传输 seam + 离线 fake transport（默认构建不引入 HTTP 栈） | `AiFactory.createAiService(Properties)` | `src/http.rs:108`（seam）、`:133`（service）、`:196`（订阅簿记）；`src/http/reqwest_transport.rs:16`（真实客户端，`http-client` feature）；`Cargo.toml` `http-client = ["dep:reqwest"]`；用例 10 个，含 `the_http_service_is_interchangeable_with_the_in_memory_one`、`a_nacos_error_body_keeps_its_code_and_message`；既有实测 92 passed | 已完成（取证/超时/错误码缺口见 NACOS-03/06/11） |

### 3.5.3 2026-11-03 Java 提交批次（`abf3c35a3` 之后 7 个提交，`abf3c35a3..1d8f9dc04`）

> 来源：用户确认的「Java 又有新提交」批次（`1d8f9dc04`，Nacos 3.3.0 适配）。core / harness / protocol / model 侧见各自 tracker。

| 编号 | Java 提交 | 变更 | Rust 落点 | 状态 / 验证 |
|---|---|---|---|---|
| `NACOS-24` | `1d8f9dc04` | Marketplace Skill 通过 published `latest` label + 精确 version 解析，不暴露 draft | `src/skill/mod.rs:41/193`（`DEFAULT_SKILL_LABEL = "latest"`；优先级 version > label > latest；`http.rs` 已原生区分 404 与业务错误） | 已完成；用例 `downloads_the_published_latest_label_by_default`、`an_explicit_label_replaces_latest`、`version_takes_precedence_over_label_on_download` |
| `NACOS-25` | `1d8f9dc04` | 注册/查询失败保留原始错误码与 cause；只有 NOT_FOUND 视为「不存在」 | `src/a2a/registry/mod.rs:449`（`try_get_agent_card_from_nacos` 仅 NOT_FOUND → `Ok(None)`，其余 `Err` 透传） | 已完成；用例 `a_failed_card_lookup_is_not_treated_as_missing`、`a_not_found_lookup_still_registers_the_card` |
| `NACOS-26` | `1d8f9dc04` | Prompt 订阅按 name/version/label 隔离；version+label 互斥；变量默认值 + 调用方覆盖；listener 级 close 等在途读取后按 selection 退订；同 selection 串行初始化、不同 selection 并发 | `src/prompt.rs:29/119/166/279`（`PromptSelection`、按 selection 缓存/订阅、`close()`、默认值合并）；`src/http.rs:388/510/784/870/998`（selection 级 listener 表、退订、互斥校验、订阅、variables 解析）；`src/service.rs:174/198/210/353`（`variables`、`NacosPromptVariable`、`SelectionKey`、`unsubscribe_prompt`） | 已完成；`cargo test -p agentscope-extensions-nacos` → **138 passed / 0 failed**（新增 20 例，含 `selections_are_cached_independently`、`version_and_label_together_are_rejected`、`close_waits_for_an_in_flight_read_and_cancels_its_subscription`、`slow_initialization_does_not_block_other_selections`） |
| `NACOS-27` | `1d8f9dc04` | Card 转换保留原生 `signatures` | `src/a2a/model.rs:200`（`NacosAgentCard.signatures`）、`src/a2a/converter.rs:69/109`（双向往返原样映射） | 已完成；用例 `native_signatures_survive_a_round_trip` |
| `NACOS-D04` | `1d8f9dc04` | Nacos client / maintainer client 升到 3.3.0 | 无 | 设计决定（不迁移）：Rust 无 Nacos SDK，是自实现 `/v1/ai/*` HTTP 客户端（`src/http.rs`），没有客户端版本可升；只跟随协议语义（上四条） |
| `NACOS-B02` | `1d8f9dc04` | A2A/RAD discovery 分离（capability 探测、显式 mode 覆盖、RAD 模式清空不可用 Card、按 agent 初始化订阅）与 RAD 原生发布（tenant、per-interface protocolVersion endpoint、canonical URL 去重、歧义拒绝） | `src/a2a/discovery.rs:53`（仅 A2A 缓存+订阅）、`src/a2a/model.rs:137`（`NacosAgentInterface` 只有 url/transport）、`src/a2a/registry/mod.rs:433`（固定 legacy `endpointType=service`） | **阻塞（本轮不做）**：RAD 需要 `NacosAiService` 不具备的 discovery/publication HTTP 接口（capability 探测、`AgentPublishRequest`/`CallInterface`/`EndpointSet`），且本机无 Nacos 3.3.0 实例可验证协议细节；照抄字段名无法验证，故整条挂账（前置：先补 HTTP 接口与集成环境） |

### 3.5.4 2026-11-08 Java 提交批次（`1d8f9dc04` 之后 31 个提交中归本模块的部分，`1d8f9dc04..5baff2613`）

> 来源：用户指定窗口 `1d8f9dc04..5baff2613`（31 提交，2026-11-02~11-08）的迁移批次；本模块只涉及 `22ae4dd40`。本轮未运行 cargo（由主智能体统一验证）。

| SHA | 摘要（可截断） | 结论 | Rust 落点（文件:行，能省则省） | 说明或遗留 |
|---|---|---|---|---|
| `22ae4dd40` | nacos-starter：仅当启用 Nacos prompt 时才构建 prompt 版 ReActAgent | ❌不可迁移（无装配层） | 无改动；已核对 `src/prompt.rs`、`src/lib.rs` | 改动本身只是 Spring `@ConditionalOnProperty(matchIfMissing = false)` 的一个默认值翻转。Rust 无 Spring 自动装配层：`NacosPromptListener` 是纯库类型，宿主不构造、不调 `get_prompt*` 就完全不触碰 Nacos；不存在「无条件构建 ReActAgent」的等价路径，缺陷无法复现 |

**遗留与风险**

1. 该提交无 Rust 侧缺口；本模块唯一未开工项仍是 §3.5.3 的 `NACOS-B02`（RAD discovery 分离与原生发布，缺 HTTP 接口与 3.3.0 实例验证环境）。

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
- **验证命令与结果**（2026-10-31 Round 2，本会话，NACOS-05/06/07）：
  - cargo build -p agentscope-extensions-nacos -j 2 --features http-client → **EXITCODE=0**。
  - cargo test -p agentscope-extensions-nacos -j 2 --no-fail-fast --features http-client → **115 passed / 0 failed / 0 ignored，EXITCODE=0**（基线 92 → +23）。
  - cargo test -p agentscope-extensions-nacos -j 2 --no-fail-fast（默认，无 http-client）→ **113 passed / 0 failed，EXITCODE=0**。
  - 反向验证：①去掉 release 的 ndpointType query 参数 ②RetryPolicy::should_retry 恒 false → cargo test 得 **6 failed / 109 passed，EXITCODE=101**（
elease_agent_card_posts_the_card_and_set_as_latest、	he_release_carries_the_endpoint_type、_retryable_failure_is_retried_until_it_succeeds、	erminal_failures_are_not_retried_and_the_budget_is_finite、_transport_timeout_is_retryable、	he_retry_policy_backs_off_exponentially_and_stops_at_the_budget）；恢复后逐字等于回退前文件并重回 115 passed。
- **验证命令与结果**（2026-11-01 Round 4，本会话，NACOS-08/09/10/12/13/14）：
  - `cargo build -p agentscope-extensions-nacos -j 2` → **EXITCODE=0**（本 crate 0 warning）。
  - `cargo build -p agentscope-extensions-nacos --features http-client -j 2` → **EXITCODE=0**（本 crate 0 warning）。
  - `cargo test -p agentscope-extensions-nacos -j 2 --no-fail-fast`（默认）→ **120 passed / 0 failed / 0 ignored，无 `failures:` 段**（基线 113 → +7：6 条新用例 + `polling_backs_off_exponentially_until_the_cap` 随实现从 `http/reqwest_transport.rs` 移到 `http.rs` 后也进入默认构建）。
  - `cargo test -p agentscope-extensions-nacos --features http-client -j 2 --no-fail-fast` → **121 passed / 0 failed**（基线 115 → +6）。
  - 反向验证（3 处同时回退 → 117 passed / **3 failed**，恢复后重回 120/121 passed）：①`generate_new_url` 去掉 `format_host_for_url` → `a2a::registry::tests::generate_new_url_brackets_ipv6_hosts` FAILED；②`unsubscribe_prompt` 改为直接返回 `false` → `http::tests::unsubscribing_and_close_stop_polling_the_key` FAILED；③`skill_exists_detailed` 把非 not_found 错误折成 `Ok(false)` → `skill::tests::a_non_not_found_failure_is_not_swallowed` FAILED。
- **已知问题**：无编译/测试阻塞。**2026-10-29 复核**：protocol crate A2A 模型统一已完成（见 history/NACOS__MIGRATION_HISTORY.md）；**`documentationUrl`、`TransportProperties`、`zip` 三项跨模块请求均已关闭**（§4.2-5 / §4.2-6 / §4.2-2）。**真实剩余**：①真实 `NacosAiService`（`reqwest`）未实现（仅 trait + `InMemoryNacosAiService`，归属待裁定，§4.2-3）；②`getAllSkillNames`/`getRepositoryInfo` 无对应（harness `SkillRepository` 缺方法，S-3）；③`SkillRepository::load` 同步 trait 内 `block_on`（§4.2-4）；④`AgentCardResolver` 无 TTL（§3.3）。

### 4.2 下一步与阻塞（跨模块需求）

1. ~~**【跨模块依赖】protocol crate 的 `a2a` 子模块不可编译**~~ → **已勘误并关闭（见 history/NACOS__MIGRATION_HISTORY.md）**：protocol 的 A2A 模型可编译，本 crate 已于 2026-10-28 复用（`agentscope-extensions-a2a-client` 为新增 path 依赖）。仅 `AgentRegistry` 仍是本 crate 本地 trait。
2. ~~**【汇总方/根依赖】`zip = 0.6` 现为 crate 内独立版本声明**~~ → **已完成（2026-10-29）**：`zip = "0.6"` 已登记根 `[workspace.dependencies]`（`default-features = false, features = ["deflate"]`），本 crate 改为 `zip = { workspace = true }`。
3. **【跨模块】真实 Nacos AI 客户端实现未提供**：`NacosAiService` 仅给出 trait + 内存实现；真实 HTTP 实现是否另立 crate / 归入 protocol 侧，待汇总方裁定。
4. **【可选】nacos-skill 的 `load()` 为同步 trait 方法**：内部用 `block_on` 驱动 async 下载。若 harness 未来为 `SkillRepository` 增加 async 变体，应切换以避免 `block_in_place` 限制。
5. ~~**【跨模块】protocol `AgentCard` 缺 `documentation_url`**~~ → **已完成（2026-10-29）**：`agentscope_extensions_a2a_client::model::AgentCard` 已新增 `documentation_url`（serde `documentationUrl`，往返用例 `agent_card_serializes_documentation_url`）；本 crate 的 `A2aAgentCard` 已删除本地字段，仅保留 `#[serde(flatten)] inner: AgentCard` 包装（`Deref`/`DerefMut` 转发字段），未改用 `pub type`（保留 `new`/getter 便利方法）。
6. ~~**【跨模块】`TransportProperties` 未复用**~~ → **已完成（2026-10-29）**：该类型已从 `agentscope-extensions-a2a-server::card` 下沉到 `agentscope-extensions-a2a-client::transport_properties`（server 侧改为 re-export，公共路径 `a2a::card::TransportProperties` 不变）。本 crate **仍保留自己的必填版** `a2a::registry::TransportProperties`（`host: String` / `port: i32`），因其来自 `AgentRegistry::register` 的必填语义，与共享版 `Option` 形态不同，不属重复实现；两者互转见 §3.5 NACOS-01。
7. **【本 crate 依赖变更】新增 path 依赖 `agentscope-extensions-a2a-server`**（2026-10-31，NACOS-01）：为让 `NacosAgentRegistry` 实现协议侧 `AgentRegistry`，本 crate 现在 **unconditionally** 依赖 a2a-server，因而默认构建会一并编译 `axum` 与 `reqwest`（a2a-server / a2a-client 的既有依赖）。这是 NACOS-01 的必要代价：注册链路必须能构造出 `Arc<dyn a2a_server::registry::AgentRegistry>`。若后续要求本 crate 默认构建无 HTTP 栈，可把该实现挪到独立 crate 或 feature 后，由汇总方裁定。
8. **【2026-11-01 Round 4】本轮无新增跨模块需求或阻塞**：NACOS-08/09/10/12/13/14 全在本 crate 内闭合（`src/a2a/registry/mod.rs`、`src/http.rs`、`src/http/reqwest_transport.rs`、`src/skill/mod.rs`），未触碰 core/harness/根 `Cargo.toml`。订阅生命周期方法不加进 `NacosAiService` trait 的理由见 §4.3.2 NACOS-X11。

### 4.3 与 Java 的已知差异（有意为之，非缺口）

> 本节由 2026-10-31 全量对齐复核建立；原先散落在 §4.2 / §2 中「属差异而非缺口」的条目已归并于此（§4.2 保留跨模块阻塞项）。

#### 4.3.1 设计决定（不迁移）

| 编号 | 项 | Java 参考 | 不迁移理由（源码级依据） | 影响面 |
|---|---|---|---|---|
| NACOS-D01 | harness `DistributedStore` 不实现 | 模块内无对应物：`rg 'DistributedStore' <java-root>` 零命中；Nacos 侧只有 config/registry 语义（`NacosSkillRepository.java` 无 state/key-value 读写） | Rust `DistributedStore` 承载 `agent_state_store` + `isolation_scope`（`crates/agentscope-harness/src/agent/distributed_store.rs:16-37`），而 Nacos AI API 无「按 key 读写 agent state」的方法（`NacosAiService` trait 见 `src/service.rs:108-169`）；强行实现只能把 registry 语义硬塞进 state store，语义错位 | 无：`HarnessAgent` 仍用本地或其它存储 crate 提供 `distributed_store`；本 crate 只做 a2a/prompt/skill |
| NACOS-D02 | `AiFactory.createAiService(Properties)` 工厂与 `Properties` 构造器 | `discovery/NacosAgentCardResolver.java:70`（`AiFactory.createAiService(properties)`）、`registry/NacosAgentRegistry.java#builder(Properties)` | 无官方 Rust Nacos AI SDK，工厂无实现可调；本 crate 以 `NacosAiService` trait 注入（`src/service.rs:108`，真实实现 `src/http.rs:133`），客户端细节（地址/鉴权/超时）由实现方决定 | 调用方需自行构造 service（`NacosHttpAiService` 已提供，含 `NacosHttpConfig`）；不损失能力 |
| NACOS-D03 | JVM 系统属性优先级层（`System.getProperty`） | `skill/NacosSkillRepository.java:152-158`（properties → `System.getProperty` → `System.getenv` 三层） | Rust 无 JVM sysprop 机制；`src/skill/mod.rs:114-126` 保留「显式属性 map → `std::env`（`agentscope.nacos.skill.version` 与 `AGENTSCOPE_NACOS_SKILL_VERSION`）」两层，部署侧注入仍完整可用 | 仅影响 `-D` 注入方式；环境变量/显式属性行为不变 |
| NACOS-D04 | `setWriteable(boolean)`（忽略）与 `close()`（no-op） | `skill/NacosSkillRepository.java:216-218`、`:265-267` | harness `SkillRepository` 无 `set_writeable`/`close`（见 `crates/agentscope-harness/src/agent/skill/repository.rs`），本 crate 以 `is_writeable() -> false` 常量表达只读（`src/skill/mod.rs:246-248`）；无副作用的方法不新造 | 无：只读语义等价，且调用方无法误开写 |
| NACOS-D05 | `getAllSkills()` / `getAllSkillNames()` 的 Java 方法形态与告警文案 | `skill/NacosSkillRepository.java:243-263` | 由 harness `SkillRepository::load` + `known_skill_names` 承载（`src/skill/mod.rs:258-280`：空列表时 `warn` 并返回空；否则逐一下载、失败跳过）；`all_skill_names()`（`:168`）只补「只取名字」这一 Java 侧能力，不新建仓库接口 | 无（行为等价；Java 的逐条 `getSkill` 网络调用次数也一致） |

#### 4.3.2 设计决定与已知差异

| 编号 | 项 | Rust 实现 | 与 Java 的差异 | 为何保留（收益/依据） |
|---|---|---|---|---|
| NACOS-X01 | `NacosAiService` 与 `AgentCardResolver` 全异步 | `src/service.rs:108`（`#[async_trait]`）、`src/a2a/discovery.rs:28` | Java Nacos SDK 阻塞、`AgentCardResolver#getAgentCard` 同步 | 与 core/harness 的 async 运行时一致，避免把 `block_on` 铺到调用链；同步消费方（harness `SkillRepository::load`）在 `src/skill/mod.rs:286-320` 局部适配（先判 `runtime_flavor`，current-thread 走 scratch runtime，已修 `block_in_place` panic） |
| NACOS-X02 | 卡片缓存可选 TTL + `invalidate` | `src/a2a/discovery.rs:62-82`（`with_ttl` / `invalidate` / `cached`） | Java 永不过期、只依赖 push（`agentCardCaches` 无 TTL） | 默认 `None` 与 Java 完全一致；HTTP 轮询可能丢事件/断连，TTL 是可用兜底（用例 `a_zero_ttl_refetches_the_card`、`a_cached_card_never_expires_without_a_ttl`、`invalidate_drops_a_cached_card`） |
| NACOS-X03 | 多 endpoint 逐次 `register_agent_endpoint` | `src/a2a/registry/mod.rs:400-430`（`endpoints` 去重后逐个 POST） | Java 1 个 transport 走单值重载、≥2 个走 `Set` 批量重载（`NacosA2aRegistry.java:109-133`） | 去重语义保留（用例 `duplicate_endpoints_are_registered_once`）；trait 面更小、失败可按 endpoint 定位；批量重载可按需后补 |
| NACOS-X04 | transport 键统一大写 | `src/a2a/registry/mod.rs:186`、`:224`、`:539`（`to_uppercase` 后入 map / 合并） | Java `addTransport` 按**原始**大小写存键（`NacosA2aRegistryProperties.java:47`），而 `overwritePreferredTransport` 用大写查（`NacosAgentRegistry.java:206`）→ 小写 transport 时覆写**静默失效** | 修复 Java 的大小写匹配缺陷：小写 `jsonrpc` 也能被 `overwritePreferredTransport` 命中（用例 `agent_registry_overwrites_preferred_transport`）；env 与 deploy 两路合并也不会产生重复条目 |
| NACOS-X05 | 安全方案以 JSON map 承载 + `type` 分派归一 | `src/a2a/security_scheme.rs:32`/`:50`（map ↔ map），`NacosAgentCard.security_schemes: serde_json::Map`（`src/a2a/model.rs:196`） | Java 同样用 map，但转换器包级私有且依赖 SDK 的 `SecurityScheme` 类 | 协议侧 `SecurityScheme` 为 `Value`（`a2a-client` 模型），保留 map 既与 wire 格式一致，又不需要平行强类型层次；未知 `type` 读写行为与 Java 一致（读丢弃/写折叠为 `{}`） |
| NACOS-X06 | skill 域复用 harness `SkillRepository` | `src/skill/mod.rs:237-280`（`repo_id`/`source`/`is_writeable`/`repo_kind`/`load`） | Java 实现 `AgentSkillRepository`（含 `getAllSkillNames`/`getRepositoryInfo`/`setWriteable`/`close`） | 不新建平行仓库抽象（既有裁决 + 本轮复核）；缺的两个方法以固有方法 `all_skill_names()`（`:168`）/`repository_info()`（`:173`）提供，`NacosSkillRepositoryInfo` 的 `Display` 与 Java `toString` 逐字一致 |
| NACOS-X07 | 订阅刷新以轮询替代 gRPC push | `src/http.rs:205-266`（`poll_once`：按订阅表重取 + 值比较后才回调）、`src/http.rs:547`（`polling` 定时驱动）、`src/http/reqwest_transport.rs`（`spawn_polling` 薄包装） | Java SDK 长连接推送，`onEvent` 被动到达 | 无官方 Rust SDK，纯 HTTP 可实现且可离线单测（fake transport）；代价是需宿主驱动轮询、且存在轮询间隔内的可见延迟（改进见 NACOS-06/13） |
| NACOS-X08 | 保留本 crate 的**必填版** `TransportProperties` | `src/a2a/registry/mod.rs:441-458`（`host: String` / `port: i32`） | 协议侧 `a2a-client/src/transport_properties.rs:19` 为 `Option<String>` / `Option<u16>` + `extra`，`effective_port()` 有 443/80 默认 | 本类型来自 `AgentRegistry::register` 的「服务已实际部署」语义，host/port 必填才安全；`Option` 形态会在缺失时静默用 443/80，与注册真实性冲突。互操作转换随 NACOS-01 一并落地 |
| NACOS-X09 | 仓库标识 `repo_id() = nacos:<namespace>` + `SkillRepo::External { source }` | `src/skill/mod.rs:238-256` | Java 无 `repo_id`（仓库实例本身即标识） | harness `SkillRepository` 契约要求稳定 id 用于去重与工具命名空间；`source` 仍与 Java `getSource()` 同值（`nacos@<ns>`） |
| NACOS-X10 | 协议 `AgentRegistry`（同步）→ 本 crate async `register` 的桥接 | `src/a2a/registry/mod.rs:544-575`（`futures::executor::block_on`） | Java 侧 `register` 本身阻塞，无桥接 | 协议 trait 是同步的（与 Java 一致），Nacos 侧 `NacosAiService` 是 async → 只能阻塞驱动。**注意**：若调用发生在 tokio 工作线程且实现内部依赖同一 runtime 的 I/O（如 `ReqwestNacosTransport`），`block_on` 会阻塞该线程；建议宿主在 blocking 上下文调用 `AgentRegistryService::register`（`InMemoryNacosAiService` 等纯内存实现不受影响，用例即此路径） |
| NACOS-X11 | 订阅生命周期方法（`unsubscribe_prompt` / `unsubscribe_agent_card` / `close`）与 `polling(interval)` 均为 `NacosHttpAiService` 的**固有方法**，不加进 `NacosAiService` trait | `src/http.rs:512`/`:522`/`:533`/`:547` | Java 无退订方法（生命周期由 Nacos SDK 客户端对象销毁承担），Nacos 官方 `AiService` 亦无 `unsubscribe*`；本 crate 的 trait 刻意与 Java 方法面一致 | 不改 trait 即不破坏既有实现（`InMemoryNacosAiService`、外部实现者）；代价是持 `Arc<dyn NacosAiService>` 的消费方（`NacosPromptListener`/`NacosAgentCardResolver`）需另持具体服务或 `Arc<NacosHttpAiService>` 才能退订 |

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-nacos` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-nacos` / `cargo test -p agentscope-extensions-nacos` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-nacos/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. 执行历史（已归档）

- 执行记录 / 静态复核 / 勘误 / 反向验证明细：[`history/NACOS__MIGRATION_HISTORY.md`](./history/NACOS__MIGRATION_HISTORY.md)
- 本文件只保留现行状态（§0–§5）；历史条目一律写进上述归档文件，不再回填本文。
