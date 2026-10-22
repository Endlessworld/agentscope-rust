# CHANNEL 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **CHANNEL** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-channel\`
> **Java 子模块**：common, dingtalk, feishu, github, gitlab, wecom, weixin（7 个，`pom.xml` 实测）
> **Rust 目标**：`crates/agentscope-extensions-channel/`（波次 A）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-channel`（**7** 个 Maven 子模块：common, dingtalk, feishu, github, gitlab, wecom, **weixin**） |
| Java 规模（实测 2026-10-22） | 主代码 **8251** 行 / 测试 **3954** 行（**非空行**口径；`Get-ChildItem -Recurse -Filter *.java` 逐文件非空行求和，共 **92** 个文件 = 67 main + 25 test；`@Test` 计 **161** 个 + `@ParameterizedTest` 1 个）。分模块 main/test：common 593/300、dingtalk 1599/801、feishu 1212/205、github 873/**0**、gitlab 766/**0**、wecom 1254/325、weixin 1954/2323（原始行数口径：主 **9012** / 测试 **4347**） |
| Rust crate | `crates/agentscope-extensions-channel`（`src/` **68** 个 `.rs`，**13596** 行（非空行口径；原始行数 **15032**）；内联测试 **327** 个 = 225 `#[test]` + 102 `#[tokio::test]`） |
| 迁移波次 | A |
| 当前状态 | **进行中（P0/P1 仍有残留，见 §3 与 §6）**：文本链路全绿（历史验证 **327 passed / 0 failed**，见 §4.1）。common 主干、七平台 `*ChannelProperties`/`*InboundMapper`/`*OutboundClient`/`*AccessTokenProvider`/签名与解密、**六个平台 `*Channel` 适配（含 weixin）+ 四个 axum CallbackController + `DingTalkStreamClient`（常驻 WebSocket 网关）** 已落地；但 **DingTalk HTTP 回调模式（Crypto+Controller）、媒体入站契约 `ChannelMediaMetadata`、`InboundEventDeduplicator`/`AccessTokenStore` 可插拔 seam** 尚未迁移，见 §6。 |

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

> **行数口径**：本节各表内标注的 Java 逐类行数与 Rust 逐文件行数均为**原始行数**（含空行与注释）；括号内补充**非空行**（有效行）数，与 §1/§6 汇总口径（Java 主 8251 / 测试 3954、Rust 13596）一致。两种口径差值恒为注释与空行。

### 2.1 common（Java **652** 行（非空 593）/ 7 类 → Rust `common/` 14 个文件 1950 行（非空 1727）含 crate 级公共件）
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `common/BotLoopGuard.java` (**160**) | 按 peer 的滑动窗口节流，防机器人自回环（含 `windowMillis+cooldownMillis` 后的空闲 peer 淘汰） | `common/bot_loop_guard.rs` (`BotLoopGuard`, `BotLoopGuardConfig`) | ⚠️ 部分：节流/冷却/窗口淘汰已等价；**缺 `evictIdlePeers()` 空闲淘汰与 `trackedPeers()` 观测**（P2） |
| `common/IdempotencyStore.java` (**106**) | 有界 + TTL 的入站事件去重（`implements InboundEventDeduplicator`） | `common/idempotency_store.rs` (`IdempotencyStore`) | ⚠️ 部分：TTL/容量/惰性 `sweep` 已等价；**未抽象为 `InboundEventDeduplicator` trait seam**（P1，见 §6-G4）；**TTL 过期后未刷新 `seen_at_ms`，与 Java「窗口重启」语义不一致**（P2，见 §6-G8） |
| `common/InboundEventDeduplicator.java` (54) | 去重策略接口（共享存储可插拔；`null` key 视为首次） | **缺失**（各 `*Channel` 硬绑具体 `IdempotencyStore`） | ❌ 缺失（P1） |
| `common/AccessTokenStore.java` (85) + `CachedAccessToken.java` (34) + `InMemoryAccessTokenStore.java` (60) | 可插拔 token 缓存槽（`get`/`put`/`putIfNewer`，单凭据一实例） | **缺失**；`common/token.rs::TokenCache` 是内嵌 `Mutex<Option<CachedToken>>` 的具体实现，无 trait seam | ❌ 缺失（P1；`TokenCache` 仅等价 `InMemoryAccessTokenStore` 语义） |
| `common/ChannelMediaMetadata.java` (153) | 入站媒体（image/audio/video/file）中性 marker 文本 + 稀疏元数据键契约（`channelMediaKind`/`channelMediaId`/`channelMediaUrl`/…） | **缺失**（全 crate 无 `channelMediaKind` / `marker_text`） | ❌ 缺失（P1，见 §6-G2） |
| —（Rust 侧新增） | 每平台必用的公共件 | `common/error.rs`(`ChannelError`)、`common/http.rs`(`HttpTransport`/`ReqwestTransport`/`PreparedRequest{Response}`)、`common/outbound_address.rs`(`parse_peer_target`/`PeerTarget`)、`common/props.rs`、`common/text.rs`、`common/token.rs`(`AccessTokenProvider`/`TokenCache`)、`common/crypto.rs`(`aes_256_cbc_decrypt`/`pkcs7_unpad`)、`common/xml.rs`、`common/testing.rs`(`#[cfg(test)]` 测试替身) | ✅ |
| —（Rust 侧新增） | `*Channel` 共用的分发管线 + 类型化注册表 | `common/channel_core.rs`(`ChannelCore`：组合 harness `PlatformCtx` + 两个守卫 + `admit(...)`/`is_first_seen(...)`/`run_turn(...)`)、`common/channel_registry.rs`(`ChannelRegistry<C>`：类型化 `channel_id -> Arc<C>` 查找，可选镜像注册到 harness `ChannelManager`) | ✅ 已接线 |

### 2.2 dingtalk（Java **1730** 行（非空 1599）/ 9 类 → Rust `dingtalk/` 7 个文件 1859 行（非空 1679））
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `DingTalkChannelProperties.java` (**136**) | 配置解析（含 `mode`=stream/http、`aesKey`） | `dingtalk/properties.rs` | ⚠️ 部分：`appKey/appSecret/robotCode/apiBase/oapiBase/streamRegisterUrl` 已等价；**缺 `mode` 与 `aesKey` 两个字段及校验**（P1） |
| `DingTalkInboundMapper.java` (**232**) | Stream/HTTP 机器人消息 JSON → `InboundMessage`（text + picture/audio/video/file 媒体） | `dingtalk/inbound.rs` | ⚠️ 部分：text、DIRECT/GROUP peer、`channelMessageId` 等价；**媒体类型全部丢弃**（P1，见 §6-G2） |
| `DingTalkOutboundClient.java` (**175**) | 单聊/群聊发送请求体构造 | `dingtalk/outbound.rs` | ✅ 等价（`/v1.0/robot/oToMessages/batchSend` + `/v1.0/robot/groupMessages/send` 路径、PeerKind 分支一致） |
| `DingTalkAccessTokenProvider.java` (**116**) | OpenAPI `accessToken` 缓存获取 | `dingtalk/token.rs` | ⚠️ 部分：`/v1.0/oauth2/accessToken` + 80% TTL 等价；**token 缓存不可注入**（P1） |
| `DingTalkChannel.java` (**346**) | `Channel` 生命周期/路由适配（stream/http 双模式，`start()` 二选一） | `dingtalk/channel.rs`（`DingTalkChannel` + `handle_stream_payload`，实现 harness `PlatformChannel`） | ⚠️ 部分：入站管线（去重→映射→防回环→分发）与 `deliver_reply` 等价；**无 `start()`/`stop()`，因此 `DingTalkStreamClient` 从未被启动**（P1，见 §6-G3）；模块文档仍写「Stream client out of scope」为过期注释 |
| `DingTalkStreamClient.java` (**305**) | 常驻 WebSocket 网关（Stream 模式） | `dingtalk/stream.rs`（`DingTalkStreamClient` + `DingTalkStreamProtocol` + `WebSocketConnector`/`StreamSocket` seam + `TungsteniteConnector`） | ✅ 协议/注册/ACK/退避重连/幂等启停等价（14 个单测）；仅缺「被 Channel 拉起」的接线（见上） |
| `DingTalkCallbackCrypto.java` (**190**) | HTTP 回调 `sign`（HMAC-SHA256，1h 时效 + 5min 未来偏差）校验 + `aesKey` AES-CBC 载荷解密 | **缺失** | ❌ 缺失（P0，见 §6-G1） |
| `DingTalkCallbackController.java` (**157**) | HTTP 回调入口（`POST /api/channels/dingtalk/{channelId}/callback`，未知 channel/验签失败→401，body 非法→400，其余 200） | **缺失** | ❌ 缺失（P0，见 §6-G1） |
| `DingTalkChannelRegistry.java` (**73**) | 注册表 | `dingtalk::DingTalkChannelRegistry` = `common::ChannelRegistry<DingTalkChannel>`（泛型收敛） | ✅ 等价（实例作用域，非进程单例） |

