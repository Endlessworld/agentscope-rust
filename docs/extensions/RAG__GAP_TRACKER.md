# RAG 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **RAG** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`D:\IdeaProjects\agentscope-java\agentscope-extensions\agentscope-extensions-rag\`（旧记录中的 `E:\local-github\agentscope-java\...` 在本机已不存在，见 history/RAG__MIGRATION_HISTORY.md 勘误 2）
> **Java 子模块**：bailian, dify, haystack, ragflow, simple
> **Rust 目标**：`crates/agentscope-extensions-rag/`（波次 A）
> **统计口径**：所有行数均为**非空行**（trim 后非空），Java 与 Rust 同口径；见 history/RAG__MIGRATION_HISTORY.md 静态复核记录。

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
| 当前状态 | **已完成**（P1/P2 清零；2026-10-31 实测 469 passed / 0 failed）—— dify / ragflow / bailian / haystack / simple 5 个子模块的离线链路与外部服务链路均已迁移，含 HTTP embedding 4 件、`ReaderUtils`/`ImageReader`、外部向量库 4 件（Qdrant / PgVector / Milvus / Elasticsearch）、`RAGMode`/`GenericRAGHook` 与 `ExternalApiReader`；§2.3 / §3 / §4.2 无未关闭条目。历史见 history/RAG__MIGRATION_HISTORY.md |

### 1.1 Rust crate 文件清单（55 个源文件；2026-10-28 两批新增 `store/{qdrant,pgvector,milvus,elasticsearch}.rs` 4 个源文件 5587 行非空 + 111 个内联测试；2026-10-31 新增 `src/hook.rs` 与 `src/reader/external_api.rs`）

> 行数口径：非空行（trim 后非空）。上一轮基线为 42 文件 / 10002 行 / 265 测试；本轮新增
> `embedding/{utils,openai,dashscope,ollama,test_support}.rs` 与 `reader/{utils,image}.rs`，
> 并扩充了 `embedding.rs`、`reader.rs`、`hash.rs`、`lib.rs`、`Cargo.toml`。新增测试数：
> openai 12 / dashscope 15 / ollama 8 / embedding::utils 10 / reader::utils 6 / reader::image 8 /
> hash 1 = **60**。**已完成（2026-10-29 勘误）**：实际 2026-10-28 第二批已实测 **436 passed / 0 failed / 0 ignored**（见 §1.2 / §4.1）；2026-10-31 全量实测 **469 passed / 0 failed**。
> **2026-10-31 loop**：新增 `src/hook.rs`（`RagMode` / `GenericRagHook` / `GenericRagMiddleware`）与 `src/reader/external_api.rs`（`ExternalApiReader`），源文件 53 → **55**；该轮新增用例已由主智能体实测（全量 **469 passed / 0 failed**），测试名见 history/RAG__MIGRATION_HISTORY.md。
> **2026-10-31 RAG-09/RAG-10**：新增 `src/reader/pdf.rs`、`src/reader/docx.rs`（源文件 55 → **57**）与集成测试 `tests/embedding_wire_contract.rs`；`Cargo.toml` 新增 `pdf` / `docx` 两个默认关闭的 feature 与 `agentscope-extensions-model` 的 **path dev-dependency**（workspace 成员，非新三方依赖），**未引入 PDF/DOCX 解析库**。本轮用例数与实测结果由主智能体统一编译/跑测后回填。
| 文件 | 职责 |
|---|---|
| `src/lib.rs` | crate 根：15 个模块声明 + re-export；说明 Java core `rag` 包已 `@Deprecated(forRemoval)` 且 core 无对应实现 |
| `src/error.rs` | `RagError`（Argument / Embedding / VectorStore / Reader / Unsupported / Http / Api / Auth / Json / Io），对应 Java 各类异常 |
| `src/hash.rs` | 自实现 MD5 + 版本 3（名称）UUID，复刻 Java `UUID.nameUUIDFromBytes` 的 Document id；**新增 `sha256_hex`**（`ReaderUtils` 用，复用已有 `sha2` 依赖） |
| `src/knowledge.rs` | `Knowledge` trait + 共享 `filter_and_rank`（阈值过滤 + 降序 + limit） |
| `src/message.rs` | 检索结果 → core `Msg`：tool 文本格式与 `<retrieved_knowledge>` 通用 hook 注入 |
| `src/model.rs` | `Document` / `DocumentMetadata` / `RetrieveConfig` / `content_block_text` |
| `src/reader.rs` | `Reader` trait、`ReaderInput`、`TextReader`、`SplitStrategy`、字符/段落/token 分块、`input_as_path`（路径型 reader 共用）；声明 `reader::{external_api,image,utils}` 子模块并 re-export |
| `src/reader/utils.rs` | `ReaderUtils`：`generate_doc_id(_md5/_sha256)`、`bytes_to_hex`、`DocIdAlgorithm`（Java 未知算法 → `InvalidArgument`） |
| `src/reader/image.rs` | `ImageReader`：图片路径/URL → `ImageBlock(URLSource)` 的单个 `Document`；docId = MD5(path)、chunkId `"0"`、OCR 仅保留开关（与 Java 同为空实现） |
| `src/reader/external_api.rs` | **`ExternalApiReader`**（Java 408 行）：`ExternalApiRequestBuilder` / `ExternalApiResponseParser` 注入式契约 + `BodyAsMarkdown` 便捷实现 + `ExternalApiReaderBuilder`（chunk 512/PARAGRAPH/50、connect 10s、read 5min、retries 3、delay 2s、supportedFormats）；`read` = 校验文件 → `maxRetries+1` 次调用（非 2xx 亦重试，同 Java）→ `chunk_text` → docId = SHA-256(path)、chunkId 序号；HTTP 走 `crate::transport`，全离线测试 |
| `src/hook.rs` | **`RagMode`**（GENERIC/AGENTIC/NONE，wire = Java 常量名）+ **`GenericRagHook`**（`enhance_messages` 纯异步逻辑，失败/无命中/空 query 原样返回）+ **`GenericRagMiddleware`**（实现 core `Middleware::on_reasoning`，order 50，即 Java `priority()`）；无需改 core/harness |
| `src/retry.rs` | 指数退避重试（对应 Java `RetryInterceptor`：429 / 5xx / 传输错误重试，4xx 不重试） |
| `src/simple.rs` | `SimpleKnowledge`（支持 `with_usage_listener`，add/retrieve 每次 embed 后回调，同 Java）、`in_memory_knowledge`、`EmptyKnowledge` |
| `src/store.rs` | `VdbStore` trait、`InMemoryStore`、`SearchQuery`、余弦/欧氏距离；声明并 re-export `store::{qdrant,pgvector,milvus,elasticsearch}` |
| `src/store/qdrant.rs` | **`QdrantStore`**（Java `QdrantStore` 996 行）：Qdrant **REST** API（collection 探测/创建、`points?wait=true` upsert、`points/query`、`points/delete`）+ `QdrantConfig`/builder + 纯函数 `parse_rest_base_url`/`create_collection_body`/`build_search_request_body`/`parse_search_response`/`parse_delete_response`/`metadata_to_payload`；1180 行非空 / 25 测试 |
| `src/store/pgvector.rs` | **`PgVectorStore`**（Java `PgVectorStore` 979 行）：pgvector SQL 纯函数（DDL/upsert/search/delete/table probe）、`PgExecutor` seam + `TokioPostgresExecutor` 薄适配、`PgVectorConfig`/builder + 标识符校验、距离→相似度换算；1562 行非空 / 26 测试 |
| `src/store/milvus.rs` | **`MilvusStore`**（Java `MilvusStore` 919 行）：Milvus **REST v2**（`/v2/vectordb/*`：`collections/has`、`collections/create`、`entities/{insert,upsert,search,delete}`、`databases/{list,create}`）+ `MilvusConfig`/builder/`MilvusMetricType` + 纯函数 `parse_milvus_uri`/`create_collection_body`/`document_row`/`insert_body`/`search_body`/`delete_body`/`parse_envelope`/`parse_search_response`；1590 行非空 / 32 测试 |
| `src/store/elasticsearch.rs` | **`ElasticsearchStore`**（Java `ElasticsearchStore` 526 行）：ES REST（索引探测 `GET /{index}`、`PUT /{index}` 的 dense_vector 映射、`POST /_bulk` NDJSON、`POST /{index}/_search` kNN、`DELETE /{index}/_doc/{id}`）+ `ElasticsearchConfig`/builder + 纯函数 `create_index_body`/`bulk_body`/`parse_bulk_response`/`search_body`/`parse_search_response`/`parse_delete_response`；1255 行非空 / 28 测试 |
| `src/embedding.rs` | `EmbeddingModel` trait（含默认 `embed_with_usage` 与默认空实现 `close`）、`EmbeddingUsage`（含 `time_seconds` = Java `EmbeddingUsage.time`）/`EmbeddingResponse`/`UsageListener`、确定性离线的 `HashEmbeddingModel`；声明并 re-export 4 个 HTTP 提供商的子模块 |
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

