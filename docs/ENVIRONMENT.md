# 环境变量参考

本文件是 [README](../README.md) 中环境变量表格的完整版。

约定：布尔型统一接受 `1/true/on/yes`（开）与 `0/false/off/no`（关），大小写不敏感；
数值型遇到非法值一律回落默认（不会静默关掉功能）。

## 模型与 provider

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `CODING_PROVIDER` | `~/.agentscope/models.json` 的 `default_provider` | ACP：覆盖默认 provider |
| `CODING_MODEL_ID` | 同上的 `default_model` | ACP：覆盖默认模型 id |
| `OPENAI_API_KEY` | 配置文件中的 `api_key` | 覆盖 provider 的 api_key（启动时会打 warn 提示它覆盖了配置） |
| `OPENAI_BASE_URL` | 配置文件中的 `base_url` | 覆盖 provider 的 base URL |
| `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` | - | Anthropic 凭据：api key 优先，其次 auth token |
| `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` | Anthropic 网关 |
| `DASHSCOPE_API_KEY` | - | DashScope 凭据（缺失即报错） |
| `DASHSCOPE_BASE_URL` | `https://dashscope.aliyuncs.com` | DashScope 网关 |
| `GEMINI_API_KEY` | - | Gemini 凭据（缺失即报错） |
| `GEMINI_BASE_URL` | `https://generativelanguage.googleapis.com` | Gemini 网关 |
| `OLLAMA_BASE_URL` | `http://localhost:11434` | Ollama 地址 |
| `DEEPSEEK_API_KEY` | - | harness `web_fetch` / `web_search` 走 DeepSeek 抓取通道时的 Bearer 密钥 |
| `TYPESAFE_API_KEY` / `JEV_API_KEY` | - | judge 扩展 `jev` 的凭据（前者优先） |

## 模型目录（models.dev catalog）

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `AGENTSCOPE_MODEL_CATALOG` | 开 | 设为 `0` 即不加载目录，模型能力与上下文窗口回落内置前缀表 |
| `AGENTSCOPE_MODEL_CATALOG_API` | 开 | 是否使用详细来源 `api.json` |
| `AGENTSCOPE_MODEL_CATALOG_DIR` | `<home>/.agentscope/model-catalog` | 本地缓存目录 |
| `AGENTSCOPE_MODEL_CATALOG_URL` | `https://models.dev/models.json?type=all` | 平铺来源 URL |
| `AGENTSCOPE_MODEL_CATALOG_API_URL` | `https://models.dev/api.json` | 详细来源 URL |
| `AGENTSCOPE_MODEL_CATALOG_TTL_HOURS` | `24` | 缓存有效期（小时） |
| `AGENTSCOPE_MODEL_CATALOG_COLD_BLOCK_MS` | `0` | 冷启动时阻塞等待目录刷新的上限（毫秒） |