### 2.3 feishu（Java **1312** 行（非空 1212）/ 8 类 → Rust `feishu/` 8 个文件 1559 行（非空 1418））
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `FeishuChannelProperties.java` (**83**) | 配置解析 | `feishu/properties.rs` | ✅ 等价（appId/appSecret/encryptKey/verificationToken/callbackPath/apiBase） |
| `FeishuInboundMapper.java` (**280**) | 事件订阅 v2 信封 → `InboundMessage`（text + image/file/folder/audio/media 媒体） | `feishu/inbound.rs` | ⚠️ 部分：text、mention 剥离、DIRECT/GROUP、`channelMessageId`/`channelReplyToId`/`channelThreadId` 等价；**媒体类型全部丢弃**（P1，见 §6-G2） |
| `FeishuOutboundClient.java` (**180**) | `POST /open-apis/im/v1/messages` 请求构造（含 threaded reply / `uuid` 去重） | `feishu/outbound.rs` | ✅ 等价（`uuid`、`reply_in_thread`、`/messages/{thread_id}/reply`） |
| `FeishuAccessTokenProvider.java` (**119**) | `tenant_access_token` 缓存获取 + 失效重取 | `feishu/token.rs` | ⚠️ 部分：缓存/失效重取等价；**token 缓存不可注入**（P1） |
| `FeishuCrypto.java` (**138**) | AES-256-CBC 载荷解密 + `X-Lark-Signature` 校验 | `feishu/crypto.rs` | ✅ 等价（`SHA-256(encryptKey)` 派生、`|16B IV|CBC+PKCS#7|`、`SHA256(ts+nonce+key+body)`） |
| `FeishuCallbackController.java` (**178**) | HTTP 回调入口 | `feishu/controller.rs`（axum 薄壳，承载全部逻辑在 `FeishuChannel::handle_callback`） | ⚠️ 部分：路由/401/400/challenge 回显/`{}` 200 等价；**缺 Java 的「普通事件 `header.token` 恒定时间比对」闸门**（P1，见 §6-G5） |
| `FeishuChannel.java` (**284**) | Channel 适配 | `feishu/channel.rs`（实现 harness `PlatformChannel`） | ✅ 等价（签名→解密→URL 握手→映射→去重→防回环→分发） |
| `FeishuChannelRegistry.java` (**50**) | 注册表 | `feishu::FeishuChannelRegistry` = `common::ChannelRegistry<FeishuChannel>`（泛型收敛） | ✅ 等价 |

### 2.4 github（Java **964** 行（非空 873）/ 8 类 → Rust `github/` 8 个文件 1369 行（非空 1230））
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `GitHubChannelProperties.java` (**78**) | 配置解析 | `github/properties.rs` | ✅ 等价 |
| `GitHubInboundMapper.java` (**127**) | webhook delivery → `InboundMessage` | `github/inbound.rs` | ✅ 等价（`issue_comment` / `pull_request_review_comment`、THREAD peer `owner/repo#n`） |
| `GitHubOutboundClient.java` (**134**) | issue/PR 评论 POST 请求构造 | `github/outbound.rs` | ✅ 等价 |
| `GitHubSignatureVerifier.java` (**77**) | `X-Hub-Signature-256` HMAC-SHA256 校验 | `github/signature.rs` | ✅ 等价 |
| `GitHubBotIdentityResolver.java` (**100**) | 机器人身份解析（防自回环判定，`refresh()` 失败保留旧值） | `github/bot_identity.rs` | ✅ 等价（含 `refresh`/`try_refresh`/`is_self`） |
| `GitHubWebhookController.java` (**154**) | HTTP webhook 入口 | `github/controller.rs`（axum 薄壳） | ✅ 等价（404/401/400/200 映射） |
| `GitHubChannel.java` (**246**) | Channel 适配 | `github/channel.rs`（`handle_webhook`：HMAC→解析→映射→自评论丢弃→去重→防回环→分发） | ✅ 等价 |
| `GitHubChannelRegistry.java` (**48**) | 注册表 | `github::GitHubChannelRegistry`（泛型收敛） | ✅ 等价 |
| —（测试） | 该子模块 Java 侧 **0 行测试**（无 `src/test` 目录） | Rust **32** 个单测 | ✅ |

### 2.5 gitlab（Java **851** 行（非空 766）/ 7 类 → Rust `gitlab/` 8 个文件 1287 行（非空 1148））
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `GitLabChannelProperties.java` (**75**) | 配置解析 | `gitlab/properties.rs` | ✅ 等价 |
| `GitLabInboundMapper.java` (**119**) | Note Hook 载荷 → `InboundMessage` | `gitlab/inbound.rs` | ✅ 等价 |
| `GitLabOutboundClient.java` (**127**) | issue/MR note POST 请求构造 | `gitlab/outbound.rs` | ✅ 等价 |
| —（Java 内联在 Controller） | `X-Gitlab-Token` 常量时间校验 | `gitlab/signature.rs` (`verify_webhook_token`) | ✅ 等价 |
| `GitLabBotIdentityResolver.java` (**89**) | 机器人身份解析 | `gitlab/bot_identity.rs` | ✅ 等价 |
| `GitLabWebhookController.java` (**158**) | HTTP webhook 入口 | `gitlab/controller.rs`（axum 薄壳） | ✅ 等价（404/401/400/200） |
| `GitLabChannel.java` (**235**) | Channel 适配 | `gitlab/channel.rs`（`handle_webhook`：token→解析→映射→自评论丢弃→去重→防回环→分发） | ✅ 等价 |
| `GitLabChannelRegistry.java` (**48**) | 注册表 | `gitlab::GitLabChannelRegistry`（泛型收敛） | ✅ 等价 |
| —（测试） | 该子模块 Java 侧 **0 行测试**（无 `src/test` 目录） | Rust **35** 个单测 | ✅ |

### 2.6 wecom（Java **1353** 行（非空 1254）/ 8 类 → Rust `wecom/` 9 个文件 1806 行（非空 1643））
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `WeComChannelProperties.java` (**108**) | 配置解析 | `wecom/properties.rs` | ✅ 等价 |
| `WeComInboundMapper.java` (**204**) | 解密后回调 XML → `InboundMessage`（text + image/voice/video 媒体） | `wecom/inbound.rs`（依赖 `common/xml.rs`） | ⚠️ 部分：text、`MsgId`、DIRECT peer 等价；**媒体类型全部丢弃**（P1，见 §6-G2） |
| `WeComOutboundClient.java` (**202**) | `/cgi-bin/message/send` + `/cgi-bin/appchat/send` 请求构造 | `wecom/outbound.rs` | ✅ 等价 |
| `WeComAccessTokenProvider.java` (**118**) | `access_token` 缓存获取 | `wecom/token.rs` | ⚠️ 部分：缓存/失效重取等价；**token 缓存不可注入**（P1） |
| `WeComCrypto.java` (**166**) | 回调 SHA-1 签名校验 + AES-256-CBC 解密（+ 加密） | `wecom/crypto.rs` + `wecom/signature.rs` | ✅ 等价（`sign`/`verify_signature`/`decrypt`/`encrypt`） |
| `WeComCallbackController.java` (**213**) | HTTP 回调入口（含 URL 验证握手） | `wecom/controller.rs`（axum 薄壳；`GET` 走 URL 验证、`POST` 走事件） | ✅ 等价 |
| `WeComChannel.java` (**288**) | Channel 适配 | `wecom/channel.rs`（`verify_url` / `handle_callback`：验签→解密→去重→映射→防回环→分发） | ✅ 等价 |
| `WeComChannelRegistry.java` (**54**) | 注册表 | `wecom::WeComChannelRegistry`（泛型收敛） | ✅ 等价 |