**2026-10-28 第一批实测**：
```
cargo build -p agentscope-extensions-rag            -> exit 0（0 errors）
cargo test  -p agentscope-extensions-rag --lib      -> 376 passed; 0 failed; 0 ignored
cargo clippy -p agentscope-extensions-rag --lib --all-targets -> exit 0（新增文件无告警）
rustfmt --edition 2021 --check (4 个改动/新增文件)   -> 无 diff
```

**最新（2026-10-28 第二批实测）**：
```
cargo build -p agentscope-extensions-rag            -> exit 0（0 errors）
cargo test  -p agentscope-extensions-rag --lib      -> 436 passed; 0 failed; 0 ignored
cargo clippy -p agentscope-extensions-rag --lib --all-targets -> exit 0
rustfmt --edition 2021 --check (store/{milvus,elasticsearch}.rs、store.rs、lib.rs) -> 无 diff
```
> 依据：见 history/RAG__MIGRATION_HISTORY.md（`MilvusStore` 32 测试 + `ElasticsearchStore` 28 测试 = +60）、§4.1。

**2026-10-31 loop**：本轮新增测试名见 history/RAG__MIGRATION_HISTORY.md；主智能体实测全量 **469 passed / 0 failed**（基线 436 → 469，+33）。

## 2. Java 清单与依赖映射

> Java 主代码位于 5 个子模块（`agentscope-extensions-rag-{bailian,dify,haystack,ragflow,simple}`），
> 另引用 Java **core** 的 `io.agentscope.core.rag.*` / `io.agentscope.core.embedding.*`（不在本扩展源码树内）。

### 2.1 依赖映射（Java → Rust）
| Java 依赖 | 用途 | Rust 替代 | 备注 |
|---|---|---|---|
| OkHttp（`okhttp3.*`） | HTTP 传输 | `reqwest`（workspace 统一 **0.13**；本 crate 为 `reqwest = { workspace = true }`）+ 可注入 transport trait。**已完成（2026-10-29 勘误）**：原记「`reqwest = "0.12"`（`json, rustls-tls`）」，实际根 `Cargo.toml` 已统一 0.13（`default-features=false` + `json,stream,rustls,...`）；依据 `crates/agentscope-extensions-rag/Cargo.toml` | 生产用 `HttpDifyTransport`，测试用内联 mock |
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

### 2.3 simple 子模块（`io.agentscope.core.rag.*` / `io.agentscope.core.embedding.*`，simple 子模块 **33 类**（rag 24 + embedding 9）+ Java core `io.agentscope.core.rag.*` **7 类** = 40 类；实际 embedding 为 **7 类**，见 history/RAG__MIGRATION_HISTORY.md 勘误 3）— **除 `ExternalApiReader`（P2）外均已完成**
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
| `core.rag.GenericRAGHook`（core） | 自动检索 hook（注入 `<retrieved_knowledge>`） | `hook::GenericRagHook`（`enhance_messages` 纯异步逻辑，新）+ `hook::GenericRagMiddleware`（core `Middleware::on_reasoning` 适配，order 50，新）+ `message::{build_retrieved_knowledge_msg, extract_query_from_messages, RETRIEVED_KNOWLEDGE_OPEN/CLOSE}` | **已完成**（2026-10-31，见 history/RAG__MIGRATION_HISTORY.md） |
| `core.embedding.dashscope.{DashScopeTextEmbedding, DashScopeMultiModalEmbedding}` | DashScope HTTP embedding | `embedding::dashscope::{DashScopeTextEmbedding, DashScopeMultiModalEmbedding}`（+ Builder） | 已完成 |
| `core.embedding.ollama.OllamaTextEmbedding` | Ollama HTTP embedding | `embedding::ollama::OllamaTextEmbedding`（+ Builder，`POST /api/embed`） | 已完成 |
| `core.embedding.openai.OpenAITextEmbedding` | OpenAI HTTP embedding | `embedding::openai::OpenAiTextEmbedding`（+ Builder，`POST /embeddings`） | 已完成 |
| `core.embedding.EmbeddingUtils` | 超时/重试包装 | `embedding::utils::EmbeddingExecutionConfig`（超时 + `retry::RetryPolicy`）、`f32_slice_to_f64` | 已完成 |
| `core.embedding.EmbeddingResponse` / `EmbeddingUsage` | embedding 响应 / token 用量 DTO | `embedding::{EmbeddingResponse, EmbeddingUsage, UsageListener}` | **已完成（字段集有差异，见 history/RAG__MIGRATION_HISTORY.md）**：Java 侧由 `5bfb36b44`（2026-10-20）新增，**确为 Java 已有**（见 history/RAG__MIGRATION_HISTORY.md 勘误 1 已更正） |
| `core.rag.RAGMode`（core） | GENERIC / AGENTIC / NONE 模式枚举 | `hook::RagMode`（`as_str` / `from_wire` / `is_generic`，serde wire = Java 常量名） | **已完成**（2026-10-31，`hook::tests::rag_mode_wire_values_match_the_java_enum`） |
| `core.rag.reader.TikaReader` / `PDFReader` / `WordReader` | 多格式文档读取（Tika / PDFBox / POI） | `reader::{pdf::PdfReader, docx::DocxReader}`（提取 seam + 可选 `pdf`/`docx` feature）；`TikaReader` 仍无对应 | **已完成（2026-10-31，RAG-09 改判）**：PDF/DOCX 以 `PdfTextExtractor`/`DocxTextExtractor` seam 落地，feature 关闭时 `new()` 直接返回 `RagError::Reader`（不静默吞文档）；Tika 无等价依赖 → 走 `ExternalApiReader` |
| `core.rag.reader.ImageReader` | 图片读取（**无第三方 JVM 依赖**，仅需 core 的 `ImageBlock`/`URLSource`，Rust core 已有） | `reader::ImageReader`（+ `SUPPORTED_FORMATS`） | 已完成 |
| `core.rag.reader.ExternalApiReader` | 外部解析 API 读取（可插拔 `RequestBuilder`/`ResponseParser`） | `reader::external_api::{ExternalApiReader, ExternalApiReaderBuilder, ExternalApiRequestBuilder, ExternalApiResponseParser, BodyAsMarkdown}`（经 `crate::transport` seam，可轮询） | **已完成**（2026-10-31，见 history/RAG__MIGRATION_HISTORY.md） |
| `core.rag.reader.ReaderUtils` | MD5/SHA-256 docId + hex | `reader::utils::{generate_doc_id, generate_doc_id_md5, generate_doc_id_sha256, bytes_to_hex, DocIdAlgorithm}` + `hash::sha256_hex` | 已完成 |
| `core.rag.reader.TableFormat` | Word 表格格式（Markdown/JSON） | 无（仅 WordReader 使用） | 设计决定（不迁移） |
| `core.rag.store.ElasticsearchStore` / `MilvusStore` / `PgVectorStore` / `QdrantStore` | 外部向量库 | `store::{PgVectorStore, QdrantStore, MilvusStore, ElasticsearchStore}`（均实现 `store::VdbStore`） | **4/4 已完成**（2026-10-28 两批，见 history/RAG__MIGRATION_HISTORY.md / 见 history/RAG__MIGRATION_HISTORY.md） |

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
| 检索结果 → core `Msg` 桥接 | `core.rag.KnowledgeRetrievalTools` / `GenericRAGHook` | 已完成 | `src/message.rs`（纯函数）+ `src/hook.rs`（`GenericRagHook` / `GenericRagMiddleware`，2026-10-31）；hook 层以 core `Middleware::on_reasoning` 承接，无需改 core/harness |
| Document id（UUID v3 / MD5） | `core.rag.model.Document` | 已完成 | `src/hash.rs` 自实现，未新增第三方 crate |
| 统一错误类型 | `EmbeddingException` / `VectorStoreException` / `ReaderException` / `Dify*Exception` | 已完成 | `src/error.rs` |
| HTTP 重试 | `DifyRAGClient.RetryInterceptor` | 已完成 | `src/retry.rs` |
| **dify 全链路** | dify 子模块 11 类 | 已完成 | `src/dify/**`；请求体 / 响应 / 转换 / 适配器 / 错误全覆盖 |
| **ragflow 全链路** | ragflow 子模块 9 类 | 已完成 | `src/ragflow/**`；请求体 / 响应（`data` 多态反序列化）/ 转换 / 适配器 / 错误全覆盖 |
| **bailian 全链路** | bailian 子模块 7 类 | 已完成 | `src/bailian/**`；9 个源文件 2608 行 / 64 个内联测试；`reqwest` + 自建 ACS4-HMAC-SHA256 签名，不引入阿里云 SDK |
| **haystack 全链路** | haystack 子模块 10 类 | 已完成 | `src/haystack/**`；7 个源文件 1692 行 / 49 个内联测试 |
| **simple 离线链路** | `SimpleKnowledge` / `InMemoryStore` / `DistanceCalculator` / `TextReader` / `TextChunker` / `SplitStrategy` / `ReaderInput` / `EmbeddingModel` | 已完成 | `src/{simple,store,reader,embedding}.rs` |
| 离线单测（265 → 325 → 376 → **436** → 2026-10-31 新增 50+） | Java 测试 16792 行 | 已完成 | 265（2026-09 基线）→ 325（2026-10-26，+60）→ 376（2026-10-28 第一批，+51：`store::qdrant` 25 / `store::pgvector` 26）→ **436（2026-10-28 第二批，+60：`store::milvus` 32 / `store::elasticsearch` 28）** → 2026-10-31 新增（qdrant 2 / dify 4 / openai 5 / hook 10 / external_api 10 / simple 2 等，详见 history/RAG__MIGRATION_HISTORY.md）；HTTP 仍走注入式 mock transport，PgVector 走 `PgExecutor` 假实现，全离线 |

