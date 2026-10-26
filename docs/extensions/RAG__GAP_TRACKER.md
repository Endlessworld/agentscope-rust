# RAG 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **RAG** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`D:\IdeaProjects\agentscope-java\agentscope-extensions\agentscope-extensions-rag\`（旧记录中的 `E:\local-github\agentscope-java\...` 在本机已不存在，见 §7.3 勘误 2）
> **Java 子模块**：bailian, dify, haystack, ragflow, simple
> **Rust 目标**：`crates/agentscope-extensions-rag/`（波次 A）
> **统计口径**：所有行数均为**非空行**（trim 后非空），Java 与 Rust 同口径；见 §6 静态复核记录。

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-rag` |
| Java 子模块 | bailian, dify, haystack, ragflow, simple |
| Java 规模（实测行数，非空行） | 主代码 **15946** / 测试 **16792**（bailian 1447/1493、dify 2509/1136、haystack 1565/1163、ragflow 1849/2091、simple 8576/10909） |
| Rust crate | `crates/agentscope-extensions-rag` |
| 迁移波次 | A |
| 当前状态 | **部分完成** — `dify`、`ragflow`、`bailian`、`haystack`、`simple` 子模块的离线链路与外部服务链路均已迁移；2026-10-26 补 `simple` 的 **HTTP embedding 4 件**（openai / dashscope text / dashscope multimodal / ollama）+ `ReaderUtils` + `ImageReader`（新增 60 个离线测试，待主智能体统一 `cargo build`/`cargo test` 验证）；**外部向量库 4 件（ES/Milvus/PgVector/Qdrant）与 `ExternalApiReader` 仍为待办** |

### 1.1 Rust crate 文件清单（49 个源文件；本轮新增 7 个源文件 2437 行非空 + 60 个内联测试）

> 行数口径：非空行（trim 后非空）。上一轮基线为 42 文件 / 10002 行 / 265 测试；本轮新增
> `embedding/{utils,openai,dashscope,ollama,test_support}.rs` 与 `reader/{utils,image}.rs`，
> 并扩充了 `embedding.rs`、`reader.rs`、`hash.rs`、`lib.rs`、`Cargo.toml`。新增测试数：
> openai 12 / dashscope 15 / ollama 8 / embedding::utils 10 / reader::utils 6 / reader::image 8 /
> hash 1 = **60**（待主智能体统一跑 `cargo test` 确认）。
| 文件 | 职责 |
|---|---|
| `src/lib.rs` | crate 根：15 个模块声明 + re-export；说明 Java core `rag` 包已 `@Deprecated(forRemoval)` 且 core 无对应实现 |
| `src/error.rs` | `RagError`（Argument / Embedding / VectorStore / Reader / Unsupported / Http / Api / Auth / Json / Io），对应 Java 各类异常 |
| `src/hash.rs` | 自实现 MD5 + 版本 3（名称）UUID，复刻 Java `UUID.nameUUIDFromBytes` 的 Document id；**新增 `sha256_hex`**（`ReaderUtils` 用，复用已有 `sha2` 依赖） |
| `src/knowledge.rs` | `Knowledge` trait + 共享 `filter_and_rank`（阈值过滤 + 降序 + limit） |
| `src/message.rs` | 检索结果 → core `Msg`：tool 文本格式与 `<retrieved_knowledge>` 通用 hook 注入 |
| `src/model.rs` | `Document` / `DocumentMetadata` / `RetrieveConfig` / `content_block_text` |
| `src/reader.rs` | `Reader` trait、`ReaderInput`、`TextReader`、`SplitStrategy`、字符/段落/token 分块；声明 `reader::{image,utils}` 子模块并 re-export |
| `src/reader/utils.rs` | `ReaderUtils`：`generate_doc_id(_md5/_sha256)`、`bytes_to_hex`、`DocIdAlgorithm`（Java 未知算法 → `InvalidArgument`） |
| `src/reader/image.rs` | `ImageReader`：图片路径/URL → `ImageBlock(URLSource)` 的单个 `Document`；docId = MD5(path)、chunkId `"0"`、OCR 仅保留开关（与 Java 同为空实现） |
| `src/retry.rs` | 指数退避重试（对应 Java `RetryInterceptor`：429 / 5xx / 传输错误重试，4xx 不重试） |
| `src/simple.rs` | `SimpleKnowledge`、`in_memory_knowledge`、`EmptyKnowledge` |
| `src/store.rs` | `VdbStore` trait、`InMemoryStore`、`SearchQuery`、余弦/欧氏距离 |
| `src/embedding.rs` | `EmbeddingModel` trait（含默认 `embed_with_usage`）、`EmbeddingUsage`/`EmbeddingResponse`/`UsageListener`、确定性离线的 `HashEmbeddingModel`；声明并 re-export 4 个 HTTP 提供商的子模块 |
| `src/embedding/utils.rs` | `EmbeddingUtils` → `EmbeddingExecutionConfig`（超时 + 复用 `retry::RetryPolicy`）、`f32_slice_to_f64`、`join_url`、`build_transport` |
| `src/embedding/openai.rs` | `OpenAiTextEmbedding`：`POST {base}/embeddings` + `Bearer`，`input:[text]`/`dimensions`/`encoding_format=float` |
| `src/embedding/dashscope.rs` | `DashScopeTextEmbedding` + `DashScopeMultiModalEmbedding`：`Bearer` 鉴权；text 走 `input.texts`，multimodal 走 `input.contents[{text|image}]`；本地图片内联为 `data:` URL |
| `src/embedding/ollama.rs` | `OllamaTextEmbedding`：`POST {base}/api/embed`，无鉴权；`embeddings[0]`（f32）→ `Vec<f64>`，`prompt_eval_count` → usage |
| `src/embedding/test_support.rs` | `#[cfg(test)]` 脚本化内存 transport（LIFO 回放 + 请求录制），四个提供商测试共用 |
| `src/dify/{mod,config}.rs` | Dify 配置 + `RetrievalMode` / `RerankConfig` / `MetadataFilter(Condition)` |
| `src/dify/{client,request}.rs` | Dify HTTP 客户端 + 可注入 `DifyTransport` + 请求体构造 |
| `src/dify/{model,converter,knowledge}.rs` | Dify 响应 DTO、`DifyDocumentConverter`、`DifyKnowledge` |
| `src/transport.rs` | 共享可注入 HTTP transport seam（ragflow / bailian / haystack 复用） |
| `src/ragflow/{mod,config}.rs` | RAGFlow 配置 + builder（dataset/document ids、检索旋钮、重试） |
| `src/ragflow/{client,request}.rs` | RAGFlow HTTP 客户端 + `POST /api/v1/retrieval` 请求体构造 |
| `src/ragflow/{model,converter,knowledge}.rs` | RAGFlow 响应 DTO（含 `data: false|null|object` 自定义反序列化）、`RAGFlowDocumentConverter`、`RAGFlowKnowledge` |
| `src/bailian/{mod,config}.rs` | 百炼配置 + builder，`RerankConfig` / `RewriteConfig`（含 Java 默认模型名与区间校验） |
| `src/bailian/{model,request}.rs` | 百炼 `RetrieveRequest` / `RetrieveResponseBody` DTO（请求 camelCase、响应 PascalCase）+ 请求体装配 |
| `src/bailian/{signing,client}.rs` | **ACS4-HMAC-SHA256 请求签名**（自建，无阿里云 SDK）+ `POST https://{endpoint}/{workspaceId}/index/retrieve` 客户端 |
| `src/bailian/{query_history,converter,knowledge}.rs` | `QueryHistoryEntry` + `Msg`→history 转换、node→`Document` 转换、`BailianKnowledge` 适配器 |
| `src/haystack/{mod,config}.rs` | `HayStackConfig` + builder、`FilterPolicy`（replace/merge） |
| `src/haystack/{model,request}.rs` | `HayStackResponse` / `HayStackDocument` / `SparseEmbedding` / `ByteStream` DTO + 请求体装配 |
| `src/haystack/{client,converter,knowledge}.rs` | `POST {baseUrl}` 客户端（`code != successCode` → API 错误）、document→`Document` 转换、`HayStackKnowledge` 适配器 |

