//! Document readers and text chunking.
//!
//! Ports `io.agentscope.core.rag.reader` (`Reader`, `ReaderInput`, `AbstractChunkingReader`,
//! `TextReader`, `TextChunker`, `SplitStrategy`, `ReaderUtils`, `ImageReader`, `ExternalApiReader`).
//!
//! Java's `TikaReader` depends on Apache Tika and is a deliberate non-migration (tracker §3.3,
//! P2). `TextReader`, the chunking algorithm, the dependency-free [`ImageReader`], the id helpers
//! (`ReaderUtils`) and the [`ExternalApiReader`] (caller-injected request/response contract over
//! [`crate::transport`]) are fully ported.
//!
//! The Java `PDFReader` / `WordReader` (PDFBox / POI) are ported as **format readers with a
//! pluggable extraction seam** ([`pdf::PdfReader`], [`docx::DocxReader`], tracker RAG-09). They
//! bundle no third-party parser: text extraction is either injected ([`pdf::PdfTextExtractor`] /
//! [`docx::DocxTextExtractor`]) or provided by the optional `pdf` / `docx` cargo features, which
//! add dependency-free extractors that shell out to `pdftotext` / `pandoc`. With the feature off
//! the reader reports a [`RagError::Reader`] instead of silently ingesting nothing. Scanned
//! documents / OCR are out of scope; a hosted parsing service is reachable through
//! [`ExternalApiReader::for_parsing_service`].

use agentscope_core::message::ContentBlock;
use async_trait::async_trait;

use crate::error::{RagError, Result};
use crate::model::{Document, DocumentMetadata};

pub mod docx;
pub mod external_api;
pub mod image;
pub mod pdf;
pub mod utils;

pub use docx::{DocxReader, DocxTextExtractor};
pub use external_api::{
    BodyAsMarkdown, ExternalApiReader, ExternalApiReaderBuilder, ExternalApiRequestBuilder,
    ExternalApiResponseParser, FileBodyRequest,
};
pub use image::{ImageReader, SUPPORTED_FORMATS};
pub use pdf::{PdfReader, PdfTextExtractor};
pub use utils::{
    bytes_to_hex, generate_doc_id, generate_doc_id_md5, generate_doc_id_sha256, DocIdAlgorithm,
};

/// Characters per token heuristic used by Java `TextChunker` (1 token ≈ 4 characters).
pub const CHARS_PER_TOKEN: usize = 4;

/// Strategy used to split text into chunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitStrategy {
    /// Split every `chunk_size` characters.
    Character,
    /// Split on blank lines, merging paragraphs up to `chunk_size`.
    #[default]
    Paragraph,
    /// Split by approximate token count (`chunk_size * 4` characters).
    Token,
    /// Semantic boundaries; currently falls back to paragraph splitting.
    Semantic,
}

/// Unified reader input source.
///
/// Mirrors Java `ReaderInput` but without the eager file-system read: `File` is resolved by
/// [`ReaderInput::as_string`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReaderInput {
    /// In-memory text.
    String(String),
    /// Path to a UTF-8 text file.
    File(std::path::PathBuf),
}

impl ReaderInput {
    /// Creates an input from a string.
    pub fn from_string(text: impl Into<String>) -> Self {
        ReaderInput::String(text.into())
    }

    /// Creates an input from a file path.
    pub fn from_file(path: impl Into<std::path::PathBuf>) -> Self {
        ReaderInput::File(path.into())
    }

    /// Resolves the input to its text contents.
    ///
    /// Returns a [`RagError::Reader`] when the file does not exist or cannot be read,
    /// mirroring Java `ReaderInput.fromFile`'s `IOException`.
    pub fn as_string(&self) -> Result<String> {
        match self {
            ReaderInput::String(s) => Ok(s.clone()),
            ReaderInput::File(path) => {
                if !path.exists() {
                    return Err(RagError::Reader(format!(
                        "File does not exist: {}",
                        path.display()
                    )));
                }
                std::fs::read_to_string(path).map_err(|e| {
                    RagError::Reader(format!("Failed to read {}: {e}", path.display()))
                })
            }
        }
    }
}

