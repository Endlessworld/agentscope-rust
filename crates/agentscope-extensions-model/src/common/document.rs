//! One-line document references for provider formatters.
//!
//! The rendering itself lives in core
//! ([`agentscope_core::formatter::render_document_reference`], mirroring
//! `DocumentReferenceRenderer`); this module only adds what the provider
//! converters need on top of it: appending the references of a message/block
//! list to an already assembled text.

use agentscope_core::formatter::render_document_reference;
use agentscope_core::{ContentBlock, DocumentBlock, Msg};

/// Renders a single document as a one-line reference, e.g.
/// `[document: report.pdf, application/pdf, url: https://example.com/report.pdf]`.
pub fn render(doc: &DocumentBlock) -> String {
    render_document_reference(doc)
}

/// Appends one reference per document carried by `msg` to `text`.
pub fn append(text: &str, msg: &Msg) -> String {
    append_blocks(text, &msg.content)
}

/// Appends one reference per document found in `blocks` to `text`.
pub fn append_blocks(text: &str, blocks: &[ContentBlock]) -> String {
    let references = blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Document(doc) => Some(render(doc)),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if references.is_empty() {
        return text.to_string();
    }
    if text.is_empty() {
        return references;
    }
    format!("{text}\n{references}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::{Base64Source, Source, TextBlock, URLSource};

    fn document(source: Source) -> DocumentBlock {
        DocumentBlock::builder()
            .source(source)
            .name("report.pdf")
            .mime_type("application/pdf")
            .build()
    }

    #[test]
    fn a_reference_names_the_file_type_and_location() {
        assert_eq!(
            render(&document(Source::Url(URLSource::new(
                "https://example.com/report.pdf"
            )))),
            "[document: report.pdf, application/pdf, url: https://example.com/report.pdf]"
        );
        assert_eq!(
            render(&document(Source::Base64(Base64Source::new(
                "application/pdf",
                "QUJD"
            )))),
            "[document: report.pdf, application/pdf, inline base64 (~3 bytes)]"
        );
    }

    #[test]
    fn append_keeps_existing_text_first() {
        let msg = Msg::user_blocks(vec![
            ContentBlock::Text(TextBlock::new("look at this")),
            ContentBlock::Document(document(Source::Url(URLSource::new(
                "https://example.com/report.pdf",
            )))),
        ]);
        assert_eq!(
            append("look at this", &msg),
            "look at this\n[document: report.pdf, application/pdf, url: https://example.com/report.pdf]"
        );
        let attached_only = Msg::user_blocks(vec![ContentBlock::Document(document(Source::Url(
            URLSource::new("https://example.com/report.pdf"),
        )))]);
        assert!(append("", &attached_only).starts_with("[document: report.pdf"));
    }

    #[test]
    fn only_documents_contribute_references() {
        let msg = Msg::user_blocks(vec![ContentBlock::Text(TextBlock::new("plain"))]);
        assert_eq!(append("plain", &msg), "plain");
    }
}