### 1.2 验证基线

上一轮（2026-09-19）实测基线：
```
cargo build -p agentscope-extensions-rag   -> Finished `dev` profile (0 errors, exit 0)
cargo test  -p agentscope-extensions-rag   -> 265 passed; 0 failed; 0 ignored
```

本轮（2026-10-26）按约定**未在子智能体内执行 cargo**（编译/测试由主智能体统一验证）。目测需重跑：
```
cargo build -p agentscope-extensions-rag
cargo test  -p agentscope-extensions-rag   # 预计 325 passed（新增 60）
```

## 2. Java 清单与依赖映射

> Java 主代码位于 5 个子模块（`agentscope-extensions-rag-{bailian,dify,haystack,ragflow,simple}`），
> 另引用 Java **core** 的 `io.agentscope.core.rag.*` / `io.agentscope.core.embedding.*`（不在本扩展源码树内）。

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| OkHttp（`okhttp3.*`） | HTTP 传输 | `reqwest = "0.12"`（`json, rustls-tls`，本 crate `[dependencies]`）+ 可注入 transport trait | 生产用 `HttpDifyTransport`，测试用内联 mock |
| Jackson（`ObjectMapper`/`@JsonProperty`） | JSON (反)序列化 | `serde` + `serde_json`（workspace） | 字段名 1:1 对齐，忽略未知字段 |
| Project Reactor（`Mono`） | 异步流 | `async-trait` + `tokio`（workspace） | — |
| Slf4j | 日志 | `tracing`（workspace） | — |
| `java.time.Duration` | 超时配置 | `std::time::Duration` | 默认 30s/60s 等常量已对齐 |
| `java.util.UUID.nameUUIDFromBytes` | Document id（MD5 v3） | `hash::{md5, uuid_v3_from_name}`（自实现，未新增 crate） | 复刻 v3 名称 UUID |
| `com.aliyun.bailian20231229` SDK | 百炼检索 | **无**（改用 `reqwest` 直连 + 自建 DTO + 自建 ACS4-HMAC-SHA256 签名） | **已裁定**：不引入阿里云 SDK（Java 中心、依赖树庞大）；端点/路径/API 版本/请求体/响应体/`Authorization` 格式均已从 SDK jar 逆向并用金标向量钉住（见 §4.2） |
| Apache Tika / PDFBox / POI | 文档解析 | 无 | 见 §3.3，待评估 pure-Rust 替代 |
| `hmac` / `sha2` | ACS4-HMAC-SHA256 请求签名（百炼；Java 由 SDK 传递引入） | `hmac = "0.12"` / `sha2 = "0.10"`（本 crate `[dependencies]`） | 本轮新增；`sha2` 与 workspace 同版本，`hmac`/`sha2` 已在 `Cargo.lock` 中 |
| `uuid` | 百炼 `x-acs-signature-nonce` 随机源（对应 Java `Common.getNonce`） | `uuid = { workspace = true }` | 复用 workspace 依赖 |
| `base64` | DashScope multimodal 将本地图片内联为 `data:` URL（Java 由 `DashScopeMediaConverter` 做同样的事） | `base64 = { workspace = true }` | 本轮新增；沿用 workspace 版本 `0.22` |
| JUnit 5 / Mockito | 测试 | `#[test]` / `#[tokio::test]` + 内联 mock transport | 全离线，`tokio` dev-dependency 提供 `rt-multi-thread,macros` |

### 2.2 dify 子模块（`io.agentscope.core.rag.integration.dify.*`，11 类）— **已迁移**
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `DifyRAGConfig` (+Builder) | API key / dataset / 超时 / 检索参数 | `dify::config::DifyRAGConfig` + `DifyRAGConfigBuilder` | 已完成 |
| `RetrievalMode` | keyword/semantic/hybrid/full_text | `dify::config::RetrievalMode`（snake_case wire 值 + `from_value`/`value`） | 已完成 |
| `RerankConfig` | rerank provider/model | `dify::config::RerankConfig` | 已完成 |
| `MetadataFilter` / `MetadataFilterCondition` | metadata 过滤条件 | `dify::config::{MetadataFilter, MetadataFilterCondition}` | 已完成 |
| `DifyRAGClient` | `POST {base}/datasets/{id}/retrieve`，Bearer 鉴权，`RetryInterceptor` | `dify::client::{DifyRAGClient, DifyTransport, HttpDifyTransport, HttpRequest, HttpResponse, map_error_response}` + `retry::retry_async` | 已完成 |
| `model.DifyResponse`（+ `Query`/`Record`/`Segment`/`DocumentInfo`） | 检索响应 DTO | `dify::model::{DifyResponse, Query, Record, Segment, DocumentInfo}` | 已完成 |
| `DifyDocumentConverter` | record/segment → `Document`（payload 逐字段装配） | `dify::converter::DifyDocumentConverter` | 已完成 |
| `DifyKnowledge` | `Knowledge` 适配器（只读；`add_documents` 抛 Unsupported） | `dify::knowledge::DifyKnowledge` | 已完成 |
| `exception.DifyApiException` / `DifyAuthException` | API / 鉴权异常 | `RagError::Api` / `RagError::Auth`（`src/error.rs`） | 已完成 |
| 请求体装配（`retrieve` 内联） | `query` + `retrieval_model` | `dify::request::build_retrieve_request_body` | 已完成 |

