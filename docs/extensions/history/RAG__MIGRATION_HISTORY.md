# RAG 迁移执行历史（归档）

> 本文件是 [`../RAG__GAP_TRACKER.md`](../RAG__GAP_TRACKER.md) 的执行历史归档（截止 2026-10-31）。
> **现行状态以 tracker 为准**；本文件只读取证，不再更新。
> 内容来源：原 GAP_TRACKER 中 §5 之后各节，原文未改写（节标题：6. 静态复核记录（2026-10-22） · 7. 本轮交付记录（2026-10-26） · 8. 执行记录（2026-10-28）：QdrantStore + PgVectorStore · 9. 执行记录（2026-10-28 第二批）：MilvusStore + ElasticsearchStore · 10. 执行记录（2026-10-31 loop）：Java 增量 + 剩余缺口清零 · 2026-10-31 loop 收口记录）。

## 6. 静态复核记录（2026-10-22）

> **已过时（外部向量库 / HTTP embedding / hook / reader 增强部分）**：本节 §6.3 / §6.4 中「外部向量库 4 件全部缺失」与「HTTP embedding 4 件全部缺失」的结论均为 2026-10-22 快照。`QdrantStore` / `PgVectorStore` / `MilvusStore` / `ElasticsearchStore` 已于 **2026-10-28** 全部落地（`src/store/{qdrant,pgvector,milvus,elasticsearch}.rs`，共 111 个离线测试），以 **§8 / §9** 为准；HTTP embedding 4 件已于 **2026-10-26** 落地（`src/embedding/{openai,dashscope,ollama}.rs`），以 **§7** 为准。另：§6.4 中「`RAGMode` 缺失」「`ExternalApiReader` 缺失」已于 **2026-10-31** 落地（`src/hook.rs` / `src/reader/external_api.rs`），以 **§10** 为准。

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
| `simple`（HTTP embedding 提供商） | 4 | 4/4 | **已完成**（2026-10-26；本节快照时为 0/4） | `DashScopeTextEmbedding` / `DashScopeMultiModalEmbedding` / `OllamaTextEmbedding` / `OpenAITextEmbedding` → `src/embedding/{dashscope,ollama,openai}.rs`，见 §7 |
| `simple`（外部向量库） | 4 | 4/4 | **已完成**（2026-10-28 补记；本节快照时为 0/4） | `ElasticsearchStore` / `MilvusStore` / `PgVectorStore` / `QdrantStore` → `src/store/{elasticsearch,milvus,pgvector,qdrant}.rs`，均实现 `store::VdbStore`，见 §8 / §9 |
| `simple`（reader 增强） | 5 + 2 辅助 | 7/7 | **已完成**（2026-10-29 / 2026-10-31） | `TikaReader`/`PDFReader`/`WordReader`/`TableFormat` → 设计决定（不迁移）；`ImageReader`/`ReaderUtils` 2026-10-26 已落地；`ExternalApiReader` 2026-10-31 已落地（`src/reader/external_api.rs`，见 §10） |

### 6.4 新发现缺口

| 优先级 | 缺口 | Java 路径 / 证据 | Rust 现状 | 说明 |
|---|---|---|---|---|
| **P1** | 外部向量库 4 件（`ElasticsearchStore`/`MilvusStore`/`PgVectorStore`/`QdrantStore`） | `agentscope-extensions-rag-simple/src/main/java/io/agentscope/core/rag/store/{ElasticsearchStore,MilvusStore,PgVectorStore,QdrantStore}.java`（非空行 526 / 919 / 979 / 996） | **已完成**（2026-10-28 两批） | Java 侧分别走 ES REST、Milvus gRPC、Postgres JDBC(+pgvector)、Qdrant gRPC；Rust 侧均实现 `VdbStore`（`add`/`search`/`delete`），HTTP 走 `crate::transport` seam + 离线 mock。详见 §8 / §9。 |
| **P1** | HTTP embedding 提供商 4 件 | `.../io/agentscope/core/embedding/{dashscope/DashScopeTextEmbedding,dashscope/DashScopeMultiModalEmbedding,ollama/OllamaTextEmbedding,openai/OpenAITextEmbedding}.java` | **已完成**（2026-10-26） | `src/embedding/{dashscope,ollama,openai}.rs`；归属裁定：留在 RAG 扩展（§4.2 第 5 条）。 |
| **P2** | `ImageReader`（新增，原被误判为"不迁移"） | `.../io/agentscope/core/rag/reader/ImageReader.java`（非空行 140） | **已完成**（2026-10-26） | `src/reader/image.rs`；docId = MD5(path)、OCR 开关仅保留。 |
| **P2** | `ReaderUtils` 的 SHA-256 侧（MD5 侧已由 `hash::md5_hex` 覆盖） | `.../io/agentscope/core/rag/reader/ReaderUtils.java` | **已完成**（2026-10-26） | `src/reader/utils.rs` + `hash::sha256_hex`（无新增依赖）。 |
| **P2** | `EmbeddingUsage` / `EmbeddingResponse` / `SimpleKnowledge.usageListener` | `.../io/agentscope/core/embedding/{EmbeddingUsage,EmbeddingResponse}.java`；`.../io/agentscope/core/rag/knowledge/SimpleKnowledge.java` | **已完成**（2026-10-26 / 2026-10-31 接线） | `EmbeddingUsage`（含 `time_seconds` = Java `time`）/`EmbeddingResponse`/`UsageListener` + `SimpleKnowledge::with_usage_listener`（add/retrieve 均回调）；用例 `simple::tests::usage_listener_receives_add_and_retrieve_usage`。 |
| **P2** | `RAGMode`（core 枚举） | `agentscope-core/src/main/java/io/agentscope/core/rag/RAGMode.java` | **已完成**（2026-10-31） | `src/hook.rs::RagMode`；core 仍无 `RagMode`（旧断言属实），故落在本 crate 而非回填 core。 |
| **P2** | `ExternalApiReader` | `.../io/agentscope/core/rag/reader/ExternalApiReader.java`（非空行 408） | **已完成**（2026-10-31） | `src/reader/external_api.rs`：契约由调用方注入，故用 trait seam 而非“不迁移”裁定（见 §3.3 / §10）。 |
| **P2** | `EmbeddingUtils`（超时/重试包装 + `float[]`/`List<Double>` 转换） | `.../io/agentscope/core/embedding/EmbeddingUtils.java` | **已完成（设计决定，不迁移）**（数组转换部分） | 超时/重试已由 `src/embedding/utils.rs::EmbeddingExecutionConfig` + `retry::RetryPolicy` 完整覆盖（`cargo` 用例 10 个）；`convertDoubleListToArray` / `convertListToDoubleArray` 等工具在 Rust 无意义（`Vec<f64>` 与 `&[f32]` 之间本就无需装箱转换，对应 `f32_slice_to_f64`），故不迁移。 |

