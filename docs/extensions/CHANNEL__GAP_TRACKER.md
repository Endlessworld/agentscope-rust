# CHANNEL 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **CHANNEL** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-channel\`
> **Java 子模块**：common, dingtalk, feishu, github, gitlab, wecom
> **Rust 目标**：`crates/agentscope-extensions-channel/`（波次 A）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-channel`（6 个 Maven 子模块：common, dingtalk, feishu, github, gitlab, wecom） |
| Java 规模（实测） | 主代码 4879 行 / 测试 0 行（`Get-ChildItem -Recurse -Filter *.java` 逐文件行数求和，39 个文件） |
| Rust crate | `crates/agentscope-extensions-channel`（`src/` 38 个 `.rs`，4123 行） |
| 迁移波次 | A |
| 当前状态 | **部分完成**：P0 离线适配层全部落地，`cargo build` + `cargo test` 全绿（118 passed / 0 failed）；未迁移项为 Stream/WebSocket 客户端、Callback Controller、Crypto 解密、`*Channel`/`*Registry`/`*BotIdentityResolver`（见 §3） |

### 1.1 与 harness 的边界（已裁定，必须遵守）
harness 提供**路由/会话绑定骨架**（不可改）：`crates/agentscope-harness/src/agent/` 下 `channel.rs`、`chatui.rs`、`inbound.rs`、`router.rs`、`binding.rs`、`peer.rs`、`dm_scope.rs`、`config.rs`。
本 crate 只提供**平台适配**：`access token provider`、`inbound mapper`、`outbound client`、`channel properties`、`signature verifier`。
不重复定义 `Channel` / `InboundMessage` / `Peer` / `PeerKind` / `OutboundAddress` —— 一律从 `agentscope_harness::agent` 导入复用。

## 2. Java 清单与依赖映射

### 2.1 common（Java 172 行 → Rust 1150 行含 crate 级公共件）
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `common/BotLoopGuard.java` (85) | 按 peer 的滑动窗口节流，防机器人自回环 | `common/bot_loop_guard.rs` (`BotLoopGuard`, `BotLoopGuardConfig`) | ✅ 已迁移 |
| `common/IdempotencyStore.java` (87) | 有界 + TTL 的入站事件去重 | `common/idempotency_store.rs` (`IdempotencyStore`) | ✅ 已迁移 |
| —（Rust 侧新增） | 每平台必用的公共件 | `common/error.rs`(`ChannelError`)、`common/http.rs`(`HttpTransport`/`ReqwestTransport`/`PreparedRequest{Response}`)、`common/outbound_address.rs`(`parse_peer_target`/`PeerTarget`)、`common/props.rs`、`common/text.rs`、`common/token.rs`(`AccessTokenProvider`/`TokenCache`)、`common/xml.rs`、`common/testing.rs`(`#[cfg(test)]` 测试替身) | ✅ |

### 2.2 dingtalk（Java 962 行）
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `DingTalkChannelProperties.java` (80) | 配置解析 | `dingtalk/properties.rs` | ✅ |
| `DingTalkInboundMapper.java` (117) | Stream 机器人消息 JSON → `InboundMessage` | `dingtalk/inbound.rs` | ✅ |
| `DingTalkOutboundClient.java` (163) | 单聊/群聊发送请求体构造 | `dingtalk/outbound.rs` | ✅ |
| `DingTalkAccessTokenProvider.java` (92) | OpenAPI `accessToken` 缓存获取 | `dingtalk/token.rs` | ✅ |
| `DingTalkChannel.java` (229) | `Channel` 生命周期/路由适配 | — | ❌ P1（见 §3.2） |
| `DingTalkStreamClient.java` (281) | 常驻 WebSocket 网关（Stream 模式） | — | ❌ P1（见 §3.2） |

### 2.3 feishu（Java 1054 行）
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `FeishuChannelProperties.java` (76) | 配置解析 | `feishu/properties.rs` | ✅ |
| `FeishuInboundMapper.java` (175) | 事件订阅 v2 信封 → `InboundMessage` | `feishu/inbound.rs` | ✅ |
| `FeishuOutboundClient.java` (167) | `POST /open-apis/im/v1/messages` 请求构造（含 threaded reply / `uuid` 去重） | `feishu/outbound.rs` | ✅ |
| `FeishuAccessTokenProvider.java` (95) | `tenant_access_token` 缓存获取 + 失效重取 | `feishu/token.rs` | ✅ |
| `FeishuCrypto.java` (130) | AES-256-CBC 载荷解密 + 签名 | — | ❌ P1（解密半部未迁；`Encrypt Key` 校验逻辑待定，见 §3.2） |
| `FeishuCallbackController.java` (164) | HTTP 回调入口（Spring MVC） | — | ⏸ 设计决定/待裁定（见 §3.4） |
| `FeishuChannel.java` (206) / `FeishuChannelRegistry.java` (41) | Channel 适配与注册 | — | ❌ P1/P2 |