### 2.3 simple 子模块（`io.agentscope.core.rag.*` / `io.agentscope.core.embedding.*`，simple 子模块 **33 类**（rag 24 + embedding 9）+ Java core `io.agentscope.core.rag.*` **7 类** = 40 类；实际 embedding 为 **7 类**，见 §7.3 勘误 3）— **除外部向量库 4 件与 `ExternalApiReader` 外均已完成**
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `core.rag.Knowledge`（core 接口） | 知识库抽象 | `knowledge::Knowledge` (trait) | 已完成 |
| `core.rag.model.Document` / `DocumentMetadata` / `RetrieveConfig` | 核心 RAG 数据模型 | `model::{Document, DocumentMetadata, RetrieveConfig}` | 已完成 |
| `core.rag.model.Document`（id 生成） | `UUID.nameUUIDFromBytes`（MD5 v3） | `hash::{md5, uuid_v3_from_name}` | 已完成 |
| `core.rag.knowledge.SimpleKnowledge` | 本地 embedding + 向量库知识库 | `simple::SimpleKnowledge`（+ `in_memory_knowledge`） | 已完成 |
| `core.rag.reader.Reader` | 文档读取抽象 | `reader::Reader` (trait) | 已完成 |
| `core.rag.reader.ReaderInput` | 统一输入源（String/File） | `reader::ReaderInput` | 已完成 |
| `core.rag.reader.AbstractChunkingReader` | 分块参数校验基类 | `reader::validate_chunking_parameters`（部分） | 已完成 |
| `core.rag.reader.TextReader` | 纯文本读取 | `reader::TextReader` | 已完成 |
| `core.rag.reader.TextChunker` | 文本分块算法 | `reader::{chunk_text, TextChunker, CHARS_PER_TOKEN}` | 已完成 |
| `core.rag.reader.SplitStrategy` | 字符/段落/token/语义分块策略 | `reader::SplitStrategy` | 已完成 |
| `core.rag.store.VDBStoreBase` | 向量库抽象 | `store::VdbStore` (trait) | 已完成 |
| `core.rag.store.InMemoryStore` | 内存向量库 | `store::InMemoryStore` | 已完成 |
| `core.rag.store.DistanceCalculator` | 余弦/欧氏距离 | `store::{cosine_similarity, euclidean_distance, squared_euclidean_distance}` | 已完成 |
| `core.rag.store.dto.SearchDocumentDto` | 检索参数 DTO | `store::SearchQuery` | 已完成 |
| `core.embedding.EmbeddingModel` | 向量化抽象 | `embedding::EmbeddingModel` (trait，含默认 `embed_with_usage`) | 已完成 |
| `core.embedding.EmbeddingException` | embedding 异常 | `RagError::Embedding` | 已完成 |
| `core.rag.exception.ReaderException` / `VectorStoreException` | reader / store 异常 | `RagError::Reader` / `RagError::VectorStore` | 已完成 |
| `core.rag.KnowledgeRetrievalTools`（core） | agentic 模式工具输出文本 | `message::format_documents_for_tool` | 已完成 |
| `core.rag.GenericRAGHook`（core） | 自动检索 hook（注入 `<retrieved_knowledge>`） | `message::{build_retrieved_knowledge_msg, extract_query_from_messages, RETRIEVED_KNOWLEDGE_OPEN/CLOSE}` | 已完成 |
| `core.embedding.dashscope.{DashScopeTextEmbedding, DashScopeMultiModalEmbedding}` | DashScope HTTP embedding | `embedding::dashscope::{DashScopeTextEmbedding, DashScopeMultiModalEmbedding}`（+ Builder） | 已完成 |
| `core.embedding.ollama.OllamaTextEmbedding` | Ollama HTTP embedding | `embedding::ollama::OllamaTextEmbedding`（+ Builder，`POST /api/embed`） | 已完成 |
| `core.embedding.openai.OpenAITextEmbedding` | OpenAI HTTP embedding | `embedding::openai::OpenAiTextEmbedding`（+ Builder，`POST /embeddings`） | 已完成 |
| `core.embedding.EmbeddingUtils` | 超时/重试包装 | `embedding::utils::EmbeddingExecutionConfig`（超时 + `retry::RetryPolicy`）、`f32_slice_to_f64` | 已完成 |
| `core.embedding.EmbeddingResponse` / `EmbeddingUsage` | embedding 响应 / token 用量 DTO | `embedding::{EmbeddingResponse, EmbeddingUsage, UsageListener}`（**Rust 侧新增**：Java 无这两个类，见 §7.3 勘误） | 已完成 |
| `core.rag.RAGMode`（core） | GENERIC / AGENTIC / NONE 模式枚举 | 无（core/harness 未迁移 hook 层，见 §4.2） | 待办 |
| `core.rag.reader.TikaReader` / `PDFReader` / `WordReader` | 多格式文档读取（Tika / PDFBox / POI） | 无 | 设计决定（不迁移） |
| `core.rag.reader.ImageReader` | 图片读取（**无第三方 JVM 依赖**，仅需 core 的 `ImageBlock`/`URLSource`，Rust core 已有） | `reader::ImageReader`（+ `SUPPORTED_FORMATS`） | 已完成 |
| `core.rag.reader.ExternalApiReader` | 外部解析 API 读取（可插拔 `RequestBuilder`/`ResponseParser`） | 无（可基于 `crate::transport` seam 实现） | 待办 |
| `core.rag.reader.ReaderUtils` | MD5/SHA-256 docId + hex | `reader::utils::{generate_doc_id, generate_doc_id_md5, generate_doc_id_sha256, bytes_to_hex, DocIdAlgorithm}` + `hash::sha256_hex` | 已完成 |
| `core.rag.reader.TableFormat` | Word 表格格式（Markdown/JSON） | 无（仅 WordReader 使用） | 设计决定（不迁移） |
| `core.rag.store.ElasticsearchStore` / `MilvusStore` / `PgVectorStore` / `QdrantStore` | 外部向量库 | 无（接口为 `store::VdbStore`） | 待办 |

### 2.4 bailian 子模块（`io.agentscope.core.rag.integration.bailian.*`，7 类）— **已完成**
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `BailianClient` | 阿里云百炼检索（基于 `com.aliyun.bailian20231229` SDK） | `bailian::{BailianClient, signing}` — `reqwest` 直连 + 自建 ACS4-HMAC-SHA256 签名，`POST https://{endpoint}/{workspaceId}/index/retrieve` | 已完成 |
| `BailianConfig` (+Builder) | accessKey/workspace/index + 检索参数 | `bailian::config::{BailianConfig, BailianConfigBuilder}` | 已完成 |
| `BailianDocumentConverter` | 百炼 node → `Document` | `bailian::BailianDocumentConverter` | 已完成 |
| `BailianKnowledge` | `Knowledge` 适配器（只读） | `bailian::BailianKnowledge`（+ `BailianKnowledgeBuilder`） | 已完成 |
| `RerankConfig` | rerank 模型名 / minScore / topN | `bailian::config::RerankConfig`（默认 `gte-rerank-hybrid`） | 已完成 |
| `RewriteConfig` | 多轮改写模型（默认 `conv-rewrite-qwen-1.8b`） | `bailian::config::RewriteConfig` | 已完成 |
| `QueryHistoryEntry` | 多轮改写历史条目（user/assistant） | `bailian::query_history::{QueryHistoryEntry, QueryRole}` + `query_history_from_messages`（对应 Java 私有 `convertToQueryHistory`/`extractTextContent`） | 已完成 |

### 2.5 haystack 子模块（`io.agentscope.core.rag.integration.haystack.*`，10 类）— **已完成**
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `HayStackClient` | OkHttp 调用自建 HayStack RAG 服务 | `haystack::HayStackClient`（经 `crate::transport` seam；`POST {baseUrl}`） | 已完成 |
| `HayStackConfig` (+Builder) | baseUrl / topK / 过滤 / 超时 / 重试 | `haystack::config::{HayStackConfig, HayStackConfigBuilder}` | 已完成 |
| `HayStackDocumentConverter` | HayStackDocument → `Document` | `haystack::HayStackDocumentConverter` | 已完成 |
| `HayStackKnowledge` | `Knowledge` 适配器 | `haystack::HayStackKnowledge`（+ `HayStackKnowledgeBuilder`） | 已完成 |
| `FilterPolicy` | 过滤器策略（REPLACE/MERGE） | `haystack::config::FilterPolicy`（含 `policy()`/`from_policy()`） | 已完成 |
| `model.HayStackResponse` | 响应根（含 `context_windows`/`context_documents`） | `haystack::model::HayStackResponse`（+ `effective_documents()` 选择语义） | 已完成 |
| `model.HayStackDocument` | 文档/chunk DTO | `haystack::model::HayStackDocument` | 已完成 |
| `model.SparseEmbedding` | sparse 向量（indices/values） | `haystack::model::SparseEmbedding`（+ `SparseEmbeddingBuilder`） | 已完成 |
| `model.ByteStream` | 二进制负载（data/meta/mime_type） | `haystack::model::ByteStream` | 已完成 |
| `exception.HayStackApiException` | API 异常 | `RagError::Api`（`src/error.rs`；`error_code` = 响应 `code`） | 已完成 |
| 请求体装配（`retrieve` 内联 `HashMap`） | `query`/`top_k`/`filters`/... | `haystack::model::HayStackRequest` + `haystack::request::build_retrieve_request_body` | 已完成 |

### 2.6 ragflow 子模块（`io.agentscope.core.rag.integration.ragflow.*`，9 类）— **已迁移**
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `RAGFlowClient` | OkHttp 调用 `POST /api/v1/retrieval` | `ragflow::RAGFlowClient`（经 `crate::transport` seam，含错误映射与重试） | 已完成 |
| `RAGFlowConfig` (+Builder) | apiKey / datasetIds / topK / threshold / page 等 | `ragflow::config::{RAGFlowConfig, RAGFlowConfigBuilder}` | 已完成 |
| `RAGFlowDocumentConverter` | chunk → `Document`（payload 装配） | `ragflow::RAGFlowDocumentConverter` | 已完成 |
| `RAGFlowKnowledge` | `Knowledge` 适配器 | `ragflow::RAGFlowKnowledge`（仅检索） | 已完成 |
| `model.RAGFlowChunk` / `RAGFlowResponse` / `ResponseDataDeserializer` | chunk / 响应 DTO（`data` 可 false/null/object） | `ragflow::model::{RAGFlowChunk, RAGFlowResponse, ResponseData, DocAgg}`（自定义反序列化复刻） | 已完成 |
| `exception.RAGFlowApiException` / `RAGFlowAuthException` | API / 鉴权异常 | `RagError::Api` / `RagError::Auth`（`src/error.rs`） | 已完成 |