### 6.5 被证伪声明的勘误列表

| # | 原文（tracker 位置） | 实际 | 处置 |
|---|---|---|---|
| 1 | `> **Java 源码**：D:\IdeaProjects\agentscope-java\...`（文件头） | 路径已失效，实际为 `E:\local-github\agentscope-java\agentscope-extensions\agentscope-extensions-rag\` | 已修正文件头 |
| 2 | `Java 规模（实测行数）: 主代码 **15438** / 测试 **16057**`（§1） | 主 **15946** / 测 **16792**（非空行）。差值来自 simple 子模块在上轮后继续演进（simple 主 8068→**8576**、测 10188→**10909**）与 bailian 测 1479→**1493** | 已修正 §1（含 5 个子模块分项） |
| 3 | `§3.3 文档读取器（Tika/PDF/Word/Image）… **已完成（设计决定，不迁移）**，依据：这四个 reader 直接依赖 JVM 库（Apache Tika / PDFBox / POI）` | `ImageReader.java` 的 import 只有 `ImageBlock` / `URLSource` / `ReaderException` / `Document` / `DocumentMetadata` / `List` / `Mono` —— **零第三方 JVM 依赖**；`TikaReader`/`PDFReader`/`WordReader` 才有 tika/pdfbox/poi | 已拆分：`ImageReader` 从"不迁移"改为**待办**（§2.3 / §3.3 / §4.2）；2026-10-29 勘误：`ImageReader` 已落地（`src/reader/image.rs`） |
| 4 | `§3.3 reader 辅助（ReaderUtils）… **已完成（设计决定，不迁移）**，依据：其 SHA-256 路径仅服务于未迁移的 Tika/PDF/Word reader` | `ReaderUtils.generateDocIdMD5` 被 **`ImageReader`**（`ImageReader.java:91`）调用；`generateDocIdSHA256` 被 `TikaReader`/`PDFReader`/`ExternalApiReader` 调用。MD5 路径服务于**可迁移**的 `ImageReader` → "无调用方即无迁移价值"前提不成立 | 已由"不迁移"改为**待办**（§2.3 / §3.3）；2026-10-29 勘误：`ReaderUtils` 已落地（`src/reader/utils.rs`） |
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

> **历史快照提示（2026-10-26 注）**：本节（§6 全部）是 2026-10-22 的静态复核快照，其中“HTTP embedding 4 类零实现”“`ImageReader`/`ReaderUtils` 缺失”等结论**已被 §7 推翻**。原文另称「`EmbeddingUsage`/`EmbeddingResponse` 在 Java 仓库中并不存在」——该说法已于 2026-10-26 复评**证伪**（见 §7.3 勘误 1）。引用计数时请以 §7.1 为准。

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
6. **`SimpleKnowledge` usage 回调（2026-10-31 已改）**：原实现只调 `embed()`，现已改为 `embed_with_usage` + `with_usage_listener`（`add_documents` 每文档一次、`retrieve` 查询一次），与 Java `notifyUsage` 一致；详见 §10。
7. **`ImageReader` 取路径而非文件内容**：Java `ReaderInput.asString()` 返回构造时的字符串（`fromFile(path)` 即路径）；Rust `ReaderInput::as_string()` 会读文件内容，故 `image.rs` 另行用 `input_as_path()` 取路径。

### 7.3 勘误（本轮新增，必须遵守）

| # | 原文 | 实际 | 处置 |
|---|---|---|---|
| 1 | `§2.3 / §6.4`：`core.embedding.{EmbeddingUsage,EmbeddingResponse}` 存在，`SimpleKnowledge` 有 `usageListener` | **【已推翻·2026-10-26 更正】** 原文称「全仓 `rg` 零命中，Java 既无这两个类也无 `usageListener`」——**错误**。实测 Java HEAD `16cb024ae`：`core/embedding/EmbeddingUsage.java`（145 行）、`EmbeddingResponse.java`（106 行）均存在，`SimpleKnowledge.java:79/230` 有 `usageListener` 字段与回调；引入提交为 `5bfb36b44`（2026-10-20），且 `git merge-base --is-ancestor 5bfb36b44 ed8073259` = **true**（已在旧基线内）。 | **已改回**：§2.3/§3.3 恢复为「Java 已有，Rust 已迁移」；原「Rust 增值」表述作废。成因：当时 `rg` 搜的是 Java 工作区（可能停在旧提交）而非 `git show HEAD:`。详见 `JAVA_BASELINE_REVIEW_2026-10-26.md` §3.1 |
| 2 | `§6.1`：`Java 路径 ... 实际为 E:\local-github\agentscope-java` | 本机该路径不存在，实际为 `D:\IdeaProjects\agentscope-java\...` | 本轮以 D 盘路径完成核对；下文引用 Java 均指 D 盘 |
| 3 | `§2.3` 多处： "simple 子模块 33 类（rag 24 + embedding 9）" | **【已推翻·2026-10-26 更正】** 原文称 `io.agentscope.core.embedding` 主代码只有 **7** 个 `.java`，且 `EmbeddingUsage`/`EmbeddingResponse` 「不存在」。实测（HEAD `16cb024ae`）：该目录主代码为 **9 类** = `EmbeddingException`/`EmbeddingModel`/`EmbeddingUtils`/`EmbeddingUsage`/`EmbeddingResponse` + `dashscope/{Text,MultiModal}` + `ollama/OllamaTextEmbedding` + `openai/OpenAITextEmbedding` | **已改回**：embedding 为 **9 类**（全部已迁移）；simple 子模块实数应为 **33 类（rag 24 + embedding 9）**，含 core `rag` 引用则为 40 类——即 §2.3 标题原口径正确，本条勘误作废 |

### 7.4 验证建议（交给主智能体）

```powershell
cargo build -p agentscope-extensions-rag
cargo test  -p agentscope-extensions-rag
```

- 全部新测试**离线**：HTTP 走 `ScriptedTransport`；两个涉及临时文件的测试（`multimodal_local_file_is_inlined_as_data_url`、`file_input_uses_the_path_not_the_contents`）只写 `std::env::temp_dir()` 并在结束时删除。
- 反向验证建议：①把 `EmbeddingExecutionConfig::apply` 的超时分支删掉 → `timeout_is_reported_as_an_embedding_error` 应 FAILED；②把 `guess_image_mime_type` 的 `png` 分支改错 → `multimodal_local_file_is_inlined_as_data_url` 应 FAILED；③把 `image_url` 的 `file://` 前缀去掉 → `local_path_gets_a_file_scheme` 应 FAILED；④把 `sha256_hex` 改成调用 `md5_hex` → `sha256_doc_id_matches_the_java_algorithm` 与 `sha256_matches_fips_vectors` 应 FAILED。

