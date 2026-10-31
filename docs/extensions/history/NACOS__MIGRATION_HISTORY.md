# NACOS 迁移执行历史（归档）

> 本文件是 [`../NACOS__GAP_TRACKER.md`](../NACOS__GAP_TRACKER.md) 的执行历史归档（截止 2026-10-31）。
> **现行状态以 tracker 为准**；本文件只读取证，不再更新。
> 内容来源：原 GAP_TRACKER 中 §5 之后各节，原文未改写（节标题：6. 静态复核记录（2026-10-22） · 7. 2026-10-28 执行记录（本轮：A2A-1/2/3/4/5、P-1、S-1/S-2） · 8. 2026-10-31 loop 收口记录（模块：NACOS；仅改本 crate + 本 tracker））。

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

- **S-3 已关闭（2026-10-31）**：`getAllSkillNames`/`getRepositoryInfo` 在 `NacosSkillRepository` 上以**固有方法**提供（`all_skill_names()` / `repository_info() -> NacosSkillRepositoryInfo`）；harness `SkillRepository` 不加方法（§8-3）。
- 既有测试中编码了错语义的 3 条已**收紧**到 Java 语义（非放宽）：`parse_frontmatter_uses_fallback_name` → `missing_name_is_rejected`；`resource_paths_are_normalized_to_forward_slashes` → `resource_paths_are_relative_to_the_root`；`empty_capabilities_round_trip_as_defaults` → `nacos_capabilities_become_definite_booleans`；`security_schemes_kept_as_opaque_json` → `security_schemes_are_converted_by_type`。
- `NacosAgentCard.url` 由 `String` 改为 `Option<String>`（对齐 Nacos/Java 可空 url）；调用点 `registry` 测试已同步。

---

## 8. 2026-10-31 loop 收口记录（模块：NACOS；仅改本 crate + 本 tracker）

> 本回合约束：**只做代码编辑，不运行 cargo / rustc / clippy / fmt**（由主智能体统一验证）。下文「用例名」均为待主智能体验证的离线用例，**不是已跑过的结论**。

### 8-1 P1：真实 `NacosAiService`（HTTP）已落地

| 维度 | 结论 |
|---|---|
| 新增文件 | `src/http.rs`（客户端主体 + 测试）、`src/http/reqwest_transport.rs`（`reqwest` 适配，feature `http-client`） |
| 选型 | 客户端逻辑与传输分离：`NacosHttpTransport`（`async fn execute(HttpRequest) -> HttpResponse`）为缝隙，避开对 `reqwest` 的硬依赖；默认构建（`default = []`）**不引入任何新依赖**，客户端逻辑仍编译且被假 transport 全覆盖 |
| 端点 | `/v1/ai/agent`（release/GET）、`/v1/ai/agent/endpoint`、`/v1/ai/prompt`、`/v1/ai/skill`；四个路径均可在 `NacosHttpConfig` 覆盖（不同 Nacos 部署的前缀 `/nacos` 与路径版本差异） |
| 语义 | ①`accessToken` 追加为 query 参数并做 RFC3986 转义；②响应同时接受裸 JSON 与 Nacos 信封 `{"code":0,"data":…}`；③`404` → `get_*` 返回 `Ok(None)`、`subscribe_*`/`download_*` 返回 `AiServiceError::not_found`；④错误体 `{code,message}` 逐字保留（`a_nacos_error_body_keeps_its_code_and_message`） |
| 订阅 | Nacos 走 gRPC 推送，HTTP 无法持久流 → `subscribe_*` 做首次拉取 + 登记 listener，新增 `poll_once()` 做变更检测（返回触发数）；`http-client` 下另有 `spawn_polling(service, interval)` 定时驱动 |
| 用例 | `get_agent_card_builds_the_v1_url_and_parses_the_card`、`an_absent_card_is_none_but_a_broken_payload_is_an_error`、`the_nacos_envelope_is_unwrapped`、`release_agent_card_posts_the_card_and_set_as_latest`、`register_agent_endpoint_posts_the_endpoint_json`、`prompts_are_parsed_and_subscriptions_fire_on_change`、`agent_card_subscriptions_are_polled_for_changes`、`subscribing_to_a_missing_card_is_not_found`、`skill_zips_are_downloaded_with_version_or_label`、`the_http_service_is_interchangeable_with_the_in_memory_one`、`urls_skip_blank_parameters`、`a_nacos_error_body_keeps_its_code_and_message` |
| 风险 | 上游 AI OpenAPI 的**精确路径/参数形状**无法离线核实（Nacos 版本差异），因此全部路径/参数集中在一处并用可覆盖的 `NacosHttpConfig` 表达；真实实例联调时只需改配置 |

