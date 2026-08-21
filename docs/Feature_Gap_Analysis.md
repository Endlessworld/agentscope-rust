 

# Java `agentscope-harness` vs Rust `agentscope-harness` — Feature Gap Analysis

## 🔴 Completely Missing Modules (No Rust Equivalent)

### 1. **`agent/team/` — Agent Teams Coordination** (整个模块缺失)
Java 有完整的多 Agent 团队协作系统：
| Java File | 用途 |
|---|---|
| `TeamClient` (接口) | 团队任务板 + 邮箱协调的客户端接口 |
| `LocalTeamClient` (31KB) | 基于 BaseStore CAS 的本地实现，支持 createTask/assignTask/claimTask/completeTask/failTask/sendMessage/broadcastMessage/listMembers/spawnMember/shutdownMember 等操作 |
| `TeamContext` | 当前 agent 的团队上下文（角色、可用操作） |
| `TeamCreateSpec` / `TeamMemberSpec` | 创建团队/成员的规格 |
| `TeamTask` | 任务实体（含 blockedBy 依赖、状态机） |
| `TeamMessage` / `TeamMemberInfo` / `TeamInfo` | 消息/成员/团队信息 |
| `TeamWakeups` | 团队消息到达时的唤醒通知 |
| `TeamConflictException` | 团队操作冲突异常 |

Rust 中**完全没有** `team` 模块，`Cargo.toml` 中也没有任何相关依赖。

### 2. **`agent/coordination/` — 分布式周期性协调** (整个模块缺失)
| Java File | 用途 |
|---|---|
| `PeriodicGate` (接口) | 最小间隔节流器，协调跨调用者/跨节点的定期后台工作 |
| `LocalPeriodicGate` | 基于 `ConcurrentHashMap` + CAS 的进程内实现 |
| `StoreBackedPeriodicGate` | 基于 `BaseStore.putIfVersion` 的分布式实现，用于跨节点的 memory maintenance / skill curator 等定期任务 |

此模块被 `MemoryMaintenanceMiddleware`、`SkillCuratorMiddleware` 等关键中间件依赖。Rust 中完全没有。

### 3. **`agent/transcript/` — 会话转录持久化** (整个模块缺失)
| Java File | 用途 |
|---|---|
| `TranscriptStore` (接口) | 追加型 JSONL 段存储，immutable segments，按 `{tenant}/{agentId}/{sessionId}/events/` 布局 |
| `FilesystemTranscriptStore` | 基于本地文件系统的实现 |
| `ObjectStoreTranscriptStore` | 基于远程 KV 存储的实现 |
| `TranscriptRef` | 转录标识（tenant + agentId + sessionId） |

配套的 `TranscriptMiddleware` 也缺失（见下）。

### 4. **`agent/middleware/TranscriptMiddleware`** (缺失)
每次 agent 调用结束后将对话转录持久化到 `TranscriptStore`。独立于 memory flush，即使关闭 memory hooks 也保持完整会话历史。

### 5. **`agent/middleware/TeamsMiddleware`** (31KB, 缺失)
注入 AgentTeams 上下文到 system prompt，暴露 role-clipped 的 `TeamTool`，处理团队消息到达时的唤醒和注入。

### 6. **`agent/middleware/SandboxLifecycleMiddleware`** (缺失)
管理沙箱会话生命周期：pre-call 时 acquire/start sandbox → 注入 `SandboxBackedFilesystem` → post-call 时 persist/release sandbox。Rust 有 `SandboxManager` trait 但没有将其编排到中间件链中。

---

## 🔴 缺失的 Tools（12 个核心工具全部缺失）