### 7.5 仍待办（2026-10-31 已清零）

1. **外部向量库**：`QdrantStore` / `PgVectorStore` 已完成（2026-10-28 第一批，见 §8），`ElasticsearchStore` / `MilvusStore` 已完成（2026-10-28 第二批，见 §9）—— **4/4 已关闭**。
2. ~~`ExternalApiReader`（P2）~~ → **已完成（2026-10-31）**：`src/reader/external_api.rs`，见 §10.2。
3. ~~`RagMode` / `GenericRAGHook` 落点待裁定（core/harness）~~ → **已裁定并落地（2026-10-31）**：落在本 crate 的 `src/hook.rs`（纯逻辑 + `GenericRAGMiddleware` 经 core 公开 `Middleware::on_reasoning` 缝接入），未改 core/harness。

> 本节自此无未关闭项；后续若再出现待办请新增到 §4.2。

### 7.6 Java 基线漂移复评（2026-10-26 晚，Java HEAD `ed8073259` → `16cb024ae`）

背景：Java 仓库刚 `git pull` 到 `16cb024ae`（共 27 个新提交）。对本模块的影响：

- **新增漂移：0**。`ed8073259..16cb024ae` 区间内与本 crate 相关的只有 `07c5f3078`（`fix(rag): reuse and close the OpenAI embedding client`），它改的是 Java SDK 客户端的生命周期（懒初始化单客户端 + `AutoCloseable`），而 Rust 走自建 `HttpTransport` seam（`build_transport` 每次 build 建一个 `reqwest::Client`，`OpenAiTextEmbedding` 本身长期持有）→ **无需对齐**。
- **但既有缺口仍在**（对齐对象均在旧基线内，本次复评才发现）：

| # | 缺口 | Java | Rust | 优先级 |
|---|---|---|---|---|
| R1 | `EmbeddingUsage` 字段集不一致 | `inputTokens` / `totalTokens` / `time`（`time` = 客户端端到端**秒数**，在 usage 内） | **已对齐（2026-10-31）**：新增 `time_seconds: Option<f64>`（Java `time`），由 `EmbeddingResponse::with_elapsed` 盖章；`output_tokens` 保留（多模态 output token，Java 无对应但更完整） | 已关闭 |
| R2 | `EmbeddingModel::embed` 返回类型 | `Mono<EmbeddingResponse>`（breaking change） | `embed() -> Vec<f64>` + 新增默认方法 `embed_with_usage()`（刻意兼容取舍） | **已完成（设计决定，不迁移）**：`embed()` 保留为便捷入口（返回载荷子集），需要 usage/耗时走 `embed_with_usage()`；行为等价、无信息丢失，故不做 breaking 重命名 |
| R3 | `SimpleKnowledge` 不向外透传 usage | builder 有 `usageListener`，`addDocuments` 与 `retrieve` 每次 embed 后都回调 | **已接线（2026-10-31）**：`SimpleKnowledge::with_usage_listener` + `usage_listener()`；`simple::tests::usage_listener_receives_add_and_retrieve_usage` | 已关闭 |