/// A document reader producing chunked [`Document`]s.
#[async_trait]
pub trait Reader: Send + Sync {
    /// Reads `input` into a list of chunked documents.
    async fn read(&self, input: ReaderInput) -> Result<Vec<Document>>;

    /// File extensions supported by this reader.
    fn supported_formats(&self) -> Vec<String>;
}

/// Resolves a [`ReaderInput`] to its path / URL string.
///
/// Java's `ReaderInput.asString()` returned whatever string the input was built with, which for
/// `fromFile(path)` is the path. Rust's [`ReaderInput::as_string`] eagerly reads file *contents*,
/// which path-based readers ([`ImageReader`], [`ExternalApiReader`]) must not do.
pub(crate) fn input_as_path(input: &ReaderInput) -> String {
    match input {
        ReaderInput::String(value) => value.clone(),
        ReaderInput::File(path) => path.display().to_string(),
    }
}

/// Resolves a [`ReaderInput`] to its raw bytes (the binary counterpart of
/// [`ReaderInput::as_string`]).
///
/// A [`ReaderInput::String`] yields the string's UTF-8 bytes; a [`ReaderInput::File`] yields the
/// file's contents, with the same [`RagError::Reader`] surface as [`ReaderInput::as_string`].
/// Binary format readers use this to hand the untouched document to an extractor
/// (tracker RAG-09).
///
/// Note that path-based readers ([`ImageReader`], [`ExternalApiReader`], [`pdf::PdfReader`],
/// [`docx::DocxReader`]) interpret even a `String` input as a path and therefore resolve their
/// bytes with [`input_path_bytes`], not with this function.
pub fn input_as_bytes(input: &ReaderInput) -> Result<Vec<u8>> {
    match input {
        ReaderInput::String(value) => Ok(value.clone().into_bytes()),
        ReaderInput::File(_) => input_path_bytes(input),
    }
}

/// Resolves a path-shaped input (a [`ReaderInput::File`], or a path carried in
/// [`ReaderInput::String`]) to the file's raw bytes.
///
/// Mirrors the [`ReaderInput::as_string`] error surface (`File does not exist: ...` /
/// `Failed to read ...`).
pub(crate) fn input_path_bytes(input: &ReaderInput) -> Result<Vec<u8>> {
    let path = std::path::PathBuf::from(input_as_path(input));
    if !path.exists() {
        return Err(RagError::Reader(format!(
            "File does not exist: {}",
            path.display()
        )));
    }
    std::fs::read(&path)
        .map_err(|e| RagError::Reader(format!("Failed to read {}: {e}", path.display())))
}

/// Chunks `text` and builds one [`Document`] per chunk, with `doc_id` and chunk ids `"0"`, `"1"`, ...
///
/// Shared by [`TextReader::create_documents`] and the binary format readers ([`pdf::PdfReader`],
/// [`docx::DocxReader`]), which chunk extracted text exactly like `TextReader`.
pub(crate) fn chunk_documents(
    text: &str,
    doc_id: &str,
    chunk_size: usize,
    strategy: SplitStrategy,
    overlap_size: usize,
) -> Result<Vec<Document>> {
    let chunks = chunk_text(text, chunk_size, strategy, overlap_size)?;
    Ok(chunks
        .iter()
        .enumerate()
        .map(|(index, chunk)| {
            Document::new(DocumentMetadata::new(
                ContentBlock::Text(agentscope_core::message::TextBlock::new(chunk.clone())),
                doc_id,
                index.to_string(),
            ))
        })
        .collect())
}

/// Validates chunking parameters.
///
/// Mirrors Java `AbstractChunkingReader.validateChunkingParameters`.
pub fn validate_chunking_parameters(chunk_size: usize, overlap_size: usize) -> Result<()> {
    if chunk_size == 0 {
        return Err(RagError::InvalidArgument(
            "Chunk size must be positive".to_string(),
        ));
    }
    if overlap_size >= chunk_size {
        return Err(RagError::InvalidArgument(
            "Overlap size must be less than chunk size".to_string(),
        ));
    }
    Ok(())
}