### 3.2 缺口（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| Bailian 集成 | `bailian.*` 7 类 | **已完成** | `src/bailian/**`；`reqwest` 直连 + 自建 ACS4-HMAC-SHA256 签名（不引入阿里云 SDK）；端点/路径/API 版本/请求体(camelCase)/响应体(PascalCase)/`Authorization` 均以 SDK jar 逆向并用金标向量钉住 |
| HayStack 集成 | `haystack.*` 10 类 | **已完成** | `src/haystack/**`；复用 `crate::transport` seam；覆盖 `documents` 与 `context_documents`（SentenceWindow）两种形态 |
| RAGFlow 集成 | `ragflow.*` 9 类 | **已完成** | `src/ragflow/**`；`POST /api/v1/retrieval`；`data` 字段可 `false`/`null`/object 的自定义反序列化已复刻（`ResponseData`） |
| simple 外部向量库 | `ElasticsearchStore`(526) / `MilvusStore`(919) / `PgVectorStore`(979) / `QdrantStore`(996) | **已完成（4/4）** | `src/store/qdrant.rs`（REST over `crate::transport`，25 测试）+ `src/store/pgvector.rs`（SQL 纯函数 + `PgExecutor` seam + `tokio-postgres` 薄适配，26 测试）+ `src/store/milvus.rs`（Milvus REST v2 over `crate::transport`，32 测试）+ `src/store/elasticsearch.rs`（ES REST：`_bulk`/kNN `_search`/`_doc` 删除/索引映射，28 测试）；均实现 `store::VdbStore`（`add`/`search`/`delete`），全离线 mock。详见 history/RAG__MIGRATION_HISTORY.md / 见 history/RAG__MIGRATION_HISTORY.md |
| embedding HTTP 提供商 | `DashScopeTextEmbedding` / `DashScopeMultiModalEmbedding` / `OllamaTextEmbedding` / `OpenAITextEmbedding` | **已完成** | `src/embedding/{dashscope,ollama,openai}.rs` + 共享 `utils.rs`；均走 `crate::transport` seam，离线 mock 覆盖（3 个提供商共 35 个测试，另 10 个共享 utils 测试）；超时/重试统一由 `EmbeddingExecutionConfig` 负责（transport 侧 `max_retries=0`，避免双重重试） |

### 3.3 增强（P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| 文档读取器（Tika/PDF/Word） | `TikaReader` / `PDFReader` / `WordReader` | **部分完成（2026-10-31 RAG-09 改判）** | **依据（原判「不迁移」）**：这三个 reader 直接依赖 JVM 库（Apache Tika / PDFBox / POI）及其文档模型，Java 实现无法机械直译；强行引入等价 Rust 库（如 `pdf-extract`/`lopdf`/`docx-rs`）会带来新的重依赖与解析语义差异。**2026-10-31 改判**：`PDFReader`/`WordReader` 的**读取语义**不必依赖库——以 `Reader` + 提取 seam 落地（`src/reader/pdf.rs`、`src/reader/docx.rs`），提取实现可注入、可选 feature 或无三方依赖外部命令，feature 关闭时报 `RagError::Reader`；`TikaReader`（MIME 自动探测 + XHTML 正文抽取）仍判不迁移，走 `ExternalApiReader` 外部解析服务。`TableFormat` 随 `WordReader` 一并落地为纯文本抽取（表格降为纯文本行）。 |
| `ImageReader` | `ImageReader`（图片 → `ImageBlock`/`URLSource`，`enableOCR` 占位不实现 OCR） | 已完成 | `src/reader/image.rs`：`ImageReader::new(enable_ocr)` / `is_ocr_enabled()` / `SUPPORTED_FORMATS`（7 项，与 Java 一致）；内容为 `ContentBlock::Image(ImageBlock{ URLSource })`，URL 保持原样、本地路径添 `file://`；docId = `generate_doc_id_md5(path)`、chunkId `"0"`；**输入取路径而非文件内容**（Rust `ReaderInput::as_string` 会读文件内容，这里显式用路径，见 history/RAG__MIGRATION_HISTORY.md 差异 2）；8 个测试 |
| `ExternalApiReader` | `ExternalApiReader` | **已完成（2026-10-31）** | `src/reader/external_api.rs`：Java 确实有契约（`RequestBuilder` + `ResponseParser` 两个函数式接口），但契约由**调用方注入**、不绑定任何厂商，因此不用“无 wire 契约”裁定，而是直接以 trait seam 迁移：`ExternalApiRequestBuilder::build_request(&str) -> HttpRequest`、`ExternalApiResponseParser::parse_response(&HttpResponse, &dyn HttpTransport)`（后者保留 Java 用 `OkHttpClient` 轮询异步任务的能力）。默认值完全对齐（chunk 512 / PARAGRAPH / 50、connect 10s、read 5min、retries 3、delay 2s、supportedFormats 空 = 任意格式）；docId = SHA-256(path)、chunkId 序号。显式差异：无 `writeTimeout`（请求体为内存 `String`）、`Thread.sleep` → `tokio::time::sleep`、最终错误消息附上最后一次原因（`RagError` 无 source 链）。10 个离线测试 |
| reader 辅助 | `ReaderUtils`（MD5/SHA-256 docId + hex） | 已完成 | `src/reader/utils.rs`：`generate_doc_id(input, "MD5" 或 "SHA-256")`、`generate_doc_id_md5`、`generate_doc_id_sha256`、`bytes_to_hex`、`DocIdAlgorithm`（未知算法 → `InvalidArgument("<algo> algorithm not available")`，与 Java 同文案）；SHA-256 新增 `hash::sha256_hex`（复用已有 `sha2`，**未新增依赖**）；6 个测试（含独立算得的 MD5/SHA-256 金标向量） |
| `TableFormat` | `TableFormat`（Word 表格 Markdown/JSON） | 设计决定（不迁移） | **依据**：`TableFormat` **仅**被未迁移的 `WordReader` 使用，无其它调用方；“不迁移”结论与 WordReader 一致。 |
| embedding 辅助 | `EmbeddingUtils`（超时/重试包装） | 已完成 | `src/embedding/utils.rs::EmbeddingExecutionConfig`（`timeout` + `retry: RetryPolicy`）提供 `apply(model, provider, operation_type, op)`：每次尝试套 `tokio::time::timeout`，重试复用 `retry_async`（仅 transport / 429 / 5xx）；工厂 `build_transport` 强制 `max_retries=0` 以免与 transport 自身重试叠加；10 个测试 |
| embedding 用量 DTO | `EmbeddingResponse` / `EmbeddingUsage`（token 用量 + 客户端耗时） | 已完成（**字段集有差异，见 history/RAG__MIGRATION_HISTORY.md**） | `embedding::{EmbeddingUsage, EmbeddingResponse, UsageListener}` + trait 默认方法 `embed_with_usage`（本地模型无需改）；各提供商从 wire 的 `usage` / `prompt_eval_count` 解析，`EmbeddingResponse.elapsed` 记录客户端耗时；`usage_listener` 在 builder 上可选注册。**见 history/RAG__MIGRATION_HISTORY.md 勘误 1 已更正（2026-10-26）**：Java **确有** `EmbeddingUsage`/`EmbeddingResponse`/`SimpleKnowledge.usageListener`（`5bfb36b44`，2026-10-20，已在旧基线内）；原「Rust 侧增值」表述作废 |