| Java Tool | 功能 | 大小 |
|---|---|---|
| **`AgentSpawnTool`** | 核心子代理生成工具：创建子代理、流式事件、权限控制、远程子代理支持、plan mode 传播、cancel/end 事件处理 | **84KB** |
| **`AgentGenerateTool`** | 从自然语言描述生成子代理 spec → 写入 `subagents/<name>.md` | 7.5KB |
| **`TeamTool`** | 统一团队协调工具：listTasks/createTask/assignTask/claimTask/completeTask/sendMessage/broadcastMessage/spawnMember/shutdownMember/submitPlan/approvePlan 等 | 26KB |
| **`MemorySearchTool`** | 搜索 MEMORY.md + memory/*.md 长期记忆文件 | 3.5KB |
| **`MemoryGetTool`** | 获取记忆文件内容 | 3.3KB |
| **`MemorySaveTool`** | 保存/更新记忆文件 | 3.9KB |
| **`SessionSearchTool`** | 搜索历史会话转录和 session history | 14.6KB |
| **`PlanModeTools`** | plan_enter / plan_write / plan_exit 三个 plan mode 驱动工具 | 10.3KB |
| **`WaitAsyncResultsTool`** | 阻塞等待异步结果（task barrier 模式 + legacy inbox 模式），含超时保护 | 21.8KB |
| **`ProposeSkillTool`** | 提案新 skill 草稿 | 9.8KB |
| **`SkillManageTool`** | Skill 管理（list/promote/archive/create） | 32.8KB |
| **`WebTools`** | web_fetch + web_search（Tavily API）| 7KB |

Rust 只有 `FilesystemTool`、`ShellExecuteTool`、`TaskTool`、`FuzzyTextMatcher`。

---

## 🟡 部分实现 / 桩文件 (Stub / Incomplete)

### 1. **`filesystem/remote.rs`** — 远程文件系统（纯桩）
Java 的 `RemoteFilesystem` (29KB) 有完整的基于 `BaseStore` 的 KV 存储实现（CRUD + glob + grep + CAS 操作）。Rust 的 `RemoteFilesystem` 所有方法都返回 `"not implemented"`。

**配套缺失的 Java 子模块**：
- `remote/store/BaseStore` — 通用 KV 存储抽象（get/put/putIfVersion/list/delete）
- `remote/store/InMemoryStore` — 内存实现
- `remote/store/NamespaceFactory` — 命名空间工厂
- `remote/store/StoreItem` — 带版本号的存储项

### 2. **`filesystem/` — 缺失的高级文件系统**
| Java File | 功能 | 状态 |
|---|---|---|
| `BakedContextFilesystem` | 将 system prompt 相关文件预烘焙到内存中 | ❌ 缺失 |
| `ProjectAwareOverlay` | 基于 project root 的 overlay（项目感知的读写路由） | ❌ 缺失 |
| `RoutedSandboxFilesystem` | 根据路由规则在本地和沙箱间分发操作 | ❌ 缺失 |
| `LocalFilesystemWithShell` | 带 shell 执行能力的本地文件系统（`ProcessBuilder`） | ❌ 缺失 |

### 3. **Filesystem Model Types** — 缺失的结果类型
Java 有丰富的文件操作结果类型，Rust 缺失：
- `EditResult` (edit 结果：old_string 找到次数等)
- `WriteResult` (写入结果)
- `ReadResult` (读取结果)
- `LsResult` (目录列表结果)
- `GlobResult` (glob 匹配结果)
- `GrepResult` (grep 结果，含 matches 列表)
- `FileDownloadResponse` / `FileUploadResponse` (文件传输结果)

### 4. **`filesystem/util/FilesystemUtils`** — 工具类缺失
文件系统辅助工具（路径匹配、Glob 模式转 PathMatcher 等）。

### 5. **`skill/curator/SkillSecurityScanner`** — 安全扫描器
Java 实现了完整的静态分析安全扫描（15KB），检测 6 类恶意模式：EXFILTRATION、INJECTION、DESTRUCTIVE、PERSISTENCE、NETWORK、OBFUSCATION。Rust 的 `SkillCurator` 中没有对应实现。

### 6. **`skill/curator/` — 部分缺失的 Curator 组件**
| Java File | 功能 | Rust 状态 |
|---|---|---|
| `CanaryFilter` | 金丝雀发布过滤器 | ❌ 缺失 |
| `EnvironmentFilter` | 环境变量过滤器 | ❌ 缺失 |
| `NotifyAndWaitGate` | 通知等待审批门控 | ❌ 缺失 |
| `NotificationSink` | 通知发送接口 | ❌ 缺失 |
| `FilesystemSkillUsageBackend` | 基于文件系统的使用记录后端 | ❌ 缺失 |

---

## 🟡 Sandbox 相关缺失

### 1. **Docker 沙箱后端** (整个子模块缺失)
Java 有完整的 Docker CLI 集成：
- `DockerSandbox` (25KB) — 容器生命周期管理（create/start/stop/exec/archive）
- `DockerSandboxClient` — 容器工厂
- `DockerSandboxClientOptions` — Docker 配置（镜像、资源限制、网络、挂载）
- `DockerSandboxState` — 可序列化状态
- `DockerFilesystemSpec` — Docker 文件系统规格

Rust 的 `Sandbox` trait 和 `SandboxClient` trait 已定义，但**没有** Docker 实现。`Cargo.toml` 中也没有 `bollard` 等 Docker 客户端依赖。

### 2. **Sandbox Snapshot** (整个子模块缺失)
- `SandboxSnapshot` (接口) / `LocalSandboxSnapshot` / `RemoteSandboxSnapshot` / `NoopSandboxSnapshot`
- `SandboxSnapshotSpec` 变体（Local/Remote/Noop）
- `RemoteSnapshotClient` — 远程快照存储

Rust 的 `SandboxSnapshotSpec` 在 mod.rs 中定义但只是简单的 struct，没有具体的 snapshot 持久化实现。

### 3. **Sandbox Layout** (整个子模块缺失)
workspace 布局 entry 类型：
- `WorkspaceEntry` / `DirEntry` / `FileEntry` / `GitRepoEntry`
- `BindMountEntry` / `LocalDirEntry` / `LocalFileEntry` / `WorkspaceProjectionEntry`

Rust 的 `WorkspaceSpec` 已有但缺少这些细粒度的布局 entry 类型。

### 4. **Sandbox 辅助类缺失**
- `WorkspaceMountSupport` — 挂载点生成
- `WorkspaceArchiveExtractor` — 归档解压
- `WorkspaceProjectionApplier` — 投影应用
- `SessionSandboxStateStore` — 会话级沙箱状态持久化

---

## 🟡 Gateway 相关缺失

| Java File | 功能 |
|---|---|
| `ChannelFactory` | 从 routing config + provider properties 构建 Channel 的工厂接口 |
| `ChannelRuntimeContextRequest/Resolver` | Channel 运行时上下文解析 |
| `SubagentGatewayBridge` | AgentSpawnTool ↔ Gateway 的桥接，将子代理暴露为可寻址入口 |
| `SessionIdUtils` | SHA-256 确定性 session ID 生成（12 位 hex） |
| `TurnBusyException` | 会话忙碌异常 |
| `LocalSessionTurnGate` | 基于 CAS 的本地会话门控实现 |
| `StoreBackedSubagentRegistry` | 基于 BaseStore 的分布式子代理注册表 |
| `StoreBackedPeriodicGate` | 分布式周期性协调（见 coordination 模块） |

Rust 有 `deterministic_hash`、`SessionTurnGate`、`InMemorySubagentRegistry`，但缺少 `ChannelFactory`、`SubagentGatewayBridge`、`SessionIdUtils`、`TurnBusyException`、`LocalSessionTurnGate`、`StoreBackedSubagentRegistry`。

---

## 🟡 Subagent Protocol / Remote 通信缺失

| Java File | 功能 |
|---|---|
| `RemoteEventCodec` (14KB) | 内部 AgentEvent ↔ RemoteAgentEvent wire DTO 的双向编解码 |
| `RemoteAgentEvent` / `RemoteEventType` | 稳定的 wire 格式事件 DTO |
| `RemoteConfirmDecision` / `RemotePendingConfirm` | 用户确认协议 |
| `RemoteStreamDetail` | 流式详情 |
| `AgentProtocolTaskClient` (16KB) | HTTP 客户端，实现内部 AgentScope task protocol（POST/GET /tasks/...）|
| `AgentProtocolTransport` | ACP 传输适配 |
| `RemoteSubagentTransport` / `RemoteTarget` / `RemoteTaskStatus` | 远程子代理传输层 |
| `RemoteSubmitContext` | 远程提交上下文 |

Rust 的 ACP 模块（`src/acp/`）是独立的 agent-client-protocol 实现，与 Java 的 subagent protocol 不同。Java 的 `subagent/protocol/` 和 `subagent/task/AgentProtocolTaskClient` 是用于内部跨节点通信的。

---

## 🟡 其他细节差异

### Middleware: `AtPathExpansionMiddleware`
Java 中实现了真正的 `@path` 扩展（将 `@file.md` 解析为实际文件路径并注入内容），Rust 中只是一个**空的 no-op struct**。

### `HarnessAgentBuilderSupport`
Java 中有完整的 builder 支持类（41KB），处理动态 hook 注入、中间件自动装配、sandbox/filesystem/spec 的复杂组装逻辑。Rust 的 builder 更简洁但功能覆盖面较小。

### `SkillRuntime` 差异
Java 的 `SkillRuntime` (6.5KB) 含完整的 skill 加载、脚本执行、资源管理。Rust 的 `SkillRuntime` 只有 2.6KB，功能较精简。

### `WorkspaceSpecApplier` 差异
Java 有 `DefaultWorkspaceSpecApplier` + `WorkspaceProjectionApplier` + `WorkspaceMountSupport` + `WorkspaceArchiveExtractor`，Rust 只有 `DefaultWorkspaceSpecApplier`。

### `SubagentDeclaration` 复杂度
Java 25KB vs Rust 4.4KB — Java 包含完整的声明阶段解析、workspace mode 推导、remote ask policy 等。

### `WorkspaceTaskRepository` 复杂度
Java 45KB vs Rust 13KB — Java 包含完整的远程流式任务投递、delivery 回调、adopted task run spec 等。

---

## 📊 量化对比

| 指标 | Java | Rust | 差距 |
|---|---|---|---|
| 主源码文件数 | ~155 个 .java | ~86 个 .rs | -45% |
| 测试文件数 | ~75 个 | ~少量 | 大幅缺失 |
| Tools 数量 | ~15 个 | 4 个 | -11 个 |
| Middleware 数量 | ~16 个 | ~12 个 | -4 个 |
| 完整模块（team/coordination/transcript） | 3 个 | 0 个 | -3 个 |

---

## 🎯 优先级建议

**P0 — 核心功能缺口：**
1. AgentSpawnTool（84KB，子代理生成是 harness 核心）
2. Docker Sandbox 后端
3. MemorySaveTool / MemoryGetTool / MemorySearchTool（记忆系统工具层）

**P1 — 重要功能缺口：**
4. Agent Teams 模块（team/ + TeamsMiddleware + TeamTool）
5. PlanModeTools（plan_enter/plan_write/plan_exit）
6. TranscriptMiddleware + TranscriptStore
7. WaitAsyncResultsTool（异步结果等待）
8. SandboxLifecycleMiddleware

**P2 — 增强功能：**
9. PeriodicGate 分布式协调
10. SkillSecurityScanner 安全扫描
11. RemoteFilesystem 完整实现
12. SubagentGatewayBridge
13. WebTools（web_fetch / web_search）
14. SessionSearchTool