/// Splits `text` into chunks according to `strategy`.
///
/// Ports Java `TextChunker.chunkText`, including the validation errors, the empty-string
/// special case (`[""]`) and the per-strategy algorithms.
pub fn chunk_text(
    text: &str,
    chunk_size: usize,
    strategy: SplitStrategy,
    overlap_size: usize,
) -> Result<Vec<String>> {
    validate_chunking_parameters(chunk_size, overlap_size)?;

    if text.is_empty() {
        return Ok(vec![String::new()]);
    }

    Ok(match strategy {
        SplitStrategy::Character => chunk_by_character(text, chunk_size, overlap_size),
        SplitStrategy::Paragraph => chunk_by_paragraph(text, chunk_size, overlap_size),
        SplitStrategy::Token => chunk_by_character(
            text,
            chunk_size * CHARS_PER_TOKEN,
            overlap_size * CHARS_PER_TOKEN,
        ),
        SplitStrategy::Semantic => chunk_by_paragraph(text, chunk_size, overlap_size),
    })
}

/// Splits `text` into overlapping character windows.
///
/// Mirrors Java `TextChunker.chunkByCharacter`. Operates on `char` boundaries (Java uses
/// UTF-16 code units) which keeps chunking valid for multi-byte text.
fn chunk_by_character(text: &str, chunk_size: usize, overlap_size: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut chunks = Vec::new();
    let mut start = 0usize;

    while start < chars.len() {
        let end = (start + chunk_size).min(chars.len());
        chunks.push(chars[start..end].iter().collect());

        if end >= chars.len() {
            break;
        }
        start = end.saturating_sub(overlap_size);
    }

    chunks
}

/// Splits `text` on blank lines, merging paragraphs up to `chunk_size`.
///
/// Mirrors Java `TextChunker.chunkByParagraph`, including the `\n\s*\n` separator, the
/// overlap carry-over and the fallback that re-splits oversized paragraphs by character.
fn chunk_by_paragraph(text: &str, chunk_size: usize, overlap_size: usize) -> Vec<String> {
    let paragraphs = split_paragraphs(text);
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();

    for paragraph in paragraphs {
        let trimmed = paragraph.trim();
        if trimmed.is_empty() {
            continue;
        }

        if !current.is_empty() && current.chars().count() + trimmed.chars().count() + 2 > chunk_size
        {
            chunks.push(current.trim().to_string());
            current = String::new();

            if overlap_size > 0 {
                if let Some(last) = chunks.last() {
                    let last_chars: Vec<char> = last.chars().collect();
                    let overlap_start = last_chars.len().saturating_sub(overlap_size);
                    current.extend(&last_chars[overlap_start..]);
                }
            }
        }

        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(trimmed);

        if current.chars().count() > chunk_size {
            let large = std::mem::take(&mut current);
            let mut sub_chunks = chunk_by_character(&large, chunk_size, overlap_size);
            if let Some(tail) = sub_chunks.pop() {
                chunks.extend(sub_chunks);
                current = tail;
            }
        }
    }

    if !current.is_empty() {
        chunks.push(current.trim().to_string());
    }

    if chunks.is_empty() {
        vec![text.to_string()]
    } else {
        chunks
    }
}

/// Splits on `\n\s*\n` (one or more blank lines), dropping the separator.
fn split_paragraphs(text: &str) -> Vec<String> {
    let mut paragraphs = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;

    while i < chars.len() {
        if chars[i] == '\n' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() && chars[j] != '\n' {
                j += 1;
            }
            if j < chars.len() && chars[j] == '\n' {
                paragraphs.push(std::mem::take(&mut current));
                i = j + 1;
                continue;
            }
        }
        current.push(chars[i]);
        i += 1;
    }

    paragraphs.push(current);
    paragraphs
}

