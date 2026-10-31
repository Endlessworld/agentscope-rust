//! Reader that delegates document parsing to an external HTTP API.
//!
//! Ports `io.agentscope.core.rag.reader.ExternalApiReader` (Java `agentscope-extensions-rag-simple`,
//! 408 non-blank lines). The class carries **no** vendor wire contract: both the request and the
//! response shape are injected by the caller (`RequestBuilder` / `ResponseParser`), which is why it
//! was tracked as "no fixed wire contract" in the module tracker.
//!
//! Mapping:
//!
//! | Java | Rust |
//! |---|---|
//! | `RequestBuilder.buildRequest(filePath, okHttpClient)` | [`ExternalApiRequestBuilder::build_request`] (`&str -> HttpRequest`) |
//! | `ResponseParser.parseResponse(response, okHttpClient)` | [`ExternalApiResponseParser::parse_response`] (`&HttpResponse`, `&dyn HttpTransport` for polling) |
//! | `OkHttpClient` (timeouts + interceptors) | injected [`HttpTransport`] (production [`crate::transport::ReqwestTransport`]) |
//! | `maxRetries` / `retryDelay` (default 3 / 2s) | [`ExternalApiReaderBuilder::max_retries`] / `retry_delay` (same defaults) |
//! | `chunkSize` 512 / `PARAGRAPH` / `overlapSize` 50 | [`crate::reader::chunk_text`] with the same defaults |
//! | docId = `ReaderUtils.generateDocIdSHA256(filePath)` | [`crate::reader::utils::generate_doc_id_sha256`] |
//!
//! Deliberate differences (non-gaps):
//!
//! * Java's `writeTimeout` has no counterpart — bodies are an in-memory `String` here, so only the
//!   connect/read timeouts are mapped onto [`crate::transport::TransportOptions`].
//! * Java blocked a Reactor `Schedulers.boundedElastic` thread and slept via
//!   `Thread.sleep`; the Rust port is `async` and uses [`tokio::time::sleep`].
//! * `RagError` has no source chain, so the last error is appended to the final message
//!   (`... after 4 attempts: API returned error status: 500, body: ...`).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use agentscope_core::message::{ContentBlock, TextBlock};

use crate::error::{RagError, Result};
use crate::model::{Document, DocumentMetadata};
use crate::reader::utils::generate_doc_id_sha256;
use crate::reader::{
    chunk_text, input_as_path, validate_chunking_parameters, Reader, ReaderInput, SplitStrategy,
};
use crate::transport::{HttpRequest, HttpResponse, HttpTransport, ReqwestTransport, TransportOptions};

/// Java `ExternalApiReader.Builder` default: connect timeout (10s).
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Java `ExternalApiReader.Builder` default: read timeout (5 min).
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(300);
/// Java `ExternalApiReader.Builder` default: retries after the first attempt.
pub const DEFAULT_MAX_RETRIES: usize = 3;
/// Java `ExternalApiReader.Builder` default: delay between attempts (2s).
pub const DEFAULT_RETRY_DELAY: Duration = Duration::from_secs(2);
/// Java `AbstractChunkingReader` default chunk size.
pub const DEFAULT_CHUNK_SIZE: usize = 512;
/// Java `AbstractChunkingReader` default overlap.
pub const DEFAULT_OVERLAP_SIZE: usize = 50;

/// Builds the HTTP request that submits `file_path` to the external parsing service.
///
/// Java `ExternalApiReader.RequestBuilder`; the `OkHttpClient` argument (used for complex upload
/// flows) is dropped because building a request never performs I/O. Implementations that need to
/// upload a file read it themselves.
pub trait ExternalApiRequestBuilder: Send + Sync {
    /// Builds the request for one document.
    fn build_request(&self, file_path: &str) -> Result<HttpRequest>;
}

/// Extracts the markdown text from the external service response.
///
/// Java `ExternalApiReader.ResponseParser`. The transport is passed in so polling implementations
/// (`parseResponse` calling `httpClient.newCall(...)` again) can be ported as-is.
#[async_trait]
pub trait ExternalApiResponseParser: Send + Sync {
    /// Parses the markdown body, optionally issuing follow-up requests through `transport`.
    async fn parse_response(
        &self,
        response: &HttpResponse,
        transport: &dyn HttpTransport,
    ) -> Result<String>;
}

