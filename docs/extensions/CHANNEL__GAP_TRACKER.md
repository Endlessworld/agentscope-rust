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
| Rust crate | `crates/agentscope-extensions-channel`（`src/` 55 个 `.rs`，8872 行） |
| 迁移波次 | A |
| 当前状态 | **已完成（P0+P1+P2 全部落地）**：`cargo build` + `cargo test` 全绿（**241 passed / 0 failed**）。common 全部、六平台 `*ChannelProperties`/`*InboundMapper`/`*OutboundClient`/`*AccessTokenProvider`/签名与解密、**五个平台 `*Channel` 适配 + 四个 axum CallbackController + `DingTalkStreamClient`（常驻 WebSocket 网关）** 均已落地。 |

### 1.1 与 harness 的边界（已裁定，必须遵守）
harness 提供**路由/会话绑定骨架**（不可改）：`crates/agentscope-harness/src/agent/` 下 `channel.rs`、`chatui.rs`、`inbound.rs`、`router.rs`、`binding.rs`、`peer.rs`、`dm_scope.rs`、`config.rs`。
本 crate 只提供**平台适配**：`access token provider`、`inbound mapper`、`outbound client`、`channel properties`、`signature verifier`。
不重复定义 `Channel` / `InboundMessage` / `Peer` / `PeerKind` / `OutboundAddress` —— 一律从 `agentscope_harness::agent` 导入复用。

### 1.2 公共层归属（本轮裁定落地）

harness 新增 **`agent/gateway/platform_channel.rs`**（`PlatformChannel` trait + `PlatformCtx`），承载「每平台 inbound 分发 + outbound 投递」中**完全相同**的部分：持有 `init` 注入的 gateway、`ChannelRouter` 路由、`apply_routing_config` 热切换，并提供 `Channel` / `ChannelSink` 的 **blanket impl** —— 因此每个平台适配器只需实现 `ctx()` + `deliver_reply()` 两个方法，就能直接当作 harness `Channel` 使用，无需额外包装类型。

本 crate 的 `common/channel_core.rs`（`ChannelCore`）**组合** `PlatformCtx`（不再自存 `channel_id`/`config`/`router`/`gateway`），只追加 Java 各 `*Channel` 私有持有的两个守卫：`IdempotencyStore` + `BotLoopGuard`，并提供 `admit(dedupe_key, peer_key)` 按 Java 顺序一次性执行「去重 → 防回环」。

> 本轮修正：`common/channel_core.rs` 与 `common/channel_registry.rs` 此前已落盘但**未在 `mod.rs` 声明**（从未参与编译）。本轮接线并收敛到 `PlatformCtx` / 泛型 `ChannelRegistry<C>`，消除了与 harness 公共层的字段重复。

> 注意事项：`agentscope_harness::agent::Channel` 是 `gateway/mod.rs` 里的**遗留事件通道 struct**（与路由 trait 同名）；路由 trait 需用全路径 `agentscope_harness::agent::gateway::channel::Channel` 导入。同时 `Channel` 与 `ChannelSink` 都有 `channel_id()`，在两者均在作用域时需限定调用（如 `Channel::channel_id(&c)`）。

## 2. Java 清单与依赖映射