## 3. 详细缺口清单

### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 骨架 + `lib.rs` 接线 | 全模块 | 已完成 | 15 个 `pub mod` 声明（本轮新增 `bailian` / `haystack`）+ re-export；`agentscope-core` / `agentscope-harness` 为 path 依赖 |
| 公共 RAG 模型 | `core.rag.model.{Document, DocumentMetadata, RetrieveConfig}` | 已完成 | `src/model.rs`；因 Java core `rag` 包 `@Deprecated(forRemoval)` 且 core 无对应，模型落在本 crate（见 §4.2） |
| `Knowledge` trait + 共享排序 | `core.rag.Knowledge` | 已完成 | `src/knowledge.rs`；Reactor `Mono` → `async_trait` |
| 检索结果 → core `Msg` 桥接 | `core.rag.KnowledgeRetrievalTools` / `GenericRAGHook` | 已完成 | `src/message.rs`；core 侧 `RagMode`/hook 未迁移，先提供纯函数 |
| Document id（UUID v3 / MD5） | `core.rag.model.Document` | 已完成 | `src/hash.rs` 自实现，未新增第三方 crate |
| 统一错误类型 | `EmbeddingException` / `VectorStoreException` / `ReaderException` / `Dify*Exception` | 已完成 | `src/error.rs` |
| HTTP 重试 | `DifyRAGClient.RetryInterceptor` | 已完成 | `src/retry.rs` |
| **dify 全链路** | dify 子模块 11 类 | 已完成 | `src/dify/**`；请求体 / 响应 / 转换 / 适配器 / 错误全覆盖 |
| **ragflow 全链路** | ragflow 子模块 9 类 | 已完成 | `src/ragflow/**`；请求体 / 响应（`data` 多态反序列化）/ 转换 / 适配器 / 错误全覆盖 |
| **bailian 全链路** | bailian 子模块 7 类 | 已完成 | `src/bailian/**`；9 个源文件 2608 行 / 64 个内联测试；`reqwest` + 自建 ACS4-HMAC-SHA256 签名，不引入阿里云 SDK |
| **haystack 全链路** | haystack 子模块 10 类 | 已完成 | `src/haystack/**`；7 个源文件 1692 行 / 49 个内联测试 |
| **simple 离线链路** | `SimpleKnowledge` / `InMemoryStore` / `DistanceCalculator` / `TextReader` / `TextChunker` / `SplitStrategy` / `ReaderInput` / `EmbeddingModel` | 已完成 | `src/{simple,store,reader,embedding}.rs` |
| 离线单测（265 → 325 个） | Java 测试 16792 行 | 已完成 | 上一轮 265；本轮新增 60（openai 12 / dashscope 15 / ollama 8 / embedding::utils 10 / reader::utils 6 / reader::image 8 / hash 1）；HTTP 仍走注入式 mock transport，全离线 |

### 3.2 缺口（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| Bailian 集成 | `bailian.*` 7 类 | **已完成** | `src/bailian/**`；`reqwest` 直连 + 自建 ACS4-HMAC-SHA256 签名（不引入阿里云 SDK）；端点/路径/API 版本/请求体(camelCase)/响应体(PascalCase)/`Authorization` 均以 SDK jar 逆向并用金标向量钉住 |
| HayStack 集成 | `haystack.*` 10 类 | **已完成** | `src/haystack/**`；复用 `crate::transport` seam；覆盖 `documents` 与 `context_documents`（SentenceWindow）两种形态 |
| RAGFlow 集成 | `ragflow.*` 9 类 | **已完成** | `src/ragflow/**`；`POST /api/v1/retrieval`；`data` 字段可 `false`/`null`/object 的自定义反序列化已复刻（`ResponseData`） |
| simple 外部向量库 | `ElasticsearchStore`(526) / `MilvusStore`(919) / `PgVectorStore`(979) / `QdrantStore`(996) | 待办 | 本轮仅做 bailian/haystack，未动此项；均以 `store::VdbStore` 为接口；各需 REST/gRPC 客户端，可离线 mock |
| embedding HTTP 提供商 | `DashScopeTextEmbedding` / `DashScopeMultiModalEmbedding` / `OllamaTextEmbedding` / `OpenAITextEmbedding` | **已完成** | `src/embedding/{dashscope,ollama,openai}.rs` + 共享 `utils.rs`；均走 `crate::transport` seam，离线 mock 覆盖（3 个提供商共 35 个测试，另 10 个共享 utils 测试）；超时/重试统一由 `EmbeddingExecutionConfig` 负责（transport 侧 `max_retries=0`，避免双重重试） |

### 3.3 增强（P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| 文档读取器（Tika/PDF/Word） | `TikaReader` / `PDFReader` / `WordReader` | 设计决定（不迁移） | **依据**：这三个 reader 直接依赖 JVM 库（Apache Tika / PDFBox / POI）及其文档模型，Java 实现无法机械直译；强行引入等价 Rust 库（如 `pdf-extract`/`lopdf`/`docx-rs`）会带来新的重依赖与解析语义差异，且与本 crate 的“高内聚低耦合”定位不符。**裁定：不迁移**，保留本 crate 现有的 `Reader` trait 作为扩展点，调用方可自行实现或在本 trait 上接入外部解析服务。 |
| `ImageReader` | `ImageReader`（图片 → `ImageBlock`/`URLSource`，`enableOCR` 占位不实现 OCR） | 已完成 | `src/reader/image.rs`：`ImageReader::new(enable_ocr)` / `is_ocr_enabled()` / `SUPPORTED_FORMATS`（7 项，与 Java 一致）；内容为 `ContentBlock::Image(ImageBlock{ URLSource })`，URL 保持原样、本地路径添 `file://`；docId = `generate_doc_id_md5(path)`、chunkId `"0"`；**输入取路径而非文件内容**（Rust `ReaderInput::as_string` 会读文件内容，这里显式用路径，见 §7.3 差异 2）；8 个测试 |
| `ExternalApiReader` | `ExternalApiReader` | 待办 | 与 `dify`/`ragflow`/`haystack` 不同，它描述的是“调用外部解析 API”，无固定 wire 契约；本轮未动。若后续迁移，建议直接复用 `crate::transport` seam。 |
| reader 辅助 | `ReaderUtils`（MD5/SHA-256 docId + hex） | 已完成 | `src/reader/utils.rs`：`generate_doc_id(input, "MD5" 或 "SHA-256")`、`generate_doc_id_md5`、`generate_doc_id_sha256`、`bytes_to_hex`、`DocIdAlgorithm`（未知算法 → `InvalidArgument("<algo> algorithm not available")`，与 Java 同文案）；SHA-256 新增 `hash::sha256_hex`（复用已有 `sha2`，**未新增依赖**）；6 个测试（含独立算得的 MD5/SHA-256 金标向量） |
| `TableFormat` | `TableFormat`（Word 表格 Markdown/JSON） | 设计决定（不迁移） | **依据**：`TableFormat` **仅**被未迁移的 `WordReader` 使用，无其它调用方；“不迁移”结论与 WordReader 一致。 |
| embedding 辅助 | `EmbeddingUtils`（超时/重试包装） | 已完成 | `src/embedding/utils.rs::EmbeddingExecutionConfig`（`timeout` + `retry: RetryPolicy`）提供 `apply(model, provider, operation_type, op)`：每次尝试套 `tokio::time::timeout`，重试复用 `retry_async`（仅 transport / 429 / 5xx）；工厂 `build_transport` 强制 `max_retries=0` 以免与 transport 自身重试叠加；10 个测试 |
| embedding 用量 DTO | `EmbeddingResponse` / `EmbeddingUsage`（token 用量 + 客户端耗时） | 已完成 | `embedding::{EmbeddingUsage, EmbeddingResponse, UsageListener}` + trait 默认方法 `embed_with_usage`（本地模型无需改）；各提供商从 wire 的 `usage` / `prompt_eval_count` 解析，`EmbeddingResponse.elapsed` 记录客户端耗时；`usage_listener` 在 builder 上可选注册。**§7.3 勘误：Java 仓库里并无 `EmbeddingUsage`/`EmbeddingResponse` 类，亦无 `usageListener`（全仓 `rg` 零命中），此项属 Rust 侧增值，不得再写作“Java 已有”** |