/// Convenience adapter for the common one-shot case: the response body *is* the markdown.
pub struct BodyAsMarkdown;

#[async_trait]
impl ExternalApiResponseParser for BodyAsMarkdown {
    async fn parse_response(
        &self,
        response: &HttpResponse,
        _transport: &dyn HttpTransport,
    ) -> Result<String> {
        Ok(response.body.clone())
    }
}

/// Posts `{"file_path": "<path>"}` to a document-parsing endpoint.
///
/// This is the request shape [`ExternalApiReader::for_parsing_service`] registers; it covers the
/// common "send me a path, get markdown back" service. A host whose service expects an upload or a
/// different JSON envelope injects its own [`ExternalApiRequestBuilder`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileBodyRequest {
    endpoint: String,
}

impl FileBodyRequest {
    /// Creates the request builder for `endpoint`.
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
        }
    }

    /// The endpoint path-requests are posted to.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

impl ExternalApiRequestBuilder for FileBodyRequest {
    fn build_request(&self, file_path: &str) -> Result<HttpRequest> {
        let body = serde_json::to_string(&serde_json::json!({ "file_path": file_path }))
            .map_err(|error| {
                RagError::Reader(format!(
                    "failed to serialize the parsing-service request: {error}"
                ))
            })?;
        Ok(HttpRequest::post_json(self.endpoint.clone(), body))
    }
}

/// Calls an external parsing API, then chunks the returned markdown into [`Document`]s.
pub struct ExternalApiReader {
    request_builder: Arc<dyn ExternalApiRequestBuilder>,
    response_parser: Arc<dyn ExternalApiResponseParser>,
    transport: Arc<dyn HttpTransport>,
    chunk_size: usize,
    split_strategy: SplitStrategy,
    overlap_size: usize,
    max_retries: usize,
    retry_delay: Duration,
    supported_formats: Vec<String>,
}

impl std::fmt::Debug for ExternalApiReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalApiReader")
            .field("chunk_size", &self.chunk_size)
            .field("split_strategy", &self.split_strategy)
            .field("overlap_size", &self.overlap_size)
            .field("max_retries", &self.max_retries)
            .field("retry_delay", &self.retry_delay)
            .field("supported_formats", &self.supported_formats)
            .finish_non_exhaustive()
    }
}

impl ExternalApiReader {
    /// Starts a builder holding the Java defaults.
    pub fn builder() -> ExternalApiReaderBuilder {
        ExternalApiReaderBuilder::default()
    }

    /// A reader wired to a hosted document-parsing service (tracker RAG-09).
    ///
    /// The returned builder already registers the JSON request shape ([`FileBodyRequest`]), the
    /// markdown response parser ([`BodyAsMarkdown`]) and `supported_formats(["pdf", "docx"])`, so
    /// PDF / DOCX ingestion works **without** the optional `pdf` / `docx` cargo features:
    ///
    /// ```no_run
    /// use agentscope_extensions_rag::reader::ExternalApiReader;
    ///
    /// let reader = ExternalApiReader::for_parsing_service("https://parser.example/v1/parse")
    ///     .build()?;
    /// # Ok::<(), agentscope_extensions_rag::RagError>(())
    /// ```
    ///
    /// The HTTP transport is registered as well: unless the caller injects one with
    /// [`ExternalApiReaderBuilder::transport`], [`ReqwestTransport`] is built from the configured
    /// connect / read timeouts.
    pub fn for_parsing_service(endpoint: impl Into<String>) -> ExternalApiReaderBuilder {
        ExternalApiReader::builder()
            .request_builder(Arc::new(FileBodyRequest::new(endpoint)))
            .response_parser(Arc::new(BodyAsMarkdown))
            .supported_formats(["pdf", "docx"])
    }

    /// Replaces the HTTP transport (tests / custom HTTP stacks).
    pub fn with_transport(mut self, transport: Arc<dyn HttpTransport>) -> Self {
        self.transport = transport;
        self
    }

    /// Configured chunk size.
    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// Configured split strategy.
    pub fn split_strategy(&self) -> SplitStrategy {
        self.split_strategy
    }

    /// Configured overlap size.
    pub fn overlap_size(&self) -> usize {
        self.overlap_size
    }