/// Text reader: reads and chunks text documents.
///
/// Ports `io.agentscope.core.rag.reader.TextReader` (defaults: `chunk_size = 512`,
/// `Paragraph`, `overlap_size = 50`).
#[derive(Debug, Clone)]
pub struct TextReader {
    chunk_size: usize,
    split_strategy: SplitStrategy,
    overlap_size: usize,
}

impl Default for TextReader {
    fn default() -> Self {
        Self {
            chunk_size: 512,
            split_strategy: SplitStrategy::Paragraph,
            overlap_size: 50,
        }
    }
}

impl TextReader {
    /// Creates a reader with explicit chunking parameters.
    pub fn new(
        chunk_size: usize,
        split_strategy: SplitStrategy,
        overlap_size: usize,
    ) -> Result<Self> {
        validate_chunking_parameters(chunk_size, overlap_size)?;
        Ok(Self {
            chunk_size,
            split_strategy,
            overlap_size,
        })
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

    /// Builds documents from chunks, assigning chunk ids `"0"`, `"1"`, ...
    ///
    /// Mirrors `TextReader.createDocuments`. Java generates a random `docId`; for
    /// reproducibility this implementation takes the id as input, so callers may pass a
    /// stable id (e.g. derived from the source path). [`Reader::read`] derives it from the
    /// input.
    pub fn create_documents(&self, doc_id: &str, chunks: &[String]) -> Vec<Document> {
        chunks
            .iter()
            .enumerate()
            .map(|(i, chunk)| {
                Document::new(DocumentMetadata::new(
                    ContentBlock::Text(agentscope_core::message::TextBlock::new(chunk.clone())),
                    doc_id,
                    i.to_string(),
                ))
            })
            .collect()
    }
}

#[async_trait]
impl Reader for TextReader {
    async fn read(&self, input: ReaderInput) -> Result<Vec<Document>> {
        let text = input.as_string()?;
        let chunks = chunk_text(
            &text,
            self.chunk_size,
            self.split_strategy,
            self.overlap_size,
        )?;
        let doc_id = doc_id_for_input(&input);
        Ok(self.create_documents(&doc_id, &chunks))
    }