## 3.5 2026-10-31 全量对齐复核清单（唯一编号）

> 复核基线：Java `main` @ `1524b3191`；Rust 工作树 HEAD 见仓库。本表为本轮逐文件核对结论，**存量 §3.1–§3.3 历史清单保持原样未重新编号**。
> 编号规则：`RAG-NN`（缺口，状态列取值 `待办` / `进行中` / `已完成` / `设计决定（不迁移）` / `待裁定`）；§4.3 差异另有 `RAG-DNN` / `RAG-XNN` 系列。编号本模块内唯一、单调递增，不复用历史编号。
> **2026-10-31 裁定（汇总方已确认）**：本文件的 `待裁定` 项已全部裁定，状态列更新为 `待办` / `设计决定（不迁移）` / `已完成`；裁定结论、依据与执行批次见 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` §3.3。
> 正文其它位置出现的旧「待裁定 N 项」为复核时点快照，一律以 §3.5/§3.6 表格的状态列为准。
> 测试口径：只引用 §1.1/§1.2 已实测 **469 passed / 0 failed**；本轮为只读核对，未运行 cargo（源码 `#[test]`/`#[tokio::test]` 计数同为 469，与实测一致）。

### 3.5.1 缺口与待裁定

| 编号 | 优先级 | 缺口项（可执行描述） | Java 参考（文件#符号） | Rust 现状（文件:行 / rg 零命中） | 执行状态 | 下一动作 |
|---|---|---|---|---|---|---|
| RAG-01 | P1 | RAGFlow 与 HayStack 的 **429/5xx 重试实际不生效**：`crate::transport::ReqwestTransport::execute` 对非 2xx 返回 `Ok(HttpResponse)`，而 `retry_async` 只在 `Err` 时重试；两 client 在**重试包裹之外**才把状态码转错误，故 `max_retries` 只对传输层失败（DNS/连接/超时/空体）生效。（对照：Dify 的 `ReqwestDifyTransport` 在闭包内把非 2xx 转 `Err`、注释明写 429/5xx are retried，重试正常；Bailian 的 Java 模块**本无**任何 retry 代码，故非缺口。） | `RAGFlowClient.java#RetryInterceptor`（L306-360，429/5xx 指数退避）、`HayStackClient.java#RetryInterceptor`（L282-330）；Dify 对照 `DifyRAGClient.java:370` | `src/transport.rs:226-231`（不论状态码一律 `Ok(HttpResponse)`）；`src/retry.rs:69-77`（`is_retryable_error` 仅 `Http`/`Io`/`Api{429,>=500}`）；`src/retry.rs:45-64`（仅 `Err` 重试）；RAGFlow `src/ragflow/client.rs:105-109`（execute 之后才 `map_status_error`）；HayStack `src/haystack/client.rs:101-130` | 待办 | 在 `ReqwestTransport::execute` 内只把 **429/5xx** 转 `Err(map_http_error(...))`（4xx 保持 `Ok` 以维持 HayStack「状态码仅记录、body 优先解析」的 Java 语义），使 `retry_async` 生效；验收：RAGFlow 与 HayStack 各补「429 后重试成功」「400 只请求 1 次」用例 |
| RAG-02 | P1 | `DifyRAGConfig` 缺 4 项参数校验（`topK >= 1`、`0 <= scoreThreshold <= 1`、`0 <= weights <= 1`、`maxRetries >= 0`），非法配置会被静默接受并发出畸形请求 | `DifyRAGConfig.java#Builder.topK`（L375-379）、`#scoreThreshold`（L393-397）、`#weights`（L443-447）、`#maxRetries`（L500-505） | `src/dify/config.rs:328-364`（`build()` 只校验 apiKey / datasetId；三个 setter 无校验，`max_retries` 为 `u32` 已由类型保证非负） | 待办 | 在 builder setter 内按 Java 文案返回 `InvalidArgument`（或集中在 `build()`）；验收 3 个新用例断言错误消息逐字一致 |
| RAG-03 | P1 | **agentic 模式缺工具**：`RagMode::Agentic` 已定义，但 crate 内没有 `retrieve_knowledge` 工具的 `impl Tool`，只有文本格式化函数，agent 无法按 Java 方式自主检索 | `KnowledgeRetrievalTools.java#retrieveKnowledge`（L125-176，`@Tool(name="retrieve_knowledge")`，limit 默认 5，从 agent state 取 conversationHistory，成功/失败分别返回 `ToolResultBlock.success/error`） | `src/message.rs:30 format_documents_for_tool`（纯格式化，无 `impl Tool`）；`rg 'retrieve_knowledge' crates/` **零命中** | 待办 | 新增 `KnowledgeRetrievalTool` 实现 `agentscope_core::tool::Tool`（`crates/agentscope-core/src/tool/tool.rs:56`；可参照 `crates/agentscope-extensions-judge/src/jev/runtime/tools.rs:239` 的 `impl Tool for KnowledgeSearchTool`），`call` 用 `ctx.runtime.memory` 填 `RetrieveConfig.conversation_history`；验收：成功/检索失败 2 用例 |
| RAG-04 | P2 | 重试退避基数与上限与 Java 不一致（Java 1s/2s/4s，RAGFlow 另有 10s 上限；Rust 200/400/800ms 且无上限） | `RAGFlowClient.java:356-357`（`Math.pow(2, attempt) * 1000`，`min(..., 10000)`）、`DifyRAGClient.java:408,424` | `src/retry.rs:26`（`base_delay: 200ms`）、`src/retry.rs:58`（`base_delay * 2^attempt`，无 cap） | 待办 | `RetryPolicy` 增 `max_delay`，默认 `base_delay=1s`；RAGFlow 设 10s；验收：退避序列与 cap 断言 |
| RAG-05 | P2 | `InMemoryStore::search` 对 `limit == 0` 静默返回空列表（Java 抛 `IllegalArgumentException("Limit must be positive")`） | `InMemoryStore.java#search`（L119-136，L132 抛 `Limit must be positive`） | `src/store.rs:206-247`（仅 `scored.truncate(query.limit)`，无 limit 校验） | 待办 | `limit == 0` 返回 `InvalidArgument`；补 1 用例 |
| RAG-06 | P2 | `QdrantConfig.check_compatibility` 是空转旋钮：字段可设、可读，但 `connect_with`/`ensure_collection`/请求构造均未使用，与 Java（构建 gRPC 客户端时启用兼容性检查）行为不等价 | `QdrantStore.java:149-155`（`QdrantGrpcClient.newBuilder(host, port, tls, checkCompatibility)`） | `src/store/qdrant.rs:89,125,193,232` 仅存取；`rg 'check_compatibility'` 无第三处读取 | 待办 | 二选一：`connect_with` 内 `GET /` 取版本按开关判定（不兼容 → `VectorStore` 错误），或删除该字段并注明 REST 不检查 |
| RAG-07 | P2 | `src/message.rs` 模块注释与现状冲突（称 core `RagMode`/hook 未迁移，实际 `src/hook.rs` 已完整落地并被 `lib.rs:84` re-export） | 无（Rust 内部文档漂移） | `src/message.rs:9-11`（「core Rust `RagMode`/hook layer has not been migrated」）vs `src/hook.rs:36`（`pub enum RagMode`）、`src/hook.rs:186`（`Middleware::order`） | 待办 | 改注释指向 `crate::hook`，无行为变更 |
| RAG-08 | P2 | `TikaReader` / `PDFReader` / `WordReader` / `TableFormat` 未迁移（**设计决定**，理由与源码级依据见 §4.3.1 D01–D03） | `TikaReader.java`、`PDFReader.java`、`WordReader.java`、`TableFormat.java` | 全 crate 无对应符号（`rg -i 'Pdf|WordReader|TikaReader' crates/` 仅 `src/reader.rs:6` 注释）；Cargo.toml 无 PDF/DOCX/Tika 依赖 | 部分完成（2026-10-31，PDF/DOCX 见 RAG-09） | 保留 `reader::Reader` trait 作为扩展点；`PdfReader`/`DocxReader` 已落地（提取 seam + 可选 feature），Tika 仍走 `ExternalApiReader`（外部解析服务） |
| RAG-09 | P1 | 裁定 **C1=A（引入 liteparse 做 RAG 文档解析）未落地**，且与 §3.3「Tika/PDF/Word 不迁移」口径互相矛盾；当前 PDF/Word 摄取能力为零 | `EXTENSIONS_GAP_TRACKER.md:68`（C1=A 及其「落地口径」） | `rg -i 'liteparse' .` 仅 `docs/extensions/EXTENSIONS_GAP_TRACKER.md:68` 与 `memory/2026-10-19.md` 命中；`rg -i 'liteparse|pdf-extract|lopdf|docx|tika' Cargo.toml crates/agentscope-extensions-rag/Cargo.toml` 零命中；`Cargo.lock` 无 liteparse | 已完成 | 裁定会：① 采纳 C1 → 新增 `src/reader/liteparse.rs`（或 liteparse 服务适配）覆盖 PDF/DOCX，并把 §3.3 改「已完成」；② 撤销 C1 → 在汇总 tracker 标注撤回，保留 §4.3.1 D01–D03 【裁定 2026-10-31：撤销 C1=A（不引入 liteparse）：改 Reader seam + 可选 feature（pdf/docx）+ ExternalApiReader 接外部服务】【2026-10-31 落地：`Reader` seam 落地——`reader::input_as_bytes` + `input_path_bytes`（二进制入口），`src/reader/pdf.rs` / `src/reader/docx.rs` 各自暴露 `PdfTextExtractor` / `DocxTextExtractor` 注入点与可选 `pdf` / `docx` cargo feature（默认关，开时提供无三方依赖的 `pdftotext` / `pandoc` 提取器，`FEATURE_DISABLED_MESSAGE` 文案指明 feature 与退路）；`ExternalApiReader::for_parsing_service(url)` = `BodyAsMarkdown` + `supported_formats(["pdf","docx"])` + 可注册 `HttpTransport`，调用方可完全避开本地解析器；扫描件/OCR 明确不在范围内。用例：`reader::pdf::tests::a_build_without_the_feature_reports_the_feature_and_the_alternative`、`reader::docx::tests::a_build_without_the_feature_reports_the_feature_and_the_alternative`、`reader::external_api::tests::{supported_formats_are_configurable, the_parsing_service_reader_advertises_pdf_and_docx_and_chunks_the_reply, the_parsing_service_reader_still_requires_an_existing_file, the_parsing_service_request_is_json_with_the_file_path}`；未引入任何第三方 PDF/DOCX 库】 |
| RAG-10 | P1 | **embedding 提供商归属与 MODEL 边界待裁定**：RAG 自带 `EmbeddingModel` trait + 4 个提供商，MODEL 侧另有 Ollama `embed`，两 crate 互不依赖，存在双实现漂移风险（§4.2 第 5 条已按「沿用 Java 分属」在本 crate 内落地，但该归属**未在汇总/跨 crate 层面正式裁定**，见裁定会） | Java 侧 `io.agentscope.core.embedding.*` 物理位于 `agentscope-extensions-rag-simple` 子模块（`EmbeddingModel.java` 等 7 类） | RAG：`src/embedding.rs:150`（trait）、`src/embedding/{openai,dashscope,ollama}.rs`；MODEL：`crates/agentscope-extensions-model/src/ollama/model.rs:115`（`pub async fn embed`）；依赖关系：两 crate 的 `Cargo.toml` 互不引用 | 已完成 | 裁定 A=维持现状（RAG 自持，wire 契约各自测试）；B=MODEL 提供 `EmbeddingModel` trait，RAG 依赖并删重复实现。裁定后同步 §4.2 第 5 条（跨模块需求）与汇总 tracker 【裁定 2026-10-31：A：维持 RAG 自持 EmbeddingModel，加 rag 对 model 的 wire 契约测试挡漂移】【2026-10-31 落地：新增集成测试 `tests/embedding_wire_contract.rs`（dev-dependency 指向 workspace 成员 `agentscope-extensions-model`，不引入新三方 crate），把 4 条 wire 形状钉死并与 MODEL 侧 DTO 交叉验证：Ollama `POST /api/embed` 请求体 = `{"input":["hello"],"model":"nomic-embed-text"}`（且 `OllamaEmbeddingRequest` 反序列化后 `to_value` 回原样，无多余键）、响应体 `embeddings`(f32)+`prompt_eval_count` 两边读出同一向量与同一 token 数、OpenAI `/embeddings` 请求体恰 4 键（`model`/`input`/`dimensions`/`encoding_format`）与响应封装、以及错形状响应（OpenAI 封装给 Ollama 客户端）必须报错而非误读。用例名：`the_ollama_embedding_request_is_the_contract_the_model_extension_reads`、`the_ollama_embedding_request_targets_api_embed_on_the_shared_endpoint`、`both_crates_read_the_same_ollama_embedding_response`、`the_openai_embedding_request_is_the_pinned_contract`、`the_openai_embedding_response_envelope_is_the_pinned_contract`、`a_provider_reply_of_the_other_shape_is_rejected_rather_than_misread`】 |
| RAG-22 | P2 | Qdrant 无 scheme location 的**端口不按协议重映射**，沿用 Java 配置会静默打错端口：Java 的 `127.0.0.1:6334` 是 gRPC 端口（Java 还会把 6333 自增为 6334），Rust REST 原样生成 `http://127.0.0.1:6334`，HTTP 请求打到 gRPC 端口。当前端口保留是**有意设计并被测试锁定**（`store/qdrant.rs:872-890` 注释：The REST port keeps the explicit port as-is），故本条只针对「迁移用户无提示」改进 | `QdrantStore.java:801-846`（`parseLocation`：gRPC 默认 6334，端口自增 6333→6334） | `src/store/qdrant.rs:53 DEFAULT_HTTP_PORT = 6333`、`247-273 parse_rest_base_url`（显式端口原样保留）；测试 `parse_base_url_accepts_scheme_less_ip_locations` 断言 `127.0.0.1:6334` → `http://127.0.0.1:6334` | 待办 | 不改映射语义，仅补提示：`connect_with` 连接失败且端口为 6334 时，错误消息追加「Qdrant Java gRPC 端口 6334 不受 REST 支持，请改用 6333」；验收：1 个用例断言错误消息含 6333 提示 |