### 2.7 weixin（Java **2150** 行（非空 1954）/ 20 类 → Rust `weixin/` 13 个文件 **5162** 行（非空 4714））
> 该子模块 **不在** tracker 旧版清单内（旧版只登记 6 个子模块），本轮补齐。Rust 侧 `weixin/` 是全 crate 最大的模块（占非空 13596 行的 35%），已有 13 个文件 / 86 个单测。

| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `WeixinChannelProperties.java` (**83**) | 配置解析（accountId/baseUrl/ilinkUserId/channelVersion/botAgent/5 个超时与租约） | `weixin/properties.rs` | ✅ 等价（10 个字段逐一对应） |
| `WeixinChannel.java` (**644**) | Channel 生命周期/长轮询/租约/收件箱/回复 | `weixin/channel.rs` (**1303**) | ✅ 等价（`begin_consuming`/`end_consuming`/`run_consumer`/`poll_once`/`drain_inbox`/`send_reply`/`spawn_lease_renewal`/`LeaseSession`；含 GAP-19 取消信号） |
| `WeixinInboundMapper.java` (**81**) | iLink 文本消息 → `InboundMessage`（仅 DIRECT，`selfId` 自过滤） | `weixin/inbound.rs` | ✅ 等价（`channelMessageId` + `weixinContextToken`） |
| `WeixinOutboundClient.java` (**297**) | `sendmessage`/`getupdates`/`notifystart`/`notifystop` | `weixin/outbound.rs` (**712**) | ✅ 等价 |
| `WeixinLoginClient.java` (**174**) | 扫码登录 `start`/`poll`/`verify` + `scaned_but_redirect` 换域名 | `weixin/login.rs` (**470**) | ✅ 等价 |
| `WeixinEndpointPolicy.java` (**85**) | 发送凭据前的域名白名单 | `weixin/endpoint.rs` | ✅ 等价 |
| `WeixinProtocolHeaders.java` (56) | iLink 公共头（`iLink-App-Id`=bot、`iLink-App-ClientVersion`=132105） | `weixin/protocol.rs` | ✅ 等价（`APP_ID`/`APP_CLIENT_VERSION`/`application_headers`/`json_post`/`authenticated_json_post`） |
| `WeixinStateStore.java` (77) + `InMemoryWeixinStateStore.java` (**270**) | 账户级状态契约（cursor / peer context token / inbox claim / 租约围栏）+ 内存实现 | `weixin/state_store.rs` (**836**) | ✅ 等价（`WeixinStateStore` trait + `InMemoryWeixinStateStore`） |
| `WeixinCredentials.java` (34) / `WeixinCredentialProvider.java` (35) | 凭据值与提供者 seam | `weixin/credentials.rs` | ✅ 等价（`WeixinCredentials`/`WeixinCredentialProvider`/`FixedCredentialProvider`） |
| `WeixinRuntimeListener.java` (48) | 运行时事件监听 seam | `weixin/listener.rs` | ✅ 等价（`WeixinRuntimeListener`/`NoopWeixinRuntimeListener`/`RecordingWeixinRuntimeListener`） |
| `WeixinOperationException.java` (27) / `WeixinCredentialRejectedException.java` (24) | 异常类型（凭据拒绝需上报） | `weixin/error.rs`（`WeixinError::{CredentialRejected,Operation,Transport}`） | ✅ 等价 |
| `WeixinInboxClaim.java` (32) / `WeixinInboxMessage.java` (32) / `WeixinLease.java` (32) / `WeixinLoginChallenge.java` (35) / `WeixinLoginSession.java` (39) / `WeixinLoginStep.java` (45) | 值对象 | `weixin/state_store.rs` + `weixin/dto.rs` | ✅ 等价 |
| —（Rust 侧新增） | — | `weixin/channel.rs::WeixinChannelRegistry` = `common::ChannelRegistry<WeixinChannel>` | ℹ️ **Java 侧无 `WeixinChannelRegistry`**（Rust 为对称性新增，非缺口） |
| —（测试） | Java 侧 **2527** 行（非空 2323）/ 9 个测试文件（`@Test` 最多：62 个 + `@ParameterizedTest` 1 个） | Rust **86** 个单测 | ✅ |

### 2.8 依赖映射（Java → Rust）
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
| Spring `@RestController` | 回调 HTTP 入口 | `axum` 0.7（workspace 依赖）+ `http`/`bytes` | **已裁定**：与 protocol/controlplane 统一用 axum；控制器为薄壳，只做参数提取与 `*CallbackOutcome` → HTTP 映射 |

## 3. 详细缺口清单

### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 接线 + 六平台模块声明 | 全部子模块 | 已完成 | `src/lib.rs` 声明 `common`/`dingtalk`/`feishu`/`github`/`gitlab`/`wecom` 并 re-export；crate 级文档注释说明 harness 边界 |
| 公共抽象归属 | `common/*` | 已完成 | 复用 harness `Channel`/`InboundMessage`/`Peer`/`OutboundAddress`；本 crate 未新建平行类型（符合汇总 tracker §2.1「Channel 抽象」裁定） |
| 各平台 provider 配置解析 | `*ChannelProperties.java` ×**7** | 已完成（DingTalk 部分） | 7 个 `properties.rs` 全部落地；但 DingTalk 缺 `mode`/`aesKey` 两字段（P0，见 §6-G1） |
| 依赖声明 | — | 已完成 | 本 crate `Cargo.toml` 均标具体版本号；`axum`/`http`/`bytes` 引 workspace |
| 离线测试覆盖 | Java 侧 **3954 行（非空）/ 25 个测试文件 / 162 个测试方法**（并非 0 行；其中 weixin 2323、dingtalk 801、wecom 325、common 300、feishu 205；github/gitlab 为 0） | 已完成 | Rust 单测 **327 个**（225 `#[test]` + 102 `#[tokio::test]`），覆盖 inbound 映射、outbound 请求体、幂等、防回环、签名校验、XML/实体解析、token 缓存、`*Channel` 回调全流程、controller 状态码映射与路由查找；**缺口：媒体入站与 DingTalk HTTP 回调无测试可写（功能本身缺失）** |