## ACP 智能体（`acp_harness_agent` / `acp_agent_cli`）

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `ACP_AGENT_EXE` | 与 CLI 同目录的 `acp_harness_agent` | CLI 拉起的 agent 可执行文件 |
| `ACP_SESSION_ID` | 新建会话 | CLI 打开的会话 id |
| `ACP_SESSION_REPLAY` | 关 | 设置任意非空值即重放该会话历史 |
| `AGENTSCOPE_LOG_DIR` | `<cwd>/.agentscope/log` | 日志目录，写 `acp-agent.log` |
| `RUST_LOG` | `info,acp_harness_agent=debug,agentscope_harness=debug` | tracing 过滤器（如 `RUST_LOG=trace`） |
| `AGENTSCOPE_ACP_WORKER_THREADS` | `min(ncpu, 8)` | tokio worker 线程数（每线程一份栈与分配器缓存） |
| `AGENTSCOPE_ACP_MEM_TRIM_IDLE` | `3` | 空闲多少秒后归还工作集；`0` 关闭 |
| `AGENTSCOPE_ACP_MEM_REPORT` | 关 | 每个 turn 额外记录序列化后的 `AgentState` 体积 |
| `AGENTSCOPE_ACP_MEM_COLLECT` | 开 | 每个 turn 后向分配器归还内存；`=0` 关闭（unix 有效，Windows 为 no-op） |
| `AGENTSCOPE_ACP_RECORD_MODEL_REQUESTS` | 关 | 把每次模型请求体（`request/prepared`）写进会话 journal，排查用；单次可达数 MB |
| `AGENTSCOPE_TURN_DRIVER` | `durable` | ACP turn 驱动：`native` 切到原生驱动；其它值（含未设置/拼错）一律保持 durable（仅编译了 `durable` feature 时有意义） |
| `AGENTSCOPE_DURABLE_STORE` | 编译 `durable-sqlite` 时 `sqlite` | 设为 `file` 时改用 FileStore；未编译 `durable-sqlite` 时只有 FileStore |
| `AGENTSCOPE_ACP_STALL_CANCEL_SECS` | `2700`（45 分钟） | durable 驱动下，`running` turn 无 durable 进展时的硬上限；`0`/`off`/`false`/`no` 关闭。高于所有单工具上限，只用于真卡死的 run |
| `AGENTSCOPE_ACP_COMPACTION_WINDOW` | 跟随所选模型（未知时 `256000`） | 显式覆盖压缩窗口（tokens）。不设时按模型自己的上下文窗口推导触发点（1M 窗口约 `888192`）；设置后用于封顶或提高触发点 |
| `AGENTSCOPE_MODELS_JSON` | `~/.agentscope/models.json` | 覆盖模型配置文件路径（测试与多配置并存） |
| `AGENTSCOPE_SESSION_RETENTION` | ACP 二进制 `12h`；库默认 30 天 | 会话 journal 保留窗口；`off/0/false/no` 关闭清理；裸数字按天，支持 `m`/`h`/`d` 后缀 |
| `AGENTSCOPE_JOURNAL_RETENTION_DAYS` | `min(会话保留天数, 30)` | 空闲会话目录（journal）按天淘汰 |

## 工具输出

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `AGENTSCOPE_OUTPUT_COMPACT` | 开 | 关闭 shell / rg 输出的重复行折叠（`0/false/off/no`） |

## 扩展（沙箱 / 对象存储 / 服务发现 / 控制面）

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `E2B_SANDBOX_URL` | 按 sandbox id 生成通配域名 | E2B envd 数据面网关 |
| `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_SESSION_TOKEN` | - | OSS(S3) 凭据，仅当启用 `env_credentials` 时读取 |
| `AGENTSCOPE_NACOS_SKILL_VERSION` / `AGENTSCOPE_NACOS_SKILL_LABEL` | - | Nacos skill 版本 / 标签（等价属性键 `agentscope.nacos.skill.version` 与 `agentscope.nacos.skill.label`） |
| `NACOS_A2A_AGENT_<TRANSPORT>_<ATTR>` | - | A2A 传输属性；`ATTR` 取 `HOST`/`PORT`/`PATH`/`PROTOCOL`/`QUERY`/`SUPPORTTLS` |
| `CONTROL_PLANE_HTTP` / `BUILDER_INTERNAL_TOKEN` / `CONTROL_PLANE_AGENT_NAME` | 必填 | control-plane store 工厂 |
| `CONTROL_PLANE_NAMESPACE` | `default` | control-plane 命名空间 |

## 测试与示例

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `AGENTSCOPE_TEST_DOCKER_IMAGE` | 内置默认镜像 | Docker 沙箱集成测试使用的镜像（无 daemon 时整组跳过） |
| `AGENTSCOPE_MODEL` | 示例内默认模型 | 仅 `crates/*/examples/` 下的示例程序读取 |

## 只读取、无需设置的系统变量

`HOME` / `USERPROFILE`（定位 `~/.agentscope`）、`LANG`（系统提示中的语言，缺省 `zh`）、
`HOSTNAME` / `COMPUTERNAME`（control-plane 实例 id 的默认值）。