### 2.4 github（Java 853 行）
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `GitHubChannelProperties.java` (72) | 配置解析 | `github/properties.rs` | ✅ |
| `GitHubInboundMapper.java` (116) | webhook delivery → `InboundMessage` | `github/inbound.rs` | ✅ |
| `GitHubOutboundClient.java` (125) | issue/PR 评论 POST 请求构造 | `github/outbound.rs` | ✅ |
| `GitHubSignatureVerifier.java` (70) | `X-Hub-Signature-256` HMAC-SHA256 校验 | `github/signature.rs` | ✅ |
| `GitHubBotIdentityResolver.java` (91) | 机器人身份解析（防自回环判定） | — | ❌ P1 |
| `GitHubWebhookController.java` (139) | HTTP webhook 入口 | — | ⏸ 待裁定（同 §3.4） |
| `GitHubChannel.java` (201) / `GitHubChannelRegistry.java` (39) | Channel 适配与注册 | — | ❌ P1/P2 |

### 2.5 gitlab（Java 746 行）
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `GitLabChannelProperties.java` (69) | 配置解析 | `gitlab/properties.rs` | ✅ |
| `GitLabInboundMapper.java` (108) | Note Hook 载荷 → `InboundMessage` | `gitlab/inbound.rs` | ✅ |
| `GitLabOutboundClient.java` (117) | issue/MR note POST 请求构造 | `gitlab/outbound.rs` | ✅ |
| —（Java 内联在 Controller） | `X-Gitlab-Token` 常量时间校验 | `gitlab/signature.rs` (`verify_webhook_token`) | ✅ |
| `GitLabBotIdentityResolver.java` (80) | 机器人身份解析 | — | ❌ P1 |
| `GitLabWebhookController.java` (142) | HTTP webhook 入口 | — | ⏸ 待裁定（同 §3.4） |
| `GitLabChannel.java` (191) / `GitLabChannelRegistry.java` (39) | Channel 适配与注册 | — | ❌ P1/P2 |

### 2.6 wecom（Java 1092 行）
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `WeComChannelProperties.java` (101) | 配置解析 | `wecom/properties.rs` | ✅ |
| `WeComInboundMapper.java` (120) | 解密后回调 XML → `InboundMessage` | `wecom/inbound.rs`（依赖 `common/xml.rs`） | ✅ |
| `WeComOutboundClient.java` (165) | `/cgi-bin/message/send` + `/cgi-bin/appchat/send` 请求构造 | `wecom/outbound.rs` | ✅ |
| `WeComAccessTokenProvider.java` (94) | `access_token` 缓存获取 | `wecom/token.rs` | ✅ |
| `WeComCrypto.java` (158) | 回调 SHA-1 签名校验 + AES-256-CBC 解密 | `wecom/signature.rs`（仅签名半部） | ⚠️ 部分（解密半部见 §3.2） |
| `WeComCallbackController.java` (200) | HTTP 回调入口（含 URL 验证握手） | — | ⏸ 待裁定（同 §3.4） |
| `WeComChannel.java` (209) / `WeComChannelRegistry.java` (45) | Channel 适配与注册 | — | ❌ P1/P2 |

### 2.7 依赖映射（Java → Rust）
以下版本均取自本 crate `crates/agentscope-extensions-channel/Cargo.toml` 的 `[dependencies]`（未改根 `Cargo.toml` 的 `[workspace.dependencies]`）。
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| Spring `WebClient` / OkHttp | 出站 HTTP | `reqwest`（workspace 版本）+ 自建 `HttpTransport` 薄封装 | `HttpTransport` 是可注入 seam，测试用 `common::testing::RecordingTransport` 离线录制，**不发起真实网络** |
| Jackson `ObjectMapper` | JSON 解析/序列化 | `serde` + `serde_json`（workspace 版本） | DTO 直接以 `serde_json::Value` 解析，保持依赖轻量、可离线测 |
| `DocumentBuilderFactory`（DOM XML） | WeCom 回调 XML 取文本 | `quick-xml = "0.39"` | `common/xml.rs::xml_element_text` 复刻 `getElementsByTagName` + `getTextContent` 语义（含 CDATA、实体解析、首个匹配元素） |
| `javax.crypto.Mac` HmacSHA256 | GitHub 签名校验 | `hmac = "0.12"` + `sha2 = "0.10"` | — |
| `MessageDigest` SHA-1 | WeCom 回调签名 | `sha1 = "0.10"` | — |
| `java.util.Base64` | 签名/密钥编解码 | `base64 = "0.22"` | — |
| `URLEncoder` | 表单编码 | `percent-encoding = "2.3"` | — |
| OkHttp WebSocket / `reactor-netty` | 常驻连接（DingTalk Stream） | （未引入） | 未迁移，故不引依赖；见 §3.2 |
| Spring `@RestController` | 回调 HTTP 入口 | （未引入） | 未迁移，故不引依赖；见 §3.4 |