> 编号说明：`RAG-22` 为 §3.5.1 内续排的最新缺口编号（11–21 属 §3.5.2 已对齐抽样），编号全局唯一、按分配先后递增。

### 3.5.2 本轮复核确认已对齐（抽样，不求穷举）

| 编号 | 能力 | Java 参考 | Rust 证据（文件:行 + 测试名） | 执行状态 |
|---|---|---|---|---|
| RAG-11 | Qdrant **无 scheme 的 IP/IPv6 location 解析**（`2a682f8bf`）已等价处理：无 `://` 按 authority 解析、保留显式端口、裸 `::1`/`2001:db8::1` 自动加方括号 | `QdrantStore.java#parseLocation`（L801-846） | `src/store/qdrant.rs:247 parse_rest_base_url`、`279 split_authority`；测试 `store::qdrant::tests::parse_base_url_accepts_scheme_less_ip_locations`（含 `127.0.0.1:6334`、`[::1]:6334`、裸 `::1`、`2001:db8::1`）、`builder_accepts_a_scheme_less_ip_location`（端口语义差异另见 RAG-22） | 已完成 |
| RAG-12 | Dify 响应 `query` 兼容裸字符串与对象两种形态（`d08c40c00`），且两种形态都不丢 `records` | `model/DifyQueryDeserializer.java` | `src/dify/model.rs:57 deserialize_query`；测试 `dify::model::tests::bare_string_query_keeps_every_record`、`object_query_is_unchanged_and_records_are_empty` | 已完成 |
| RAG-13 | OpenAI embedding 客户端复用 + `close()`（`07c5f3078`）：关闭后立即失败且不进入重试管道，关闭前创建的 future 首次 poll 也会失败 | `OpenAITextEmbedding.java#getClient/close`（L228-270） | `src/embedding/openai.rs:99 close`、`104 is_closed`、`163 ensure_open`、`226 trait close`；测试 `close_is_idempotent_and_rejects_later_embeddings`、`closed_model_bypasses_the_retry_pipeline`、`future_created_before_close_is_rejected`、`trait_object_close_marks_the_model_closed` | 已完成 |
| RAG-14 | Milvus L2 距离归一化 `1/(1+distance)`，阈值在归一化后比较（`97696a3a2`） | `MilvusStore.java#normalizeScore` | `src/store/milvus.rs:132 normalize_score`、`668`（search 时归一化后再过滤阈值）；测试 `normalize_score_only_touches_l2`、`search_normalizes_l2_distances`、`search_filters_below_the_score_threshold` | 已完成 |
| RAG-15 | RAGFlow `rerank_id` 为字符串（`643905e6b`）+ 重试耗尽后错误体保留（`6b535067a`） | `RAGFlowConfig#rerankId`、`RAGFlowClient`（末次响应体） | `src/ragflow/config.rs:59 pub rerank_id: Option<String>`；`src/ragflow/client.rs:132-158 map_status_error(status_code, body)` 把响应体拼入消息 | 已完成 |
| RAG-16 | 检索结果统一「阈值过滤（含边界）→ 降序稳定排序 → limit」 | `SimpleKnowledge#retrieve`、`DifyKnowledge#retrieve`、`BailianKnowledge#retrieve`、`HayStackKnowledge#retrieve` | `src/knowledge.rs:35 filter_and_rank`；测试 `knowledge::tests::{filter_drops_below_threshold, filter_drops_missing_scores, filter_sorts_descending_and_truncates, filter_includes_threshold_boundary}` | 已完成 |
| RAG-17 | 远端知识库只读语义：`add_documents` 返回 `Unsupported` 且文案与 Java `UnsupportedOperationException` 一致（无文档 CRUD，与 Java 相同） | `DifyKnowledge.java:125`、`RAGFlowKnowledge.java:182`、`BailianKnowledge.java:131`、`HayStackKnowledge.java:109` | `src/{dify,ragflow,bailian,haystack}/knowledge.rs`；测试 `dify::knowledge::tests::add_documents_is_unsupported_like_java`、`ragflow::knowledge::tests::add_documents_is_unsupported`、`bailian::knowledge::tests::add_documents_is_unsupported_with_the_java_message`、`haystack::knowledge::tests::add_documents_is_unsupported_with_the_java_message` | 已完成 |
| RAG-18 | 检索旋钮透传与省略语义：RAGFlow `page/page_size/top_k/similarity_threshold/vector_similarity_weight/use_kg/toc_enhance/rerank_id/keyword/highlight/cross_languages/metadata_condition`、Dify `retrieval_model`（含 rerank/weights/metadata 过滤）、HayStack 全部可选字段 | `RAGFlowClient.java:110-195`、`DifyRAGClient.java:131-211`、`HayStackClient.java:135-205` | `src/ragflow/request.rs:16 build_retrieval_request_body`、`src/dify/request.rs:16 build_retrieve_request_body`、`src/haystack/request.rs:17 build_retrieve_request_body`（均按 Java `if (x != null) put` 的相同省略规则构造）；分页仅透传 `page/page_size`（Java 同样不做客户端翻页） | 已完成 |
| RAG-19 | 错误分类：401/403 → `Auth`，其余（含 429/5xx、业务 code 非 0）→ `Api` 且保留 status/error_code | `DifyAuthException`/`DifyApiException`、`RAGFlowAuthException`/`RAGFlowApiException`、`HayStackApiException` | `src/error.rs:15-66`、`src/transport.rs:263 map_http_error`、`src/ragflow/client.rs:132`；测试 `transport::tests::map_http_error_splits_auth_from_api`、`dify::client::tests::auth_error_is_typed`、`error_mapping_handles_forbidden_as_auth` | 已完成 |
| RAG-20 | 分块算法与默认值：`CHARACTER`/`PARAGRAPH`/`TOKEN`（4 字符≈1 token）/`SEMANTIC`（回退段落），默认 512 / PARAGRAPH / 50，空串返回 `[""]` | `TextChunker.java#chunkText`、`TextReader.java`（L57-60 默认值） | `src/reader.rs:137 chunk_text`、`165 chunk_by_character`、`187 chunk_by_paragraph`、`32 CHARS_PER_TOKEN = 4`；测试 `empty_text_yields_single_empty_chunk`、`character_chunking_applies_overlap`、`token_chunking_uses_four_chars_per_token`、`validation_rejects_bad_parameters` | 已完成 |
| RAG-21 | `SimpleKnowledge` 用量回调（`5bfb36b44`）：每次 embed 后回调，add 每文档一次、retrieve 查询一次 | `SimpleKnowledge.java#notifyUsage` | `src/simple.rs:46 with_usage_listener`、`67 notify_usage`、`87`（add 循环内回调）、`104`（retrieve 回调）；测试 `simple::tests::usage_listener_receives_add_and_retrieve_usage`、`no_usage_listener_is_a_no_op` | 已完成 |

