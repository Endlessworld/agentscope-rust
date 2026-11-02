//! Word (`.docx`) reader (`io.agentscope.core.rag.reader.WordReader`).
//!
//! Java's `WordReader` extracts text with Apache POI and then chunks it like `TextReader`. This
//! workspace bundles **no** Office/zip library (tracker RAG-09), so the port follows [`pdf`]:
//!
//! * [`DocxTextExtractor`] is the extraction seam ([`DocxReader::with_extractor`]).
//! * the optional **`docx`** cargo feature (off by default) adds [`PandocExtractor`], a
//!   dependency-free extractor that runs `pandoc -t plain` on a temporary file, plus
//!   [`DocxReader::with_builtin_extractor`].
//! * with the feature off [`DocxReader::new`] fails with a [`RagError::Reader`] naming the feature
//!   and the alternatives.
//!
//! Only the OOXML `.docx` container is claimed. The legacy binary `.doc` format (POI's `HWPF`) is
//! out of scope: it is a different, unmaintained container and `pandoc` does not read it either.
//! A hosted parser is reachable through
//! [`ExternalApiReader::for_parsing_service`](crate::reader::ExternalApiReader::for_parsing_service).

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::{RagError, Result};
use crate::model::Document;
use crate::reader::external_api::{DEFAULT_CHUNK_SIZE, DEFAULT_OVERLAP_SIZE};
use crate::reader::{
    chunk_documents, doc_id_for_input, input_as_path, input_path_bytes,
    validate_chunking_parameters, Reader, ReaderInput, SplitStrategy,
};

/// The cargo feature that enables the bundled `pandoc` extractor.
pub const FEATURE: &str = "docx";

/// Extensions this reader claims (`WordReader.getSupportedFormats`).
pub const SUPPORTED_FORMATS: [&str; 1] = ["docx"];

/// The error text returned when the `docx` feature is off.
pub const FEATURE_DISABLED_MESSAGE: &str = "DOCX text extraction is unavailable: this build has the `docx` feature disabled. Rebuild with `--features docx` (bundled `pandoc` extractor) or delegate parsing to a hosted service with `ExternalApiReader::for_parsing_service(..)`";

/// Extracts the plain text of one `.docx` document (the POI `XWPFWordExtractor` seam).
pub trait DocxTextExtractor: Send + Sync {
    /// Extracts text from `bytes`; `source` is the path the bytes came from (for diagnostics).
    fn extract_text(&self, bytes: &[u8], source: &str) -> Result<String>;
}

/// Whether this build ships the bundled DOCX extractor (`docx` feature on).
pub fn is_available() -> bool {
    cfg!(feature = "docx")
}

/// Reads a `.docx` and chunks its extracted text into [`Document`]s.
pub struct DocxReader {
    chunk_size: usize,
    split_strategy: SplitStrategy,
    overlap_size: usize,
    extractor: Option<Arc<dyn DocxTextExtractor>>,
}

impl std::fmt::Debug for DocxReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocxReader")
            .field("chunk_size", &self.chunk_size)
            .field("split_strategy", &self.split_strategy)
            .field("overlap_size", &self.overlap_size)
            .field("has_extractor", &self.extractor.is_some())
            .finish()
    }
}