    fn supported_formats(&self) -> Vec<String> {
        vec!["txt".to_string(), "md".to_string(), "rst".to_string()]
    }
}

/// Derives a stable document id from the reader input.
///
/// Java uses `UUID.randomUUID()`. A random id would make chunk ids non-reproducible and
/// would prevent deterministic tests, so the Rust port derives a version-3 UUID from the
/// input source instead (see tracker §2.1 for the deliberate deviation).
pub(crate) fn doc_id_for_input(input: &ReaderInput) -> String {
    let seed = match input {
        ReaderInput::String(_) => "reader-input:string".to_string(),
        ReaderInput::File(path) => format!("reader-input:file:{}", path.display()),
    };
    crate::hash::uuid_v3_from_name(seed.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_rejects_bad_parameters() {
        assert!(chunk_text("x", 0, SplitStrategy::Character, 0).is_err());
        assert!(chunk_text("x", 10, SplitStrategy::Character, 10).is_err());
        assert!(chunk_text("x", 10, SplitStrategy::Character, 5).is_ok());
    }

    #[test]
    fn empty_text_yields_single_empty_chunk() {
        let chunks = chunk_text("", 10, SplitStrategy::Character, 0).unwrap();
        assert_eq!(chunks, vec![String::new()]);
    }

    #[test]
    fn character_chunking_applies_overlap() {
        let text = "abcdefghij";
        let chunks = chunk_text(text, 4, SplitStrategy::Character, 1).unwrap();
        assert_eq!(chunks, vec!["abcd", "defg", "ghij"]);
    }

    #[test]
    fn character_chunking_without_overlap() {
        let chunks = chunk_text("abcdefghij", 4, SplitStrategy::Character, 0).unwrap();
        assert_eq!(chunks, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn token_chunking_uses_four_chars_per_token() {
        let text = "a".repeat(20);
        let chunks = chunk_text(&text, 2, SplitStrategy::Token, 0).unwrap();
        // 2 tokens * 4 chars = 8 chars per chunk -> 8 + 8 + 4.
        assert_eq!(chunks, vec!["a".repeat(8), "a".repeat(8), "a".repeat(4)]);
    }

    #[test]
    fn paragraph_chunking_splits_on_blank_lines() {
        let text = "one\n\ntwo\n\nthree";
        let chunks = chunk_text(text, 6, SplitStrategy::Paragraph, 0).unwrap();
        assert_eq!(chunks, vec!["one", "two", "three"]);
    }

    #[test]
    fn paragraph_chunking_merges_small_paragraphs() {
        let text = "aaa\n\nbbb";
        let chunks = chunk_text(text, 100, SplitStrategy::Paragraph, 0).unwrap();
        assert_eq!(chunks, vec!["aaa\n\nbbb"]);
    }

    #[test]
    fn document_ids_are_deterministic_per_input() {
        let reader = TextReader::default();
        let same = reader.create_documents("doc", &["a".to_string(), "b".to_string()]);
        let again = reader.create_documents("doc", &["a".to_string(), "b".to_string()]);
        assert_eq!(same[0].id, again[0].id);
        assert_ne!(same[0].id, same[1].id);
        assert_eq!(same[0].metadata.chunk_id, "0");
        assert_eq!(same[1].metadata.chunk_id, "1");
    }

    #[test]
    fn text_reader_defaults_match_java() {
        let r = TextReader::default();
        assert_eq!(r.chunk_size(), 512);
        assert_eq!(r.overlap_size(), 50);
        assert_eq!(r.split_strategy(), SplitStrategy::Paragraph);
    }

    #[tokio::test]
    async fn read_reports_expected_formats() {
        let reader = TextReader::default();
        assert_eq!(reader.supported_formats(), vec!["txt", "md", "rst"]);
    }

    #[tokio::test]
    async fn read_from_string_produces_chunks() {
        let reader = TextReader::new(5, SplitStrategy::Character, 0).unwrap();
        let docs = reader
            .read(ReaderInput::from_string("abcdefghij"))
            .await
            .unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].metadata.content_text(), "abcde");
        assert_eq!(docs[1].metadata.content_text(), "fghij");
    }

    #[tokio::test]
    async fn read_missing_file_is_a_reader_error() {
        let reader = TextReader::default();
        let err = reader
            .read(ReaderInput::from_file("definitely/missing.txt"))
            .await
            .unwrap_err();
        assert!(matches!(err, RagError::Reader(_)));
    }

    #[test]
    fn input_as_bytes_returns_text_bytes_and_file_contents() {
        let text = input_as_bytes(&ReaderInput::from_string("h\u{e9}llo")).unwrap();
        assert_eq!(text, "h\u{e9}llo".as_bytes());

        let path = std::env::temp_dir().join(format!(
            "rag-reader-bytes-{}-{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, [0x25u8, 0x50, 0x44, 0x46, 0x00, 0xff]).unwrap();
        let bytes = input_as_bytes(&ReaderInput::from_file(path.clone())).unwrap();
        assert_eq!(bytes, vec![0x25, 0x50, 0x44, 0x46, 0x00, 0xff]);
        assert_eq!(
            input_path_bytes(&ReaderInput::from_file(path.clone())).unwrap(),
            bytes,
            "a path-shaped input resolves to the same file bytes"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn input_as_bytes_reports_a_missing_file_like_as_string() {
        let missing = ReaderInput::from_file("definitely/missing.pdf");
        let error = input_as_bytes(&missing).unwrap_err();
        assert_eq!(
            error.to_string(),
            missing.as_string().unwrap_err().to_string(),
            "both resolvers share one error surface"
        );
        assert!(error.to_string().contains("File does not exist"));

        // A path carried in `String` is resolved by the path-shaped helper.
        let error =
            input_path_bytes(&ReaderInput::from_string("definitely/missing.pdf")).unwrap_err();
        assert!(error.to_string().contains("File does not exist"));
    }
}