## 3. 详细缺口清单

### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 接线 + 六平台模块声明 | 全部子模块 | 已完成 | `src/lib.rs` 声明 `common`/`dingtalk`/`feishu`/`github`/`gitlab`/`wecom` 并 re-export；crate 级文档注释说明 harness 边界 |
| 公共抽象归属 | `common/*` | 已完成 | 复用 harness `Channel`/`InboundMessage`/`Peer`/`OutboundAddress`；本 crate 未新建平行类型（符合汇总 tracker §2.1「Channel 抽象」裁定） |
| 各平台 provider 配置解析 | `*ChannelProperties.java` ×5 | 已完成 | 5 个 `properties.rs` 全部落地 |
| 依赖声明 | — | 已完成 | 8 个新依赖写入本 crate `Cargo.toml`，均标具体版本号 |
| 离线测试覆盖 | Java 侧 0 行测试 | 已完成 | Rust 新建 118 个单测，覆盖 inbound 映射、outbound 请求体、幂等、防回环、签名校验、XML/实体解析、token 缓存 |

### 3.2 功能缺口（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| DingTalk Stream 常驻 WebSocket 客户端 | `dingtalk/DingTalkStreamClient.java` (281) | 待办 | 需引入 WebSocket 依赖（`tokio-tungstenite`）与重连/心跳逻辑；本 crate 内可独立完成 |
| `FeishuCrypto` AES-256-CBC 解密 | `feishu/FeishuCrypto.java` (130) | 待办 | 需 `aes` + `cbc` 依赖；属本 crate 内缺依赖，非跨模块需求 |
| `WeComCrypto` AES-256-CBC 解密 | `wecom/WeComCrypto.java` (158) | 待办 | 同上；签名半部已在 `wecom/signature.rs` 落地 |
| 各平台 `*Channel` 适配 | `dingtalk/DingTalkChannel.java` (229)、`feishu/FeishuChannel.java` (206)、`github/GitHubChannel.java` (201)、`gitlab/GitLabChannel.java` (191)、`wecom/WeComChannel.java` (209) | 待办 | 需先确认 harness `agent::Channel` trait 的完整契约（现有骨架是否足以承载每平台的 inbound 分发 + outbound 投递）；若不足 → 记入 §4.2，不得改 harness |
| 机器人身份解析（防自回环） | `github/GitHubBotIdentityResolver.java` (91)、`gitlab/GitLabBotIdentityResolver.java` (80) | 待办 | 与 `common/bot_loop_guard.rs` 配合使用 |

### 3.3 增强项（P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| Channel 注册表 | `feishu/FeishuChannelRegistry.java` (41)、`github/GitHubChannelRegistry.java` (39)、`gitlab/GitLabChannelRegistry.java` (39)、`wecom/WeComChannelRegistry.java` (45) | 待办 | 依赖 P1 的 `*Channel` 先落地；另需复核是否与 harness `router.rs`/`binding.rs` 的注册能力重叠（避免平行实现） |
| A2A/长轮询等拉取式入站 | — | 待确认 | Java 侧无对应类，暂不处理 |

### 3.4 设计决定 / 待裁定
| 项 | Java 参考 | 状态 | 理由 |
|---|---|---|---|
| HTTP Webhook Controller 不迁移 | `feishu/FeishuCallbackController.java` (164)、`github/GitHubWebhookController.java` (139)、`gitlab/GitLabWebhookController.java` (142)、`wecom/WeComCallbackController.java` (200) | **待裁定** | Java 用 Spring MVC 注解暴露端点。Rust 侧需选一个 HTTP 框架（如 `axum`，`agentscope-extensions-protocol` 已用它）。**未擅自引入**：属「为框架选型决策」，请汇总方裁定后再实施。当前只保留可复用的纯逻辑（签名校验、解密、inbound 映射），Controller 薄壳待框架确认后补 |
| Spring AutoConfiguration / `@ConfigurationProperties` 绑定 | 各子模块 Spring 配置类 | **设计决定（不迁移）** | Rust 无 Spring 容器；配置改为 `*ChannelProperties` 显式解析函数（已落地） |
| `DingTalkChannelProperties` 等字段命名策略 | 各 `*ChannelProperties.java` | 设计决定 | 保持 Java 的 camelCase 键名，避免下游配置迁移成本 |