- 完整复评方法与反证命令见 `docs/extensions/JAVA_BASELINE_REVIEW_2026-10-26.md`。

## 8. 执行记录（2026-10-28）：QdrantStore + PgVectorStore

**范围**：`simple` 子模块外部向量库 4 件中的 2 件（`QdrantStore.java` 996 行 / `PgVectorStore.java` 979 行）。`MilvusStore` / `ElasticsearchStore` **本轮未动**。

### 8.1 接口与映射

| Java 方法 | Rust | 语义 |
|---|---|---|
| `VDBStoreBase.add(List<Document>)` | `VdbStore::add(Vec<Document>)` | 空列表 no-op；逐条校验 embedding 存在 + 维度；Qdrant `PUT /collections/{c}/points?wait=true` upsert；PgVector `INSERT ... ON CONFLICT (id) DO UPDATE SET` 批量 |
| `search(SearchDocumentDto)` | `VdbStore::search(&SearchQuery)` | 维度 + `limit > 0` 校验；`vector_name` → Qdrant `using`（PgVector 与 Java 一样忽略）；`score_threshold` → Qdrant 服务端 `score_threshold` / PgVector 客户端 `score < threshold` 跳过 |
| `delete(String id)` | `VdbStore::delete(&str)` | Qdrant 按 point UUID 删除，返回 `result.status == "completed"`；PgVector **按 `doc_id` 列**删除（与 Java 一致，即删该文档全部 chunk），返回 `affected > 0` |
| `ensureCollection` / `ensureTable` | `QdrantStore::connect_with` / `PgVectorStore::connect_with`（也公开 `ensure_collection` / `initialize`） | Java 在构造函数内建；Rust 的 `QdrantStore::new(config, transport)` / `PgVectorStore::new(config, executor)` 明确不做 I/O（可离线构造与断言） |

### 8.2 显式差异（有意为之，非缺口）

1. **Qdrant 走 REST 而非 gRPC**：Java 用 `io.qdrant:client` gRPC SDK；Rust 用 `crate::transport::{HttpTransport, HttpRequest, HttpResponse}`（生产 `ReqwestTransport`，测试注入 mock），因此以 `?wait=true` 代替 `future.get()`；`:memory:` / `file://` 这类 gRPC/本地模式被拒（`RagError::Unsupported`）；裸 `host` 默认 REST 端口 **6333**（Java gRPC 6334）。
2. **Qdrant 错误不包装**：Java 把所有失败包成 `VectorStoreException("Failed to add documents to Qdrant")`（保留 cause 链）；Rust 直接返回状态码可判的 `RagError::Api{status_code}` / `RagError::Auth`（只有 `ensure_collection` 的建 collection 步骤按 Java 文案包装）。
3. **`check_compatibility` 仅保留字段**：REST 端口没有 gRPC 兼容性探测，字段接受但不产生请求。
4. **距离度量**：Java `createCollection` 硬编码 `Distance.Cosine`；Rust `QdrantConfig::distance` 默认 `Cosine`，可切 `Euclid` / `Dot`（体现在建 collection 体）。
5. **PgVector 不注册 `vector` 类型**：搜索投影为 `embedding::text` / `payload::text`，写入绑定 pgvector 文本字面量 + `::vector` 显式 cast，因此无需 `pgvector` crate（Java 需 `PGvector.addVectorType`）；存进去的字节仍是同一向量格式。
6. **PgVector 空 schema**：Java `schema("")` 会拼出 `".table"` 并在 JDBC 层失败；Rust 回退为 `public`。
7. **PgVector 连接**：Java 用 JDBC URL + `Properties`；Rust 由 JDBC URL 推导 libpq 连接串（`host/port/dbname/user/password/connect_timeout`，超时毫秒 → 秒，同 Java `connectionTimeoutMs / 1000`），也可 `connection_string(...)` 直接覆盖；连接用 `NoTls`（Java 走 JDBC 默认 SSL 协商）。
8. **PgVector 错误文案**：`Failed to add documents to PostgreSQL` / `Failed to search documents in PostgreSQL` / `Failed to delete document from PostgreSQL` / `Failed to ensure table exists: schema.table` 与 Java 一致，但把 cause 以 `: {error}` 拼在消息尾部（`RagError` 无 source 链）。
9. **空值参数**：Java 的 `documents == null` / `document == null` / `queryEmbedding == null` 在 Rust 类型系统上不可达，对应校验只保留 `Document must have embedding set` 与维度/limit 检查。

### 8.3 新增依赖

- 本 crate `Cargo.toml`：`tokio-postgres = { workspace = true }`（根 `[workspace.dependencies]` 已有 `tokio-postgres = "0.7"`，**未改根 Cargo.toml**，`Cargo.lock` 无需新增包）。
- 无其它新增依赖：不引 `pgvector` crate、不引 Qdrant SDK。

### 8.4 测试（全离线，51 个）

```
cargo build -p agentscope-extensions-rag                     -> exit 0 (0 errors)
cargo test  -p agentscope-extensions-rag --lib               -> 376 passed; 0 failed; 0 ignored   (基线 325，+51)
cargo clippy -p agentscope-extensions-rag --lib --all-targets -> exit 0（新增文件无告警）
```

