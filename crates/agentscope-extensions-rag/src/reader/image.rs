//! Image reader.
//!
//! Ports `io.agentscope.core.rag.reader.ImageReader`, which has no third-party JVM dependency: it
//! builds a `Document` whose content is an `ImageBlock` backed by a `URLSource`, with the doc id
//! derived from the MD5 of the image path (the Python implementation did the same). Objects are
//! never fetched or decoded here, so the reader works fully offline and lets a multimodal
//! embedding/LLM layer resolve the URL later.
//!
//! OCR is a placeholder in Java too (`extractTextFromImage` returned `""` and was never called);
//! the flag is kept on the reader so callers can introspect it.

use agentscope_core::message::{ContentBlock, ImageBlock, Source, URLSource};
use async_trait::async_trait;

use crate::error::Result;
use crate::model::{Document, DocumentMetadata};
use crate::reader::utils::generate_doc_id_md5;
use crate::reader::{Reader, ReaderInput};

/// Extensions supported by [`ImageReader`] (Java `getSupportedFormats`).
pub const SUPPORTED_FORMATS: [&str; 7] = ["jpg", "jpeg", "png", "gif", "bmp", "tiff", "webp"];

/// Reads an image path / URL into a single image [`Document`].
#[derive(Debug, Clone)]
pub struct ImageReader {
    enable_ocr: bool,
}

impl Default for ImageReader {
    fn default() -> Self {
        Self::new(false)
    }
}

impl ImageReader {
    /// Creates a reader; `enable_ocr` mirrors the Java constructor flag.
    pub fn new(enable_ocr: bool) -> Self {
        Self { enable_ocr }
    }

    /// Whether OCR was requested (Java `isOcrEnabled`).
    pub fn is_ocr_enabled(&self) -> bool {
        self.enable_ocr
    }

    /// Builds the single-document result for `image_path` (Java `loadImageDocument`).
    fn load_image_document(&self, image_path: &str) -> Document {
        let doc_id = generate_doc_id_md5(image_path);
        let url = image_url(image_path);
        let content = ContentBlock::Image(ImageBlock::builder().source(url).build());
        Document::new(DocumentMetadata::new(content, doc_id, "0"))
    }
}

#[async_trait]
impl Reader for ImageReader {
    async fn read(&self, input: ReaderInput) -> Result<Vec<Document>> {
        Ok(vec![self.load_image_document(&input_as_path(&input))])
    }

    fn supported_formats(&self) -> Vec<String> {
        SUPPORTED_FORMATS
            .iter()
            .map(|format| (*format).to_string())
            .collect()
    }
}

/// Resolves a [`ReaderInput`] to the path / URL string the reader needs.
///
/// Java's `ReaderInput.asString()` returned whatever string the input was built with, which for
/// `fromFile(path)` is the path. Rust's [`ReaderInput::as_string`] eagerly reads file *contents*,
/// which an image reader must not do, so the path is taken directly here.
fn input_as_path(input: &ReaderInput) -> String {
    match input {
        ReaderInput::String(value) => value.clone(),
        ReaderInput::File(path) => path.display().to_string(),
    }
}

/// Whether `path` is already a URL (Java `isUrl`, case-insensitive).
fn is_url(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("file://")
}

/// Builds the `URLSource` for `image_path`: URLs are kept, local paths get a `file://` scheme.
fn image_url(image_path: &str) -> Source {
    let url = if is_url(image_path) {
        image_path.to_string()
    } else {
        format!("file://{image_path}")
    };
    Source::Url(URLSource::new(url))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image_block(document: &Document) -> &ImageBlock {
        match &document.metadata.content {
            ContentBlock::Image(image) => image,
            other => panic!("expected an image block, got {other:?}"),
        }
    }

    fn source_url(document: &Document) -> &str {
        match &image_block(document).source {
            Source::Url(url) => &url.url,
            other => panic!("expected a URL source, got {other:?}"),
        }
    }

    #[test]
    fn ocr_flag_defaults_to_disabled_and_round_trips() {
        assert!(!ImageReader::default().is_ocr_enabled());
        assert!(!ImageReader::new(false).is_ocr_enabled());
        assert!(ImageReader::new(true).is_ocr_enabled());
    }

    #[test]
    fn supported_formats_match_java() {
        let formats = ImageReader::default().supported_formats();
        assert_eq!(
            formats,
            vec!["jpg", "jpeg", "png", "gif", "bmp", "tiff", "webp"]
        );
    }

    #[tokio::test]
    async fn url_input_is_kept_verbatim() {
        let documents = ImageReader::default()
            .read(ReaderInput::from_string("https://example.com/image.png"))
            .await
            .unwrap();

        assert_eq!(documents.len(), 1);
        assert_eq!(source_url(&documents[0]), "https://example.com/image.png");
    }

    #[tokio::test]
    async fn local_path_gets_a_file_scheme() {
        let documents = ImageReader::default()
            .read(ReaderInput::from_string("images/cat.png"))
            .await
            .unwrap();
        assert_eq!(source_url(&documents[0]), "file://images/cat.png");
    }

    #[tokio::test]
    async fn file_url_is_not_prefixed_twice() {
        let documents = ImageReader::default()
            .read(ReaderInput::from_string("file:///tmp/cat.png"))
            .await
            .unwrap();
        assert_eq!(source_url(&documents[0]), "file:///tmp/cat.png");
    }

    #[tokio::test]
    async fn file_input_uses_the_path_not_the_contents() {
        let path = std::env::temp_dir().join(format!(
            "rag-image-reader-{}-{}.png",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"not really a png").unwrap();

        let documents = ImageReader::default()
            .read(ReaderInput::from_file(path.clone()))
            .await
            .unwrap();
        assert_eq!(
            source_url(&documents[0]),
            format!("file://{}", path.display())
        );

        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn doc_id_is_the_md5_of_the_path_and_chunk_id_is_zero() {
        let documents = ImageReader::default()
            .read(ReaderInput::from_string("test.jpg"))
            .await
            .unwrap();
        let metadata = &documents[0].metadata;
        assert_eq!(metadata.doc_id, generate_doc_id_md5("test.jpg"));
        assert_eq!(metadata.doc_id, "0412c29576c708cf0155e8de242169b1");
        assert_eq!(metadata.chunk_id, "0");
        // Document id is the deterministic v3 UUID over the metadata.
        assert_eq!(documents[0].id.len(), 36);
        assert_eq!(&documents[0].id[14..15], "3");
    }

    #[tokio::test]
    async fn doc_id_is_stable_and_path_sensitive() {
        let reader = ImageReader::default();
        let first = reader
            .read(ReaderInput::from_string("a.png"))
            .await
            .unwrap();
        let second = reader
            .read(ReaderInput::from_string("a.png"))
            .await
            .unwrap();
        let other = reader
            .read(ReaderInput::from_string("b.png"))
            .await
            .unwrap();

        assert_eq!(first[0].id, second[0].id);
        assert_ne!(first[0].id, other[0].id);
        assert_eq!(first[0].metadata.payload_map().len(), 0);
    }
}