### 3.2 功能缺口（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| DingTalk Stream 常驻 WebSocket 客户端 | `dingtalk/DingTalkStreamClient.java` (305) | 已完成 | `dingtalk/stream.rs`：注册 → 连接 → ACK → 分发 → 指数退避重连（1s→60s，连上后复位）全流程；`start`/`stop` 幂等（对应 Java `AtomicBoolean` 守卫）。I/O 边界为 `HttpTransport`（注册调用）与 `WebSocketConnector`/`StreamSocket`（socket）两个 trait，测试用脚本化 mock 离线驱动；生产连接器 `TungsteniteConnector`（tokio-tungstenite）。ACK 在分发前发出（对齐 Java，避免重试缓冲重复投递），可见回复仍走 `DingTalkOutboundClient`。**注意：该客户端尚未被 `DingTalkChannel` 持有/启动（P1，见 §6-G3）** |
| `FeishuCrypto` AES-256-CBC 解密 | `feishu/FeishuCrypto.java` (138) | 已完成 | `feishu/crypto.rs`：`SHA-256(encryptKey)` 派生密钥、`|16B IV|AES-256-CBC+PKCS#7|` 布局、`X-Lark-Signature` 校验 |
| `WeComCrypto` AES-256-CBC 解密 | `wecom/WeComCrypto.java` (166) | 已完成 | `wecom/crypto.rs`：签名 + 解密 + 加密（回环测试）全落地 |
| `FeishuChannel` 适配 | `feishu/FeishuChannel.java` (284) | 已完成 | `feishu/channel.rs`：组合 harness `PlatformCtx` + `ChannelCore`；`handle_callback` 承载签名校验/解密/URL 握手/去重/防回环/分发全流程；`feishu/controller.rs` 为 axum 薄壳 |
| `DingTalk` / `WeCom` / `GitHub` / `GitLab` / `Weixin` 的 `*Channel` 适配 | 各平台 `*Channel.java` | 已完成 | 每平台一个 `channel.rs`（实现 harness `PlatformChannel`，只提供 `ctx()` + `deliver_reply()`）+ 一个 axum `controller.rs` 薄壳（weixin 无 HTTP 入口，走长轮询）；routing/gateway 全部复用 harness `PlatformCtx`。入站入口：`DingTalkChannel::handle_stream_payload`、`WeComChannel::{verify_url,handle_callback}`、`GitHubChannel::handle_webhook`、`GitLabChannel::handle_webhook`、`WeixinChannel::run_consumer`/`poll_once` |
| 机器人身份解析（防自回环） | `github/GitHubBotIdentityResolver.java` (100)、`gitlab/GitLabBotIdentityResolver.java` (89) | 已完成 | `github/bot_identity.rs`、`gitlab/bot_identity.rs` 已落地，并在 `*Channel::handle_webhook` 中与 `common/bot_loop_guard.rs` 协同 |
| DingTalk HTTP 回调模式 | `dingtalk/DingTalkCallbackCrypto.java` (190)、`DingTalkCallbackController.java` (157)、`DingTalkChannelProperties.{mode,aesKey}` | **待办** | **P0 真实缺口（G1）**：Rust `DingTalkChannelProperties` 无 `mode`/`aesKey`，无 `DingTalkCallbackCrypto`（HMAC-SHA256 `sign` 时效校验 + `aesKey` AES-CBC 解密），无 `dingtalk/controller.rs`。Java 该模式已由 commit `ca524056a` 落地并有 228 行 `DingTalkCallbackControllerTest`，属已发布能力，不能视为「不迁移」 |
| 入站媒体契约 | `common/ChannelMediaMetadata.java` (153) + DingTalk/Feishu/WeCom 三个 mapper 的 `mapMedia*` | **待办** | **P1 真实缺口（G2）**：Rust 三平台 mapper 均只映射 text，媒体消息（DingTalk picture/audio/video/file、Feishu image/file/folder/audio/media、WeCom image/voice/video）被整体丢弃；Java 会产出中性 marker 文本 + `channelMediaKind`/`channelMediaId`/`channelMediaUrl`/`channelMediaFileName` 等稀疏元数据键 |
| 可插拔去重 / token 缓存 seam | `common/InboundEventDeduplicator.java` (54)、`common/AccessTokenStore.java` (85)、`CachedAccessToken.java` (34)、`InMemoryAccessTokenStore.java` (60) | **待办** | **P1 真实缺口（G4）**：Rust 只有具体类型 `IdempotencyStore` / `TokenCache`，各 `*Channel::new`/`*AccessTokenProvider::new` 硬绑；Java 通过 `fromProperties(..., InboundEventDeduplicator, AccessTokenStore)` 三档工厂重载注入共享存储（Redis 等），Rust 无对应注入点 |

### 3.3 增强项（P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| Channel 注册表 | `dingtalk/DingTalkChannelRegistry.java` (73)、`feishu/FeishuChannelRegistry.java` (50)、`github/GitHubChannelRegistry.java` (48)、`gitlab/GitLabChannelRegistry.java` (48)、`wecom/WeComChannelRegistry.java` (54) | 已完成（收敛为泛型） | **设计决定**：Java 的 5 个 `*ChannelRegistry` 只在持有的 channel 类型上有差异，故 Rust 侧共用一个泛型 `common::ChannelRegistry<C>`，以 `pub type FeishuChannelRegistry = ChannelRegistry<FeishuChannel>` 等别名落地（Rust 侧共 6 个别名，额外含 Java 没有的 `WeixinChannelRegistry`）；额外支持镜像注册到 harness `ChannelManager`（主动推送）。另：不做进程级单例，改为实例作用域，便于测试隔离 |
| A2A/长轮询等拉取式入站 | — | 待确认 | Java 侧无对应类，暂不处理（注：weixin 的 iLink 长轮询属 Java 已有能力，已迁移至 `weixin/channel.rs`，不属本项） |
| `BotLoopGuard` 空闲 peer 淘汰 | `common/BotLoopGuard.java` (`evictIdlePeers`/`trackedPeers`，idle = `window+cooldown`) | **待办** | P2：Rust 仅有窗口内事件淘汰（`events_outside_window_are_evicted`），无 `evictIdlePeers()` 与 `tracked_peers()`；长时间运行下 `HashMap` 会按 peer 数无界增长（G6） |
| `IdempotencyStore` TTL 窗口重启语义 | `common/IdempotencyStore.java:62-78`（TTL 过期后 CAS 刷新 `seen_at`，紧接着的重投被丢弃） | **待办** | P2（G8）：Rust `common/idempotency_store.rs:87-110` 的 `first_seen_at` 在 TTL 过期时返回 `true` 但**不刷新 `seen_at_ms`** → 同一窗口内连续重投会被反复放行，与 Java 语义不一致；现有单测未覆盖该边界 |
| 过期文档注释清理 | — | **待办** | P2（G7）：5 处注释已被实现证伪（`dingtalk/channel.rs:8,11-16`、`lib.rs:23-25`、`feishu/mod.rs:9-10`、`common/channel_registry.rs:1`），会误导后续会话 |

### 3.4 设计决定 / 待裁定
| 项 | Java 参考 | 状态 | 理由 |
|---|---|---|---|
| HTTP Webhook Controller | `feishu/FeishuCallbackController.java` (178)、`github/GitHubWebhookController.java` (154)、`gitlab/GitLabWebhookController.java` (158)、`wecom/WeComCallbackController.java` (213)、**`dingtalk/DingTalkCallbackController.java` (157)** | **已裁定：用 axum 迁移** | 汇总方裁定与 `agentscope-extensions-protocol`/`controlplane` 统一用 `axum`（见汇总 tracker §3.3）。实现方式：每平台一个 `controller.rs` 薄壳（`axum::Router` + `with_state(Arc<*ChannelRegistry>)`），只做参数提取与 `*CallbackOutcome` → HTTP 状态码映射；签名校验/解密/去重/分发全部留在 `*Channel`，保持可离线单测。**已落地 4 个（Feishu/WeCom/GitHub/GitLab），DingTalk 尚未落地（P0）** |
| Spring AutoConfiguration / `@ConfigurationProperties` 绑定 | 各子模块 Spring 配置类 | **设计决定（不迁移）** | Rust 无 Spring 容器；配置改为 `*ChannelProperties` 显式解析函数（已落地） |
| `DingTalkChannelProperties` 等字段命名策略 | 各 `*ChannelProperties.java` | 设计决定 | 保持 Java 的 camelCase 键名，避免下游配置迁移成本 |

## 4. 项目状态与下一步

### 4.1 当前状态
- **已完成**：
  - common 主干：`BotLoopGuard`、`IdempotencyStore`、`ChannelCore`（组合 harness `PlatformCtx`）、`ChannelRegistry<C>` 及公共件 `HttpTransport`/`AccessTokenProvider`/`TokenCache`/`crypto`/XML 提取/地址解析/配置取值。
  - 七平台的 `*ChannelProperties`、`*InboundMapper`（**仅 text**）、`*OutboundClient`、`*AccessTokenProvider`、签名校验（GitHub HMAC-SHA256 / GitLab token / WeCom SHA-1 / Feishu Lark）；Feishu / WeCom 的 AES-256-CBC 解密。
  - **六个平台的 `*Channel` 适配**（Feishu / DingTalk / WeCom / GitHub / GitLab / **Weixin**），均实现 harness `PlatformChannel`。
  - **四个 axum CallbackController 薄壳**（Feishu / WeCom / GitHub / GitLab）。
  - **`DingTalkStreamClient`**（`dingtalk/stream.rs`）：常驻 WebSocket 网关，含注册/ACK/退避重连/幂等启停；I/O 走 trait seam，可离线单测。
- **验证命令与结果**（**2026-10-22 静态复核：未编译、未运行测试**；下列数字沿用历史记录，其中测试数已按源码实测校正）：
  - `cargo build -p agentscope-extensions-channel` → Finished，0 error（2026-09-18 历史记录）
  - `cargo test -p agentscope-extensions-channel` → **327 passed / 0 failed**（源码内联 `#[test]`+`#[tokio::test]` 实测计数 = 327；历史记录先记 241、GAP-19 后记 327）
  - `cargo build --workspace` → **BUILD_EXIT=0**（历史记录）
  - 全量 `cargo test --workspace -j 2` → **1846 passed / 0 failed**（历史记录，86+ test target）