- `store::qdrant::tests`（25）：base URL 归一化/拒绝 `:memory:`/`file://`、builder 三条 Java 文案、建 collection 请求体（`{"vectors":{"size":N,"distance":"Cosine"}}`）、`connect_with` 404→建 collection + `api-key` 头、已存在仅 GET、探测 403→`Auth`、upsert 请求体（payload `content`/`doc_id`/`chunk_id`/`payload`）、命名向量、无 embedding/维度/UUID 校验（均不触网）、空列表 no-op、query 体（`query`/`limit`/`with_payload`/`with_vector`/`using`/`score_threshold`）+ 响应映射（分数/内容/自定义 payload/embedding/vector_name）、legacy `result[]` + 命名向量响应、缺 payload 的点被跳过、维度与 limit 校验、delete `completed`→true / `acknowledged`→false、delete 参数校验、500→`Api{500}` / 401→`Auth{401}`、closed 后拒绝、payload 往返与缺字段报错、非 JSON content 回退 `TextBlock`。
- `store::pgvector::tests`（26）：builder 三条 Java 文案、标识符注入/长度/首字符校验、空 schema 回退、JDBC URL 解析（query 参数/无端口/错前缀）、libpq 连接串（超时秒 + override 优先）、三句 DDL 精确比对 + `information_schema` 小写探测、三种距离算子与索引类、upsert/delete SQL、向量字面量往返、距离→相似度三种类型、`initialize` 建 extension/schema/table/索引、表已存在时跳过、DDL 失败包装、add 参数表（id/向量/doc_id/chunk_id/content JSON/payload JSON）、空 payload→`{}`、无 embedding/维度校验 + 空批次、SQL 错误包装、search 参数与行映射（分数/embedding/payload）、阈值过滤、维度/limit 校验、delete 按 `doc_id` + 计数、blank id 校验、closed 拒绝、行回退（非 JSON content / 非 JSON payload / 空 payload）、document id 稳定性。

### 8.5 反向验证（回退实现 → FAILED → 恢复）

| # | 回退点 | 结果 |
|---|---|---|
| 1 | `QdrantStore` 的 query 体去掉 `score_threshold` 字段 | `store::qdrant::tests::search_builds_query_body_and_maps_results` **FAILED**（0 passed / 1 failed）；恢复后全绿 |
| 2 | `distance_to_score` 的 `Cosine => 1.0 - distance` 改为 `Cosine => distance` | `store::pgvector::tests` **4 failed**（`distance_to_score_matches_java` / `row_to_document_falls_back_on_non_json_content_and_payload` / `search_applies_the_score_threshold` / `search_maps_rows_to_scored_documents`）；恢复后全绿 |

### 8.6 未完成（第一批口径；已被第二批关闭）

- ~~`ElasticsearchStore`（Java 526 行，ES REST `_bulk` / `_search` / `_delete_by_query` + `dense_vector` 映射）~~ → **2026-10-28 第二批已完成**（见 §9）。实际 Java 删除走 `DELETE /{index}/_doc/{id}`（不是 `_delete_by_query`），§8.6 原表述有误，已按源码更正。
- ~~`MilvusStore`（Java 919 行，gRPC + `io.milvus` SDK）~~ → **2026-10-28 第二批已完成**（按 Milvus REST v2 `/v2/vectordb/...` 实现，协议差异见 §9.2）。

## 9. 执行记录（2026-10-28 第二批）：MilvusStore + ElasticsearchStore

**范围**：`simple` 子模块外部向量库 4 件中剩余的 2 件（`MilvusStore.java` 919 行 / `ElasticsearchStore.java` 526 行）。至此外部向量库 **4/4 完成**。

### 9.1 接口与映射

| Java 方法 | Rust | 语义 |
|---|---|---|
| `VDBStoreBase.add(List<Document>)` | `VdbStore::add(Vec<Document>)` | 空列表 no-op；逐条校验 embedding 存在 + 维度；Milvus `POST /v2/vectordb/entities/insert`（另加 REST-only `MilvusStore::upsert` → `entities/upsert`）；ES `POST /_bulk`（NDJSON，action 行 + `_source` 行） |
| `search(SearchDocumentDto)` | `VdbStore::search(&SearchQuery)` | Milvus：维度 + `limit > 0` 校验，`data` 单查询向量、`annsField`（`vector_name` 覆盖）、`limit`、`outputFields`、`searchParams.metricType`；`score_threshold` **客户端**过滤（先 `normalizeScore`，L2 → `1/(1+d)`，同 Java）。ES：维度 + `limit > 0`，`knn.{field,query_vector,k,num_candidates}` + `size` + `min_score`（无阈值时 `0.0`，同 Java） |
| `delete(String id)` | `VdbStore::delete(&str)` | Milvus：按 `doc_id in ["…"]` 过滤删除，返回 `deleteCount > 0`；ES：`DELETE /{index}/_doc/{id}`，只有 `result == deleted` 返回 `true`（`not_found` → `false`） |
| `hasCollection` / `createCollection` | `MilvusStore::{has_collection, ensure_collection}` + `connect_with`/`initialize` | Java 构造函数内建库 + 建集合；Rust `MilvusStore::new` 明确不做 I/O |
| `listDatabases` / `createDatabase` | `MilvusStore::{list_databases, ensure_database}` | 仅 `databaseName != "default"` 时探测/建库（同 Java），`databaseProperties` 透传到 `databases/create` |
| `indices().exists` / `indices().create` | `ElasticsearchStore::{index_exists, ensure_index}` + `connect_with` | `GET /{index}` 200 存在 / 404 则 `PUT /{index}` 建索引；`ElasticsearchStore::new` 不做 I/O |

### 9.2 Milvus 显式差异（有意为之，非缺口）