    /// Retries after the first attempt (Java `maxRetries`).
    pub fn max_retries(&self) -> usize {
        self.max_retries
    }

    /// Delay between attempts (Java `retryDelay`).
    pub fn retry_delay(&self) -> Duration {
        self.retry_delay
    }

    /// Calls the external API, retrying as configured (Java `callExternalApi`).
    async fn call_external_api(&self, file_path: &str) -> Result<String> {
        let mut last_error: Option<RagError> = None;

        for attempt in 0..=self.max_retries {
            match self.attempt_once(file_path).await {
                Ok(markdown) => return Ok(markdown),
                Err(error) => {
                    last_error = Some(error);
                    if attempt < self.max_retries {
                        tokio::time::sleep(self.retry_delay).await;
                    }
                }
            }
        }

        let cause = last_error
            .map(|error| format!(": {error}"))
            .unwrap_or_default();
        Err(RagError::Reader(format!(
            "Failed to call external API after {} attempts{cause}",
            self.max_retries + 1
        )))
    }

    async fn attempt_once(&self, file_path: &str) -> Result<String> {
        let request = self.request_builder.build_request(file_path)?;
        let response = self.transport.execute(request).await?;
        if !response.is_success() {
            let body = if response.body.is_empty() {
                "No response body".to_string()
            } else {
                response.body.clone()
            };
            return Err(RagError::Reader(format!(
                "API returned error status: {}, body: {body}",
                response.status_code
            )));
        }
        self.response_parser
            .parse_response(&response, self.transport.as_ref())
            .await
    }

    /// Builds documents with `docId = SHA-256(filePath)` and chunk ids `"0"`, `"1"`, ...
    /// (Java `createDocuments`).
    fn create_documents(&self, chunks: &[String], file_path: &str) -> Vec<Document> {
        let doc_id = generate_doc_id_sha256(file_path);
        chunks
            .iter()
            .enumerate()
            .map(|(index, chunk)| {
                Document::new(DocumentMetadata::new(
                    ContentBlock::Text(TextBlock::new(chunk.clone())),
                    doc_id.clone(),
                    index.to_string(),
                ))
            })
            .collect()
    }
}

#[async_trait]
impl Reader for ExternalApiReader {
    async fn read(&self, input: ReaderInput) -> Result<Vec<Document>> {
        let file_path = input_as_path(&input);
        if !Path::new(&file_path).exists() {
            return Err(RagError::Reader(format!(
                "File does not exist: {file_path}"
            )));
        }

        let markdown = self.call_external_api(&file_path).await?;

        // Java runs chunking inside the same try block and wraps any failure as
        // "Failed to read document via external API"; keep the message and the original reason.
        let chunks = chunk_text(
            &markdown,
            self.chunk_size,
            self.split_strategy,
            self.overlap_size,
        )
        .map_err(|error| {
            RagError::Reader(format!("Failed to read document via external API: {error}"))
        })?;

        Ok(self.create_documents(&chunks, &file_path))
    }

    fn supported_formats(&self) -> Vec<String> {
        // Empty means "try any format" (Java javadoc).
        self.supported_formats.clone()
    }
}

/// Consuming builder for [`ExternalApiReader`].
#[derive(Clone)]
pub struct ExternalApiReaderBuilder {
    request_builder: Option<Arc<dyn ExternalApiRequestBuilder>>,
    response_parser: Option<Arc<dyn ExternalApiResponseParser>>,
    transport: Option<Arc<dyn HttpTransport>>,
    connect_timeout: Duration,
    read_timeout: Duration,
    chunk_size: usize,
    split_strategy: SplitStrategy,
    overlap_size: usize,
    max_retries: usize,
    retry_delay: Duration,
    supported_formats: Vec<String>,
}

impl Default for ExternalApiReaderBuilder {
    fn default() -> Self {
        Self {
            request_builder: None,
            response_parser: None,
            transport: None,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            read_timeout: DEFAULT_READ_TIMEOUT,
            chunk_size: DEFAULT_CHUNK_SIZE,
            split_strategy: SplitStrategy::Paragraph,
            overlap_size: DEFAULT_OVERLAP_SIZE,
            max_retries: DEFAULT_MAX_RETRIES,
            retry_delay: DEFAULT_RETRY_DELAY,
            supported_formats: Vec::new(),
        }
    }
}

