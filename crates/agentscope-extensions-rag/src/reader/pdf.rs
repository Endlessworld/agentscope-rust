//! PDF reader (`io.agentscope.core.rag.reader.PDFReader`).
//!
//! Java's `PDFReader` extracts text with PDFBox (`PDFTextStripper`) and then chunks it exactly like
//! `TextReader`. This workspace deliberately bundles **no** PDF library (tracker RAG-09, 裁定
//! 2026-10-31: 撤销「引入 liteparse」), so the port keeps the *reader seam* and makes extraction
//! pluggable:
//!
//! * [`PdfTextExtractor`] is the extraction seam. [`PdfReader::with_extractor`] injects any
//!   implementation a host owns (an in-process parser, or a client of a hosted parsing API).
//! * the optional **`pdf`** cargo feature (off by default) adds [`PdftotextExtractor`], a
//!   dependency-free extractor that runs the `pdftotext` binary (poppler-utils) on a temporary
//!   file, plus [`PdfReader::with_builtin_extractor`].
//! * with the feature off [`PdfReader::new`] fails with a [`RagError::Reader`] naming the feature
//!   and the alternatives, so a caller can never silently ingest an empty document.
//!
//! Out of scope (out of scope in Java too): scanned documents and OCR. A PDF without a text layer
//! yields whatever the extractor returns; no image is decoded here. A hosted OCR / parsing service
//! is reachable through
//! [`ExternalApiReader::for_parsing_service`](crate::reader::ExternalApiReader::for_parsing_service),
//! which already advertises `pdf` / `docx`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::{RagError, Result};
use crate::model::Document;
use crate::reader::external_api::{DEFAULT_CHUNK_SIZE, DEFAULT_OVERLAP_SIZE};
use crate::reader::{
    chunk_documents, doc_id_for_input, input_as_path, input_path_bytes,
    validate_chunking_parameters, Reader, ReaderInput, SplitStrategy,
};

/// The cargo feature that enables the bundled `pdftotext` extractor.
pub const FEATURE: &str = "pdf";

/// Extensions this reader claims (`PDFReader.getSupportedFormats`).
pub const SUPPORTED_FORMATS: [&str; 1] = ["pdf"];

/// The error text returned when the `pdf` feature is off.
pub const FEATURE_DISABLED_MESSAGE: &str = "PDF text extraction is unavailable: this build has the `pdf` feature disabled. Rebuild with `--features pdf` (bundled `pdftotext` extractor) or delegate parsing to a hosted service with `ExternalApiReader::for_parsing_service(..)`";

/// Extracts the plain text of one PDF document (the `PDFTextStripper` seam).
pub trait PdfTextExtractor: Send + Sync {
    /// Extracts text from `bytes`; `source` is the path the bytes came from (for diagnostics).
    fn extract_text(&self, bytes: &[u8], source: &str) -> Result<String>;
}

/// Whether this build ships the bundled PDF extractor (`pdf` feature on).
pub fn is_available() -> bool {
    cfg!(feature = "pdf")
}

/// Reads a PDF and chunks its extracted text into [`Document`]s.
pub struct PdfReader {
    chunk_size: usize,
    split_strategy: SplitStrategy,
    overlap_size: usize,
    extractor: Option<Arc<dyn PdfTextExtractor>>,
}

impl std::fmt::Debug for PdfReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PdfReader")
            .field("chunk_size", &self.chunk_size)
            .field("split_strategy", &self.split_strategy)
            .field("overlap_size", &self.overlap_size)
            .field("has_extractor", &self.extractor.is_some())
            .finish()
    }
}

impl PdfReader {
    /// Creates a reader with explicit chunking parameters.
    ///
    /// Fails with [`RagError::Reader`] when the `pdf` feature is disabled; the same error shape is
    /// used for invalid chunking parameters elsewhere ([`RagError::InvalidArgument`]).
    #[cfg_attr(not(feature = "pdf"), allow(unused_variables))]
    pub fn new(
        chunk_size: usize,
        split_strategy: SplitStrategy,
        overlap_size: usize,
    ) -> Result<Self> {
        validate_chunking_parameters(chunk_size, overlap_size)?;

        #[cfg(not(feature = "pdf"))]
        return Err(RagError::Reader(FEATURE_DISABLED_MESSAGE.to_string()));

        #[cfg(feature = "pdf")]
        Ok(Self {
            chunk_size,
            split_strategy,
            overlap_size,
            extractor: None,
        })
    }

    /// Creates a reader with the Java defaults (`512` / `PARAGRAPH` / `50`).
    pub fn with_defaults() -> Result<Self> {
        Self::new(
            DEFAULT_CHUNK_SIZE,
            SplitStrategy::Paragraph,
            DEFAULT_OVERLAP_SIZE,
        )
    }

    /// Injects the text extractor (`PdfReader::withExtractor`).
    pub fn with_extractor(mut self, extractor: Arc<dyn PdfTextExtractor>) -> Self {
        self.extractor = Some(extractor);
        self
    }

    /// Uses the bundled `pdftotext` extractor (`pdf` feature only).
    #[cfg(feature = "pdf")]
    pub fn with_builtin_extractor(self) -> Self {
        self.with_extractor(Arc::new(PdftotextExtractor::default()))
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

    /// Whether an extractor was injected.
    pub fn has_extractor(&self) -> bool {
        self.extractor.is_some()
    }
}

/// The error used when a reader exists but cannot extract text.
fn no_extractor_error() -> RagError {
    let message = if cfg!(feature = "pdf") {
        "no PDF text extractor configured: inject one with `PdfReader::with_extractor(..)` (or `with_builtin_extractor()`), or delegate to a hosted service with `ExternalApiReader::for_parsing_service(..)`"
    } else {
        FEATURE_DISABLED_MESSAGE
    };
    RagError::Reader(message.to_string())
}

#[async_trait]
impl Reader for PdfReader {
    async fn read(&self, input: ReaderInput) -> Result<Vec<Document>> {
        let source = input_as_path(&input);
        let bytes = input_path_bytes(&input)?;
        let extractor = self.extractor.as_ref().ok_or_else(no_extractor_error)?;
        let text = extractor.extract_text(&bytes, &source)?;
        let doc_id = doc_id_for_input(&input);
        chunk_documents(
            &text,
            &doc_id,
            self.chunk_size,
            self.split_strategy,
            self.overlap_size,
        )
    }

