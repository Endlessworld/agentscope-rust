# RAG 模块迁移 — 跨会话进度跟踪文件（Gap Tracker）

> **用途**：本文件是 **RAG** 模块迁移的**单一事实来源**。开工前必须先读本文件 + `docs/extensions/EXTENSIONS_GAP_TRACKER.md` + `docs/MIGRATION_PLAN.md`；完成后必须更新对应条目的 `执行状态` 与 `交接信息`。切勿臆断代码现状——以本文件 + `git status` + `cargo build` 实际验证为准。
> **Java 源码**：`E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-rag\`
> **Java 子模块**：bailian, dify, haystack, ragflow, simple
> **Rust 目标**：`crates/agentscope-extensions-rag/`（波次 A）

## 0. 如何阅读本文件
- **执行状态**取值：`待办` / `进行中` / `已完成` / `设计决定（不迁移）`。
- **优先级**：P0=核心链路 / P1=重要 / P2=增强。
- 每完成一项：把状态改 `已完成`，在 `交接信息` 写最新验证结果（命令 + 结果）。

## 1. 模块概览
| 项 | 内容 |
|---|---|
| Java 模块 | `agentscope-extensions-rag` |
| Java 子模块 | bailian, dify, haystack, ragflow, simple |
| Java 规模（实测行数） | 主代码 **15438** / 测试 **16057**（bailian 1447/1479、dify 2509/1136、haystack 1565/1163、ragflow 1849/2091、simple 8068/10188） |
| Rust crate | `crates/agentscope-extensions-rag` |
| 迁移波次 | A |
| 当前状态 | **部分完成** — `dify` 子模块全量迁移、`simple` 子模块的离线链路已落地，`cargo build`/`cargo test` 全绿（101 passed / 0 failed）；`bailian` / `haystack` / `ragflow` **尚未迁移**；`simple` 的 HTTP embedding 提供商与外部向量库 **尚未迁移** |

### 1.1 Rust crate 文件清单（18 个源文件，~3705 行，101 个内联测试）
| 文件 | 职责 |
|---|---|
| `src/lib.rs` | crate 根：12 个模块声明 + re-export；说明 Java core `rag` 包已 `@Deprecated(forRemoval)` 且 core 无对应实现 |
| `src/error.rs` | `RagError`（Argument / Embedding / VectorStore / Reader / Unsupported / Http / Api / Auth / Json / Io），对应 Java 各类异常 |
| `src/hash.rs` | 自实现 MD5 + 版本 3（名称）UUID，复刻 Java `UUID.nameUUIDFromBytes` 的 Document id |
| `src/knowledge.rs` | `Knowledge` trait + 共享 `filter_and_rank`（阈值过滤 + 降序 + limit） |
| `src/message.rs` | 检索结果 → core `Msg`：tool 文本格式与 `<retrieved_knowledge>` 通用 hook 注入 |
| `src/model.rs` | `Document` / `DocumentMetadata` / `RetrieveConfig` / `content_block_text` |
| `src/reader.rs` | `Reader` trait、`ReaderInput`、`TextReader`、`SplitStrategy`、字符/段落/token 分块 |
| `src/retry.rs` | 指数退避重试（对应 Java `RetryInterceptor`：429 / 5xx / 传输错误重试，4xx 不重试） |
| `src/simple.rs` | `SimpleKnowledge`、`in_memory_knowledge`、`EmptyKnowledge` |
| `src/store.rs` | `VdbStore` trait、`InMemoryStore`、`SearchQuery`、余弦/欧氏距离 |
| `src/embedding.rs` | `EmbeddingModel` trait、确定性离线的 `HashEmbeddingModel` |
| `src/dify/{mod,config}.rs` | Dify 配置 + `RetrievalMode` / `RerankConfig` / `MetadataFilter(Condition)` |
| `src/dify/{client,request}.rs` | Dify HTTP 客户端 + 可注入 `DifyTransport` + 请求体构造 |
| `src/dify/{model,converter,knowledge}.rs` | Dify 响应 DTO、`DifyDocumentConverter`、`DifyKnowledge` |

### 1.2 验证基线（最近一次实测，由汇总方提供）
```
cargo build -p agentscope-extensions-rag   -> Finished (0 errors)
cargo test  -p agentscope-extensions-rag   -> 101 passed; 0 failed
cargo test --workspace                     -> 1485 passed; 0 failed
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
| `com.aliyun.bailian20231229` SDK | 百炼检索 | 无 | 待裁定：SDK 语义 vs reqwest 直连（见 §4.2） |
| Apache Tika / PDFBox / POI | 文档解析 | 无 | 见 §3.3，待评估 pure-Rust 替代 |
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