1. **REST v2 而非 gRPC**：Java 用 `io.milvus:milvus-sdk-java` v2 SDK；Rust 走 `crate::transport::{HttpTransport, HttpRequest, HttpResponse}`（测试注入 mock），因此 `ConnectConfig` 的 `account`/`hash`/`consistency` 等 gRPC 专有旋钮无对应项。
2. **URI 强校验**：Java `ConnectConfig.uri` 接受任意字符串、到连接时才失败；Rust `parse_milvus_uri` 在建配置时即要求 `http://` / `https://` 前缀（可诊断错误），并去掉尾部 `/`。
3. **超时映射**：`connectTimeoutMs` 在 Java 是 gRPC channel 超时；REST 端口把它映射为 `TransportOptions` 的 connect + read 超时（`MilvusConfig::transport_options`），`0` 表示沿用 transport 默认。
4. **错误分层**：Milvus 的 API 级失败是 HTTP 200 + `code != 0` → `RagError::VectorStore("Milvus error {code}: {message}")`；HTTP 级失败保留状态码可判的 `RagError::Api` / `RagError::Auth`（与 §8 Qdrant 口径一致）。
5. **delete 计数缺失**：REST 删除是异步的，响应可能不含 `deleteCount` → 视为“已受理”返回 `true`；Java 读 `DeleteResp.getDeleteCnt() > 0`。
6. **filter 转义**：Java 直接字符串拼接 `doc_id in ["…"]`；Rust `escape_filter_string` 转义 `\` 与 `"`（防注入，正常 id 字节相同）。
7. **schema 表达**：Java SDK 的 `AddFieldReq.maxLength/dimension` 在 REST v2 写作 `elementTypeParams.{max_length,dim}`（字符串值），字段顺序/长度（64/256/65535）与 `AUTOINDEX` + metric、`enableDynamicField=true` 一一对应。
8. **upsert 为新增能力**：Java 只有 `insert`；`MilvusStore::upsert` 走 REST `entities/upsert`，供幂等重建索引使用。

### 9.3 Elasticsearch 显式差异（有意为之，非缺口）

1. **错误不包装**：Java 把失败包成 `VectorStoreException("Failed to add documents")` / `"Vector search failed"` / `"Failed to delete document from Elasticsearch"`；Rust 直接返回带状态码的 `RagError::Api` / `RagError::Auth`，仅 `ensure_index` 的建索引步骤按 Java 文案包装（`Failed to ensure index exists: {index}: …`）。bulk `errors: true` 仍保留 Java 文案 `Elasticsearch bulk indexing failed`，并追加 Java 只会打日志的逐条 `id: reason`。
2. **存在性探测用 GET**：Java `indices().exists` 走 `HEAD /{index}`；本 crate 的 `HttpMethod` 只有 GET/POST/PUT/DELETE，因此用 `GET /{index}`（200 / 404 语义等价，仅多返回 body），未改公共 transport。
3. **`disableSslVerification` 仅为字段**：TLS 归注入的 transport（`ReqwestTransport`），本 crate 不会静默关闭校验；字段保留用于对齐 Java builder。
4. **basic auth 条件同 Java**：只有 `username` 与 `password` **都**非空才发 `Authorization: Basic …`（Java 仅在这种情况下装 `CredentialsProvider`）。
5. **不设 `_source`**：Java `SearchRequest` 未显式设置 `_source`，ES 默认返回，Rust 同样不发该键（保持 body 与 Java 一致）。
6. **路径段转义**：`DELETE /{index}/_doc/{id}` 与索引名做 RFC 3986 百分号编码（Java 客户端内部转义）；UUID 形态的 id 字节不变。
7. **`vector_name` 覆盖 `knn.field`**：Java 固定 `vector`；Rust 在 `SearchQuery::vector_name` 提供时改用该字段名（单字段默认仍为 `vector`）。

### 9.4 新增依赖

- **无**。Milvus 复用 `crate::transport`（`reqwest` 已在依赖中）；ES basic auth 复用已有 `base64`，转义/NDJSON 均为手写纯函数。未改根 `Cargo.toml`。

### 9.5 测试（全离线，60 个）

```
cargo build -p agentscope-extensions-rag                      -> exit 0 (0 errors)
cargo test  -p agentscope-extensions-rag --lib                -> 436 passed; 0 failed; 0 ignored   (基线 376，+60)
cargo clippy -p agentscope-extensions-rag --lib --all-targets  -> exit 0（新增文件无告警；仅剩 ragflow 旧告警）
rustfmt --edition 2021 --check <4 文件>                        -> 无 diff
```