impl ExternalApiReaderBuilder {
    /// Sets the request builder (required).
    pub fn request_builder(mut self, builder: Arc<dyn ExternalApiRequestBuilder>) -> Self {
        self.request_builder = Some(builder);
        self
    }

    /// Sets the response parser (required).
    pub fn response_parser(mut self, parser: Arc<dyn ExternalApiResponseParser>) -> Self {
        self.response_parser = Some(parser);
        self
    }

    /// Sets the HTTP transport; defaults to [`ReqwestTransport`] built from the timeouts below.
    pub fn transport(mut self, transport: Arc<dyn HttpTransport>) -> Self {
        self.transport = Some(transport);
        self
    }

    /// Sets the chunk size (Java default 512).
    pub fn chunk_size(mut self, chunk_size: usize) -> Self {
        self.chunk_size = chunk_size;
        self
    }

    /// Sets the split strategy (Java default `PARAGRAPH`).
    pub fn split_strategy(mut self, strategy: SplitStrategy) -> Self {
        self.split_strategy = strategy;
        self
    }

    /// Sets the overlap size (Java default 50).
    pub fn overlap_size(mut self, overlap_size: usize) -> Self {
        self.overlap_size = overlap_size;
        self
    }

    /// Sets the connect timeout (Java default 10s).
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Sets the read timeout (Java default 5 min); also used as the per-request timeout.
    pub fn read_timeout(mut self, timeout: Duration) -> Self {
        self.read_timeout = timeout;
        self
    }

    /// Sets the retries after the first attempt (Java default 3).
    pub fn max_retries(mut self, max_retries: usize) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Sets the delay between attempts (Java default 2s).
    pub fn retry_delay(mut self, retry_delay: Duration) -> Self {
        self.retry_delay = retry_delay;
        self
    }

    /// Sets the supported extensions; empty means "any format" (Java default).
    pub fn supported_formats<I, S>(mut self, formats: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.supported_formats = formats.into_iter().map(Into::into).collect();
        self
    }