### 2.1 common（Java 172 行 → Rust 1150 行含 crate 级公共件）
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `common/BotLoopGuard.java` (85) | 按 peer 的滑动窗口节流，防机器人自回环 | `common/bot_loop_guard.rs` (`BotLoopGuard`, `BotLoopGuardConfig`) | ✅ 已迁移 |
| `common/IdempotencyStore.java` (87) | 有界 + TTL 的入站事件去重 | `common/idempotency_store.rs` (`IdempotencyStore`) | ✅ 已迁移 |
| —（Rust 侧新增） | 每平台必用的公共件 | `common/error.rs`(`ChannelError`)、`common/http.rs`(`HttpTransport`/`ReqwestTransport`/`PreparedRequest{Response}`)、`common/outbound_address.rs`(`parse_peer_target`/`PeerTarget`)、`common/props.rs`、`common/text.rs`、`common/token.rs`(`AccessTokenProvider`/`TokenCache`)、`common/xml.rs`、`common/testing.rs`(`#[cfg(test)]` 测试替身) | ✅ |
| —（Rust 侧新增） | `*Channel` 共用的分发管线 + 类型化注册表 | `common/channel_core.rs`(`ChannelCore`：组合 harness `PlatformCtx` + 两个守卫 + `admit(...)`/`run_turn(...)`)、`common/channel_registry.rs`(`ChannelRegistry<C>`：类型化 `channel_id -> Arc<C>` 查找，可选镜像注册到 harness `ChannelManager`) | ✅ 本轮接线 |

### 2.2 dingtalk（Java 962 行）
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `DingTalkChannelProperties.java` (80) | 配置解析 | `dingtalk/properties.rs` | ✅ |
| `DingTalkInboundMapper.java` (117) | Stream 机器人消息 JSON → `InboundMessage` | `dingtalk/inbound.rs` | ✅ |
| `DingTalkOutboundClient.java` (163) | 单聊/群聊发送请求体构造 | `dingtalk/outbound.rs` | ✅ |
| `DingTalkAccessTokenProvider.java` (92) | OpenAPI `accessToken` 缓存获取 | `dingtalk/token.rs` | ✅ |
| `DingTalkChannel.java` (229) | `Channel` 生命周期/路由适配 | `dingtalk/channel.rs`（`DingTalkChannel` + `handle_stream_payload`，实现 harness `PlatformChannel`） | ✅ |
| `DingTalkStreamClient.java` (281) | 常驻 WebSocket 网关（Stream 模式） | `dingtalk/stream.rs`（`DingTalkStreamClient` + `DingTalkStreamProtocol` + `WebSocketConnector`/`StreamSocket` seam + `TungsteniteConnector`） | ✅ |

### 2.3 feishu（Java 1054 行）
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `FeishuChannelProperties.java` (76) | 配置解析 | `feishu/properties.rs` | ✅ |
| `FeishuInboundMapper.java` (175) | 事件订阅 v2 信封 → `InboundMessage` | `feishu/inbound.rs` | ✅ |
| `FeishuOutboundClient.java` (167) | `POST /open-apis/im/v1/messages` 请求构造（含 threaded reply / `uuid` 去重） | `feishu/outbound.rs` | ✅ |
| `FeishuAccessTokenProvider.java` (95) | `tenant_access_token` 缓存获取 + 失效重取 | `feishu/token.rs` | ✅ |
| `FeishuCrypto.java` (130) | AES-256-CBC 载荷解密 + 签名 | `feishu/crypto.rs` | ✅ 本轮完成解密半部 |
| `FeishuCallbackController.java` (164) | HTTP 回调入口 | `feishu/controller.rs`（axum 薄壳，承载全部逻辑在 `FeishuChannel::handle_callback`） | ✅ 本轮完成 |
| `FeishuChannel.java` (206) | Channel 适配 | `feishu/channel.rs`（实现 harness `PlatformChannel`） | ✅ 本轮完成 |
| `FeishuChannelRegistry.java` (41) | 注册表 | `feishu::FeishuChannelRegistry` = `common::ChannelRegistry<FeishuChannel>`（泛型收敛） | ✅ 本轮完成 |