## 4. 项目状态与下一步

### 4.1 当前状态
- **已完成**：crate 骨架、公共 RAG 模型/trait、消息桥接、Dify 全链路、simple 离线链路（`SimpleKnowledge` + `InMemoryStore` + `TextReader`/`TextChunker` + `HashEmbeddingModel` + 距离计算）、RAGFlow 全链路、**Bailian 全链路（含自建 ACS4-HMAC-SHA256 签名）**、**HayStack 全链路**；全 crate 共 **49 个源文件**。
- **2026-10-26 本轮完成**：`simple` 的 HTTP embedding 4 件（`OpenAiTextEmbedding` / `DashScopeTextEmbedding` / `DashScopeMultiModalEmbedding` / `OllamaTextEmbedding`）、`EmbeddingUtils`→`EmbeddingExecutionConfig`、`EmbeddingUsage`/`EmbeddingResponse`/`UsageListener`、`ReaderUtils`（MD5+SHA-256）、`ImageReader`；新增 7 个源文件 2437 行非空 / 60 个离线测试。详见 §7。
- **验证命令与结果**（上一轮实测，2026-09-19）：
  - `cargo build -p agentscope-extensions-rag` → **通过（0 errors，exit 0）**
  - `cargo test -p agentscope-extensions-rag` → **265 passed / 0 failed / 0 ignored**（untittests src/lib.rs）
  - 本 crate 无新增 warning（`warning:` 匹配 `extensions-rag` 计数 = 0）
- **已知问题**：无编译/测试失败。设计上有一处显式取舍——core 侧 `RagMode`/hook 与 `io.agentscope.core.rag` 包未迁移，检索结果到消息的桥接以纯函数形式提供（`src/message.rs`），调用方需自行组装 hook/工具。
- **2026-10-22 静态复核**：未运行 `cargo build`/`cargo test`（本轮为纯静态核对），仅校验源码与统计口径；详见 §6。

### 4.2 下一步与阻塞
1. **simple 外部向量库（P1，唯一剩余大块）**：`ElasticsearchStore` / `MilvusStore` / `PgVectorStore` / `QdrantStore`，均实现 `store::VdbStore`；**本轮未动，仍为待办**。
2. **`ExternalApiReader`（P2）**：无固定 wire 契约，建议基于 `crate::transport` seam 实现（§3.3）。
3. **已裁定不迁移**：`TikaReader`/`PDFReader`/`WordReader`、`TableFormat`（理由见 §3.3）。
4. **待裁定事项**
   - `RagMode` / `GenericRAGHook` 落点：core 是否新增 hook 抽象，或由 harness 承接（当前 `message.rs` 纯函数过渡）。
   - `SimpleKnowledge` 是否接 `usage_listener`：`EmbeddingResponse`/`UsageListener` 已可用，但 `SimpleKnowledge` 目前仍调 `embed()`，未向外透传 usage（本轮未改，避免扩大改动面）。
5. **跨模块需求**
   - 若需在 core/harness 增加 RAG hook 或 embedding 抽象，本 crate 无权修改，需汇总方裁定（记入本 §4.2，不直接改 core/harness）。
   - 若 embedding 提供商确定归 MODEL 扩展，本 crate 仅依赖其 trait，不重复实现。**→ 2026-10-26 裁定（落地）：沿用 Java 分属——提供商留在 RAG 扩展（`src/embedding/*`），否则 `SimpleKnowledge` 无法自包含；MODEL 扩展的 `ollama` 只保留 chat/通用 `embed`，两边 wire 契约一致但不共享代码（本 crate 不依赖 `agentscope-extensions-model`）。**
6. **Bailian 不引入阿里云 SDK（本轮裁定，已落地）**
   - **背景**：Java `BailianClient` 把检索委托给 `com.aliyun.bailian20231229` SDK（Tea/OpenAPI 体系，经 `com.aliyun.gateway.pop.Client` 以 `authType=AK` / `style=ROA` / ACS4-HMAC-SHA256 签名）。
   - **裁定**：不将阿里云 SDK（Java 生态、依赖树庞大）引入 Rust workspace；本 crate 用 `reqwest` 直连并**自建签名**（`src/bailian/signing.rs`，仅依赖已在 `Cargo.lock` 的 `hmac`/`sha2`）。
   - **保真度保障**：端点(`bailian.cn-beijing.aliyuncs.com`)、路径(`/{workspaceId}/index/retrieve`)、API 版本/动作(`x-acs-version=2023-12-29`/`x-acs-action=Retrieve`)、请求体键(camelCase)、响应体键(PascalCase)、region 推导、`x-acs-date` 格式、`Authorization` 格式均已从 SDK jar（2.8.2 + gateway-pop 0.0.6）字节码逆向；并用**真实 SDK 生成的金标向量**（`signing_key`/`signature`/`Authorization`/`region`）写成单测钉住。
   - **已知取舍**：不实现 `ACS4-HMAC-SM3`（国密）分支与 `Bearer`/`Anonymous` 鉴权分支（Java `BailianClient` 仅使用 `AK`），需要时可扩展。
   - **新增依赖**：`hmac = "0.12"`、`sha2 = "0.10"` 已写入本 crate `[dependencies]`（未改根 `[workspace.dependencies]`）；两者均已在仓库 `Cargo.lock` 中存在（`sha2` 与 workspace 同版本）。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-rag` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-rag` / `cargo test -p agentscope-extensions-rag` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-rag/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

## 6. 静态复核记录（2026-10-22）

### 6.1 复核方式
- **纯静态核对**：本轮**未执行** `cargo build` / `cargo test`，**未修改任何 Rust 代码**；仅读取 Java 源码、Rust 源码并做符号级 grep / 行数统计。所有"已完成"结论沿用上一轮（2026-09-19）的编译与测试证据，本节只做**源码级保真度复核 + 数据勘误**。
- **Java 路径更正**：原文 `D:\IdeaProjects\agentscope-java\...` 已失效，实际为 `E:\local-github\agentscope-java\...`（已在文件头修正）。
- **统计口径**：Java 与 Rust 统一按**非空行**（trim 后非空）统计；此口径下 Java 合计与 §6.2 的实测参考值完全吻合，故沿用。
- **逐类核对方式**：对 tracker 中"Java 类 → Rust 对应"的每一条映射，逐个在 Rust `src/**` 中 grep 符号名，并打开对应 Java 源文件比对**行为**（而非仅同名）。

### 6.2 实测统计

**Java 侧**（`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-rag`，共 133 个 `.java`）

| 子模块 | 主文件 | 主代码（非空行） | 测文件 | 测试（非空行） |
|---|---|---|---|---|
| bailian | 7 | 1447 | 8 | 1493 |
| dify | 11 | 2509 | 6 | 1136 |
| haystack | 10 | 1565 | 5 | 1163 |
| ragflow | 9 | 1849 | 5 | 2091 |
| simple | 33 | 8576 | 39 | 10909 |
| **合计** | **70** | **15946** | **63** | **16792** |

- Java core 另引用 `io.agentscope.core.rag.*` **7 类**（`Knowledge` / `KnowledgeRetrievalTools` / `GenericRAGHook` / `RAGMode` / `model.Document` / `model.DocumentMetadata` / `model.RetrieveConfig`）。
- simple 子模块拆分：`io.agentscope.core.rag.*` **24 类** + `io.agentscope.core.embedding.*` **9 类** = **33 类**。

**Rust 侧**（`crates/agentscope-extensions-rag/src`，共 **42** 个 `.rs`）

| 分区 | 文件数 | 非空行 | `#[test]` | `#[tokio::test]` | 合计 |
|---|---|---|---|---|---|
| 根（`lib.rs`/`error`/`hash`/`knowledge`/`message`/`model`/`reader`/`retry`/`simple`/`store`/`embedding`/`transport`） | 12 | 2373 | 42 | 21 | 63 |
| `dify/` | 7 | 1683 | 32 | 13 | 45 |
| `ragflow/` | 7 | 1646 | 31 | 13 | 44 |
| `bailian/` | 9 | 2608 | 55 | 9 | 64 |
| `haystack/` | 7 | 1692 | 32 | 17 | 49 |
| **合计** | **42** | **10002** | **192** | **73** | **265** |