#### 4.1.1 本轮新增（会话续作）
> 行数为 **2026-10-22 静态复核实测**（`Get-Content | Measure`），旧版记录中 `dingtalk/stream.rs` 记 739、各 channel/controller 行数普遍偏低约 5–10%，已按实测修正。

| 文件 | 行数 | 内容 |
|---|---|---|
| `common/channel_core.rs` | 314 | 接线并**收敛**：由自存 `channel_id`/`config`/`router`/`gateway` 改为组合 harness `PlatformCtx`；`admit(dedupe_key, peer_key)` / `is_first_seen` / `admitted()` |
| `common/channel_registry.rs` | 238 | 接线；`Channel`/`ChannelSink` 同名方法歧义修为 `Channel::channel_id(&*c)` |
| `feishu/channel.rs` | 370 | `FeishuChannel` + `handle_callback`（签名→解密→URL 握手→映射→去重→防回环→分发） |
| `feishu/controller.rs` | 136 | axum 薄壳 `POST /api/channels/feishu/:channel_id/callback` |
| `dingtalk/channel.rs` | 273 | `DingTalkChannel` + `handle_stream_payload` |
| `wecom/channel.rs` | 502 | `WeComChannel` + `verify_url` / `handle_callback` |
| `wecom/controller.rs` | 284 | axum 薄壳（`GET` URL 验证 + `POST` 事件） |
| `github/channel.rs` | 368 | `GitHubChannel` + `handle_webhook`（HMAC 校验 + 防自回环） |
| `github/controller.rs` | 136 | axum 薄壳 `POST /api/channels/github/:channel_id/webhook` |
| `gitlab/channel.rs` | 375 | `GitLabChannel` + `handle_webhook`（token 校验 + 防自回环） |
| `gitlab/controller.rs` | 200 | axum 薄壳 `POST /api/channels/gitlab/:channel_id/webhook` |
| `dingtalk/stream.rs` | 822 | `DingTalkStreamClient`（常驻 WebSocket 网关）+ `DingTalkStreamProtocol` + 连接器 seam + **14** 个单测 |

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
1. **仍有 P0/P1 项**（2026-10-22 静态复核新发现，见 §6）：
   - **P0** DingTalk HTTP 回调模式（`DingTalkCallbackCrypto` + `DingTalkCallbackController` + `properties.mode/aesKey`）。
   - **P1** 入站媒体契约 `ChannelMediaMetadata`（DingTalk/Feishu/WeCom 三平台 mapper 丢弃媒体）。
   - **P1** `DingTalkStreamClient` 未接入 `DingTalkChannel`（无 `start()`/`stop()`）。
   - **P1** `InboundEventDeduplicator` / `AccessTokenStore` 可插拔 seam 缺失。
   - **P1** Feishu 普通事件 `header.token` 恒定时间比对闸门缺失。
   - **P2**（收尾批）`BotLoopGuard` 空闲淘汰缺失（G6）、过期文档注释 5 处（G7）、`IdempotencyStore` TTL 过期后未刷新 `seen_at_ms`，与 Java「窗口重启」语义不一致（G8）。
2. **已裁定事项（均已执行）**：
   - harness `agent::Channel` 契约 → **已裁定：契约足够，公共层写 harness**（`PlatformChannel`/`PlatformCtx` 已落地）。
   - HTTP 框架选型 → **已裁定：axum**（4 个 controller 已落地，DingTalk 待补）。
3. **无阻塞项**：静态复核未发现编译级冲突；上列缺口均为「未实现」而非「实现错误」——唯一的**实现语义差异**是 G8（幂等 TTL 窗口重启），已定位到 `common/idempotency_store.rs:87-110`，非编译级问题。

### 4.3 与汇总 tracker 的对应
- 本文件是 CHANNEL 模块的**单一事实来源**；`docs/extensions/EXTENSIONS_GAP_TRACKER.md` §1 第 5 行、§3.1 由汇总方登记。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-channel` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-channel` / `cargo test -p agentscope-extensions-channel` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在平台目录内改动**；新增依赖优先用根 `Cargo.toml` 的 `[workspace.dependencies]` 已有项（如 `axum`/`http`/`bytes`/`reqwest`）。
- **harness 公共层已裁定授权**：`crates/agentscope-harness/src/agent/gateway/platform_channel.rs` 属公共层归属落地点，可在**汇总方授权下**修改；除此之外不得改 `agentscope-core` / `agentscope-harness`，不得改其它模块的 crate。
- **子智能体并发写作约束**：并发改造本 crate 时，每个子智能体只改自己平台的目录；跨目录错误忽略并重跑自己的测试子集。

## 6. 静态复核记录（2026-10-22）

### 6.1 复核方式
- **纯静态**：只读源码 + `Get-Content | Measure-Object` 计数 + 符号 grep。**未执行 `cargo build` / `cargo test`**，未修改任何 Rust 代码；本文件是唯一被修改的文件。
- 统计口径：
  - **主口径 = 非空行（有效行）**：剔除空行后的代码行数，与汇总 tracker 的 CHANNEL 口径（Java 主 **8251** / 测试 **3954**、Rust **13596**）一致。
  - 副口径 = 原始行数（`Get-Content` 行数，含 license 头、空行与注释），按 `src\main` / `src\test` 分离；两种口径差值恒为注释与空行。
  - Rust 测试数 = `^\s*#\[(tokio::)?test\]` 逐文件计数（不含 `#[cfg(test)]` 模块本身）；已用 `Select-String` 反查「非行首写法」确认无遗漏。
  - Java 测试方法数 = `@Test`（含全限定 `@org.junit.jupiter.api.Test`）+ `@ParameterizedTest`。
- 复核范围：`crates/agentscope-extensions-channel/src/` 全部 68 个 `.rs`；Java 7 个子模块全部 92 个 `.java`。

### 6.2 实测统计（与旧记录对比）
| 指标 | 旧记录 | 实测（2026-10-22） | 差异 |
|---|---|---|---|
| Java 子模块数 | 6 | **7**（漏 `weixin`） | 旧记录漏登最大子模块 |
| Java 文件数 | 39 | **92**（67 main + 25 test） | 旧记录只数了部分 main |
| Java 主代码行数 | 4879 | **8251**（非空）／**9012**（原始） | +3372（旧值约为实际 41%）；旧值把 `weixin` 整块漏掉 |
| Java 测试行数 | 0 | **3954**（非空）／**4347**（原始） | 旧值错误；**测试目录真实存在**（25 个测试文件） |
| Java 测试方法数 | 未记 | **162**（`@Test` 161 + `@ParameterizedTest` 1） | — |
| Rust `.rs` 文件数 | 55 | **68** | +13（weixin 13 个文件） |
| Rust 行数 | 8872 | **13596**（非空）／**15032**（原始） | +4724 |
| Rust 内联测试数 | 241（后改 327） | **327**（225 `#[test]` + 102 `#[tokio::test]`） | 241 已过期；327 准确 |
| Rust `*Channel` 适配数 | 5 | **6**（含 `WeixinChannel`） | 旧记录漏 weixin |
| axum Controller 数 | 4 | **4**（Feishu/WeCom/GitHub/GitLab） | ✅ 属实；但 Java 有 5 个 |
| `DingTalkStreamClient` | 声称落地 | **存在**（`dingtalk/stream.rs`，822 行，14 测试） | ✅ 符号属实；但未被 Channel 启动 |

**Java 测试目录实测明细**（非空行 / 测试方法数）：common 4 文件 300 行 / 25 个；dingtalk 7 文件 801 行 / 46 个；feishu 2 文件 205 行 / 12 个；wecom 3 文件 325 行 / 16 个；weixin 9 文件 2323 行 / 70 个；**github 0、gitlab 0**。合计 **25 个测试文件 / 3954 非空行 / 162 个测试方法**。

**「5 个 `*Channel`」的解释**：旧记录的 5 指 Feishu/DingTalk/WeCom/GitHub/GitLab——它把当时的 6 个 Java 平台里的 **weixin 整块漏掉了**（既不在子模块清单，也不在类清单）。复核后实际是 **6 个 Java 平台 ↔ 6 个 Rust `*Channel`**（`impl PlatformChannel for` 命中 6 处：DingTalk/Feishu/GitHub/GitLab/WeCom/Weixin），一一对应、**无「某平台故意没有独立适配」的设计理由**。反方向的唯一不对称是 Rust 多出一个 `WeixinChannelRegistry`（Java 无此类），属对称性新增而非缺口。

