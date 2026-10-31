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
| 当前状态 | **总判：已完成（无剩余条目）**（2026-10-31 清零）—— `dify`、`ragflow`、`bailian`、`haystack`、`simple` 子模块的离线链路与外部服务链路均已迁移；2026-10-26 补 `simple` 的 **HTTP embedding 4 件**（openai / dashscope text / dashscope multimodal / ollama）+ `ReaderUtils` + `ImageReader`；2026-10-28 两批补齐外部向量库 4 件（`QdrantStore` / `PgVectorStore` 51 测试；`MilvusStore` / `ElasticsearchStore` 60 测试），实测 `436 passed / 0 failed`；**2026-10-31 loop 补齐 `RAGMode`/`GenericRAGHook`、`ExternalApiReader` 与 3 个 Java 增量（Qdrant 无 scheme IP、Dify 裸字符串 query、OpenAI embedding 生命周期），并接线 `SimpleKnowledge` usage listener，至此 §2.3 / §3 / §4.2 无未关闭条目（见 history/RAG__MIGRATION_HISTORY.md）** |

### 1.1 Rust crate 文件清单（55 个源文件；2026-10-28 两批新增 `store/{qdrant,pgvector,milvus,elasticsearch}.rs` 4 个源文件 5587 行非空 + 111 个内联测试；2026-10-31 新增 `src/hook.rs` 与 `src/reader/external_api.rs`）

> 行数口径：非空行（trim 后非空）。上一轮基线为 42 文件 / 10002 行 / 265 测试；本轮新增
> `embedding/{utils,openai,dashscope,ollama,test_support}.rs` 与 `reader/{utils,image}.rs`，
> 并扩充了 `embedding.rs`、`reader.rs`、`hash.rs`、`lib.rs`、`Cargo.toml`。新增测试数：
> openai 12 / dashscope 15 / ollama 8 / embedding::utils 10 / reader::utils 6 / reader::image 8 /
> hash 1 = **60**。**已完成（2026-10-29 勘误）**：原记「待主智能体统一跑 `cargo test` 确认」，实际 2026-10-28 第二批已实测 **436 passed / 0 failed / 0 ignored**（见 §1.2 / §4.1）。
> **2026-10-31 loop**：新增 `src/hook.rs`（`RagMode` / `GenericRagHook` / `GenericRagMiddleware`）与 `src/reader/external_api.rs`（`ExternalApiReader`），源文件 53 → **55**；按本轮约定子智能体**未执行 cargo**，编译/测试由主智能体统一验证；测试名与预期增量见 history/RAG__MIGRATION_HISTORY.md。
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