### 2.3 simple 子模块（`io.agentscope.core.rag.*` / `io.agentscope.core.embedding.*`，31 类）— **离线部分已迁移**
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
| `core.embedding.EmbeddingModel` | 向量化抽象 | `embedding::EmbeddingModel` (trait) | 已完成 |
| `core.embedding.EmbeddingException` | embedding 异常 | `RagError::Embedding` | 已完成 |
| `core.rag.exception.ReaderException` / `VectorStoreException` | reader / store 异常 | `RagError::Reader` / `RagError::VectorStore` | 已完成 |
| `core.rag.KnowledgeRetrievalTools`（core） | agentic 模式工具输出文本 | `message::format_documents_for_tool` | 已完成 |
| `core.rag.GenericRAGHook`（core） | 自动检索 hook（注入 `<retrieved_knowledge>`） | `message::{build_retrieved_knowledge_msg, extract_query_from_messages, RETRIEVED_KNOWLEDGE_OPEN/CLOSE}` | 已完成 |
| `core.embedding.dashscope.{DashScopeTextEmbedding, DashScopeMultiModalEmbedding}` | DashScope HTTP embedding | 无（`embedding::HashEmbeddingModel` 为离线替代） | **待办 P1** |
| `core.embedding.ollama.OllamaTextEmbedding` | Ollama HTTP embedding | 无 | **待办 P1** |
| `core.embedding.openai.OpenAITextEmbedding` | OpenAI HTTP embedding | 无 | **待办 P1** |
| `core.embedding.EmbeddingUtils` | 超时/重试包装 | 无（可用 `retry::RetryPolicy`） | **待办 P2** |
| `core.rag.reader.TikaReader` / `PDFReader` / `WordReader` / `ImageReader` / `ExternalApiReader` | 多格式文档读取（Tika/PDFBox/POI/外部服务） | 无 | **待办 P2** |
| `core.rag.reader.ReaderUtils` | MD5/SHA-256 docId + hex | 无（可基于 `hash.rs` 扩展） | **待办 P2** |
| `core.rag.reader.TableFormat` | Word 表格格式（Markdown/JSON） | 无（仅 WordReader 使用） | **待办 P2** |
| `core.rag.store.ElasticsearchStore` / `MilvusStore` / `PgVectorStore` / `QdrantStore` | 外部向量库 | 无（接口为 `store::VdbStore`） | **待办 P1** |

### 2.4 bailian 子模块（`io.agentscope.core.rag.integration.bailian.*`，7 类）— **未迁移**
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `BailianClient` | 阿里云百炼检索（基于 `com.aliyun.bailian20231229` SDK） | 无 | **待办 P1** |
| `BailianConfig` (+Builder) | accessKey/workspace/index + 检索参数 | 无 | **待办 P1** |
| `BailianDocumentConverter` | 百炼 node → `Document` | 无 | **待办 P1** |
| `BailianKnowledge` | `Knowledge` 适配器（只读） | 无 | **待办 P1** |
| `RerankConfig` | rerank 模型名 | 无 | **待办 P1** |
| `RewriteConfig` | 多轮改写模型（默认 `conv-rewrite-qwen-1.8b`） | 无 | **待办 P1** |
| `QueryHistoryEntry` | 多轮改写历史条目（user/assistant） | 无 | **待办 P1** |

### 2.5 haystack 子模块（`io.agentscope.core.rag.integration.haystack.*`，10 类）— **未迁移**
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `HayStackClient` | OkHttp 调用自建 HayStack RAG 服务 | 无 | **待办 P1** |
| `HayStackConfig` (+Builder) | baseUrl / topK / 过滤 / 超时 / 重试 | 无 | **待办 P1** |
| `HayStackDocumentConverter` | HayStackDocument → `Document` | 无 | **待办 P1** |
| `HayStackKnowledge` | `Knowledge` 适配器 | 无 | **待办 P1** |
| `FilterPolicy` | 过滤器策略（REPLACE/MERGE） | 无 | **待办 P1** |
| `model.HayStackDocument` / `HayStackResponse` / `SparseEmbedding` / `ByteStream` | 响应/文档/sparse 向量 DTO | 无 | **待办 P1** |
| `exception.HayStackApiException` | API 异常 | 无 | **待办 P1** |

