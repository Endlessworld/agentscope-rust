# CHANNEL 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **CHANNEL** 模块迁移的**单一事实来源**与**增量跟踪入口**。开工前读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md`；历史过程记录见 `docs/extensions/CHANNEL__MIGRATION_HISTORY.md`（**冻结**）。切勿臆断代码现状——以本文件 + `git status` + `cargo test` 实际验证为准。
> **当前状态**：**已完成（基线 2026-10-23）**。Java 后续新特性按 §6 增量跟踪流程登记到 §3。
> **Java 源码**：`D:\IdeaProjects\agentscope-java\agentscope-extensions\agentscope-extensions-channel\`（本机 `main` 现为 6 个子模块，**已无 weixin**，见 §4.2）
> **Java 子模块**：common, dingtalk, feishu, github, gitlab, wecom, weixin（**7** 个；weixin 基线来自早期快照）
> **Rust 目标**：`crates/agentscope-extensions-channel/`（波次 A）

## 0. 如何阅读本文件
- 章节导航：§1 基线 → §2 逐类映射 → §3 缺口清单（当前为空）→ §4 设计决定/差异 → §5 已知限制 → §6 增量流程 → §7 验证基线 → §8 归档。
- **状态取值**：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把 §3 状态改 `已完成`，记录关闭日期，并在 §7 写最新验证结果（命令 + 结果）。
- 本文件只保留**当前事实与待办**；过程记录（复核日志、逐项修复细节、反向验证）一律写入 `CHANNEL__MIGRATION_HISTORY.md`。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-channel`（**7** 个 Maven 子模块：common, dingtalk, feishu, github, gitlab, wecom, **weixin**） |
| Java 规模（实测 2026-10-22） | 主代码 **8251** 行 / 测试 **3954** 行（**非空行**口径；`Get-ChildItem -Recurse -Filter *.java` 逐文件非空行求和，共 **92** 个文件 = 67 main + 25 test；`@Test` 计 **161** 个 + `@ParameterizedTest` 1 个）。分模块 main/test：common 593/300、dingtalk 1599/801、feishu 1212/205、github 873/**0**、gitlab 766/**0**、wecom 1254/325、weixin 1954/2323（原始行数口径：主 **9012** / 测试 **4347**） |
| Rust crate | `crates/agentscope-extensions-channel`（`src/` **71** 个 `.rs`，**16334** 行（非空行口径；原始行数 **18000**）；内联测试 **385** 个 = 266 `#[test]` + 119 `#[tokio::test]`） |
| 迁移波次 | A |
| 当前状态 | **已完成（2026-10-23 基线）**：全部 P0/P1/P2 缺口已关闭（§3，历史细节见归档 §7/§8），文本 + 媒体链路全绿。common 主干（含 `ChannelMediaMetadata`、`InboundEventDeduplicator`/`AccessTokenStore` seam）、七平台 `*ChannelProperties`/`*InboundMapper`（text + 媒体）/`*OutboundClient`/`*AccessTokenProvider`/签名与解密、六个平台 `*Channel` 适配（含 weixin）+ 五个 axum CallbackController + `DingTalkStreamClient`（常驻 WebSocket 网关）均已落地。验证基线见 §7。 |

### 1.1 与 harness 的边界（已裁定，必须遵守）
harness 提供**路由/会话绑定骨架**（不可改）：`crates/agentscope-harness/src/agent/` 下 `channel.rs`、`chatui.rs`、`inbound.rs`、`router.rs`、`binding.rs`、`peer.rs`、`dm_scope.rs`、`config.rs`。
本 crate 只提供**平台适配**：`access token provider`、`inbound mapper`、`outbound client`、`channel properties`、`signature verifier`。
不重复定义 `Channel` / `InboundMessage` / `Peer` / `PeerKind` / `OutboundAddress` —— 一律从 `agentscope_harness::agent` 导入复用。

### 1.2 公共层归属（已裁定）

harness 新增 **`agent/gateway/platform_channel.rs`**（`PlatformChannel` trait + `PlatformCtx`），承载「每平台 inbound 分发 + outbound 投递」中**完全相同**的部分：持有 `init` 注入的 gateway、`ChannelRouter` 路由、`apply_routing_config` 热切换，并提供 `Channel` / `ChannelSink` 的 **blanket impl** —— 因此每个平台适配器只需实现 `ctx()` + `deliver_reply()` 两个方法，就能直接当作 harness `Channel` 使用，无需额外包装类型。

本 crate 的 `common/channel_core.rs`（`ChannelCore`）**组合** `PlatformCtx`（不再自存 `channel_id`/`config`/`router`/`gateway`），只追加 Java 各 `*Channel` 私有持有的两个守卫（`InboundEventDeduplicator` + `BotLoopGuard`，经 `ChannelSeams` 可注入），并提供 `admit(dedupe_key, peer_key)` 按 Java 顺序一次性执行「去重 → 防回环」。

> 历史坑（勿重蹈）：`common/channel_core.rs` 与 `common/channel_registry.rs` 曾「已落盘但未在 `mod.rs` 声明」，从不参与编译。新增文件后必须确认 `mod.rs` 已声明并跑测试。

> 注意事项：`agentscope_harness::agent::Channel` 是 `gateway/mod.rs` 里的**遗留事件通道 struct**（与路由 trait 同名）；路由 trait 需用全路径 `agentscope_harness::agent::gateway::channel::Channel` 导入。同时 `Channel` 与 `ChannelSink` 都有 `channel_id()`，在两者均在作用域时需限定调用（如 `Channel::channel_id(&c)`）。

## 2. Java 清单与依赖映射

> **行数口径**：本节各表内标注的 Java 逐类行数与 Rust 逐文件行数均为**原始行数**（含空行与注释）；括号内补充**非空行**（有效行）数，与 §1 汇总口径（Java 主 8251 / 测试 3954、Rust 16334）一致。两种口径差值恒为注释与空行。

### 2.1 common（Java **652** 行（非空 593）/ 7 类 → Rust `common/` 15 个文件 2482 行（非空 2215）含 crate 级公共件）
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `common/BotLoopGuard.java` (**160**) | 按 peer 的滑动窗口节流，防机器人自回环（含 `windowMillis+cooldownMillis` 后的空闲 peer 淘汰） | `common/bot_loop_guard.rs` (`BotLoopGuard`, `BotLoopGuardConfig`) | ✅ 等价：节流/冷却/窗口淘汰 + `evict_idle_peers()`/`tracked_peers()` + 搭车 `maybe_sweep` |
| `common/IdempotencyStore.java` (**106**) | 有界 + TTL 的入站事件去重（`implements InboundEventDeduplicator`） | `common/idempotency_store.rs` (`IdempotencyStore`) | ✅ 等价：TTL/容量/惰性 `sweep`，且**过期放行后刷新 `seen_at_ms`（窗口重启语义）** |
| `common/InboundEventDeduplicator.java` (54) | 去重策略接口（共享存储可插拔；`null` key 视为首次） | `common/idempotency_store.rs::InboundEventDeduplicator`（+ `impl for IdempotencyStore`） | ✅ 等价；经 `ChannelSeams::deduplicator()` 注入进 `ChannelCore` |
| `common/AccessTokenStore.java` (85) + `CachedAccessToken.java` (34) + `InMemoryAccessTokenStore.java` (60) | 可插拔 token 缓存槽（`get`/`put`/`putIfNewer`，单凭据一实例） | `common/token.rs`：`AccessTokenStore` / `CachedAccessToken` / `InMemoryAccessTokenStore` | ✅ 等价（`put_if_newer` 在内存实现中原子合并；三个 provider 均有 `with_store(...)` 注入点） |
| `common/ChannelMediaMetadata.java` (153) | 入站媒体（image/audio/video/file）中性 marker 文本 + 稀疏元数据键契约（`channelMediaKind`/`channelMediaId`/`channelMediaUrl`/…） | `common/media.rs`（常量 + `marker_text` / `sanitize_marker_file_name` / `put_if_present`） | ✅ 等价（DingTalk/Feishu/WeCom 三平台 mapper 均已接入） |
| —（Rust 侧新增） | 每平台必用的公共件 | `common/error.rs`(`ChannelError`)、`common/http.rs`(`HttpTransport`/`ReqwestTransport`/`PreparedRequest{Response}`)、`common/outbound_address.rs`(`parse_peer_target`/`PeerTarget`)、`common/props.rs`、`common/text.rs`、`common/token.rs`(`AccessTokenProvider`/`AccessTokenStore`)、`common/crypto.rs`(`aes_256_cbc_decrypt`/`pkcs7_unpad`)、`common/xml.rs`、`common/media.rs`、`common/testing.rs`(`#[cfg(test)]` 测试替身) | ✅ |
| —（Rust 侧新增） | `*Channel` 共用的分发管线 + 类型化注册表 + 注入缝 | `common/channel_core.rs`(`ChannelCore`：组合 harness `PlatformCtx` + 两个守卫 + `admit(...)`/`is_first_seen(...)`/`run_turn(...)`；`ChannelSeams`：`deduplicator`/`token_store` 注入包)、`common/channel_registry.rs`(`ChannelRegistry<C>`：类型化 `channel_id -> Arc<C>` 查找，可选镜像注册到 harness `ChannelManager`) | ✅ 已接线 |

### 2.2 dingtalk（Java **1730** 行（非空 1599）/ 9 类 → Rust `dingtalk/` 9 个文件 3795 行（非空 3454））
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `DingTalkChannelProperties.java` (**136**) | 配置解析（含 `mode`=stream/http、`aesKey`） | `dingtalk/properties.rs` | ✅ 等价（含 `mode`=stream/http、`aesKey` 43 字符校验、可选 `token`、Rust-only `requireEncryptedCallback`；见 history §7） |
| `DingTalkInboundMapper.java` (**232**) | Stream/HTTP 机器人消息 JSON → `InboundMessage`（text + picture/audio/video/file 媒体） | `dingtalk/inbound.rs` | ✅ 等价（text、DIRECT/GROUP peer、`channelMessageId`、媒体 marker + `channelMediaKind`/`Id`/`SecondaryId`/`Format`/`DurationMs`/`Recognition`/`FileName`） |
| `DingTalkOutboundClient.java` (**175**) | 单聊/群聊发送请求体构造 | `dingtalk/outbound.rs` | ✅ 等价（`/v1.0/robot/oToMessages/batchSend` + `/v1.0/robot/groupMessages/send` 路径、PeerKind 分支一致） |
| `DingTalkAccessTokenProvider.java` (**116**) | OpenAPI `accessToken` 缓存获取 | `dingtalk/token.rs` | ✅ 等价（`/v1.0/oauth2/accessToken` + 80% TTL + `with_store(...)` 注入 `AccessTokenStore`） |
| `DingTalkChannel.java` (**346**) | `Channel` 生命周期/路由适配（stream/http 双模式，`start()` 二选一） | `dingtalk/channel.rs`（`DingTalkChannel` + `handle_stream_payload`，实现 harness `PlatformChannel`） | ✅ 等价（入站管线 + `deliver_reply`；`start(self: &Arc<Self>)` / `stop()` / `is_stream_running()` 按模式拉起 Stream 客户端或自检 crypto，见 history §7） |
| `DingTalkStreamClient.java` (**305**) | 常驻 WebSocket 网关（Stream 模式） | `dingtalk/stream.rs`（`DingTalkStreamClient` + `DingTalkStreamProtocol` + `WebSocketConnector`/`StreamSocket` seam + `TungsteniteConnector`） | ✅ 协议/注册/ACK/退避重连/幂等启停等价（14 个单测）；已由 `DingTalkChannel::start()/stop()` 拉起与关停（history §7） |
| `DingTalkCallbackCrypto.java` (**190**) | HTTP 回调 `sign`（HMAC-SHA256，1h 时效 + 5min 未来偏差）校验 + `aesKey` AES-CBC 载荷解密 | `dingtalk/crypto.rs`（`DingTalkCallbackCrypto`） | ✅ 等价（见 history §7.2） |
| `DingTalkCallbackController.java` (**157**) | HTTP 回调入口（`POST /api/channels/dingtalk/{channelId}/callback`，未知 channel/验签失败→401，body 非法→400，其余 200） | `dingtalk/controller.rs`（`DingTalkCallbackController` + `router`） | ✅ 等价（401/400/200 + `check_url` 加密应答） |
| `DingTalkChannelRegistry.java` (**73**) | 注册表 | `dingtalk::DingTalkChannelRegistry` = `common::ChannelRegistry<DingTalkChannel>`（泛型收敛） | ✅ 等价（实例作用域，非进程单例） |

### 2.3 feishu（Java **1312** 行（非空 1212）/ 8 类 → Rust `feishu/` 8 个文件 1856 行（非空 1700））
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `FeishuChannelProperties.java` (**83**) | 配置解析 | `feishu/properties.rs` | ✅ 等价（appId/appSecret/encryptKey/verificationToken/callbackPath/apiBase） |
| `FeishuInboundMapper.java` (**280**) | 事件订阅 v2 信封 → `InboundMessage`（text + image/file/folder/audio/media 媒体） | `feishu/inbound.rs` | ✅ 等价（text、mention 剥离、DIRECT/GROUP、`channelMessageId`/`channelReplyToId`/`channelThreadId`、媒体 marker + `channelMediaKind`/`Id`/`SecondaryId`/`DurationMs`/`FileName`） |
| `FeishuOutboundClient.java` (**180**) | `POST /open-apis/im/v1/messages` 请求构造（含 threaded reply / `uuid` 去重） | `feishu/outbound.rs` | ✅ 等价（`uuid`、`reply_in_thread`、`/messages/{thread_id}/reply`） |
| `FeishuAccessTokenProvider.java` (**119**) | `tenant_access_token` 缓存获取 + 失效重取 | `feishu/token.rs` | ✅ 等价（缓存/失效重取 + `with_store(...)` 注入 `AccessTokenStore`） |
| `FeishuCrypto.java` (**138**) | AES-256-CBC 载荷解密 + `X-Lark-Signature` 校验 | `feishu/crypto.rs` | ✅ 等价（`SHA-256(encryptKey)` 派生、`|16B IV|CBC+PKCS#7|`、`SHA256(ts+nonce+key+body)`） |
| `FeishuCallbackController.java` (**178**) | HTTP 回调入口 | `feishu/controller.rs`（axum 薄壳，承载全部逻辑在 `FeishuChannel::handle_callback`） | ✅ 等价（路由/401/400/challenge 回显/`{}` 200；含普通事件 `header.token` 恒定时间闸门，history §8.5） |
| `FeishuChannel.java` (**284**) | Channel 适配 | `feishu/channel.rs`（实现 harness `PlatformChannel`） | ✅ 等价（签名→解密→URL 握手→映射→去重→防回环→分发） |
| `FeishuChannelRegistry.java` (**50**) | 注册表 | `feishu::FeishuChannelRegistry` = `common::ChannelRegistry<FeishuChannel>`（泛型收敛） | ✅ 等价 |

### 2.4 github（Java **964** 行（非空 873）/ 8 类 → Rust `github/` 8 个文件 1383 行（非空 1243））
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

### 2.5 gitlab（Java **851** 行（非空 766）/ 7 类 → Rust `gitlab/` 8 个文件 1300 行（非空 1160））
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

### 2.6 wecom（Java **1353** 行（非空 1254）/ 8 类 → Rust `wecom/` 9 个文件 1960 行（非空 1790））
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `WeComChannelProperties.java` (**108**) | 配置解析 | `wecom/properties.rs` | ✅ 等价 |
| `WeComInboundMapper.java` (**204**) | 解密后回调 XML → `InboundMessage`（text + image/voice/video 媒体） | `wecom/inbound.rs`（依赖 `common/xml.rs`） | ✅ 等价（text、`MsgId`、DIRECT peer、媒体 marker + `channelMediaKind`/`Id`/`Url`/`Format`/`SecondaryId`） |
| `WeComOutboundClient.java` (**202**) | `/cgi-bin/message/send` + `/cgi-bin/appchat/send` 请求构造 | `wecom/outbound.rs` | ✅ 等价 |
| `WeComAccessTokenProvider.java` (**118**) | `access_token` 缓存获取 | `wecom/token.rs` | ✅ 等价（缓存/失效重取 + `with_store(...)` 注入 `AccessTokenStore`） |
| `WeComCrypto.java` (**166**) | 回调 SHA-1 签名校验 + AES-256-CBC 解密（+ 加密） | `wecom/crypto.rs` + `wecom/signature.rs` | ✅ 等价（`sign`/`verify_signature`/`decrypt`/`encrypt`） |
| `WeComCallbackController.java` (**213**) | HTTP 回调入口（含 URL 验证握手） | `wecom/controller.rs`（axum 薄壳；`GET` 走 URL 验证、`POST` 走事件） | ✅ 等价 |
| `WeComChannel.java` (**288**) | Channel 适配 | `wecom/channel.rs`（`verify_url` / `handle_callback`：验签→解密→去重→映射→防回环→分发） | ✅ 等价 |
| `WeComChannelRegistry.java` (**54**) | 注册表 | `wecom::WeComChannelRegistry`（泛型收敛） | ✅ 等价 |

### 2.7 weixin（Java **2150** 行（非空 1954）/ 20 类 → Rust `weixin/` 13 个文件 **5181** 行（非空 4732））
> 注：weixin 子模块不在 tracker 旧版清单内（旧版只登记 6 个子模块）。Rust 侧 `weixin/` 是全 crate 最大的模块（4732 非空行，占 16334 的 29%），13 个文件 / 86 个单测。**本机 Java checkout 已无 weixin 子模块（见 §4.2），下表的 Java 行数/类数来自早期快照，下次有 weixin 分支时需重新核对。**

| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `WeixinChannelProperties.java` (**83**) | 配置解析（accountId/baseUrl/ilinkUserId/channelVersion/botAgent/5 个超时与租约） | `weixin/properties.rs` | ✅ 等价（10 个字段逐一对应） |
| `WeixinChannel.java` (**644**) | Channel 生命周期/长轮询/租约/收件箱/回复 | `weixin/channel.rs` (**1322**) | ✅ 等价（`begin_consuming`/`end_consuming`/`run_consumer`/`poll_once`/`drain_inbox`/`send_reply`/`spawn_lease_renewal`/`LeaseSession`；含 GAP-19 取消信号） |
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
## 3. 缺口清单（当前：无）

> 历史 P0/P1/P2 的发现、修复与反向验证细节见归档 `CHANNEL__MIGRATION_HISTORY.md`（章节编号沿用原 §6/§7/§8）。

| 编号 | 优先级 | 缺口 | 状态 | 关闭日期 | 关闭记录 |
|---|---|---|---|---|---|
| G1 | P0 | DingTalk HTTP 回调模式（`crypto.rs` + `controller.rs` + `properties.{mode,aesKey,token}`） | 已完成 | 2026-10-22 | history §7 |
| G3 | P1 | `DingTalkStreamClient` 未被 Channel 启动（无 `start()`/`stop()`） | 已完成 | 2026-10-22 | history §7.5 |
| G2 | P1 | 入站媒体契约 `ChannelMediaMetadata`（三平台 mapper 丢媒体） | 已完成 | 2026-10-23 | history §8.1 |
| G4 | P1 | `InboundEventDeduplicator` / `AccessTokenStore` 可插拔 seam | 已完成 | 2026-10-23 | history §8.2 |
| G5 | P1 | Feishu 普通事件 `header.token` 恒定时间闸门 | 已完成 | 2026-10-23 | history §8.5 |
| G6 | P2 | `BotLoopGuard` 空闲 peer 淘汰 | 已完成 | 2026-10-23 | history §8.3 |
| G7 | P2 | 过期文档注释（5 处） | 已完成 | 2026-10-23 | history §8.6 |
| G8 | P2 | `IdempotencyStore` TTL 窗口重启语义 | 已完成 | 2026-10-23 | history §8.4 |
| G9 | P2 | 飞书 `responseTimeout` 不可配（`FeishuAccessTokenProvider` / `FeishuOutboundClient` 构造参数，默认 10 s，构造期校验正数） | 已完成 | 2026-10-26 | 下表 + `72bfef4` |
| — | — | 骨架/接线/依赖/离线测试覆盖（原 §3.1 四项） | 已完成 | 2026-10-22 | history §4.1 |

### 3.1 G9 关闭记录（2026-10-26）

> Java 基线 `16cb024ae`；该 `responseTimeout` 参数正是此提交引入（**新漂移**）。
> 来源：`docs/extensions/JAVA_BASELINE_REVIEW_2026-10-26.md` §4.1。

**缺口**：Rust 把 10 s 写死在 `common/http.rs::DEFAULT_TIMEOUT` + `ReqwestTransport::new()`，
两个飞书客户端都没有超时形参，云端慢响应只能改代码。

**修法**：
- `common/http.rs`：`ensure_positive_timeout()`（零值 → `ChannelError::Config("responseTimeout must be positive")`，对齐 Java `IllegalArgumentException`）；`ReqwestTransport` 新增 `timeout: Option<Duration>` + `with_timeout()`，`execute()` 时按请求 `.timeout(t)` 覆盖；`with_client()` 保留外部 client 自身配置（`timeout = None`）
- `HttpTransport` 新增**带默认实现**的 `with_response_timeout()`：默认 `None`（注入的 fake / 外部持有的 client 不可重配），`ReqwestTransport` 覆写为复用底层 client（保留连接池）只换超时
- `feishu/token.rs` / `feishu/outbound.rs`：各加 `DEFAULT_RESPONSE_TIMEOUT = 10s`、`response_timeout` 字段、链式 `with_response_timeout()` 与读取器；**`new()` / `with_store()` 签名不变**

**为何不用 builder / 平行类型**：超时在 Rust 挂在 transport 而非 client 上；用 trait 默认方法可以让「可重配」（`ReqwestTransport`）与「不可重配」（测试双、外部 client）两种情况共用一条调用路径，且不改动既有 trait 注入设计。

**测试**（全离线，不真发请求）：`reqwest_transport_defaults_to_ten_seconds`、`reqwest_transport_accepts_explicit_timeout`、`reqwest_transport_rejects_zero_timeout`、`with_client_defers_to_the_wrapped_client_timeout`、`with_response_timeout`（trait 层）；`outbound` / `token` 各自：默认 10 s、显式配置对可重配 transport 生效、不可重配时仅记录、零值被拒。新增测试双 `common/testing.rs::TimeoutAwareTransport`。

**验证**：`cargo test -p agentscope-extensions-channel` → **399 passed / 0 failed**。

新条目一律追加在表尾（编号接 G9、G10…），状态先置 `待办`，关闭后补日期与历史引用。

已定性、不属遗留工作的事项：
- **A2A / 长轮询等拉取式入站**：Java 侧无对应类 → 设计决定（不迁移）；weixin 的 iLink 长轮询已迁移在 `weixin/channel.rs`。
- **真实平台联调**：见 §5（需租户环境，非代码缺口）。

## 4. 设计决定与已知差异

### 4.1 设计决定（不迁移）
| 项 | Java 参考 | 理由 |
|---|---|---|
| Spring AutoConfiguration / `@ConfigurationProperties` 绑定 | 各子模块 Spring 配置类 | Rust 无 Spring 容器；改为 `*ChannelProperties` 显式解析函数 |
| 进程级 `*ChannelRegistry` 单例 | `DingTalk/Feishu/GitHub/GitLab/WeComChannelRegistry` | 收敛为泛型 `common::ChannelRegistry<C>`（Rust 共 6 个别名，含 Java 没有的 `WeixinChannelRegistry`），实例作用域便于测试隔离 |
| A2A / 长轮询等拉取式入站 | — | Java 侧无对应类 |
| 字段命名 | 各 `*ChannelProperties` camelCase 键名 | 保留 Java 键名，降低下游配置迁移成本 |

### 4.2 与 Java 的已知差异（有意为之，非缺口）
| 项 | Java | Rust | 理由 |
|---|---|---|---|
| seam 注入形式 | `fromProperties(..., InboundEventDeduplicator[, AccessTokenStore])` 三档重载 | `from_json(...)` + `from_json_with(..., &ChannelSeams)` | 避免每平台 3 个几乎相同的函数；两字段均可选，语义等价 |
| token 缓存类型 | `InMemoryAccessTokenStore` | `InMemoryAccessTokenStore`（取代旧 `TokenCache`） | 接口化后不需要独立 `TokenCache` 类型；本 crate 无下游依赖方 |
| weixin 工厂 | Java **有** weixin 子模块（HEAD `16cb024ae` 主 20 文件 / 1955 非空行，测 9 文件 / 2323 非空行） | `WeixinChannel::create(..., &ChannelSeams)` / `from_json_with` | **2026-10-26 复评更正**：原文称「本机 checkout 已无 weixin 子模块」**不成立**（`git cat-file -e HEAD:.../channel-weixin` 成功）；子模块由 `63a6f6168`（2026-09-23）引入，一直在基线内。§2.7 的早期快照数字与实测一致（20 类 / 2323 测试非空行），可继续引用 |
| `ChannelMediaMetadata` 文件名净化 | 正则 `\p{Cf}` | 显式 Cf 区间枚举 | Rust `std` 无 Unicode general-category API，不为此新增依赖 |
| DingTalk 回调签名 | 仅 `HmacSHA256(appSecret, ts+"\n"+appSecret)` | 同上 + 可选经典 `token+SHA1` 兼容路径 | 默认不配置 `token` 即完全等同 Java |
| DingTalk `check_url` 首次验证 | 返回空 200 | 返回加密 `success` | 依据 DingTalk 事件订阅协议；**尚未在真实租户验证** |

## 5. 已知限制（需真实环境验证，非代码缺口）

- **DingTalk**：真实租户的 HTTP 回调验签/解密、`check_url` 加密应答、Stream 连接与退避重连。
- **Feishu**：真实事件订阅需配置 `verificationToken`（未配置时普通事件一律 401，与 Java 一致）。
- **WeCom / DingTalk / Feishu**：真实 token 刷新与失效重取。
- 全部单测均为离线（`HttpTransport` / `WebSocketConnector` seam + 金标向量），不发真实网络请求。

## 6. 增量跟踪流程（下次 Java 出现新特性时）

1. **读本文件**（§1 基线 / §2 逐类映射 / §4 差异 / §5 限制）+ `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 的 CHANNEL 行；需要历史背景时查 `CHANNEL__MIGRATION_HISTORY.md`。
2. **确认基线**：`git status` + `cargo test -p agentscope-extensions-channel`（期望结果见 §7）。数字与期望不符时先查明原因再动工。
3. **分析 Java 提交**：`git -C <java-repo> log --oneline <range>`；先 `--stat`，再按文件 `show <hash> -- <path>`。**禁用整提交全量 `show`**（上下文爆炸会导致分析中断）。
4. **登记条目**：在 §3 增加一行（沿用 `G<n>` 编号，附 Java 提交 hash / 类名），状态置 `待办`。
5. **实现**：只改 `crates/agentscope-extensions-channel/**` 与本文件；复用 harness 既有抽象（`PlatformChannel`/`PlatformCtx`/`InboundMessage`/`Peer`），**不新建平行类型**。
6. **测试**：新增/调整单测必须离线；行为缺陷先写复现用例（改前 FAILED），关键项做**反向验证**（回退实现 → 确认 FAILED → 恢复）。
7. **收尾**：更新 §1 基线数字、§2 对应行状态、§3 条目状态与关闭日期、§7 验证结果；在汇总 tracker 登记；过程记录写入 `CHANNEL__MIGRATION_HISTORY.md`（不写回本文件）。

### 6.1 硬约束

- 不得改 `agentscope-core` / `agentscope-harness`（`agent/gateway/platform_channel.rs` 属公共层落地点，仅在汇总方授权下可改）与其它模块 crate。
- 根 `Cargo.toml` 仅在「新建 crate 登记 member」时经授权改动，不得动 `[workspace.dependencies]`。
- 不得删既有功能 / 删断言 / `#[ignore]` 凑绿。
- 依赖优先用根 `[workspace.dependencies]` 已有项；新增依赖须在本 crate `Cargo.toml` 标具体版本并说明理由。
- 风格：最小改动、高内聚低耦合、不臆造 API。

### 6.2 状态取值

`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。

## 7. 验证基线（2026-10-23）

| 命令 | 结果 |
|---|---|
| `cargo check -p agentscope-extensions-channel --all-targets` | 0 error；本 crate 0 warning |
| `cargo test -p agentscope-extensions-channel` | **385 passed / 0 failed / 0 ignored**（doc-test 0） |
| `cargo clippy -p agentscope-extensions-channel --all-targets` | 仅 crate 内既有历史提示，无新增 |

- 测试构成：266 `#[test]` + 119 `#[tokio::test]`。
- 分模块：common 62 / dingtalk 78 / feishu 44 / github 32 / gitlab 35 / wecom 48 / weixin 86。
- 本 crate 无下游依赖方（仅根 `Cargo.toml` 的 members 登记），影响面即本 crate。

## 8. 归档

- 历史记录：`docs/extensions/CHANNEL__MIGRATION_HISTORY.md`（**冻结**）——原 §4 项目状态、§6 静态复核快照、§7 P0 修复（G1）、§8 P1/P2 修复（G2–G8）。
- 汇总登记：`docs/extensions/EXTENSIONS_GAP_TRACKER.md` §1 第 5 行（CHANNEL = 已完成）与 §4.8 #14。