**符号逐个确认（任务第 4 项）**：
| 声明 | 结论 | 证据 |
|---|---|---|
| 5 个 `*Channel` 适配 | ❌ **不实**（实为 6 个） | `impl PlatformChannel for` 命中 `dingtalk/channel.rs:142`、`feishu/channel.rs:212`、`github/channel.rs:187`、`gitlab/channel.rs:186`、`wecom/channel.rs:240`、`weixin/channel.rs:807` |
| 4 个 axum CallbackController | ✅ **属实** | `feishu/controller.rs`、`wecom/controller.rs`、`github/controller.rs`、`gitlab/controller.rs` 各含 `axum::Router::new()` + `.route(...)`；Java 有 **5** 个（缺 DingTalk） |
| `DingTalkStreamClient` 常驻 WebSocket 网关 | ✅ **属实但未接线** | `dingtalk/stream.rs:188` `pub struct DingTalkStreamClient`，配套 `DingTalkStreamProtocol`/`WebSocketConnector`/`StreamSocket`/`TungsteniteConnector`；但全 crate 无 `DingTalkChannel` 持有该 client 的字段（G3） |
| Java 6 平台 ↔ Rust 6 平台 | ✅ **一一对应** | 唯一缺失的「入口」是 **DingTalk HTTP 回调控制器**（不是某平台缺独立适配）；Java 侧 5 个 Registry ↔ Rust 6 个泛型别名（多 `WeixinChannelRegistry`） |

### 6.3 逐子模块对齐结论
| 子模块 | Java main/test 行（非空） | Java 类数 | Rust 文件/行（非空） | Rust 测试 | 对齐结论 |
|---|---|---|---|---|---|
| common | 593 / 300 | 7 | 14 / 1727 | **50** | ⚠️ **部分**：`BotLoopGuard`/`IdempotencyStore` 语义等价；缺 `InboundEventDeduplicator`、`AccessTokenStore`+`CachedAccessToken`+`InMemoryAccessTokenStore`、`ChannelMediaMetadata` 三类 |
| dingtalk | 1599 / 801 | 9 | 7 / 1679 | **42** | ⚠️ **部分**：Stream 链路等价（含 stream 客户端）；**HTTP 回调模式整块缺失**（Crypto+Controller+`mode`/`aesKey`）；mapper 丢媒体 |
| feishu | 1212 / 205 | 8 | 8 / 1418 | **37** | ⚠️ **接近等价**：仅 mapper 丢媒体 + controller 缺 `header.token` 事件鉴权闸门 |
| github | 873 / 0 | 8 | 8 / 1230 | **32** | ✅ **等价**（Java 侧本就无测试，Rust 有 32 个） |
| gitlab | 766 / 0 | 7 | 8 / 1148 | **35** | ✅ **等价**（Java 侧本就无测试，Rust 有 35 个） |
| wecom | 1254 / 325 | 8 | 9 / 1643 | **45** | ⚠️ **部分**：仅 mapper 丢媒体（image/voice/video） |
| weixin | 1954 / 2323 | 20 | 13 / 4714 | **86** | ✅ **等价**（含 GAP-19 取消语义，Rust 侧额外增强） |
| **合计** | **8251 / 3954** | **67** | **68 / 13596** | **327** | 6 平台 3 完全等价 / 4 部分 |

> 原始行数口径（供与历史记录对账）：Java **9012 / 4347**、Rust **15032**；Rust 分模块原始行 common 1950 / dingtalk 1859 / feishu 1559 / github 1369 / gitlab 1287 / wecom 1806 / weixin 5162 + `lib.rs` 40。


### 6.4 新发现缺口
| ID | 优先级 | 缺口 | Java 依据 | Rust 现状 |
|---|---|---|---|---|
| G1 | **P0** | **DingTalk HTTP 回调模式缺失** | `DingTalkCallbackCrypto.java`(190) 的 `verify(timestamp,sign)`（HMAC-SHA256，1h 时效 + 5min 未来偏差）+ `decrypt`（`aesKey` 43 字符 base64，解 32 字节，前 16 字节作 IV）；`DingTalkCallbackController.java`(157) `POST /api/channels/dingtalk/{channelId}/callback`；`DingTalkChannelProperties` 的 `mode`/`aesKey` | `dingtalk/` 下**无 crypto.rs、无 controller.rs**；`properties.rs` 无 `mode`/`aesKey` 字段；`DingTalkChannel` 无 `crypto()` 访问器。Java 侧该能力由 commit `ca524056a` 发布且有 228 行测试，**不能按「不迁移」关闭** |
| G2 | **P1** | **入站媒体契约 `ChannelMediaMetadata` 整体缺失** | `common/ChannelMediaMetadata.java`(153)：4 种 kind + `markerText(kind,fileName)`（文件名截断 80 code point）+ 稀疏键 `channelMediaKind`/`channelMediaProviderType`/`channelMediaId`/`channelMediaUrl`/`channelMediaSecondaryId`/`channelMediaFormat`/`channelMediaFileName`/`channelMediaDurationMs`/`channelMediaRecognition` | 全 crate 无 `channelMediaKind`/`marker_text` 符号。三处 mapper 在非 text 时直接 `return None`：`dingtalk/inbound.rs:42-44`、`feishu/inbound.rs:71-76`、`wecom/inbound.rs:39-41`。影响：DingTalk picture/audio/video/file、Feishu image/file/folder/audio/media、WeCom image/voice/video **全部静默丢弃**（Java 会回中性 marker 让 agent 感知并引导用户） |
| G3 | **P1** | **`DingTalkStreamClient` 未接线** | `DingTalkChannel.start()` 在 stream 模式调 `streamClient.start()`、`stop()` 对称关闭；`DingTalkChannel` 持有 `streamClient` 字段 | `DingTalkChannel` 无 `stream_client` 字段、无 `start()`/`stop()`；`DingTalkStreamClient::for_channel` 已存在但**全 crate 无调用点**。`dingtalk/channel.rs:11-16` 模块文档仍写「Stream client is out of scope」——已过期（stream.rs 已落地），会误导后续会话 |
| G4 | **P1** | **`InboundEventDeduplicator` / `AccessTokenStore` seam 缺失** | 两个接口 + `CachedAccessToken` + `InMemoryAccessTokenStore`；5 个 `*Channel.fromProperties` 各有「默认 / 注入 deduplicator / 再注入 tokenStore」三档重载 | `IdempotencyStore`、`TokenCache` 是具体类型且被硬编码进 `*Channel::new` / `*AccessTokenProvider::new`；无 trait、无注入点。多实例部署无法共享去重与 token 缓存（Java 文档明确要求 Redis 场景） |
| G5 | **P1** | **Feishu 普通事件鉴权闸门缺失** | `FeishuCallbackController.java:136-148`：普通事件要求 `header.token` 与配置的 `verificationToken` **恒定时间相等**，否则 401 | `feishu/channel.rs` 仅在 `url_verification` 分支（:184）比对 body 的 `token`；普通事件路径无 token 校验。当 `verificationToken` 未配置时，Java 也 401，Rust 直接放行 → **行为差异，安全相关** |
| G6 | P2 | `BotLoopGuard` 空闲淘汰缺失 | `evictIdlePeers()`（idle > `window+cooldown`）+ `trackedPeers()`；随 `allow()` 搭车扫描 | 只有窗口内事件淘汰；`HashMap<String, PeerState>` 随 peer 数单调增长，无 `tracked_peers()` |
| G7 | P2 | 过期文档注释（5 处，非 3 处） | — | `dingtalk/channel.rs:8,11-16`（说 Stream client 未移植 / 「out of scope」）、`lib.rs:23-25`（说 Stream 客户端、Feishu/WeCom 控制器、AES-CBC 解密「Not yet ported」）、`feishu/mod.rs:9-10`（说 `FeishuCallbackController`「Not yet ported」，但 `feishu/controller.rs` 已落地）、`common/channel_registry.rs:1`（说控制器「not-yet-ported」）——均已被实现证伪，建议清理以免误导后续会话 |
| G8 | P2 | **`IdempotencyStore` TTL 过期后的「窗口重启」语义差异** | `common/IdempotencyStore.java:62-78`：TTL 过期后 `firstSeen` 返回 `true` **且用 CAS 把 `seen_at` 刷新为 now**（"the accepted observation restarts the retention window"），因此紧接着的同 key 重投**再次被丢弃**（Java `IdempotencyStoreTest.entriesExpireAfterTtl` 断言第 50 行 `assertFalse(store.firstSeen("ch\|1"))` 明确覆盖）；另 `sweep` 在容量未满时**直接 return**，不做 TTL 清理（`:81-86`） | `common/idempotency_store.rs:87-110` `first_seen_at`：TTL 过期时返回 `true`，但**不刷新 `seen_at_ms`**，旧时间戳保留 → 同一 TTL 窗口内的**连续重投会被反复放行**（Java 只放行一次）；且 `Inner::sweep`（:123-126）在 `entries.len() < max_entries` 时提前 return，与 Java 一致 ✓。现有 Rust 测试 `key_is_treated_as_new_again_after_ttl`（:167-174）只断言「过期后为 true」，**未覆盖「过期后紧接着的重投应被丢弃」**，故该差异未被发现。影响面：WeCom / Feishu / GitHub / GitLab 的重试去重在 TTL 边界上会多放行一次 |