- `store::milvus::tests`（32）：URI 归一化/拒绝裸 `host:port`、builder 四条 Java 文案、默认值（`default` 库 / `COSINE` / 30000ms）、token 优先于用户名密码（其余 `root:` 无密码）、`transport_options` 超时映射、建集合请求体（6 字段 + `elementTypeParams` + `AUTOINDEX`/metric）、`connect_with` 建库+建集合（URL/方法/`Bearer` 头/Content-Type 逐项断言）、库与集合已存在则跳过、`default` 库不探测、insert 行（id/vector float32/doc_id/chunk_id/content JSON/payload）、空 payload → `{}`、空列表 no-op、无 embedding/维度校验（不触网）、`upsert` 端点、search 体（`data`/`annsField`/`limit`/`outputFields`/`metricType`）+ 结果映射（分数/内容/payload/确定性 id）、`vector_name` → `annsField`、阈值客户端过滤（体里无阈值）、L2 归一化（3.0 → 0.25）、嵌套查询结果集、无法重建的条目被跳过、维度/limit 校验、delete filter、`deleteCount=0` → false、缺计数 → true、filter 转义、空 id 校验、`code!=0` → VectorStore、500→Api / 401→Auth、关闭后拒绝、content/payload 容错回退、envelope 缺字段、`normalize_score` 三度量。
- `store::elasticsearch::tests`（28）：base URL 归一化/拒绝裸 `host:port`、builder 三条 Java 文案、默认值、basic auth（仅单凭据不发头 + base64 字面量）、索引 mapping（keyword/text-non-indexed/dense_vector dims+cosine）、404 → 建索引（请求体/头）、已存在仅 1 次请求、建索引失败包装（400 文案）、`index_exists` 三态、`_bulk` NDJSON（2 行 + 结尾换行 + `application/x-ndjson` + source 各字段 + payload JSON 字符串）、空 payload 不写该字段、空列表 no-op、embedding/维度校验（不触网）、bulk 逐条错误信息、kNN 体（field/vector/k=3/num_candidates=50/size/min_score=0.0）+ hit 映射（score/content/payload/embedding）、`vector_name` + 阈值（num_candidates=80 / min_score=0.75）、`num_candidates` 公式、部分 hit 容错（缺 `_source`/缺 `doc_id` 被跳过、无 `_score` 则无分）、坏 payload → 空 map、空响应体/无 hits、维度/limit 校验、500→Api / 401→Auth、`_doc` 删除（deleted→true / not_found→false）、空 id 校验、路径段转义、`result` 大小写不敏感、bulk `errors=true` 无 items 用 Java 文案、关闭后拒绝。

### 9.6 反向验证（回退实现 → FAILED → 恢复）

| # | 回退点 | 结果 |
|---|---|---|
| 1 | `MilvusMetricType::normalize_score` 的 `L2 => 1.0 / (1.0 + raw)` 改为 `L2 => raw` | `store::milvus::tests` **2 failed**（`normalize_score_only_touches_l2` / `search_normalizes_l2_distances`，30 passed / 2 failed）；恢复后全绿 |
| 2 | `elasticsearch::num_candidates` 的 `.max(50)` 改为 `.max(10)` | `store::elasticsearch::tests` **2 failed**（`num_candidates_matches_java` / `search_builds_the_knn_request_and_maps_hits`，26 passed / 2 failed）；恢复后全绿 |

恢复后 `cargo test -p agentscope-extensions-rag --lib` = **436 passed / 0 failed**，`rustfmt --check` 无 diff。

### 9.7 后续/已知取舍

- 无遗留缺口：外部向量库 4/4 完成。真实服务集成测试（Milvus/ES 容器）未纳入离线套件，如需 CI 集成需另加 `#[ignore]` 之外的独立测试任务（本轮按“全离线”约束未做）。
- 仍待办（非本批范围）：~~`ExternalApiReader`（P2，§3.3）~~ → **2026-10-31 已完成**（`src/reader/external_api.rs`）；~~`RagMode`/`GenericRAGHook` 落点待裁定~~ → **2026-10-31 已裁定并落地**（`src/hook.rs`）。**本节无未关闭项。**

## 10. 执行记录（2026-10-31 loop）：Java 增量 + 剩余缺口清零

**依据**：`.agentscope/workspace/loop-2026-10-31.md`（Java HEAD `1524b3191`）；本回只改 `crates/agentscope-extensions-rag/**` 与本 tracker，**未改 core / harness / 根 `Cargo.toml`**，**新增依赖 0**。

### 10.1 Java 增量对照（2026-10-26 基线之后）

| Java 提交 | 变更 | Rust 落点 | 结论 |
|---|---|---|---|
| `2a682f8bf` fix(rag): parse scheme-less IP locations in QdrantStore | `parseLocation` 对无 `://` 的 location 前置 `grpc://` 再取 authority（原 `URI.create("127.0.0.1:6334")` 抛异常后整串被当 hostname） | `store::qdrant::split_authority` / `parse_rest_base_url`；新增 2 个回归用例（含 `127.0.0.1:6334`、`10.0.0.5:7334`、`my_host:6334`、`[::1]:6334`） | 已完成 |
| `d08c40c00` fix(dify): accept a bare string for the retrieval response `query` | 新增 `DifyQueryDeserializer`：`"query": "..."` 与 `{"content": "..."}` 均接受，其它形状 → `null`（保记录） | `dify::model::deserialize_query`（字段级 `deserialize_with`）；4 个用例 | 已完成 |
| `07c5f3078` fix(rag): reuse and close the OpenAI embedding client | 懒初始化单客户端 + `AutoCloseable` 幂等关闭 + 关闭后订阅不入重试管道（含 close 前创建、close后才订阅的 Mono） | 本 crate 本就单 transport（`build()` 建一次）；新增 `OpenAiTextEmbedding::{close, is_closed}` + `ensure_open`（在重试前）+ `EmbeddingModel::close` 默认方法/覆盖；5 个用例 | 已完成 |

**显式差异**：① Rust 走 REST，`http://host:6333` 不会被改写为 6334（Java 因随后讲 gRPC）；② 裸 IPv6（`::1`）在本 crate 自动加方括号（Java 只接受 `[::1]:6334`）；③ Rust 的 transport 在 `build()` 即创建（`reqwest::Client` 不建连、无 I/O），不采用 Java 的懒初始化时机，但复用/关闭语义等价。

### 10.2 剩余缺口清零