### 2.4 github（Java 853 行）
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `GitHubChannelProperties.java` (72) | 配置解析 | `github/properties.rs` | ✅ |
| `GitHubInboundMapper.java` (116) | webhook delivery → `InboundMessage` | `github/inbound.rs` | ✅ |
| `GitHubOutboundClient.java` (125) | issue/PR 评论 POST 请求构造 | `github/outbound.rs` | ✅ |
| `GitHubSignatureVerifier.java` (70) | `X-Hub-Signature-256` HMAC-SHA256 校验 | `github/signature.rs` | ✅ |
| `GitHubBotIdentityResolver.java` (91) | 机器人身份解析（防自回环判定） | `github/bot_identity.rs` | ✅ |
| `GitHubWebhookController.java` (139) | HTTP webhook 入口 | `github/controller.rs`（axum 薄壳） | ✅ 本轮完成 |
| `GitHubChannel.java` (201) / `GitHubChannelRegistry.java` (39) | Channel 适配与注册 | `github/channel.rs` / `github::GitHubChannelRegistry`（泛型收敛） | ✅ 本轮完成 |

### 2.5 gitlab（Java 746 行）
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `GitLabChannelProperties.java` (69) | 配置解析 | `gitlab/properties.rs` | ✅ |
| `GitLabInboundMapper.java` (108) | Note Hook 载荷 → `InboundMessage` | `gitlab/inbound.rs` | ✅ |
| `GitLabOutboundClient.java` (117) | issue/MR note POST 请求构造 | `gitlab/outbound.rs` | ✅ |
| —（Java 内联在 Controller） | `X-Gitlab-Token` 常量时间校验 | `gitlab/signature.rs` (`verify_webhook_token`) | ✅ |
| `GitLabBotIdentityResolver.java` (80) | 机器人身份解析 | `gitlab/bot_identity.rs` | ✅ |
| `GitLabWebhookController.java` (142) | HTTP webhook 入口 | `gitlab/controller.rs`（axum 薄壳） | ✅ 本轮完成 |
| `GitLabChannel.java` (191) / `GitLabChannelRegistry.java` (39) | Channel 适配与注册 | `gitlab/channel.rs` / `gitlab::GitLabChannelRegistry`（泛型收敛） | ✅ 本轮完成 |

### 2.6 wecom（Java 1092 行）
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `WeComChannelProperties.java` (101) | 配置解析 | `wecom/properties.rs` | ✅ |
| `WeComInboundMapper.java` (120) | 解密后回调 XML → `InboundMessage` | `wecom/inbound.rs`（依赖 `common/xml.rs`） | ✅ |
| `WeComOutboundClient.java` (165) | `/cgi-bin/message/send` + `/cgi-bin/appchat/send` 请求构造 | `wecom/outbound.rs` | ✅ |
| `WeComAccessTokenProvider.java` (94) | `access_token` 缓存获取 | `wecom/token.rs` | ✅ |
| `WeComCrypto.java` (158) | 回调 SHA-1 签名校验 + AES-256-CBC 解密 | `wecom/crypto.rs`（签名 + 解密 + 加密半部全落地） | ✅ 本轮补全解密半部 |
| `WeComCallbackController.java` (200) | HTTP 回调入口（含 URL 验证握手） | `wecom/controller.rs`（axum 薄壳；`GET` 走 URL 验证、`POST` 走事件） | ✅ 本轮完成 |
| `WeComChannel.java` (209) / `WeComChannelRegistry.java` (45) | Channel 适配与注册 | `wecom/channel.rs` / `wecom::WeComChannelRegistry`（泛型收敛） | ✅ 本轮完成 |

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
| OkHttp WebSocket / `reactor-netty` | 常驻连接（DingTalk Stream） | `tokio-tungstenite` 0.29（`connect` + `handshake` + `rustls-tls-webpki-roots`） | 生产连接器 `dingtalk/stream.rs::TungsteniteConnector`；协议逻辑与 I/O 边界分离，可离线单测 |
| Spring `@RestController` | 回调 HTTP 入口 | `axum` 0.7（workspace 依赖）+ `http`/`bytes` | **已裁定**：与 protocol/aistio 统一用 axum；控制器为薄壳，只做参数提取与 `*CallbackOutcome` → HTTP 映射 |

## 3. 详细缺口清单

### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 接线 + 六平台模块声明 | 全部子模块 | 已完成 | `src/lib.rs` 声明 `common`/`dingtalk`/`feishu`/`github`/`gitlab`/`wecom` 并 re-export；crate 级文档注释说明 harness 边界 |
| 公共抽象归属 | `common/*` | 已完成 | 复用 harness `Channel`/`InboundMessage`/`Peer`/`OutboundAddress`；本 crate 未新建平行类型（符合汇总 tracker §2.1「Channel 抽象」裁定） |
| 各平台 provider 配置解析 | `*ChannelProperties.java` ×5 | 已完成 | 5 个 `properties.rs` 全部落地 |
| 依赖声明 | — | 已完成 | 本 crate `Cargo.toml` 均标具体版本号；`axum`/`http`/`bytes` 改引 workspace（已由汇总方写入根 `Cargo.toml`） |
| 离线测试覆盖 | Java 侧 0 行测试 | 已完成 | Rust 单测 **241 个**（本轮由 118 → 241），覆盖 inbound 映射、outbound 请求体、幂等、防回环、签名校验、XML/实体解析、token 缓存、`*Channel` 回调全流程（含去重/解密/防自回环/分发）、controller 状态码映射与路由查找 |

### 3.2 功能缺口（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| DingTalk Stream 常驻 WebSocket 客户端 | `dingtalk/DingTalkStreamClient.java` (281) | 已完成 | `dingtalk/stream.rs`：注册 → 连接 → ACK → 分发 → 指数退避重连（1s→60s，连上后复位）全流程；`start`/`stop` 幂等（对应 Java `AtomicBoolean` 守卫）。I/O 边界为 `HttpTransport`（注册调用）与 `WebSocketConnector`/`StreamSocket`（socket）两个 trait，测试用脚本化 mock 离线驱动；生产连接器 `TungsteniteConnector`（tokio-tungstenite）。ACK 在分发前发出（对齐 Java，避免重试缓冲重复投递），可见回复仍走 `DingTalkOutboundClient` |
| `FeishuCrypto` AES-256-CBC 解密 | `feishu/FeishuCrypto.java` (130) | 已完成 | `feishu/crypto.rs`：`SHA-256(encryptKey)` 派生密钥、`|16B IV|AES-256-CBC+PKCS#7|` 布局、`X-Lark-Signature` 校验 |
| `WeComCrypto` AES-256-CBC 解密 | `wecom/WeComCrypto.java` (158) | 已完成 | `wecom/crypto.rs`：签名 + 解密 + 加密（回环测试）全落地 |
| `FeishuChannel` 适配 | `feishu/FeishuChannel.java` (206) | 已完成 | `feishu/channel.rs`：组合 harness `PlatformCtx` + `ChannelCore`；`handle_callback` 承载签名校验/解密/URL 握手/去重/防回环/分发全流程；`feishu/controller.rs` 为 axum 薄壳 |
| `DingTalk` / `WeCom` / `GitHub` / `GitLab` 的 `*Channel` 适配 | 各平台 `*Channel.java` | 已完成 | 每平台一个 `channel.rs`（实现 harness `PlatformChannel`，只提供 `ctx()` + `deliver_reply()`）+ 一个 axum `controller.rs` 薄壳；routing/gateway 全部复用 harness `PlatformCtx`。入站入口：`DingTalkChannel::handle_stream_payload`、`WeComChannel::{verify_url,handle_callback}`、`GitHubChannel::handle_webhook`、`GitLabChannel::handle_webhook` |
| 机器人身份解析（防自回环） | `github/GitHubBotIdentityResolver.java` (91)、`gitlab/GitLabBotIdentityResolver.java` (80) | 已完成 | `github/bot_identity.rs`、`gitlab/bot_identity.rs` 已落地，并在 `*Channel::handle_webhook` 中与 `common/bot_loop_guard.rs` 协同 |