- 原始行数（含空行/注释）为 11144；上表"非空行"= 10002，与 §1.1 的既有口径一致。
- `lib.rs` 声明 `pub mod` **15** 个（`bailian`/`dify`/`embedding`/`error`/`hash`/`haystack`/`knowledge`/`message`/`model`/`ragflow`/`reader`/`retry`/`simple`/`store`/`transport`），与 §1.1 一致。

### 6.3 逐子模块对齐结论

| 子模块 | Java 类数 | Rust 覆盖 | 结论 | 备注 |
|---|---|---|---|---|
| `dify` | 11 | 11/11 符号存在且行为等价 | **等价** | 请求体 `search_method`/`top_k`/`score_threshold_enabled`/`reranking_enable`/`weights`/`metadata_filtering_conditions` 逐键对齐；`buildPayloadFromSegment` 的 23 个 payload 键逐一对应；401/403 → `RagError::Auth`，其余 → `RagError::Api`；`RetryInterceptor` 的 429/5xx 重试语义与 `retry::is_retryable_error` 一致。 |
| `ragflow` | 9 | 9/9 | **等价** | 端点 `POST /api/v1/retrieval`（Java 注释写的 `retrieve-chunks` 是**注释错误**，实际代码即 `/api/v1/retrieval`，Rust 跟的是实际行为）；`data` 可 `false`/`null`/object 的多态反序列化已复刻；错误分支 401/403→Auth、404/429/5xx→Api 逐条对齐。 |
| `bailian` | 7 | 7/7 | **等价** | `POST https://{endpoint}/{workspaceId}/index/retrieve`；`x-acs-version=2023-12-29` / `x-acs-action=Retrieve`；请求体 camelCase（`#[serde(rename_all="camelCase")]`）、响应体 PascalCase（逐字段 `rename`）；`QueryHistoryEntry` 的 role 白名单由 `QueryRole` 枚举表达；ACS4-HMAC-SHA256 有金标向量单测。 |
| `haystack` | 10 | 10/10 | **等价** | `code != successCode` → `RagError::Api`（Java 只记日志不抛 status 错误，Rust 保留"仅 body 不可解析才是传输错"的分裂语义）；`documents` 优先、否则 `context_documents`（SentenceWindow）已覆盖；Java 保留无 score 文档且**不重排序**，Rust 同样保留顺序。 |
| `simple`（离线部分） | 33 + core 7 | 21/40 | **部分** | 已覆盖：`Knowledge`/`Document`/`DocumentMetadata`/`RetrieveConfig`/`SimpleKnowledge`/`Reader`/`ReaderInput`/`AbstractChunkingReader`(→`validate_chunking_parameters`)/`TextReader`/`TextChunker`/`SplitStrategy`/`VDBStoreBase`(→`VdbStore`)/`InMemoryStore`/`DistanceCalculator`/`SearchDocumentDto`(→`SearchQuery`)/`EmbeddingModel`/`EmbeddingException`/`ReaderException`/`VectorStoreException`/`KnowledgeRetrievalTools`(→`message::format_documents_for_tool`)/`GenericRAGHook`(→`message::*`)。**未覆盖见 §6.4。** |
| `simple`（HTTP embedding 提供商） | 4 | 0/4 | **缺失** | `DashScopeTextEmbedding` / `DashScopeMultiModalEmbedding` / `OllamaTextEmbedding` / `OpenAITextEmbedding` —— Rust 全库 grep `DashScope`/`Ollama`/`OpenAI` **仅在注释中出现**，无任何实现。 |
| `simple`（外部向量库） | 4 | 0/4 | **缺失** | `ElasticsearchStore` / `MilvusStore` / `PgVectorStore` / `QdrantStore` —— Rust 全库 grep `Elasticsearch`/`Milvus` **命中 0**，`PgVector`/`Qdrant` **仅命中注释**。 |
| `simple`（reader 增强） | 5 + 2 辅助 | 0/7 | **缺失 / 已裁定不迁移** | `TikaReader`/`PDFReader`/`WordReader`/`TableFormat` → 设计决定（不迁移）；`ImageReader`/`ExternalApiReader`/`ReaderUtils` → **新增待办**（原裁定有误，见 §6.5 勘误 3/4）。 |

### 6.4 新发现缺口

| 优先级 | 缺口 | Java 路径 / 证据 | Rust 现状 | 说明 |
|---|---|---|---|---|
| **P1** | 外部向量库 4 件（`ElasticsearchStore`/`MilvusStore`/`PgVectorStore`/`QdrantStore`） | `agentscope-extensions-rag-simple/src/main/java/io/agentscope/core/rag/store/{ElasticsearchStore,MilvusStore,PgVectorStore,QdrantStore}.java`（非空行 526 / 919 / 979 / 996） | **缺失**（仅 `store::VdbStore` trait 存在） | Java 侧分别走 ES REST、Milvus gRPC、Postgres JDBC(+pgvector)、Qdrant gRPC；Rust 侧需各自实现 `VdbStore`（`add`/`search`/`delete`）。均可用 `crate::transport` 或 mock trait object 做离线单测。**仍是原 §3.2 的最大缺口，未被本轮缩小。** |
| **P1** | HTTP embedding 提供商 4 件 | `.../io/agentscope/core/embedding/{dashscope/DashScopeTextEmbedding,dashscope/DashScopeMultiModalEmbedding,ollama/OllamaTextEmbedding,openai/OpenAITextEmbedding}.java` | **缺失**（仅离线 `HashEmbeddingModel`） | 归属待裁定（MODEL 扩展 vs RAG 扩展）。`src/embedding.rs` 注释已明确倾向 MODEL。 |
| **P2** | `ImageReader`（新增，原被误判为"不迁移"） | `.../io/agentscope/core/rag/reader/ImageReader.java`（非空行 140） | **缺失，但可机械直译** | 该文件**无任何第三方 JVM import**，仅用 core 的 `ImageBlock` + `URLSource`（Rust core `agentscope-core::message::{ImageBlock, Source::URLSource}` 已有等价类型）；docId 用 `ReaderUtils.generateDocIdMD5`（Rust `hash::md5_hex` 已覆盖）；OCR 在 Java 侧本身也是**空实现占位**。迁移成本极低。 |
| **P2** | `ReaderUtils` 的 SHA-256 侧（MD5 侧已由 `hash::md5_hex` 覆盖） | `.../io/agentscope/core/rag/reader/ReaderUtils.java` | **部分** | `sha2` 依赖已在 crate 内（`bailian/signing.rs` 使用），补 `sha256_hex` 无需新增依赖。 |
| **P2** | `EmbeddingUsage` / `EmbeddingResponse` / `SimpleKnowledge.usageListener` | `.../io/agentscope/core/embedding/{EmbeddingUsage,EmbeddingResponse}.java`；`.../io/agentscope/core/rag/knowledge/SimpleKnowledge.java` | **缺失** | Rust `EmbeddingModel::embed` 返回 `Vec<f64>`，**不携带 token usage**；Java `SimpleKnowledge` 每次 embed 后回调 `usageListener(EmbeddingUsage)`。若接入真实 HTTP provider，token 计量与计费回调无处落地。 |
| **P2** | `RAGMode`（core 枚举） | `agentscope-core/src/main/java/io/agentscope/core/rag/RAGMode.java` | **缺失** | 已复核：`agentscope-core` 与 `agentscope-harness` 的 Rust 源码中 grep `RagMode`/`rag_mode`/`GenericRAGHook`/`retrieved_knowledge` **命中 0**，确认 core 侧 hook 层确实未迁移（§4.2 表述属实）。`message.rs` 的纯函数过渡是当前唯一路径。 |
| **P2** | `ExternalApiReader` | `.../io/agentscope/core/rag/reader/ExternalApiReader.java`（非空行 408） | **缺失** | 依赖 OkHttp 但 wire 契约由调用方通过 `RequestBuilder`/`ResponseParser` 注入，**本身不绑定外部服务**；可直接基于 `crate::transport` seam 实现。原 §3.3 记为"无固定 wire 契约"属实。 |
| **P2** | `EmbeddingUtils`（超时/重试包装 + `float[]`/`List<Double>` 转换） | `.../io/agentscope/core/embedding/EmbeddingUtils.java` | **部分（能力已由 `retry::RetryPolicy` 覆盖）** | 超时/重试已由 `retry.rs` + `transport` 提供；但 `convertDoubleListToArray` 等数组转换工具在 Rust 侧无对应（`Vec<f64>` 原生可用，影响可忽略）。 |