| tracker 项 | 处置 | 落点 | 代表用例 |
|---|---|---|---|
| `RAGMode`（§2.3 / §6.4） | 已完成 | `src/hook.rs::RagMode`（`as_str`/`from_wire`/`is_generic`，serde = Java 常量名） | `hook::tests::rag_mode_wire_values_match_the_java_enum` |
| `GenericRAGHook`（§2.3 / §3.1） | 已完成 | `src/hook.rs::GenericRagHook`（默认 limit 5 / threshold 0.5，priority 50）+ `GenericRagMiddleware`（core `Middleware::on_reasoning`，order 50） | `hook::tests::enhance_appends_the_retrieved_knowledge_message`、`middleware_injects_the_knowledge_message_before_next` |
| `ExternalApiReader`（§2.3 / §3.3 / §4.2 #2） | 已完成 | `src/reader/external_api.rs`（`ExternalApiRequestBuilder` + `ExternalApiResponseParser` + `BodyAsMarkdown` + builder） | `external_api::tests::read_posts_the_built_request_and_chunks_the_markdown`、`the_parser_can_poll_through_the_injected_transport` |
| `SimpleKnowledge` usage listener（§4.2 #4 / §7.6 R3） | 已完成 | `src/simple.rs::{with_usage_listener, usage_listener, notify_usage}`（`add_documents` 逐文档 + `retrieve` 查询各一次） | `simple::tests::usage_listener_receives_add_and_retrieve_usage` |
| `EmbeddingUsage.time` 字段集（§7.6 R1） | 已完成 | `src/embedding.rs::EmbeddingUsage.time_seconds` + `with_time_seconds` + `EmbeddingResponse::with_elapsed` 盖章 | `embedding::openai::tests::parses_embedding_and_usage` |
| `EmbeddingUtils` 数组转换（§6.4） | 已完成（设计决定，不迁移） | `f32_slice_to_f64` 已覆盖；`List<Double>`↔`float[]` 在 Rust 无意义（`Vec<f64>`/`&[f32]` 不是装箱集合） | — |
| `EmbeddingModel::embed` 返回类型差异（§7.6 R2） | 已完成（设计决定，不迁移） | `embed()` 为载荷子集便捷入口，`embed_with_usage()` 为完整响应；无信息丢失 | `embedding::openai::tests::usage_listener_receives_every_call` |
| §8.2 / §9.2 / §9.3 各条 | 均是**显式协议/语义差异（非缺口）**，无待办 | 已在原文说明 | — |

**新增用例（33 个，全离线）**：`store::qdrant` 2 / `dify::model` 4 / `embedding::openai` 5 / `hook` 9 / `reader::external_api` 11 / `simple` 2。预期总数 436 → **469**（精确值以主智能体实测为准）。

### 10.3 反向验证建议（交主智能体）

| # | 回退点 | 预期 |
|---|---|---|
| 1 | `split_authority` 还原为“无 `://` 则只补 scheme” | `parse_base_url_accepts_scheme_less_ip_locations` FAILED（`127.0.0.1:6334` → `http://127.0.0.1:6334:6333`） |
| 2 | `deserialize_query` 去掉 `Value::String` 分支 | `bare_string_query_keeps_every_record` FAILED |
| 3 | `embed_with_usage` 去掉 `ensure_open()` | `closed_model_bypasses_the_retry_pipeline` / `future_created_before_close_is_rejected` FAILED（transport 被请求） |
| 4 | `GenericRagHook::enhance_messages` 去掉 `documents.is_empty()` 分支 | `enhance_keeps_the_messages_when_nothing_is_retrieved` FAILED |
| 5 | `SimpleKnowledge::notify_usage` 改为空实现 | `usage_listener_receives_add_and_retrieve_usage` FAILED |
| 6 | `with_elapsed` 不再盖章 `time_seconds` | `parses_embedding_and_usage`、`usage_listener_receives_add_and_retrieve_usage` FAILED |

## 2026-10-31 loop 收口记录

- **关闭条目**：Java 增量 3 项（`2a682f8bf` / `d08c40c00` / `07c5f3078`）；tracker 项 `RAGMode`、`GenericRAGHook`、`ExternalApiReader`、`SimpleKnowledge` usage listener（R3）、`EmbeddingUsage.time`（R1）、`EmbeddingUtils` 数组转换、R2 返回类型差异。
- **状态口径**：§2.3 / §3 / §4.2 / §6.4 / §7.2 / §7.5 / §7.6 / §8.2 / §9.2 / §9.3 / §9.7 **已无未关闭条目**。
- **本条新增测试计数（静态核对）**：`store::qdrant` 2 / `dify::model` 4 / `embedding::openai` 5 / `hook` 9 / `reader::external_api` 11 / `simple` 2 = **33 个新用例**（按 `fn` 声明逐个数出，非估算）。
- **代码落点**：`src/hook.rs`（新）、`src/reader/external_api.rs`（新）、`src/reader.rs`（`input_as_path` + 模块声明）、`src/reader/image.rs`（改用共享 `input_as_path`）、`src/lib.rs`（模块 + re-export）、`src/store/qdrant.rs`、`src/dify/model.rs`、`src/embedding.rs`、`src/embedding/{openai,ollama,dashscope}.rs`、`src/simple.rs`。
- **新增依赖**：无（`futures` / `tokio` / `serde_json` 均为既有 workspace 依赖）。
- **未执行**：按本轮约定**未运行 cargo/rustc/clippy/fmt**；编译与测试由主智能体统一验证。真实后端集成（Qdrant/Milvus/ES/外部解析 API）仍未纳入离线套件，属预期。
- **跨模块需求**：无（未改 core/harness；`Middleware` 与 `RuntimeContext` 均为公开 API）。