### 3.3 增强项（P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| Channel 注册表 | `feishu/FeishuChannelRegistry.java` (41)、`github/GitHubChannelRegistry.java` (39)、`gitlab/GitLabChannelRegistry.java` (39)、`wecom/WeComChannelRegistry.java` (45) | 已完成（收敛为泛型） | **设计决定**：Java 的 5 个 `*ChannelRegistry` 只在持有的 channel 类型上有差异，故 Rust 侧共用一个泛型 `common::ChannelRegistry<C>`，以 `pub type FeishuChannelRegistry = ChannelRegistry<FeishuChannel>` 等别名落地；额外支持镜像注册到 harness `ChannelManager`（主动推送）。另：不做进程级单例，改为实例作用域，便于测试隔离 |
| A2A/长轮询等拉取式入站 | — | 待确认 | Java 侧无对应类，暂不处理 |

### 3.4 设计决定 / 待裁定
| 项 | Java 参考 | 状态 | 理由 |
|---|---|---|---|
| HTTP Webhook Controller | `feishu/FeishuCallbackController.java` (164)、`github/GitHubWebhookController.java` (139)、`gitlab/GitLabWebhookController.java` (142)、`wecom/WeComCallbackController.java` (200) | **已裁定：用 axum 迁移** | 汇总方裁定与 `agentscope-extensions-protocol`/`aistio` 统一用 `axum`（见汇总 tracker §3.3）。实现方式：每平台一个 `controller.rs` 薄壳（`axum::Router` + `with_state(Arc<*ChannelRegistry>)`），只做参数提取与 `*CallbackOutcome` → HTTP 状态码映射；签名校验/解密/去重/分发全部留在 `*Channel`，保持可离线单测 |
| Spring AutoConfiguration / `@ConfigurationProperties` 绑定 | 各子模块 Spring 配置类 | **设计决定（不迁移）** | Rust 无 Spring 容器；配置改为 `*ChannelProperties` 显式解析函数（已落地） |
| `DingTalkChannelProperties` 等字段命名策略 | 各 `*ChannelProperties.java` | 设计决定 | 保持 Java 的 camelCase 键名，避免下游配置迁移成本 |

## 4. 项目状态与下一步

### 4.1 当前状态
- **已完成**：
  - common 全量：`BotLoopGuard`、`IdempotencyStore`、`ChannelCore`（组合 harness `PlatformCtx`）、`ChannelRegistry<C>` 及公共件 `HttpTransport`/`AccessTokenProvider`/`TokenCache`/`crypto`/XML 提取/地址解析/配置取值。
  - 六平台的 `*ChannelProperties`、`*InboundMapper`、`*OutboundClient`、`*AccessTokenProvider`、签名校验（GitHub HMAC-SHA256 / GitLab token / WeCom SHA-1）；Feishu / WeCom 的 AES-256-CBC 解密。
  - **五个平台的 `*Channel` 适配**（Feishu / DingTalk / WeCom / GitHub / GitLab），均实现 harness `PlatformChannel`。
  - **四个 axum CallbackController 薄壳**（Feishu / WeCom / GitHub / GitLab）。
  - **`DingTalkStreamClient`**（`dingtalk/stream.rs`）：常驻 WebSocket 网关，含注册/ACK/退避重连/幂等启停；I/O 走 trait seam，可离线单测。
- **验证命令与结果**（2026-09-18，汇总会话实测，携带本轮全部新增）：
  - `cargo build -p agentscope-extensions-channel` → Finished，0 error
  - `cargo test -p agentscope-extensions-channel` → **241 passed / 0 failed / 0 ignored**（`stream.rs` 落地后 227 → 241）
  - `cargo build --workspace` → **BUILD_EXIT=0**
  - 全量 `cargo test --workspace -j 2` → **1846 passed / 0 failed**（86+ test target）