### 6.5 勘误列表（原文 → 实际）
| # | 位置 | 原文 | 实际 |
|---|---|---|---|
| 1 | §1 Java 子模块 | 6 个（common…wecom） | **7 个**，漏 `weixin` |
| 2 | §1 Java 规模 | 主 4879 / 测试 0 行，39 文件 | 主 **8251** / 测试 **3954** 行（非空口径；原始行 9012 / 4347），**92** 文件（67+25），**162** 个测试方法（`@Test` 161 + `@ParameterizedTest` 1） |
| 3 | §1/§3.2 Java 测试 | 「Java 侧 0 行测试」 | **25 个测试文件 / 3954 非空行**：weixin 2323、dingtalk 801、wecom 325、common 300、feishu 205；仅 github、gitlab 为 0 |
| 4 | §1 Rust 规模 | 55 个 `.rs` / 8872 行 | **68** 个 / **13596** 行（非空口径；原始行 15032） |
| 5 | §1/§3.1 测试数 | 241（§4.1 又写 327） | **327**（225 `#[test]` + 102 `#[tokio::test]`），241 为过期值 |
| 6 | §2.1 common | Java 172 行 | **652** 行（原始）/ **593** 行（非空）/ 7 类（原表只列 2 类） |
| 7 | §2.1 BotLoopGuard | 85 行 / ✅ 已迁移 | **160** 行 / ⚠️ 部分（缺 `evictIdlePeers`/`trackedPeers`） |
| 8 | §2.1 IdempotencyStore | 87 行 / ✅ 已迁移 | **106** 行 / ⚠️ 部分（缺 `InboundEventDeduplicator` seam；**另有 TTL 窗口重启语义差异，见 G8**） |
| 9 | §2.2 dingtalk | Java 962 行 / 6 类 | **1730** 行（原始）/ **1599**（非空）/ **9** 类（漏 `DingTalkCallbackCrypto`、`DingTalkCallbackController`、`DingTalkChannelRegistry`） |
| 10 | §2.2 各文件行数 | Properties 80 / Inbound 117 / Outbound 163 / Token 92 / Channel 229 / Stream 281 | **136 / 232 / 175 / 116 / 346 / 305** |
| 11 | §2.3 feishu | Java 1054 行 | **1312** 行（原始）/ **1212**（非空）；各文件行数普遍偏小（Properties 76→**83**、Inbound 175→**280**、Outbound 167→**180**、Token 95→**119**、Crypto 130→**138**、Controller 164→**178**、Channel 206→**284**、Registry 41→**50**） |
| 12 | §2.3 Controller 状态 | ✅ 本轮完成 | ⚠️ 部分：缺 Java 的普通事件 `header.token` 恒定时间比对（G5） |
| 13 | §2.4 github | Java 853 行 | **964** 行（原始）/ **873**（非空）；行数：72→**78**、116→**127**、125→**134**、70→**77**、91→**100**、139→**154**、201→**246**、39→**48** |
| 14 | §2.5 gitlab | Java 746 行 | **851** 行（原始）/ **766**（非空）；行数：69→**75**、108→**119**、117→**127**、80→**89**、142→**158**、191→**235**、39→**48** |
| 15 | §2.6 wecom | Java 1092 行 | **1353** 行（原始）/ **1254**（非空）；行数：101→**108**、120→**204**、165→**202**、94→**118**、158→**166**、200→**213**、209→**288**、45→**54** |
| 16 | §2.7（缺） | 无 weixin 章节 | 已新增 §2.7：Java **2150** 行（原始）/ **1954**（非空）/ 20 类 ↔ Rust 13 文件 / **5162** 行（原始）/ **4714**（非空）/ **86** 测试，✅ 等价 |
| 17 | §3.1 provider 配置 | `*ChannelProperties.java` ×5，5 个 properties.rs | ×**7**，**7** 个 `properties.rs` |
| 18 | §4.1 「五个平台的 `*Channel` 适配」 | 5 个 | **6** 个（含 Weixin） |
| 19 | §4.1.1 文件行数 | 表中 12 行数字 | 全部按实测修正（如 `stream.rs` 739→**822**、`wecom/channel.rs` 459→**502**、`gitlab/controller.rs` 178→**200**） |
| 20 | §4.2 「无剩余 P0/P1 项」 | 无剩余 | **不成立**：P0×1（G1）+ P1×4（G2–G5） |
| 21 | §1 当前状态 | 已完成（P0+P1+P2 全部落地） | **进行中**（见 §6.4） |
| 22 | §2.3 表 | `FeishuCrypto`「✅ 本轮完成解密半部」 | ✅ **全等价**（签名 + 解密均落地，非「半部」） |
| 23 | §3.3 注册表 | 4 个 Java Registry | Java 有 **5** 个（含 `DingTalkChannelRegistry`），Rust 6 个别名（多 `WeixinChannelRegistry`） |
| 24 | §1 「五个平台 `*Channel` + 四个 Controller」 | 表述含混，暗示 5 个平台之一无适配 | 实为 **6 平台 6 适配**；缺的 Controller 是 **DingTalk**（不是某平台「无独立适配」的设计决定） |
| 25 | §6.2/§6.3 统计口径 | 上一版复核节用**原始行**（9012 / 4347 / 15032） | 本轮统一改为**非空行**主口径（**8251 / 3954 / 13596**）以对齐汇总 tracker；原始行数在 §1 与 §6.2 中并列保留 |
| 26 | §6.3 Rust 分模块测试数 | common **49**、gitlab **34**、weixin **90** | 实测 **50 / 35 / 86**（合计仍 327；dingtalk 42、feishu 37、github 32、wecom 45 均正确） |
| 27 | §2.4 / §2.5 测试行 | github「Rust 9 个单测」、gitlab「Rust 10 个单测」 | 实测 **github 32**、**gitlab 35**（旧值为写表时的局部计数，非模块合计） |
| 28 | §2.7 weixin 测试行 | 「Java 侧 2527 行 / Rust 90 个单测」 | Java **2323** 非空行（原始 2527）/ **9** 个测试文件 / 70 个测试方法；Rust **86** 个单测 |
| 29 | §6.4 G7 | 过期注释「三处」 | 实为 **5 处**（新增 `feishu/mod.rs:9-10` 声称 `FeishuCallbackController` 未移植，实际已落地） |
| 30 | §6.4（新增 G8） | 未记录 | **`IdempotencyStore` TTL 过期后不刷新 `seen_at_ms`**，与 Java「窗口重启」语义不一致（Java 过期后放行一次并立即丢弃下一次重投；Rust 会连续放行）——上一版复核未发现 |