    fn supported_formats(&self) -> Vec<String> {
        SUPPORTED_FORMATS
            .iter()
            .map(|format| (*format).to_string())
            .collect()
    }
}

/// Stages `bytes` in a uniquely named temporary file and removes it after `action` ran.
#[cfg(feature = "pdf")]
fn with_temp_file<T>(
    bytes: &[u8],
    source: &str,
    extension: &str,
    action: impl FnOnce(&std::path::Path) -> T,
) -> Result<T> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!(
        "agentscope-rag-{}-{stamp}.{extension}",
        std::process::id()
    ));
    std::fs::write(&path, bytes).map_err(|error| {
        RagError::Reader(format!(
            "failed to stage {source} in {}: {error}",
            path.display()
        ))
    })?;
    let outcome = action(&path);
    std::fs::remove_file(&path).ok();
    Ok(outcome)
}

/// The bundled, dependency-free extractor: runs `pdftotext` (poppler-utils).
#[cfg(feature = "pdf")]
#[derive(Debug, Clone)]
pub struct PdftotextExtractor {
    binary: String,
}

#[cfg(feature = "pdf")]
impl Default for PdftotextExtractor {
    fn default() -> Self {
        Self {
            binary: "pdftotext".to_string(),
        }
    }
}

#[cfg(feature = "pdf")]
impl PdftotextExtractor {
    /// Uses `binary` instead of the `pdftotext` on `PATH`.
    pub fn new(binary: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
        }
    }

    /// The configured binary name.
    pub fn binary(&self) -> &str {
        &self.binary
    }
}

#[cfg(feature = "pdf")]
impl PdfTextExtractor for PdftotextExtractor {
    fn extract_text(&self, bytes: &[u8], source: &str) -> Result<String> {
        let output = with_temp_file(bytes, source, "pdf", |path| {
            std::process::Command::new(&self.binary)
                .arg(path)
                .arg("-")
                .output()
        })?;
        let output = output.map_err(|error| {
            RagError::Reader(format!(
                "failed to run `{}` for {source}: {error}",
                self.binary
            ))
        })?;
        if !output.status.success() {
            return Err(RagError::Reader(format!(
                "`{}` exited with {:?} for {source}: {}",
                self.binary,
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "pdf")]
    fn temp_file(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "rag-pdf-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"%PDF-1.4\n%%EOF\n").unwrap();
        path
    }

    #[test]
    fn the_reader_claims_only_pdf() {
        assert_eq!(SUPPORTED_FORMATS, ["pdf"]);
        assert_eq!(is_available(), cfg!(feature = "pdf"));
    }

    #[cfg(not(feature = "pdf"))]
    #[test]
    fn a_build_without_the_feature_reports_the_feature_and_the_alternative() {
        let error = PdfReader::with_defaults().unwrap_err();
        assert!(matches!(error, RagError::Reader(_)), "err={error:?}");
        let message = error.to_string();
        assert!(message.contains("`pdf` feature"), "err={message}");
        assert!(
            message.contains("ExternalApiReader::for_parsing_service"),
            "err={message}"
        );
    }

    #[cfg(feature = "pdf")]
    #[tokio::test]
    async fn an_injected_extractor_supplies_the_text_that_gets_chunked() {
        struct Fixed;

        impl PdfTextExtractor for Fixed {
            fn extract_text(&self, bytes: &[u8], source: &str) -> Result<String> {
                assert_eq!(
                    bytes, b"%PDF-1.4\n%%EOF\n",
                    "raw document bytes reach the seam"
                );
                assert!(source.ends_with(".pdf"));
                Ok("first page\n\nsecond page".to_string())
            }
        }

        let path = temp_file("injected.pdf");
        // 16 chars holds either paragraph ("first page" / "second page") without the
        // oversized-paragraph fallback, so paragraph chunking yields exactly two chunks.
        let reader = PdfReader::new(16, SplitStrategy::Paragraph, 0)
            .unwrap()
            .with_extractor(Arc::new(Fixed));
        assert!(reader.has_extractor());
        let documents = reader
            .read(ReaderInput::from_file(path.clone()))
            .await
            .unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(documents.len(), 2);
        assert_eq!(documents[0].metadata.chunk_id, "0");
        assert_eq!(documents[1].metadata.chunk_id, "1");
        assert_eq!(
            documents[0].metadata.doc_id,
            doc_id_for_input(&ReaderInput::from_file(path.clone()))
        );
        assert!(matches!(
            &documents[0].metadata.content,
            agentscope_core::message::ContentBlock::Text(_)
        ));
    }

    #[cfg(feature = "pdf")]
    #[test]
    fn the_builtin_extractor_reports_a_missing_binary() {
        let extractor = PdftotextExtractor::new("agentscope-definitely-missing-pdftotext");
        assert_eq!(
            extractor.binary(),
            "agentscope-definitely-missing-pdftotext"
        );
        let error = extractor.extract_text(b"%PDF-1.4", "doc.pdf").unwrap_err();
        assert!(matches!(error, RagError::Reader(_)), "err={error:?}");
        assert!(error.to_string().contains("doc.pdf"), "err={error}");
    }
}