#### 4.1.1 本轮新增（会话续作）
| 文件 | 行数 | 内容 |
|---|---|---|
| `common/channel_core.rs` | — | 接线并**收敛**：由自存 `channel_id`/`config`/`router`/`gateway` 改为组合 harness `PlatformCtx`；新增 `admit(dedupe_key, peer_key)` 与 `admitted()` |
| `common/channel_registry.rs` | — | 接线；`Channel`/`ChannelSink` 同名方法歧义修为 `Channel::channel_id(&*c)` |
| `feishu/channel.rs` | 336 | `FeishuChannel` + `handle_callback`（签名→解密→URL 握手→映射→去重→防回环→分发） |
| `feishu/controller.rs` | 123 | axum 薄壳 `POST /api/channels/feishu/:id/callback` |
| `dingtalk/channel.rs` | 245 | `DingTalkChannel` + `handle_stream_payload` |
| `wecom/channel.rs` | 459 | `WeComChannel` + `verify_url` / `handle_callback` |
| `wecom/controller.rs` | 263 | axum 薄壳（`GET` URL 验证 + `POST` 事件） |
| `github/channel.rs` | 330 | `GitHubChannel` + `handle_webhook`（HMAC 校验 + 防自回环） |
| `github/controller.rs` | 123 | axum 薄壳 `POST /api/channels/github/:id/webhook` |
| `gitlab/channel.rs` | 337 | `GitLabChannel` + `handle_webhook`（token 校验 + 防自回环） |
| `gitlab/controller.rs` | 178 | axum 薄壳 `POST /api/channels/gitlab/:id/webhook` |
| `dingtalk/stream.rs` | 739 | `DingTalkStreamClient`（常驻 WebSocket 网关）+ `DingTalkStreamProtocol` + 连接器 seam + 14 个单测 |

#### 4.1.2 跨模块改动（本轮经汇总方授权）
- **harness 新增 `agent/gateway/platform_channel.rs`**（`PlatformChannel` / `PlatformCtx` + `Channel`/`ChannelSink` 的 blanket impl，6 个单测）。属「公共层写 harness」裁定的执行（见汇总 tracker §3.3）。
- **根 `Cargo.toml`**：`axum`/`http`/`bytes` 及真实后端驱动依赖已入 `[workspace.dependencies]`，与本 crate 引用对齐。

#### 4.1.3 前轮修复的真实缺陷（3 处，保留备查）
1. `common/xml.rs::xml_element_text` **实现 bug**：`quick-xml` 把 `&amp;` 从 `Event::Text` 拆出为 `Event::GeneralRef`，原实现对 `GeneralRef` 落到 `_ => {}` 而**静默丢弃实体**（`"hello & goodbye"` 被解成 `"hello  goodbye"`，同时影响 WeCom inbound）。修法：新增 `Event::GeneralRef` 分支（`#xHH`/`#NN`/预定义实体），未声明实体按 XML 解析失败返回 `None`，对齐 Java `DocumentBuilderFactory` 异常路径。
2. `feishu/outbound.rs` 测试 `send_executes_one_request_per_message` **测试缺陷**：未播种 `code:0` 响应，而 `parse_send_response` 正确要求 `code` 字段（对齐 Java `checkSendResponse`）。修法：为两次请求 seeding 合法响应。
3. `common/idempotency_store.rs` 测试 `capacity_is_bounded_by_evicting_oldest` **期望与 Java 语义冲突**：`order` 是**惰性重建**的（仅在 `sweep` 内按 `entry.seq` 重建），而 `first_seen_at("a", 4)` 本身会先跑 `sweep`（对齐 Java `sweep(now)` 在 `putIfAbsent` 之前），此时最老者已变成 `"b"` 而被淘汰。修法：断言改为 `first_seen_at("b", 4) == true` 并补 `size() == 3`。
- **已知问题**：无已知编译失败。全量 `cargo test --workspace` 曾在默认并行度下偶发 3 个 `agentscope-extensions-training::runner` 失败（该 crate 用进程级 `OnceLock` 静态注册表 + `clear_all()`，属**既有的测试间竞争**，与本轮改动无关；单独跑与 `-j 2` 均 3/3 全绿）。另：全量测试需保证页面文件充足，否则会因 `os error 1455/112`（mmap 失败）产生「could not compile」假阳性。