impl DocxReader {
    /// Creates a reader with explicit chunking parameters.
    ///
    /// Fails with [`RagError::Reader`] when the `docx` feature is disabled.
    #[cfg_attr(not(feature = "docx"), allow(unused_variables))]
    pub fn new(
        chunk_size: usize,
        split_strategy: SplitStrategy,
        overlap_size: usize,
    ) -> Result<Self> {
        validate_chunking_parameters(chunk_size, overlap_size)?;

        #[cfg(not(feature = "docx"))]
        return Err(RagError::Reader(FEATURE_DISABLED_MESSAGE.to_string()));

        #[cfg(feature = "docx")]
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

    /// Injects the text extractor (`WordReader::withExtractor`).
    pub fn with_extractor(mut self, extractor: Arc<dyn DocxTextExtractor>) -> Self {
        self.extractor = Some(extractor);
        self
    }

    /// Uses the bundled `pandoc` extractor (`docx` feature only).
    #[cfg(feature = "docx")]
    pub fn with_builtin_extractor(self) -> Self {
        self.with_extractor(Arc::new(PandocExtractor::default()))
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
    let message = if cfg!(feature = "docx") {
        "no DOCX text extractor configured: inject one with `DocxReader::with_extractor(..)` (or `with_builtin_extractor()`), or delegate to a hosted service with `ExternalApiReader::for_parsing_service(..)`"
    } else {
        FEATURE_DISABLED_MESSAGE
    };
    RagError::Reader(message.to_string())
}

#[async_trait]
impl Reader for DocxReader {
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

/// The bundled, dependency-free extractor: runs `pandoc -t plain`.
#[cfg(feature = "docx")]
#[derive(Debug, Clone)]
pub struct PandocExtractor {
    binary: String,
}

#[cfg(feature = "docx")]
impl Default for PandocExtractor {
    fn default() -> Self {
        Self {
            binary: "pandoc".to_string(),
        }
    }
}

#[cfg(feature = "docx")]
impl PandocExtractor {
    /// Uses `binary` instead of the `pandoc` on `PATH`.
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

#[cfg(feature = "docx")]
impl DocxTextExtractor for PandocExtractor {
    fn extract_text(&self, bytes: &[u8], source: &str) -> Result<String> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "agentscope-rag-{}-{stamp}.docx",
            std::process::id()
        ));
        std::fs::write(&path, bytes).map_err(|error| {
            RagError::Reader(format!(
                "failed to stage {source} in {}: {error}",
                path.display()
            ))
        })?;
        let output = std::process::Command::new(&self.binary)
            .arg("-t")
            .arg("plain")
            .arg(&path)
            .output();
        std::fs::remove_file(&path).ok();
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

    #[cfg(feature = "docx")]
    fn temp_file(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "rag-docx-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"PK\x03\x04docx-bytes").unwrap();
        path
    }

    #[test]
    fn the_reader_claims_only_docx() {
        assert_eq!(SUPPORTED_FORMATS, ["docx"]);
        assert_eq!(is_available(), cfg!(feature = "docx"));
    }

    #[cfg(not(feature = "docx"))]
    #[test]
    fn a_build_without_the_feature_reports_the_feature_and_the_alternative() {
        let error = DocxReader::with_defaults().unwrap_err();
        assert!(matches!(error, RagError::Reader(_)), "err={error:?}");
        let message = error.to_string();
        assert!(message.contains("`docx` feature"), "err={message}");
        assert!(
            message.contains("ExternalApiReader::for_parsing_service"),
            "err={message}"
        );
    }

    #[cfg(feature = "docx")]
    #[tokio::test]
    async fn an_injected_extractor_supplies_the_text_that_gets_chunked() {
        struct Fixed;

        impl DocxTextExtractor for Fixed {
            fn extract_text(&self, bytes: &[u8], source: &str) -> Result<String> {
                assert!(!bytes.is_empty(), "raw document bytes reach the seam");
                assert!(source.ends_with(".docx"));
                Ok("aaaa\n\nbbbb\n\ncccc".to_string())
            }
        }

        let path = temp_file("injected.docx");
        let reader = DocxReader::new(4, SplitStrategy::Paragraph, 0)
            .unwrap()
            .with_extractor(Arc::new(Fixed));
        let documents = reader
            .read(ReaderInput::from_file(path.clone()))
            .await
            .unwrap();
        let expected_doc_id = doc_id_for_input(&ReaderInput::from_file(path.clone()));
        std::fs::remove_file(&path).ok();

        assert_eq!(documents.len(), 3);
        assert!(documents
            .iter()
            .all(|document| document.metadata.doc_id == expected_doc_id));
        assert_eq!(documents[2].metadata.chunk_id, "2");
    }

    #[cfg(feature = "docx")]
    #[test]
    fn the_builtin_extractor_reports_a_missing_binary() {
        let extractor = PandocExtractor::new("agentscope-definitely-missing-pandoc");
        let error = extractor.extract_text(b"PK", "doc.docx").unwrap_err();
        assert!(matches!(error, RagError::Reader(_)), "err={error:?}");
        assert!(error.to_string().contains("doc.docx"), "err={error}");
    }
}