### 2.6 ragflow 子模块（`io.agentscope.core.rag.integration.ragflow.*`，9 类）— **未迁移**
| Java 类 | 职责 | Rust 对应 | 状态 |
|---|---|---|---|
| `RAGFlowClient` | OkHttp 调用 `POST /api/v1/retrieval` | 无 | **待办 P1** |
| `RAGFlowConfig` (+Builder) | apiKey / datasetIds / topK / threshold / page 等 | 无 | **待办 P1** |
| `RAGFlowDocumentConverter` | chunk → `Document`（payload 装配） | 无 | **待办 P1** |
| `RAGFlowKnowledge` | `Knowledge` 适配器 | 无 | **待办 P1** |
| `model.RAGFlowChunk` / `RAGFlowResponse` / `ResponseDataDeserializer` | chunk / 响应 DTO（`data` 可 false/null/object） | 无 | **待办 P1** |
| `exception.RAGFlowApiException` / `RAGFlowAuthException` | API / 鉴权异常 | 无 | **待办 P1** |

## 3. 详细缺口清单

### 3.1 骨架与依赖（P0）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| crate 骨架 + `lib.rs` 接线 | 全模块 | 已完成 | 12 个 `mod` 声明 + re-export；`agentscope-core` / `agentscope-harness` 为 path 依赖 |
| 公共 RAG 模型 | `core.rag.model.{Document, DocumentMetadata, RetrieveConfig}` | 已完成 | `src/model.rs`；因 Java core `rag` 包 `@Deprecated(forRemoval)` 且 core 无对应，模型落在本 crate（见 §4.2） |
| `Knowledge` trait + 共享排序 | `core.rag.Knowledge` | 已完成 | `src/knowledge.rs`；Reactor `Mono` → `async_trait` |
| 检索结果 → core `Msg` 桥接 | `core.rag.KnowledgeRetrievalTools` / `GenericRAGHook` | 已完成 | `src/message.rs`；core 侧 `RagMode`/hook 未迁移，先提供纯函数 |
| Document id（UUID v3 / MD5） | `core.rag.model.Document` | 已完成 | `src/hash.rs` 自实现，未新增第三方 crate |
| 统一错误类型 | `EmbeddingException` / `VectorStoreException` / `ReaderException` / `Dify*Exception` | 已完成 | `src/error.rs` |
| HTTP 重试 | `DifyRAGClient.RetryInterceptor` | 已完成 | `src/retry.rs` |
| **dify 全链路** | dify 子模块 11 类 | 已完成 | `src/dify/**`；请求体 / 响应 / 转换 / 适配器 / 错误全覆盖 |
| **simple 离线链路** | `SimpleKnowledge` / `InMemoryStore` / `DistanceCalculator` / `TextReader` / `TextChunker` / `SplitStrategy` / `ReaderInput` / `EmbeddingModel` | 已完成 | `src/{simple,store,reader,embedding}.rs` |
| 离线单测（101 个） | Java 测试 16057 行 | 已完成 | 见 §1.2；HTTP 走注入式 mock transport，全离线 |

### 3.2 缺口（P1）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| Bailian 集成 | `bailian.*` 7 类 | 待办 | Java 用 `com.aliyun.bailian20231229` SDK；Rust 需评估 reqwest 直连或 SDK 取舍（§4.2） |
| HayStack 集成 | `haystack.*` 10 类 | 待办 | OkHttp → reqwest；建议复用 `dify` 的 transport seam 模式 |
| RAGFlow 集成 | `ragflow.*` 9 类 | 待办 | `POST /api/v1/retrieval`；`data` 字段可 `false`/`null`/object，需自定义反序列化（`ResponseDataDeserializer`） |
| simple 外部向量库 | `ElasticsearchStore`(526) / `MilvusStore`(899) / `PgVectorStore`(979) / `QdrantStore`(996) | 待办 | 均以 `store::VdbStore` 为接口；各需 REST/gRPC 客户端，可离线 mock |
| embedding HTTP 提供商 | `DashScopeTextEmbedding` / `DashScopeMultiModalEmbedding` / `OllamaTextEmbedding` / `OpenAITextEmbedding` | 待办 | 与 MODEL 扩展边界可能重叠，待裁定（§4.2） |