### 4.1.4 GAP-19：lease 丢失可中止在跑的一轮（2026-10-21，汇总 tracker 关闭 GAP-19）
- **裁定**：用户「(c) 为主 + (a) 兜底」——先协作式中止、宽限期内优雅收尾，超时再硬中止。
- **Java 依据**：`WeixinChannel.java:200` 的 `takeUntilOther(session.cancelled.asMono())`（取消订阅 = 中断在飞的那一轮）、`:435-438` `loseLease` 广播 `cancelled`、`:65` 的 `weixin-lease-renewal` 续租线程（续租失败 → 丢租约 → 广播）。Rust 无调度器，续租改为 spawn 任务。
- **本 crate 改动**：
  - `weixin/channel.rs`：`LeaseSession` 持有 `agentscope_core::agent::CancelSignal`（`invalidate()` 即广播 = Java 的 `cancelled` sink）；新增 `spawn_lease_renewal`（每 `leaseMs/3` 续租，失败即 fence）；`process_claim` 改为 `select!` —— 租约丢失时先让 agent 协作式收尾（宽限 2s，`TURN_CANCEL_GRACE`），超时则丢弃 turn future 硬中止；**发送前的 `require_lease` 闸门保留**（“晚到的回复绝不外发”契约不变）。
  - `common/channel_core.rs`：`run_turn` 增 `Option<CancelSignal>` 参数并下传。
- **跨模块改动（经授权）**：harness `GatewayTurnContext` 增 `cancel` 字段，`run_with_outbound` / `PlatformCtx::run_turn` 增 `Option<CancelSignal>` 参数，`CallableAgentBridge` 公开并转发信号；core 侧新增 `CancelSignal` / `Error::Cancelled` / `RunGuard`（drop 安全）。
- **测试**：`weixin::channel::tests::a_lost_lease_aborts_the_turn_in_flight`（租约在 turn 运行中丢失 → agent 观察到取消 → 不发任何请求）；已反向验证（把 `run_turn` 改回不传信号 → 该用例 FAILED）。
- **基线**：`cargo test -p agentscope-extensions-channel` → 327 passed / 0 failed；`cargo check --workspace --all-targets` 通过。

### 4.2 下一步与阻塞
1. **无剩余 P0/P1 项**：`DingTalkStreamClient` 已落地（`dingtalk/stream.rs`，227 → 241 tests）。§3.3 唯一「待确认」项为 A2A/长轮询拉取式入站，Java 侧无对应类，属不迁移项。
2. **已裁定事项（本轮均已执行）**：
   - harness `agent::Channel` 契约 → **已裁定：契约足够，公共层写 harness**（`PlatformChannel`/`PlatformCtx` 已落地）。
   - HTTP 框架选型 → **已裁定：axum**（4 个 controller 已落地）。
3. **无阻塞项**：当前 build/test 全绿。

### 4.3 与汇总 tracker 的对应
- 本文件是 CHANNEL 模块的**单一事实来源**；`docs/extensions/EXTENSIONS_GAP_TRACKER.md` §1 第 5 行、§3.1 由汇总方登记。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-channel` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-channel` / `cargo test -p agentscope-extensions-channel` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在平台目录内改动**；新增依赖优先用根 `Cargo.toml` 的 `[workspace.dependencies]` 已有项（如 `axum`/`http`/`bytes`/`reqwest`）。
- **harness 公共层已裁定授权**：`crates/agentscope-harness/src/agent/gateway/platform_channel.rs` 属公共层归属落地点，可在**汇总方授权下**修改；除此之外不得改 `agentscope-core` / `agentscope-harness`，不得改其它模块的 crate。
- **子智能体并发写作约束**：并发改造本 crate 时，每个子智能体只改自己平台的目录；跨目录错误忽略并重跑自己的测试子集。