### 8-2 Java 增量 `5da592b4a`（fix(nacos): preserve A2A capability extensions）已对齐

- **Java**：`AgentCardConverterUtil` 双向补 `capabilities.extensions`：`uri` / `description` / `required`（缺失→`false`）/ `params`，并保留 `null` vs 空列表的区别。
- **Rust 原状**：`convert_capabilities_to_a2a` 硬编码 `extensions: None`、`convert_capabilities_to_nacos` 丢字段 —— 即该 Java bug 的同构缺陷（注册/解析经 Nacos 往返后扩展全丢）。
- **落地**：`a2a/model.rs` 新增 `NacosAgentExtension`（四字段 + `#[serde(flatten)] extra`）与 `NacosAgentCapabilities::extensions: Option<Vec<..>>`；`a2a/converter.rs` 新增 `convert_extension_to_a2a` / `convert_extension_to_nacos`。`Option::as_ref().map(..)` 保留 `None` vs `Some(vec![])`。**Rust 额外**：A2A 侧把 extensions 建模为不透明 `Vec<Value>`，未知字段进 `extra` 后能原样回写（Java 会丢）。
- **用例**：`nacos_extensions_keep_uri_description_required_and_params`、`a_missing_nacos_required_flag_becomes_false`、`absent_and_empty_extension_lists_stay_distinct`、`unknown_extension_fields_survive_a_round_trip`、`a2a_extensions_without_a_nacos_counterpart_are_kept_verbatim`。

### 8-3 P2 三项裁定 + 落地

| 项 | 裁定 | 落地 |
|---|---|---|
| `AgentCardResolver` 无 TTL | 补 TTL，默认 `None`（Java 行为不变） | `with_ttl(Duration)` / `invalidate(name)`；缓存项带 `fetched_at: Instant` |
| `SkillRepository::load` 同步 trait 内 `block_on` | 保留同步 `block_on`（harness trait 不改），但修正 runtime flavour 判断，消除 current-thread runtime panic | `Handle::runtime_flavor() == MultiThread` 才 `block_in_place`，否则临时 current-thread runtime |
| `getAllSkillNames` / `getRepositoryInfo` 无对应 | **harness `SkillRepository` 不加方法**（跨 crate，越界）；在 crate 内以固有方法提供 | `NacosSkillRepository::all_skill_names()`、`repository_info() -> NacosSkillRepositoryInfo`（`Display` 对齐 Java `toString`） |

用例：`a_zero_ttl_refetches_the_card`、`a_cached_card_never_expires_without_a_ttl`、`invalidate_drops_a_cached_card`、`load_from_a_current_thread_runtime_does_not_panic`、`all_skill_names_and_repository_info_are_available`。

### 8-4 改动文件

| 文件 | 改动 |
|---|---|
| `src/http.rs`（新） | `HttpRequest` / `HttpResponse` / `NacosHttpTransport` / `NacosHttpConfig` / `NacosHttpAiService`（完整 trait 实现 + `poll_once`）+ 12 用例 |
| `src/http/reqwest_transport.rs`（新） | `ReqwestNacosTransport`、`spawn_polling`（feature `http-client`） |
| `src/lib.rs` | `pub mod http;` + 导出；`http-client` 下导出 `ReqwestNacosTransport`/`spawn_polling` |
| `src/a2a/model.rs` | `NacosAgentExtension` + `NacosAgentCapabilities::extensions` |
| `src/a2a/converter.rs` | capability extensions 双向转换 + 5 用例 |
| `src/a2a/mod.rs` | 导出 `NacosAgentExtension` |
| `src/a2a/discovery.rs` | TTL/`invalidate` + `CachedCard` + 3 用例 |
| `src/skill/mod.rs` | `block_on` flavour 修正、`all_skill_names`/`repository_info` + `NacosSkillRepositoryInfo` + 2 用例 |
| `Cargo.toml` | `[features] default=[] / http-client=["dep:reqwest"]`；`reqwest = { workspace = true, optional = true }` |

### 8-5 跨模块需求 / 未关闭项

1. **跨模块**：harness `AgentRegistry`/`SkillRepository::get_all_skill_names` 仍缺位 —— 本回合按约束不改 harness，nacos 侧用本地 trait + 固有方法补偿。
2. **未关闭**：Nacos 长连接（gRPC/`ConfigService` 监听）未实现 —— 以 `poll_once`/`spawn_polling` 代替，属**语义近似**（延时不等于推送）。若要真推送需引入 Nacos gRPC 客户端（无成熟 Rust SDK）。
3. **未关闭**：`http-client` feature 下的 `reqwest` 传输层无可离线单测（需真实 server），其逻辑极薄（造请求 → 取 status/body），由 `NacosHttpTransport` 假实现覆盖同构路径。