## 4. 项目状态与下一步

### 4.1 当前状态
- **已完成**：crate 骨架、公共 RAG 模型/trait、消息桥接、Dify 全链路、simple 离线链路（`SimpleKnowledge` + `InMemoryStore` + `TextReader`/`TextChunker` + `HashEmbeddingModel` + 距离计算）、RAGFlow 全链路、**Bailian 全链路（含自建 ACS4-HMAC-SHA256 签名）**、**HayStack 全链路**、**外部向量库 4/4（Qdrant/PgVector/Milvus/Elasticsearch）**；全 crate 共 **53 个源文件**。
- **2026-10-26 本轮完成**：`simple` 的 HTTP embedding 4 件（`OpenAiTextEmbedding` / `DashScopeTextEmbedding` / `DashScopeMultiModalEmbedding` / `OllamaTextEmbedding`）、`EmbeddingUtils`→`EmbeddingExecutionConfig`、`EmbeddingUsage`/`EmbeddingResponse`/`UsageListener`、`ReaderUtils`（MD5+SHA-256）、`ImageReader`；新增 7 个源文件 2437 行非空 / 60 个离线测试。详见 history/RAG__MIGRATION_HISTORY.md。
- **2026-10-28 第一批完成**：外部向量库 2 件——`QdrantStore`（REST over `crate::transport`）与 `PgVectorStore`（pgvector SQL 纯函数 + `PgExecutor` seam + `tokio-postgres` 薄适配）；新增 2 个源文件 2742 行非空 / **51 个离线测试**（qdrant 25 / pgvector 26）；总源文件 49 → **51**。详见 history/RAG__MIGRATION_HISTORY.md。
- **2026-10-28 第二批完成**：外部向量库剩下 2 件——`MilvusStore`（Milvus **REST v2** over `crate::transport`）与 `ElasticsearchStore`（ES REST：`_bulk` / kNN `_search` / `_doc` 删除 / 索引映射）；新增 2 个源文件 2845 行非空 / **60 个离线测试**（milvus 32 / elasticsearch 28）；总源文件 51 → **53**，外部向量库 **4/4 完成**。详见 history/RAG__MIGRATION_HISTORY.md。
- **2026-10-31 loop 完成**：① 3 个 Java 增量——Qdrant 无 scheme IP/IPv6 location（`2a682f8bf`）、Dify 裸字符串 `query`（`d08c40c00`）、OpenAI embedding client 复用+关闭（`07c5f3078`）；② `RAGMode`/`GenericRAGHook`（`src/hook.rs`，含 core `Middleware` 适配）；③ `ExternalApiReader`（`src/reader/external_api.rs`）；④ `SimpleKnowledge` usage listener 接线 + `EmbeddingUsage.time_seconds`。总源文件 53 → **55**。详见 history/RAG__MIGRATION_HISTORY.md。
- **验证命令与结果**（2026-10-28 第二批实测）：
  - `cargo build -p agentscope-extensions-rag` → **通过（0 errors，exit 0）**
  - `cargo test -p agentscope-extensions-rag --lib` → **436 passed / 0 failed / 0 ignored**（基线 376 → +60）
  - `cargo clippy -p agentscope-extensions-rag --lib --all-targets` → **exit 0**，新增文件无告警（仅剩 `ragflow` 的 2 条旧告警）
  - `rustfmt --edition 2021 --check`（`store/{milvus,elasticsearch}.rs`、`store.rs`、`lib.rs`）→ 无 diff