### 6.5 被证伪声明的勘误列表

| # | 原文（tracker 位置） | 实际 | 处置 |
|---|---|---|---|
| 1 | `> **Java 源码**：D:\IdeaProjects\agentscope-java\...`（文件头） | 路径已失效，实际为 `E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-rag\` | 已修正文件头 |
| 2 | `Java 规模（实测行数）: 主代码 **15438** / 测试 **16057**`（§1） | 主 **15946** / 测 **16792**（非空行）。差值来自 simple 子模块在上轮后继续演进（simple 主 8068→**8576**、测 10188→**10909**）与 bailian 测 1479→**1493** | 已修正 §1（含 5 个子模块分项） |
| 3 | `§3.3 文档读取器（Tika/PDF/Word/Image）… **已完成（设计决定，不迁移）**，依据：这四个 reader 直接依赖 JVM 库（Apache Tika / PDFBox / POI）` | `ImageReader.java` 的 import 只有 `ImageBlock` / `URLSource` / `ReaderException` / `Document` / `DocumentMetadata` / `List` / `Mono` —— **零第三方 JVM 依赖**；`TikaReader`/`PDFReader`/`WordReader` 才有 tika/pdfbox/poi | 已拆分：`ImageReader` 从"不迁移"改为**待办**（§2.3 / §3.3 / §4.2） |
| 4 | `§3.3 reader 辅助（ReaderUtils）… **已完成（设计决定，不迁移）**，依据：其 SHA-256 路径仅服务于未迁移的 Tika/PDF/Word reader` | `ReaderUtils.generateDocIdMD5` 被 **`ImageReader`**（`ImageReader.java:91`）调用；`generateDocIdSHA256` 被 `TikaReader`/`PDFReader`/`ExternalApiReader` 调用。MD5 路径服务于**可迁移**的 `ImageReader` → "无调用方即无迁移价值"前提不成立 | 已由"不迁移"改为**待办**（§2.3 / §3.3） |
| 5 | `§3.2 simple 外部向量库 … MilvusStore(899)` | `MilvusStore.java` 非空行 **919**（899 为旧值） | 已修正为 919 |
| 6 | `§3.1 离线单测（265 个）\| Java 测试 16057 行` | Java 测试 **16792** 行 | 已修正 |
| 7 | `§2.3 simple 子模块 … 31 类` | simple 子模块实际 **33 类**（rag 24 + embedding 9）；若含 Java core `io.agentscope.core.rag.*` 则为 **40 类** | 已修正 §2.3 标题 |
| 8 | `§3.3 TableFormat / 文档读取器` 状态写作 `**已完成（设计决定，不迁移）**` | 状态取值不在 §0 约定的四值域内（`待办`/`进行中`/`已完成`/`设计决定（不迁移）`），"已完成"会与"未迁移"语义冲突 | 全部规范为 `设计决定（不迁移）`；其余 `待办 P1`/`待办 P2`/`待办（非本轮范围）` 一并规范为 `待办`（P 级与"非本轮范围"信息移入说明列） |
| 9 | `§1.1 Rust crate 文件清单（42 个源文件，10002 行，265 个内联测试）` | 数字正确，但**未说明行数口径**（原始行 11144 / 非空 10002），且未拆分 `#[test]` 与 `#[tokio::test]` | 已补充口径说明与拆分（192 + 73 = 265） |
| 10 | `§1.1 src/lib.rs … 15 个模块声明` | 复核通过：实际 `pub mod` 计数 = **15** | 属实，无需修改 |

### 6.6 复核后 rollup 状态建议

- **建议 rollup 状态**：`进行中`（模块整体）。
- **理由**：
  1. 五个子模块中 `dify` / `ragflow` / `bailian` / `haystack` 四个**已完整等价迁移**，且有源码级逐类/逐键核对支撑；
  2. `simple` 子模块**仅离线部分完成**，其体量最大的三块（外部向量库 4 类 ≈ 3420 行、HTTP embedding 4 类 ≈ 1315 行、reader 增强 7 类 ≈ 1400 行）在 Rust 侧**确认为零实现**，且经本次复核**新发现** `ImageReader` / `ReaderUtils` / `EmbeddingUsage` / `RAGMode` 四项此前未单列的缺口；
  3. 因此模块既不能判 `已完成`（仍有 40 类中的 19 类未迁移），也不宜判 `待办`（已有 4/5 子模块落地且质量经得起逐类复核）。
  4. 附注：若采用**子模块级** rollup，则 `dify`/`ragflow`/`bailian`/`haystack` 各自可判 `已完成`，`simple` 判 `进行中`。

> **历史快照提示（2026-10-26 注）**：本节（§6 全部）是 2026-10-22 的静态复核快照，其中“HTTP embedding 4 类零实现”“`ImageReader`/`ReaderUtils`/`EmbeddingUsage` 缺失”等结论**已被 §7 推翻**；且 `EmbeddingUsage`/`EmbeddingResponse` 在 Java 仓库中并不存在（见 §7.3）。引用计数时请以 §7.1 为准。

## 7. 本轮交付记录（2026-10-26）

### 7.1 交付物

| 文件 | 行数（非空） | 测试 | 说明 |
|---|---|---|---|
| `src/embedding.rs`（扩充） | 260 | 4（原有） | 新增 `EmbeddingUsage` / `EmbeddingResponse` / `UsageListener`；trait 新增默认方法 `embed_with_usage`；声明并 re-export 4 个子模块 |
| `src/embedding/utils.rs`（新） | 299 | 10 | `EmbeddingExecutionConfig`（`timeout` + `RetryPolicy`，`apply()` 统一套超时/重试）、`f32_slice_to_f64`、`join_url`、`block_type_name`、`build_transport` |
| `src/embedding/openai.rs`（新） | 520 | 12 | `OpenAiTextEmbedding` + Builder；`POST {base}/embeddings`，`Bearer`，`input:[text]`+`dimensions`+`encoding_format=float` |
| `src/embedding/dashscope.rs`（新） | 875 | 15 | `DashScopeTextEmbedding`（`input.texts`）+ `DashScopeMultiModalEmbedding`（`input.contents[{text|image}]`）；本地图片 → `data:` URL；信封 `code` → `RagError::Api` |
| `src/embedding/ollama.rs`（新） | 381 | 8 | `OllamaTextEmbedding` + Builder；`POST {base}/api/embed`，无鉴权；`prompt_eval_count` → usage |
| `src/embedding/test_support.rs`（新） | 48 | — | `#[cfg(test)]` 脚本化内存 transport（LIFO 回放 + 请求录制） |
| `src/reader/utils.rs`（新） | 129 | 6 | `ReaderUtils`：`generate_doc_id(_md5/_sha256)`、`bytes_to_hex`、`DocIdAlgorithm` |
| `src/reader/image.rs`（新） | 185 | 8 | `ImageReader` + `SUPPORTED_FORMATS` |
| `src/hash.rs`（扩充） | 179 | 4（+1 新） | `sha256_hex`；`hex` 改为 `pub(crate)` 供 reader 复用 |
| `src/reader.rs`（扩充） | 395 | 12（原有） | 声明 `reader::{image,utils}` + re-export |
| `src/lib.rs` / `src/simple.rs` / `Cargo.toml`（扩充） | — | — | re-export 与文档；`Cargo.toml` 新增 `base64 = { workspace = true }`（仅本 crate） |

新增测试（共 60）：