    /// Validates and builds the reader.
    ///
    /// Java threw `IllegalArgumentException` with these messages; here they are
    /// [`RagError::InvalidArgument`], matching the rest of the crate.
    pub fn build(self) -> Result<ExternalApiReader> {
        let request_builder = self.request_builder.ok_or_else(|| {
            RagError::InvalidArgument("Request builder cannot be null".to_string())
        })?;
        let response_parser = self.response_parser.ok_or_else(|| {
            RagError::InvalidArgument("Response parser cannot be null".to_string())
        })?;
        validate_chunking_parameters(self.chunk_size, self.overlap_size)?;

        let transport = match self.transport {
            Some(transport) => transport,
            None => {
                // Set-by-call retries would stack with the reader's own retry loop.
                let options = TransportOptions {
                    connect_timeout: self.connect_timeout,
                    read_timeout: self.read_timeout,
                    max_retries: 0,
                    base_delay: self.retry_delay,
                };
                Arc::new(ReqwestTransport::new(&options)?)
            }
        };

        Ok(ExternalApiReader {
            request_builder,
            response_parser,
            transport,
            chunk_size: self.chunk_size,
            split_strategy: self.split_strategy,
            overlap_size: self.overlap_size,
            max_retries: self.max_retries,
            retry_delay: self.retry_delay,
            supported_formats: self.supported_formats,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Offline transport double: records requests and replays scripted responses LIFO.
    struct MockTransport {
        responses: Mutex<VecDeque<HttpResponse>>,
        requests: Mutex<Vec<HttpRequest>>,
    }

    impl MockTransport {
        fn with_responses(responses: Vec<HttpResponse>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses.into_iter().collect()),
                requests: Mutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> Vec<HttpRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl HttpTransport for MockTransport {
        async fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
            self.requests.lock().unwrap().push(request);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| RagError::Http("no scripted response left".to_string()))
        }
    }

    struct FixedRequest { url: &'static str, body: &'static str }

    impl ExternalApiRequestBuilder for FixedRequest {
        fn build_request(&self, file_path: &str) -> Result<HttpRequest> {
            let body = self.body.replace("{file}", file_path);
            Ok(HttpRequest::post_json(self.url, body))
        }
    }

    fn temp_file(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "rag-external-api-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"pdf-bytes").unwrap();
        path
    }

    fn reader(transport: Arc<MockTransport>, chunk_size: usize) -> ExternalApiReader {
        ExternalApiReader::builder()
            .request_builder(Arc::new(FixedRequest {
                url: "https://parser.test/v1/parse",
                body: "{\"file_path\":\"{file}\"}",
            }))
            .response_parser(Arc::new(BodyAsMarkdown))
            .transport(transport)
            .chunk_size(chunk_size)
            .overlap_size(0)
            .max_retries(0)
            .build()
            .unwrap()
    }

    #[test]
    fn builder_validates_required_fields_with_java_messages() {
        let error = ExternalApiReader::builder().build().unwrap_err();
        assert!(
            error.to_string().contains("Request builder cannot be null"),
            "err={error}"
        );

        let error = ExternalApiReader::builder()
            .request_builder(Arc::new(FixedRequest {
                url: "https://parser.test",
                body: "{}",
            }))
            .build()
            .unwrap_err();
        assert!(
            error.to_string().contains("Response parser cannot be null"),
            "err={error}"
        );
    }

    #[test]
    fn builder_defaults_match_java() {
        let reader = ExternalApiReader::builder()
            .request_builder(Arc::new(FixedRequest {
                url: "https://parser.test",
                body: "{}",
            }))
            .response_parser(Arc::new(BodyAsMarkdown))
            .transport(MockTransport::with_responses(Vec::new()))
            .build()
            .unwrap();

        assert_eq!(reader.chunk_size(), 512);
        assert_eq!(reader.split_strategy(), SplitStrategy::Paragraph);
        assert_eq!(reader.overlap_size(), 50);
        assert_eq!(reader.max_retries(), 3);
        assert_eq!(reader.retry_delay(), Duration::from_secs(2));
        assert!(reader.supported_formats().is_empty());
    }

    #[test]
    fn builder_rejects_invalid_chunking() {
        let error = ExternalApiReader::builder()
            .request_builder(Arc::new(FixedRequest {
                url: "https://parser.test",
                body: "{}",
            }))
            .response_parser(Arc::new(BodyAsMarkdown))
            .transport(MockTransport::with_responses(Vec::new()))
            .chunk_size(0)
            .build()
            .unwrap_err();
        assert!(error.to_string().contains("Chunk size must be positive"));
    }

    #[tokio::test]
    async fn read_posts_the_built_request_and_chunks_the_markdown() {
        let path = temp_file("doc.txt");
        let transport = MockTransport::with_responses(vec![HttpResponse::new(
            200,
            "first paragraph\n\nsecond paragraph",
        )]);
        let reader = reader(transport.clone(), 512);

        let documents = reader
            .read(ReaderInput::String(path.display().to_string()))
            .await
            .unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(documents.len(), 1);
        assert_eq!(
            documents[0].metadata.content_text(),
            "first paragraph\n\nsecond paragraph"
        );
        assert_eq!(documents[0].metadata.chunk_id, "0");
        assert_eq!(
            documents[0].metadata.doc_id,
            generate_doc_id_sha256(&path.display().to_string())
        );

        let requests = transport.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url, "https://parser.test/v1/parse");
        let expected_body = format!("{{\"file_path\":\"{}\"}}", path.display());
        assert_eq!(requests[0].body.as_deref(), Some(expected_body.as_str()));
    }

    #[tokio::test]
    async fn read_assigns_sequential_chunk_ids() {
        let path = temp_file("chunks.txt");
        let transport = MockTransport::with_responses(vec![HttpResponse::new(
            200,
            "aaaa\n\nbbbb\n\ncccc",
        )]);
        let reader = reader(transport, 8);

        let documents = reader
            .read(ReaderInput::String(path.display().to_string()))
            .await
            .unwrap();
        std::fs::remove_file(&path).ok();

        let ids: Vec<&str> = documents
            .iter()
            .map(|document| document.metadata.chunk_id.as_str())
            .collect();
        assert_eq!(ids, vec!["0", "1", "2"]);
        assert!(documents
            .iter()
            .all(|document| document.metadata.doc_id == generate_doc_id_sha256(&path.display().to_string())));
    }

    #[tokio::test]
    async fn missing_file_is_rejected_before_any_request() {
        let transport = MockTransport::with_responses(Vec::new());
        let reader = reader(transport.clone(), 512);

        let error = reader
            .read(ReaderInput::String("definitely/missing.pdf".to_string()))
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("File does not exist: definitely/missing.pdf"),
            "err={error}"
        );
        assert!(transport.requests().is_empty());
    }