### 6.6 建议 rollup 状态
- 建议 `EXTENSIONS_GAP_TRACKER.md` 中 CHANNEL 行由 `已完成` 改为 **`进行中`**：文本链路已全绿（327 测试、6 平台 6 适配、4 控制器、Stream 客户端齐备），但存在 **1 个 P0（DingTalk HTTP 回调模式）** 与 **4 个 P1**（媒体契约、Stream 未接线、可插拔 seam、Feishu 事件鉴权），其中 G1/G5 属「Java 已发布能力 + 安全语义」缺口，不宜以「不迁移」关闭。
- 建议 rollup 行内的规模数字同步改为：**Java 主 8251 / 测试 3954（非空行），25 个测试文件，162 个测试方法；Rust 68 文件 / 13596 行 / 327 测试**。原文「4879 / 0」「241 test」「5 个 `*Channel` 适配」三处均需订正：
  - `4879 / 0` → Java **无测试**的前提错误，测试目录真实存在（25 个文件 / 3954 非空行）；
  - `241 test` → 已过期，当前源码实测 **327**（225 `#[test]` + 102 `#[tokio::test]`）；
  - `5 个 *Channel 适配` → 实为 **6 个**（漏 `WeixinChannel`），「4 个 axum CallbackController」与「`DingTalkStreamClient` 常驻 WebSocket 网关」两项属实。
- 剩余 P2 3 项（G6 空闲淘汰、G7 过期注释 5 处、G8 幂等 TTL 窗口重启语义）建议归入下一批「收尾批」，不阻塞波次 A。

## 7. P0 修复记录（2026-10-22）

### 7.1 G1：DingTalk HTTP 回调模式（已关闭）

| 改动文件 | 内容 |
|---|---|
| `crates/agentscope-extensions-channel/src/dingtalk/crypto.rs` | **新增** `DingTalkCallbackCrypto`：`verify`/`verify_at`（HMAC-SHA256 + 1h 时效 + 5min 未来偏移）、`decrypt`（AES-256-CBC / 32 字节 PKCS#7 / 16B 随机前缀 + 4B big-endian `msg_len`，忽略尾部 trailer）、`encrypt`/`encrypt_with_random`（32 字节块 PKCS#7）、经典 `msg_signature`（SHA1 排序拼接）以及严格解填充 |
| `crates/agentscope-extensions-channel/src/dingtalk/controller.rs` | **新增** `DingTalkCallbackController` + `router`（`POST /api/channels/dingtalk/:channel_id/callback`）：未知 channel/签名失败 401、空/超 64K/非法 JSON/解密失败 400、其余 200；`check_url` 首次验证返回加密 `success`；普通消息交给 `handle_stream_payload` |
| `crates/agentscope-extensions-channel/src/dingtalk/properties.rs` | 新增 `mode`（默认 `stream`，仅允许 `stream`/`http`）、`aes_key`（43 字符校验）、`token`（经典方案，可选）、Rust-only `requireEncryptedCallback`（严格 http 校验：缺 `aesKey`/`token` 直接报错）；`from_json`/`from_map` 全部读取 |
| `crates/agentscope-extensions-channel/src/dingtalk/channel.rs` | 新增 `crypto: Option<_>`、`with_security`、`start(self: &Arc<Self>)`/`stop()`/`is_stream_running()`；stream 模式懒建并启停 `DingTalkStreamClient`（`Weak` sink 避免 Arc 环），http 模式不启动 Stream；`from_json` 仅在 http 模式构造 crypto |
| `crates/agentscope-extensions-channel/src/dingtalk/mod.rs` | 导出 `crypto` / `controller` 模块与 `MODE_STREAM`/`MODE_HTTP` |
| `crates/agentscope-extensions-channel/src/dingtalk/stream.rs` | 仅更新 `DingTalkChannelProperties::new` 调用点的参数（无行为变化） |
| `crates/agentscope-extensions-channel/src/lib.rs` | 修正「Stream/回调控制器/AES 未移植」的过期 crate 文档 |
| `crates/agentscope-extensions-channel/Cargo.toml` | **未改**：`aes 0.8` / `cbc 0.1` / `sha1 0.10` / `hmac 0.12` / `sha2 0.10` / `base64 0.22` 已齐备，未新增依赖 |

### 7.2 与 Java 逐项对照

- **签名**：Java `DingTalkCallbackCrypto.verify` 为 `Base64(HmacSHA256(appSecret, ts + "\n" + appSecret))`，时间戳先 trim 一次，`age > 3_600_000` 或 `-age > 300_000` 拒绝；Rust `verify_at` 逐项复刻（含 `Long.parseLong` 失败即 false、常量时间比较）。
- **解密**：Java `AES/CBC/NoPadding` + 自写严格 PKCS#7（pad 上界 32）、`msgLen < 0 || 20+msgLen > len` 校验、`new String(..., UTF_8)`；Rust 使用 `NoPadding` + `pkcs7_unpad_strict`，UTF-8 采用 lossy（等价 Java 替换语义）。
- **Controller**：401（未知 channel / 缺头 / 签名错 / 过期）、400（空体、>64K 字符、非法 JSON、aesKey 与 body 不匹配）、200（重复/非文本/派发成功）全部对齐；未知 channel 与坏签名同为 401，不泄露 channelId 是否存在。
- **properties**：`mode` 默认 `stream`、非法值报错；`aesKey` 长度 43 校验与 Java 一致；`aesKey` 为可选，plain http 回调继续可用（Java 语义），因此**未**把 `aesKey`/`token` 设为 http 模式强制项。
- **lifecycle**：Java `start()` 在 http 模式注册 registry、stream 模式 `streamClient.start()`，`stop()` 对称；Rust 的 registry 是实例级显式注册（不引入进程单例），`start()` 在 http 模式只做 crypto 自检，stream 模式懒建/启动客户端，`stop()` 异步关停。

### 7.3 金标向量来源

- `HMAC_GOLD = dhJB8TJ0cHPEbwpUauugV2Wz8v4qlWoO+C42nEhJW+c=`：独立 Node `crypto` 对 `HmacSHA256("test-app-secret", "1700000000000\n" + "test-app-secret")` 求得（与 Java `DingTalkCallbackTestSupport.sign` 同式）。
- `LEGACY_GOLD = a514a5a6a03a5f8a76b09bcba6693e42668a076d`：独立 Node 对排序后的 `1700000000|abc123|nonce-1|test-token` 拼接求 SHA-1。
- `CIPHERTEXT_GOLD` / `CIPHERTEXT_GOLD_TRAILER`：独立 Node `aes-256-cbc` 按 Java 测试夹具（零 16B 前缀、`msg_len`、32B 块 PKCS#7、可选 `receive-id` trailer）生成；`AES_KEY` 与 Java 测试常量完全相同（`MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY`）。
- Java 原测试不含固定密文/签名期望值（运行时现算），故金标向量由独立实现生成，而非直接抄录。

### 7.4 验证

- `cargo check -p agentscope-extensions-channel --all-targets`：通过（无新增 error/warning）。
- `cargo test -p agentscope-extensions-channel`：**358 passed / 0 failed**（改动前 327；新增 31 条 DingTalk 回调/生命周期用例）。
- 新增覆盖：HMAC 金标、时效边界（1h / 5min 的 `=` 与 `+1ms`）、经典 msg_signature 金标、加解密往返（含 trailer/中文）、Java 金标密文、篡改首块/末块、非法 base64/长度不足/错块长的显式错误、check_url 加密应答、plain/encrypted 双向不匹配 400、重复 msgId、properties 的 mode/aesKey 校验与严格 http 缺 key 报错、stream `start()/stop()` 与 http 不启 Stream。
- 限制：未做真实 DingTalk 网络回调联调；`check_url` 的加密 `success` 应答是对 Java（返回空 200）的补充，依据 DingTalk 事件订阅协议，尚未在真实租户验证。

### 7.5 与任务描述的差异（据实说明）

- 任务描述中的「`token` + `msg_signature = SHA1(sort(...))` + 加密应答」是经典/事件订阅方案；本仓库 Java 基准（commit `ca524056a`）实际使用 **`appSecret` HMAC-SHA256 + `aesKey` 解密、响应空 200**，且 properties 无 `token`。本实现以 Java 为准，同时把 `token`/SHA1 作为可选兼容路径保留（不配置即完全等同 Java）。
- 「http 模式要求 mode/aesKey/token 三者齐备」与 Java 冲突（Java 允许无 `aesKey` 的 plain http 回调，其控制器测试即如此）。为不破坏 Java 行为，三者齐备改为 Rust-only `requireEncryptedCallback=true` 的显式严格模式；默认关闭。
- G3（Stream 未接线）同步关闭：`DingTalkChannel::start()/stop()` 已驱动 Stream 客户端。