## 4. 项目状态与下一步

### 4.1 当前状态
- **已完成**：common 全量（`BotLoopGuard`、`IdempotencyStore` 及本 crate 公共件 `HttpTransport`/`AccessTokenProvider`/`TokenCache`/XML 提取/地址解析/配置取值）；六平台的 `*ChannelProperties`、`*InboundMapper`、`*OutboundClient`、`*AccessTokenProvider`、签名校验（GitHub HMAC-SHA256 / GitLab token / WeCom SHA-1）。
- **验证命令与结果**（2026-09-18，主会话实测）：
  - `cargo build -p agentscope-extensions-channel` → Finished，0 error（仅 unused import 警告）
  - `cargo test -p agentscope-extensions-channel` → **118 passed / 0 failed / 0 ignored**
  - 全量 `cargo test --workspace` → **1485 passed / 0 failed**（exit 0）
- **本轮修复的真实缺陷（3 处）**：
  1. `common/xml.rs::xml_element_text` **实现 bug**：`quick-xml` 把 `&amp;` 从 `Event::Text` 拆出为 `Event::GeneralRef`，原实现对 `GeneralRef` 落到 `_ => {}` 分支而**静默丢弃实体**，导致 `Content` 解析出 `"hello  goodbye"` 而非 `"hello & goodbye"`（同时影响 WeCom inbound 映射）。修法：新增 `Event::GeneralRef` 分支，按 `#xHH` / `#NN` / 预定义实体（`resolve_predefined_entity`）解析；未声明实体按 XML 解析失败处理返回 `None`，对齐 Java `DocumentBuilderFactory` 的异常路径。
  2. `feishu/outbound.rs` 测试 `send_executes_one_request_per_message` **测试缺陷**：未给 `send` 播种 `code:0` 响应，而 `parse_send_response` 正确要求 `code` 字段（对齐 Java `checkSendResponse`：`!node.path("code").isIntegralNumber()` → 抛错）。修法：为两次请求 seeding 合法响应。
  3. `common/idempotency_store.rs` 测试 `capacity_is_bounded_by_evicting_oldest` **测试期望与 Java 语义冲突**：末条断言 `!first_seen_at("b", 4)` 的推理链错误 —— `order` 是**惰性重建**的（仅在 `sweep` 内按 `entry.seq` 重建），而 `first_seen_at("a", 4)` 本身会先跑 `sweep`（对齐 Java `sweep(now)` 在 `putIfAbsent` 之前），此时最老者已变成 `"b"` 而被淘汰。修法：断言改为 `first_seen_at("b", 4) == true` 并补 `size() == 3`，同步把「惰性 order 重建」固化为不变量。
- **已知问题**：截至撰写时，无已知编译或测试失败。

### 4.2 下一步与阻塞
1. **P1 续作顺序建议**：先做 `FeishuCrypto` / `WeComCrypto` 解密（纯函数，依赖 `aes`/`cbc`，可完全离线测）→ 再做各平台 `*Channel` 适配（需确认 harness `agent::Channel` 契约）→ 最后做 `DingTalkStreamClient` WebSocket。
2. **跨模块需求（需汇总方裁定，本 crate 不得自行改动）**：
   - **harness `agent::Channel` 契约是否足够**：若承载五个平台的 inbound 分发 + outbound 投递需要扩展 `channel.rs` 的 trait，请裁定「扩展 harness」还是「本 crate 内新增适配层」。**本会话未改 harness**。
   - **HTTP 框架选型**：4 个 `*CallbackController` 需要一个 HTTP 框架承载，且需与 `agentscope-extensions-protocol` 的选型（`axum`）保持一致，请汇总方统一裁定。
3. **无阻塞项**：当前 build/test 全绿。

### 4.3 与汇总 tracker 的对应
- 本文件是 CHANNEL 模块的**单一事实来源**；`docs/extensions/EXTENSIONS_GAP_TRACKER.md` §1 第 5 行、§3.1 由汇总方登记。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-channel` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-channel` / `cargo test -p agentscope-extensions-channel` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-channel/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