- `embedding::openai::tests`：`build_embedding_request_pins_the_wire_contract` / `parses_embedding_and_usage` / `embed_returns_the_bare_vector` / `usage_listener_receives_every_call` / `rejects_non_text_blocks_with_the_java_message` / `rejects_blank_text` / `maps_401_to_auth_error` / `retries_a_500_before_failing` / `empty_data_is_an_error` / `empty_vector_is_an_error` / `blank_body_is_an_error` / `builder_validates_required_fields`
- `embedding::dashscope::tests`：`text_request_uses_texts_input_and_dimension_parameter` / `text_embedding_parses_output_and_total_tokens` / `text_embedding_rejects_image_block` / `text_embedding_rejects_blank_text` / `multimodal_text_request_uses_contents` / `multimodal_base64_image_becomes_data_url` / `multimodal_remote_image_is_forwarded_verbatim` / `multimodal_local_file_is_inlined_as_data_url` / `multimodal_missing_local_file_is_an_error` / `multimodal_rejects_unsupported_blocks` / `multimodal_usage_carries_input_and_output_tokens` / `envelope_code_becomes_api_error` / `missing_output_is_an_error` / `empty_embeddings_list_is_an_error` / `builder_validates_required_fields`
- `embedding::ollama::tests`：`request_targets_api_embed_without_auth` / `parses_f32_embeddings_and_prompt_eval_count` / `missing_prompt_eval_count_reports_empty_usage` / `empty_embeddings_is_an_error` / `empty_vector_is_an_error` / `blank_body_is_an_error` / `transport_error_is_surfaced` / `builder_requires_model_name`
- `embedding::utils::tests`：`default_config_matches_java_constants` / `ensure_default_only_fills_in_none` / `timeout_is_reported_as_an_embedding_error` / `no_timeout_lets_the_operation_finish` / `retries_non_retryable_errors_only_once` / `retries_retryable_errors_until_success` / `f32_slice_widens_to_f64` / `join_url_normalizes_slashes` / `block_type_names_match_java_simple_class_names` / `usage_listener_alias_is_object_safe`
- `reader::utils::tests`：`md5_doc_id_matches_the_java_algorithm` / `sha256_doc_id_matches_the_java_algorithm` / `generate_doc_id_dispatches_by_name` / `unknown_algorithm_is_rejected_with_the_java_message` / `algorithm_names_round_trip` / `bytes_to_hex_is_lowercase_and_padded`
- `reader::image::tests`：`ocr_flag_defaults_to_disabled_and_round_trips` / `supported_formats_match_java` / `url_input_is_kept_verbatim` / `local_path_gets_a_file_scheme` / `file_url_is_not_prefixed_twice` / `file_input_uses_the_path_not_the_contents` / `doc_id_is_the_md5_of_the_path_and_chunk_id_is_zero` / `doc_id_is_stable_and_path_sensitive`
- `hash::tests`：`sha256_matches_fips_vectors`

### 7.2 与 Java 的显式取舍（非缺口）

1. **HTTP 一律走 `crate::transport` seam**：生产用 `ReqwestTransport`（`max_retries = 0`），测试注入 `ScriptedTransport`；重试/超时只在 `EmbeddingExecutionConfig::apply` 一处发生，避免 Java「SDK 重试 + `EmbeddingUtils` 重试」两层叠加。
2. **Java 的 `Duration`/`maxBackoff`**：`RetryPolicy` 只有 `base_delay`（无上限），默认 3 次重试最大睡眠 8s，低于 Java `DEFAULT_MAX_BACKOFF = 10s`，因此不引入额外上限参数。
3. **Java `IllegalStateException` → `RagError::InvalidArgument`**：文案逐字对齐（`apiKey is required and cannot be null or empty` 等），错误类型随本 crate 既有惯例。
4. **Ollama `dimensions = -1`（未指定）→ Rust `0`**：`usize` 不能为负；`dimensions()` 原样返回（`0` = 未指定），仅在 `> 0` 时校验并 warn，与 Java `dimensions > 0` 分支一致。
5. **DashScope 端点常量**：`DEFAULT_BASE_URL = https://dashscope.aliyuncs.com/api/v1`，text 端点 `/services/embeddings/text-embedding/text-embedding`，multimodal 端点 `/services/embeddings/multimodal-embedding/multimodal-embedding`（Java 由 SDK 内部拼）；鉴权统一 `Authorization: Bearer`。
6. **`SimpleKnowledge` 未改**：仍调 `embed()`（经默认 `embed_with_usage` 包装不丢信息），如需 usage 回调可由调用方在模型上注册 `usage_listener`。`SimpleKnowledge` 不调 `usage_listener`（待裁定，见 §4.2）。
7. **`ImageReader` 取路径而非文件内容**：Java `ReaderInput.asString()` 返回构造时的字符串（`fromFile(path)` 即路径）；Rust `ReaderInput::as_string()` 会读文件内容，故 `image.rs` 另行用 `input_as_path()` 取路径。

### 7.3 勘误（本轮新增，必须遵守）

| # | 原文 | 实际 | 处置 |
|---|---|---|---|
| 1 | `§2.3 / §6.4`：`core.embedding.{EmbeddingUsage,EmbeddingResponse}` 存在，`SimpleKnowledge` 有 `usageListener` | 对 **`D:\IdeaProjects\agentscope-java` 全仓 `rg "usageListener\|EmbeddingUsage\|EmbeddingResponse"` 零命中**（仅测试里出现 `Embedding` 字样的无关命中）；Java 既无这两个类，也无 `usageListener` | 已改 §2.3/§3.3：Rust 侧 `EmbeddingUsage`/`EmbeddingResponse`/`UsageListener` 标为 **Rust 增值**，不得再写“Java 已有” |
| 2 | `§6.1`：`Java 路径 ... 实际为 E:\local-github\agentscope-java` | 本机该路径不存在，实际为 `D:\IdeaProjects\agentscope-java\...` | 本轮以 D 盘路径完成核对；下文引用 Java 均指 D 盘 |
| 3 | `§2.3` 多处： "simple 子模块 33 类（rag 24 + embedding 9）" | `io.agentscope.core.embedding` 下**主代码只有 7 个 `.java`**：`EmbeddingException`/`EmbeddingModel`/`EmbeddingUtils` + `dashscope/{Text,MultiModal}` + `ollama/OllamaTextEmbedding` + `openai/OpenAITextEmbedding`。原“9 类”里的 2 类正是不存在的 `EmbeddingUsage`/`EmbeddingResponse` | 勘误：embedding 实为 **7 类**（全部已迁移）；simple 子模块实数应为 **31 类（rag 24 + embedding 7）**，含 core `rag` 引用则为 38 类；§2.3 标题的 33/40 采用旧口径，以本行为准 |

### 7.4 验证建议（交给主智能体）

```powershell
cargo build -p agentscope-extensions-rag
cargo test  -p agentscope-extensions-rag
```

- 全部新测试**离线**：HTTP 走 `ScriptedTransport`；两个涉及临时文件的测试（`multimodal_local_file_is_inlined_as_data_url`、`file_input_uses_the_path_not_the_contents`）只写 `std::env::temp_dir()` 并在结束时删除。
- 反向验证建议：①把 `EmbeddingExecutionConfig::apply` 的超时分支删掉 → `timeout_is_reported_as_an_embedding_error` 应 FAILED；②把 `guess_image_mime_type` 的 `png` 分支改错 → `multimodal_local_file_is_inlined_as_data_url` 应 FAILED；③把 `image_url` 的 `file://` 前缀去掉 → `local_path_gets_a_file_scheme` 应 FAILED；④把 `sha256_hex` 改成调用 `md5_hex` → `sha256_doc_id_matches_the_java_algorithm` 与 `sha256_matches_fips_vectors` 应 FAILED。

### 7.5 仍待办

1. **外部向量库 4 件**：`ElasticsearchStore` / `MilvusStore` / `PgVectorStore` / `QdrantStore`（**本轮未动**）。
2. `ExternalApiReader`（P2）。
3. `RagMode` / `GenericRAGHook` 落点待裁定（core/harness）。