    #[tokio::test]
    async fn non_success_status_is_retried_then_wrapped() {
        let path = temp_file("retry.txt");
        let transport = MockTransport::with_responses(vec![
            HttpResponse::new(500, "boom"),
            HttpResponse::new(500, "boom"),
            HttpResponse::new(500, "boom"),
        ]);
        let reader = ExternalApiReader::builder()
            .request_builder(Arc::new(FixedRequest {
                url: "https://parser.test/v1/parse",
                body: "{}",
            }))
            .response_parser(Arc::new(BodyAsMarkdown))
            .transport(transport.clone())
            .max_retries(2)
            .retry_delay(Duration::ZERO)
            .build()
            .unwrap();

        let error = reader
            .read(ReaderInput::String(path.display().to_string()))
            .await
            .unwrap_err();
        std::fs::remove_file(&path).ok();

        let message = error.to_string();
        assert!(
            message.contains("Failed to call external API after 3 attempts"),
            "err={message}"
        );
        assert!(
            message.contains("API returned error status: 500, body: boom"),
            "err={message}"
        );
        assert_eq!(transport.requests().len(), 3);
    }

    #[tokio::test]
    async fn a_later_attempt_can_succeed() {
        let path = temp_file("eventual.txt");
        let transport = MockTransport::with_responses(vec![
            HttpResponse::new(503, "warming up"),
            HttpResponse::new(200, "recovered"),
        ]);
        let reader = ExternalApiReader::builder()
            .request_builder(Arc::new(FixedRequest {
                url: "https://parser.test/v1/parse",
                body: "{}",
            }))
            .response_parser(Arc::new(BodyAsMarkdown))
            .transport(transport.clone())
            .max_retries(1)
            .retry_delay(Duration::ZERO)
            .build()
            .unwrap();

        let documents = reader
            .read(ReaderInput::String(path.display().to_string()))
            .await
            .unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(documents[0].metadata.content_text(), "recovered");
        assert_eq!(transport.requests().len(), 2);
    }

    /// Parser that polls a task endpoint through the injected transport, like Java's MinerU
    /// example (`ResponseParser` issuing follow-up requests with the `OkHttpClient`).
    struct PollingParser;

    #[async_trait]
    impl ExternalApiResponseParser for PollingParser {
        async fn parse_response(
            &self,
            response: &HttpResponse,
            transport: &dyn HttpTransport,
        ) -> Result<String> {
            let task_id: serde_json::Value = serde_json::from_str(&response.body)
                .map_err(|error| RagError::Api {
                    status_code: 500,
                    error_code: None,
                    message: format!("bad submit response: {error}"),
                })?;
            let task_id = task_id["task_id"].as_str().unwrap_or_default().to_string();
            let poll = HttpRequest::new(
                crate::transport::HttpMethod::Get,
                format!("https://parser.test/v1/tasks/{task_id}"),
            );
            let polled = transport.execute(poll).await?;
            Ok(polled.body)
        }
    }