- **已知问题**：无编译/测试失败。2026-10-31 起 core `RagMode`/`GenericRAGHook` 已有本 crate 内对应（`src/hook.rs`，以 core `Middleware::on_reasoning` 承接，**未改 core/harness**）；原「无未关闭缺口」仅对应 §3.1–§3.3 口径。**2026-10-31 全量对齐复核新增清单见 §3.5（RAG-01…RAG-10、RAG-22）与 §4.3（D01–D06 / X01–X10）；其中 RAG-09 / RAG-10 已于同日按 §3.3 裁定落地（见对应行【2026-10-31 落地】）。**
- **2026-10-22 静态复核**：未运行 `cargo build`/`cargo test`（本轮为纯静态核对），仅校验源码与统计口径；详见 history/RAG__MIGRATION_HISTORY.md。

### 4.2 下一步与阻塞
1. **simple 外部向量库（P1）**：**4/4 已完成**（`QdrantStore` / `PgVectorStore` 2026-10-28 第一批；`MilvusStore` / `ElasticsearchStore` 2026-10-28 第二批）——均实现 `store::VdbStore`，HTTP 走 `crate::transport` seam + 离线 mock（见 history/RAG__MIGRATION_HISTORY.md / 见 history/RAG__MIGRATION_HISTORY.md）。**本项已关闭。**
2. **`ExternalApiReader`（P2）**：**已完成（2026-10-31）** — `src/reader/external_api.rs`（trait seam 注入请求/响应，HTTP 走 `crate::transport`，10 个离线测试）。**本项已关闭：**
3. **已裁定不迁移**：`TikaReader`/`PDFReader`/`WordReader`、`TableFormat` → 状态口径为 **已完成（设计决定，不迁移）**（理由见 §3.3）。
4. **待裁定事项（2026-10-31 已全部裁定并落地）**
   - `RagMode` / `GenericRAGHook` 落点 → **裁定 A：留在本 crate**（`src/hook.rs`），以 core 已公开的 `Middleware::on_reasoning` 作 seam；不回填 core，避免为已 `@Deprecated(forRemoval)` 的 Java `rag` 包新增 core 抽象。
   - `SimpleKnowledge` 是否接 `usage_listener` → **已接线**：新增 `with_usage_listener` / `usage_listener()`，`add_documents`（每文档一次）与 `retrieve`（查询一次）均回调，与 Java 一致（`simple::tests::usage_listener_receives_add_and_retrieve_usage`）。
5. **跨模块需求**
   - ~~若需在 core/harness 增加 RAG hook 或 embedding 抽象~~ → **2026-10-31 不再需要**：hook 以 core `Middleware` 实现、`EmbeddingModel::close` 为本 crate trait 默认方法，本轮**未改 core/harness**。
   - 若 embedding 提供商确定归 MODEL 扩展，本 crate 仅依赖其 trait，不重复实现。**→ 2026-10-26 裁定（落地）：沿用 Java 分属——提供商留在 RAG 扩展（`src/embedding/*`），否则 `SimpleKnowledge` 无法自包含；MODEL 扩展的 `ollama` 只保留 chat/通用 `embed`，两边 wire 契约一致但不共享代码（本 crate 不依赖 `agentscope-extensions-model`）。**
6. **Bailian 不引入阿里云 SDK（本轮裁定，已落地）**
   - **背景**：Java `BailianClient` 把检索委托给 `com.aliyun.bailian20231229` SDK（Tea/OpenAPI 体系，经 `com.aliyun.gateway.pop.Client` 以 `authType=AK` / `style=ROA` / ACS4-HMAC-SHA256 签名）。
   - **裁定**：不将阿里云 SDK（Java 生态、依赖树庞大）引入 Rust workspace；本 crate 用 `reqwest` 直连并**自建签名**（`src/bailian/signing.rs`，仅依赖已在 `Cargo.lock` 的 `hmac`/`sha2`）。
   - **保真度保障**：端点(`bailian.cn-beijing.aliyuncs.com`)、路径(`/{workspaceId}/index/retrieve`)、API 版本/动作(`x-acs-version=2023-12-29`/`x-acs-action=Retrieve`)、请求体键(camelCase)、响应体键(PascalCase)、region 推导、`x-acs-date` 格式、`Authorization` 格式均已从 SDK jar（2.8.2 + gateway-pop 0.0.6）字节码逆向；并用**真实 SDK 生成的金标向量**（`signing_key`/`signature`/`Authorization`/`region`）写成单测钉住。
   - **已知取舍**：不实现 `ACS4-HMAC-SM3`（国密）分支与 `Bearer`/`Anonymous` 鉴权分支（Java `BailianClient` 仅使用 `AK`），需要时可扩展。
   - **新增依赖**：`hmac = "0.12"`、`sha2 = "0.10"` 已写入本 crate `[dependencies]`（未改根 `[workspace.dependencies]`）；两者均已在仓库 `Cargo.lock` 中存在（`sha2` 与 workspace 同版本）。

## 4.3 已知差异

> 说明：本节用于记录**不构成迁移缺口**的差异——或 Rust 侧设计决定（不迁移）、或是 Rust 更安全/更规范的实现。Java 是功能基线，但**不因 Rust 不同即判缺口**。编号 `RAG-DNN`（设计决定）/ `RAG-XNN`（实现差异），与 §3.5 的 `RAG-NN` 不交叉复用。

#### 4.3.1 设计决定（不迁移）

| 编号 | 能力 | Java 参考（源码级依据） | 不迁移的源码级依据 | 影响/替代路径 |
|---|---|---|---|---|
| RAG-D01 | `TikaReader`（通用文档解析：PDF/DOCX/HTML/…，MIME 自动探测 + XHTML 正文抽取） | `agentscope-extensions-rag-simple/src/main/java/io/agentscope/core/rag/reader/TikaReader.java:34-46` 直接 import `org.apache.tika.detect.DefaultDetector`、`org.apache.tika.parser.AutoDetectParser`、`org.apache.tika.sax.BodyContentHandler` 等 JVM 库 | `crates/agentscope-extensions-rag/src/reader.rs:6` 头注释即声明「Java `TikaReader`/`PDFReader`/`WordReader` depend on JVM libraries」；全仓 `rg -i 'liteparse|tika'` 在 `Cargo.toml`/`Cargo.lock` 零命中（仅文档与 memory 提及） | 无等价 Rust 库且 Apache Tika 无官方 Rust 移植；保留 `reader::Reader` trait，需要时实现之或接外部解析服务（`ExternalApiReader` 已支持任意 HTTP 解析器） |
| RAG-D02 | `PDFReader`（PDFBox 抽文本，分页 → chunk） | `.../reader/PDFReader.java:29-31` import `org.apache.pdfbox.Loader`、`org.apache.pdfbox.pdmodel.PDDocument`、`org.apache.pdfbox.text.PDFTextStripper` | 同上（`reader.rs:6`）；`rg -i 'pdf-extract|lopdf|pdfium' Cargo.toml crates/**/Cargo.toml` 零命中 | 同上；纯文本类 PDF 可先由调用方转文本再用 `TextReader` |
| RAG-D03 | `WordReader`（POI 解析 XWPF，段落 + 表格 → Markdown/JSON）+ `TableFormat` | `.../reader/WordReader.java:35-43` import `org.apache.poi.xwpf.usermodel.*`；`TableFormat` 仅被 `WordReader.java:72,263,549` 与其测试引用（`rg -l 'TableFormat'` 仅 3 个文件） | 依赖 Apache POI（JVM）；`TableFormat` 无独立调用方，随 `WordReader` 一并判不迁移 | 同 D01；`.docx` 属 ZIP+XML，如需自研可后续按 C1 裁定补 `docx` 解析 |
| RAG-D04 | `EmbeddingUtils.applyTimeoutAndRetryBatch` / `convertDoubleListToArray` / `sleep` 等批量辅助 | `.../embedding/EmbeddingUtils.java:183`（`applyTimeoutAndRetryBatch`） | `rg 'applyTimeoutAndRetryBatch'` Java 仓仅 `.../embedding/EmbeddingUtilsTest.java:169,205` 命中——**生产代码零调用**（死代码）；生产路径实际用 `applyTimeoutAndRetry` | Rust 已迁生产用到的 `EmbeddingExecutionConfig::apply`（`src/embedding/utils.rs:103`）；批量重试无调用方故不迁，避免写投机性代码 |
| RAG-D05 | Qdrant `:memory:` / `file://` 本地内嵌模式（gRPC localhost 内置服务） | `.../store/QdrantStore.java:801-846`（`parseLocation`：`":memory:"` / `"file://"` → `localhost:6334` 本地 gRPC） | Rust 走 REST（`src/store/qdrant.rs:29-30` 头注释已注明「no local `:memory:` mode」）；REST 协议不提供进程内存储语义，无法等价实现 | 测试改用 `MemoryTransport` 断言请求体（既有的离线测试范式）；本地开发用 `docker run qdrant/qdrant` |
| RAG-D06 | Bailian SDK 的 `ACS4-HMAC-SM3`、`Bearer`、`Anonymous` 三种鉴权分支 | `.../integration/bailian/BailianClient.java`（构造 `com.aliyun.bailian` SDK client，凭据类型由 SDK 决定） | Rust 未引入阿里云 SDK，`src/bailian/signing.rs` 只实现 `ACS4-HMAC-SHA256`（`rg -i 'sm3|anonymous' crates/agentscope-extensions-rag/src/bailian/` 零命中）；业务调用只用 AK 凭据 | 已在 §4.2 记录取舍：仅支持 AccessKey 路径；SM3/Bearer 场景由调用方在 `crate::transport` 层注入 `Authorization` 头 |