### 3.3 增强（P2）
| 缺口项 | Java 参考 | 执行状态 | 交接信息/说明 |
|---|---|---|---|
| 文档读取器 | `TikaReader` / `PDFReader` / `WordReader` / `ImageReader` / `ExternalApiReader` | 待办 | 依赖 JVM 库（Apache Tika / PDFBox / POI）或外部服务；Rust 需评估 pure-Rust 替代或标「设计决定（不迁移）」 |
| reader 辅助 | `ReaderUtils`（MD5/SHA-256 docId + hex） / `TableFormat` | 待办 | `ReaderUtils` 可基于 `hash.rs` 扩展；`TableFormat` 仅 Word reader 使用 |
| embedding 辅助 | `EmbeddingUtils`（超时/重试包装） | 待办 | 可复用 `retry::RetryPolicy`；HTTP provider 迁移后一并落地 |

## 4. 项目状态与下一步

### 4.1 当前状态
- **已完成**：crate 骨架、公共 RAG 模型/trait、消息桥接、Dify 全链路、simple 离线链路（`SimpleKnowledge` + `InMemoryStore` + `TextReader`/`TextChunker` + `HashEmbeddingModel` + 距离计算），共 18 个源文件 ~3705 行、101 个内联测试。
- **验证命令与结果**（最近一次实测，由汇总方提供）：
  - `cargo build -p agentscope-extensions-rag` → **通过（0 errors）**
  - `cargo test -p agentscope-extensions-rag` → **101 passed / 0 failed**
  - `cargo test --workspace` → **1485 passed / 0 failed**
- **已知问题**：无编译/测试失败。设计上有一处显式取舍——core 侧 `RagMode`/hook 与 `io.agentscope.core.rag` 包未迁移，检索结果到消息的桥接以纯函数形式提供（`src/message.rs`），调用方需自行组装 hook/工具。

### 4.2 下一步与阻塞
1. **bailian / haystack / ragflow 子模块迁移（P1）**：按 `dify` 的「可注入 transport trait + 离线 mock」模式实现各自 DTO / 客户端 / 转换器 / `Knowledge` 适配器。
2. **simple 外部向量库（P1）**：`ElasticsearchStore` / `MilvusStore` / `PgVectorStore` / `QdrantStore`，均实现 `store::VdbStore`。
3. **文档读取器与 embedding HTTP 提供商（P1/P2）**：见 §3.3。
4. **待裁定事项**
   - `RagMode` / `GenericRAGHook` 落点：core 是否新增 hook 抽象，或由 harness 承接（当前 `message.rs` 纯函数过渡）。
   - embedding HTTP 提供商（dashscope/ollama/openai）归属：RAG 扩展 vs MODEL 扩展（`src/embedding.rs` 注释倾向 MODEL）。
   - Bailian 是否保留 `com.aliyun.bailian20231229` SDK 语义，或改为 reqwest 直连（影响鉴权/请求签名）。
5. **跨模块需求**
   - 若需在 core/harness 增加 RAG hook 或 embedding 抽象，本 crate 无权修改，需汇总方裁定（记入本 §4.2，不直接改 core/harness）。
   - 若 embedding 提供商确定归 MODEL 扩展，本 crate 仅依赖其 trait，不重复实现。

## 5. 执行约定（所有会话必须遵守）
- 开工前：读本文件 + `docs/MIGRATION_GAP_TRACKER.md` + `git status`；确认 `cargo build -p agentscope-extensions-rag` 基线。
- 改动中：保持 `cargo build -p agentscope-extensions-rag` / `cargo test -p agentscope-extensions-rag` 通过。
- 完工后：更新 §3 条目状态与 §4，并在 `docs/extensions/EXTENSIONS_GAP_TRACKER.md` 对应行登记。
- **仅在本 crate 内改动**；新增依赖写入 `crates/agentscope-extensions-rag/Cargo.toml` 的 `[dependencies]`，不要改根 `Cargo.toml` 的 `[workspace.dependencies]`（如需请记入 §4.2 由汇总方处理）。
- 不修改 `crates/agentscope-core` / `crates/agentscope-harness`（除非本 tracker 明确授权），不修改其它模块的 crate。