    #[tokio::test]
    async fn the_parser_can_poll_through_the_injected_transport() {
        let path = temp_file("poll.txt");
        let transport = MockTransport::with_responses(vec![
            HttpResponse::new(200, "{\"task_id\":\"t-1\"}"),
            HttpResponse::new(200, "parsed markdown"),
        ]);
        let reader = ExternalApiReader::builder()
            .request_builder(Arc::new(FixedRequest {
                url: "https://parser.test/v1/tasks/submit",
                body: "{}",
            }))
            .response_parser(Arc::new(PollingParser))
            .transport(transport.clone())
            .max_retries(0)
            .build()
            .unwrap();

        let documents = reader
            .read(ReaderInput::String(path.display().to_string()))
            .await
            .unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(documents[0].metadata.content_text(), "parsed markdown");
        let requests = transport.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].url, "https://parser.test/v1/tasks/t-1");
    }

    #[tokio::test]
    async fn supported_formats_are_configurable() {
        let reader = ExternalApiReader::builder()
            .request_builder(Arc::new(FixedRequest {
                url: "https://parser.test",
                body: "{}",
            }))
            .response_parser(Arc::new(BodyAsMarkdown))
            .transport(MockTransport::with_responses(Vec::new()))
            .supported_formats(["pdf", "docx"])
            .build()
            .unwrap();

        assert_eq!(reader.supported_formats(), vec!["pdf", "docx"]);
    }

    #[test]
    fn the_parsing_service_request_is_json_with_the_file_path() {
        let request = FileBodyRequest::new("https://parser.test/v1/parse")
            .build_request("C:\\docs\\a b.pdf")
            .unwrap();
        assert_eq!(request.method, crate::transport::HttpMethod::Post);
        assert_eq!(request.url, "https://parser.test/v1/parse");
        assert_eq!(
            request.header("Content-Type"),
            Some("application/json; charset=utf-8")
        );
        let body: serde_json::Value =
            serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["file_path"], "C:\\docs\\a b.pdf");
        assert_eq!(body.as_object().unwrap().len(), 1, "only `file_path` is sent");
        assert_eq!(
            FileBodyRequest::new("https://parser.test").endpoint(),
            "https://parser.test"
        );
    }

    #[tokio::test]
    async fn the_parsing_service_reader_advertises_pdf_and_docx_and_chunks_the_reply() {
        let path = temp_file("service.pdf");
        let transport = MockTransport::with_responses(vec![HttpResponse::new(
            200,
            "page one\n\npage two",
        )]);
        let reader = ExternalApiReader::for_parsing_service("https://parser.test/v1/parse")
            .transport(transport.clone())
            .chunk_size(512)
            .overlap_size(0)
            .build()
            .unwrap();

        assert_eq!(reader.supported_formats(), vec!["pdf", "docx"]);
        assert_eq!(reader.chunk_size(), 512);

        let documents = reader
            .read(ReaderInput::from_file(path.clone()))
            .await
            .unwrap();
        let expected_doc_id = generate_doc_id_sha256(&path.display().to_string());
        std::fs::remove_file(&path).ok();

        assert_eq!(documents.len(), 1);
        assert_eq!(
            documents[0].metadata.content_text(),
            "page one\n\npage two"
        );
        assert_eq!(documents[0].metadata.doc_id, expected_doc_id);

        let requests = transport.requests();
        assert_eq!(requests.len(), 1, "the registered transport carried the call");
        assert_eq!(requests[0].url, "https://parser.test/v1/parse");
        let body: serde_json::Value =
            serde_json::from_str(requests[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(body["file_path"], path.display().to_string());
    }

    #[tokio::test]
    async fn the_parsing_service_reader_still_requires_an_existing_file() {
        let transport = MockTransport::with_responses(Vec::new());
        let reader = ExternalApiReader::for_parsing_service("https://parser.test/v1/parse")
            .transport(transport.clone())
            .build()
            .unwrap();
        let error = reader
            .read(ReaderInput::from_string("definitely/missing.docx".to_string()))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("File does not exist"), "err={error}");
        assert!(transport.requests().is_empty());
    }

    #[tokio::test]
    async fn a_failing_request_builder_is_retried_within_the_budget() {
        struct Broken;

        impl ExternalApiRequestBuilder for Broken {
            fn build_request(&self, _file_path: &str) -> Result<HttpRequest> {
                Err(RagError::Reader("cannot read file".to_string()))
            }
        }

        let path = temp_file("broken.txt");
        let transport = MockTransport::with_responses(Vec::new());
        let reader = ExternalApiReader::builder()
            .request_builder(Arc::new(Broken))
            .response_parser(Arc::new(BodyAsMarkdown))
            .transport(transport.clone())
            .max_retries(1)
            .retry_delay(Duration::ZERO)
            .build()
            .unwrap();

        let error = reader
            .read(ReaderInput::String(path.display().to_string()))
            .await
            .unwrap_err();
        std::fs::remove_file(&path).ok();

        assert!(
            error.to_string().contains("after 2 attempts"),
            "err={error}"
        );
        assert!(transport.requests().is_empty());
    }
}