#### 4.3.2 设计决定与已知差异

> 编号 `RAG-XNN`：Rust 已实现但与 Java 行为不同（架构差异 / 有意优化 / 修复），**非缺口**。

| 编号 | 项 | Rust 实现 | 与 Java 的差异 | 为何保留（收益/依据） |
|---|---|---|---|---|
| RAG-X01 | Qdrant 传输协议（REST vs gRPC） | `src/store/qdrant.rs:53 DEFAULT_HTTP_PORT = 6333`、`247-273 parse_rest_base_url`（REST over `crate::transport`） | Java `agentscope-extensions-rag-simple/src/main/java/io/agentscope/core/rag/store/QdrantStore.java#parseLocation`（L801-846）用 `io.qdrant.client.QdrantGrpcClient`（gRPC，默认 6334，无 scheme 时 6333→6334 自增）；Rust 走 REST 6333，显式端口原样保留 | 不引入 gRPC/protobuf 工具链，与 workspace 其它扩展一致（零新依赖）；迁移代价（6334→6333 无提示）已单列 RAG-22 |
| RAG-X02 | HTTP 传输层：注入式 transport seam | `src/transport.rs:132 trait HttpTransport`、`199 ReqwestTransport`（各 client `with_transport` 注入） | Java 各 client 内建 `OkHttpClient` + `Interceptor` 直连：`agentscope-extensions-rag-ragflow/.../RAGFlowClient.java#RetryInterceptor`（L306）、`agentscope-extensions-rag-dify/.../DifyRAGClient.java#RetryInterceptor`（L370） | 全链路离线可测（469 用例零网络）、可注入观测/代理/重试；代价：状态码重试需自行装配（已单列 RAG-01） |
| RAG-X03 | `SimpleKnowledge` 写入：顺序 embed | `src/simple.rs:76-90`（`add_documents` 循环内逐条 `await`，`87` 处回调 usage） | Java `agentscope-extensions-rag-simple/.../rag/knowledge/SimpleKnowledge.java#addDocuments`（L129 区间 `Flux.flatMap` 并发 embed） | 顺序执行避免对端 429 与 embedding 服务过载，usage 回调顺序确定；大文档集吞吐低于 Java（可由调用方并发调用缓解） |
| RAG-X04 | 内存向量库检索语义：保留原文档 id + 跳过无 embedding 文档 | `src/store.rs:206-247`（`document.clone()` 保留 id，`217-222` 跳过 `embedding == None`） | Java `agentscope-extensions-rag-simple/.../rag/store/InMemoryStore.java#search`（L119-170：`new Document(doc.getMetadata())` 重建 → 新 id；无 null embedding 分支） | 检索结果可直接 `delete(doc.id())`（Java 重查得新 id 会删不掉）；半成品文档被跳过而非把 null 传给距离计算 |
| RAG-X05 | `TextReader` 的 docId：输入确定性生成 | `src/reader.rs:347`、`361 doc_id_for_input` | Java `agentscope-extensions-rag-simple/.../rag/reader/TextReader.java:97`（`UUID.randomUUID().toString()`，每次导入都不同） | 幂等重导入不产生重复文档；代价：同内容不同来源会合并（调用方可用 `ExternalApiReader` 的 path 派生 docId 规避） |
| RAG-X06 | 分块边界：按 Unicode char 切分 | `src/reader.rs:165 chunk_by_character`（`char_indices`） | Java `agentscope-extensions-rag-simple/.../rag/reader/TextChunker.java#chunkByCharacter`（L81，`String.substring` 按 UTF-16 code unit，可能切断代理对） | Rust 永不产生半个代理对/半个 UTF-8 序列；代价：emoji/非 BMP 文本的块长按码点计，与 Java 略有差异 |
| RAG-X07 | 错误模型：扁平枚举 vs 异常层次 | `src/error.rs:15-66`（`InvalidArgument`/`Embedding`/`VectorStore`/`Reader`/`Unsupported`/`Http`/`Api`/`Auth`/`Json`/`Io`）；`src/transport.rs:263 map_http_error` | Java 8 个异常子类（如 `agentscope-extensions-rag-dify/.../model/DifyApiException.java` 与 `DifyAuthException`）且向量库统一包 `VectorStoreException` | 错误可穷举匹配（不必 `instanceof`），且向量库错误直出 `Api`/`Auth` 保留 HTTP status 供上层重试/告警分流；代价：无 `source()` 链，底层细节压成字符串 |
| RAG-X08 | Dify 配置新增 `custom_headers` | `src/dify/config.rs:210`（`custom_headers: HashMap<String,String>`）+ `src/dify/client.rs:163` 注入请求头（测试 `custom_headers_are_merged`） | Java `DifyRAGConfig` 无该字段（仅 `agentscope-extensions-rag-ragflow/.../RAGFlowConfig#customHeaders`、`agentscope-extensions-rag-haystack/.../HayStackConfig#customHeaders` 有） | 自建 Dify 网关/多租户可带鉴权头；纯增量，不影响既有对齐行为 |
| RAG-X09 | `EmbeddingUsage` 字段集与序列化命名 | `src/embedding.rs:54`（`input_tokens`/`output_tokens`/`total_tokens`/`time_seconds`，serde 默认 snake_case） | Java `agentscope-extensions-rag-simple/.../embedding/EmbeddingUsage.java`（`getInputTokens`/`getTotalTokens`/`getTime`，Jackson camelCase；无 output 字段） | `output_tokens` 覆盖多模态 embedding 计费；若将来与 Java 共享该 DTO 的 JSON，需补 `serde(rename_all = "camelCase")` |
| RAG-X10 | Agentic/Generic 钩子落点：core `Middleware` | `src/hook.rs:36 pub enum RagMode`、`186 fn order -> 50`、`impl Middleware` 在 `on_reasoning` 注入知识消息（测试 `middleware_injects_the_knowledge_message_before_next`） | Java `agentscope-core/src/main/java/io/agentscope/core/rag/GenericRAGHook.java#preCall`（`Hook` 接口在 `PreCallEvent` 阶段注入），且 `RAGMode` 已被 `@Deprecated(forRemoval)` | 复用 core 既有中间件链，不为已废弃的 Java rag 包新增 core 抽象；语义等价（均在下一步 reasoning 前注入）；差异：Java 可在 `preCall` 外扩展钩子，Rust 受 `Middleware` 阶段约束 |

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-rag` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-rag` / `cargo test -p agentscope-extensions-rag` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-rag/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。


## 6. 执行历史（已归档）

- 执行记录 / 静态复核 / 勘误 / 反向验证明细：[`history/RAG__MIGRATION_HISTORY.md`](./history/RAG__MIGRATION_HISTORY.md)
- 本文件只保留现行状态（§0–§5）；历史条目一律写进上述归档文件，不再回填本文。