**2026-10-31 loop（本轮）**：按本轮约定**子智能体未执行 cargo/rustc/clippy/fmt**，离线用例由主智能体统一跑；本轮新增测试名见 history/RAG__MIGRATION_HISTORY.md，预期总数 436 → **约 490**（新用例 50+，精确值以主智能体实测为准）。

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
| `core.rag.reader.TikaReader` / `PDFReader` / `WordReader` | 多格式文档读取（Tika / PDFBox / POI） | 无 | 设计决定（不迁移） |
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
| 文档读取器（Tika/PDF/Word） | `TikaReader` / `PDFReader` / `WordReader` | 设计决定（不迁移） | **依据**：这三个 reader 直接依赖 JVM 库（Apache Tika / PDFBox / POI）及其文档模型，Java 实现无法机械直译；强行引入等价 Rust 库（如 `pdf-extract`/`lopdf`/`docx-rs`）会带来新的重依赖与解析语义差异，且与本 crate 的“高内聚低耦合”定位不符。**裁定：不迁移**，保留本 crate 现有的 `Reader` trait 作为扩展点，调用方可自行实现或在本 trait 上接入外部解析服务。 |
| `ImageReader` | `ImageReader`（图片 → `ImageBlock`/`URLSource`，`enableOCR` 占位不实现 OCR） | 已完成 | `src/reader/image.rs`：`ImageReader::new(enable_ocr)` / `is_ocr_enabled()` / `SUPPORTED_FORMATS`（7 项，与 Java 一致）；内容为 `ContentBlock::Image(ImageBlock{ URLSource })`，URL 保持原样、本地路径添 `file://`；docId = `generate_doc_id_md5(path)`、chunkId `"0"`；**输入取路径而非文件内容**（Rust `ReaderInput::as_string` 会读文件内容，这里显式用路径，见 history/RAG__MIGRATION_HISTORY.md 差异 2）；8 个测试 |
| `ExternalApiReader` | `ExternalApiReader` | **已完成（2026-10-31）** | `src/reader/external_api.rs`：Java 确实有契约（`RequestBuilder` + `ResponseParser` 两个函数式接口），但契约由**调用方注入**、不绑定任何厂商，因此不用“无 wire 契约”裁定，而是直接以 trait seam 迁移：`ExternalApiRequestBuilder::build_request(&str) -> HttpRequest`、`ExternalApiResponseParser::parse_response(&HttpResponse, &dyn HttpTransport)`（后者保留 Java 用 `OkHttpClient` 轮询异步任务的能力）。默认值完全对齐（chunk 512 / PARAGRAPH / 50、connect 10s、read 5min、retries 3、delay 2s、supportedFormats 空 = 任意格式）；docId = SHA-256(path)、chunkId 序号。显式差异：无 `writeTimeout`（请求体为内存 `String`）、`Thread.sleep` → `tokio::time::sleep`、最终错误消息附上最后一次原因（`RagError` 无 source 链）。10 个离线测试 |
| reader 辅助 | `ReaderUtils`（MD5/SHA-256 docId + hex） | 已完成 | `src/reader/utils.rs`：`generate_doc_id(input, "MD5" 或 "SHA-256")`、`generate_doc_id_md5`、`generate_doc_id_sha256`、`bytes_to_hex`、`DocIdAlgorithm`（未知算法 → `InvalidArgument("<algo> algorithm not available")`，与 Java 同文案）；SHA-256 新增 `hash::sha256_hex`（复用已有 `sha2`，**未新增依赖**）；6 个测试（含独立算得的 MD5/SHA-256 金标向量） |
| `TableFormat` | `TableFormat`（Word 表格 Markdown/JSON） | 设计决定（不迁移） | **依据**：`TableFormat` **仅**被未迁移的 `WordReader` 使用，无其它调用方；“不迁移”结论与 WordReader 一致。 |
| embedding 辅助 | `EmbeddingUtils`（超时/重试包装） | 已完成 | `src/embedding/utils.rs::EmbeddingExecutionConfig`（`timeout` + `retry: RetryPolicy`）提供 `apply(model, provider, operation_type, op)`：每次尝试套 `tokio::time::timeout`，重试复用 `retry_async`（仅 transport / 429 / 5xx）；工厂 `build_transport` 强制 `max_retries=0` 以免与 transport 自身重试叠加；10 个测试 |
| embedding 用量 DTO | `EmbeddingResponse` / `EmbeddingUsage`（token 用量 + 客户端耗时） | 已完成（**字段集有差异，见 history/RAG__MIGRATION_HISTORY.md**） | `embedding::{EmbeddingUsage, EmbeddingResponse, UsageListener}` + trait 默认方法 `embed_with_usage`（本地模型无需改）；各提供商从 wire 的 `usage` / `prompt_eval_count` 解析，`EmbeddingResponse.elapsed` 记录客户端耗时；`usage_listener` 在 builder 上可选注册。**见 history/RAG__MIGRATION_HISTORY.md 勘误 1 已更正（2026-10-26）**：Java **确有** `EmbeddingUsage`/`EmbeddingResponse`/`SimpleKnowledge.usageListener`（`5bfb36b44`，2026-10-20，已在旧基线内）；原「Rust 侧增值」表述作废 |

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
- **已知问题**：无编译/测试失败。2026-10-31 起 core `RagMode`/`GenericRAGHook` 已有本 crate 内对应（`src/hook.rs`，以 core `Middleware::on_reasoning` 承接，**未改 core/harness**）；无未关闭缺口。
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

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-rag` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-rag` / `cargo test -p agentscope-extensions-rag` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-rag/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。


## 6. 执行历史（已归档）

- 执行记录 / 静态复核 / 勘误 / 反向验证明细：[`history/RAG__MIGRATION_HISTORY.md`](./history/RAG__MIGRATION_HISTORY.md)
- 本文件只保留现行状态（§0–§5）；历史条目一律写进上述归档文件，不再回填本文。
