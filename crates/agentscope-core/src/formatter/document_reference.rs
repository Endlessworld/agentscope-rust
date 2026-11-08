//! Renders a [`DocumentBlock`] as a one-line reference for text-only sinks.
//!
//! Mirrors `io.agentscope.core.formatter.DocumentReferenceRenderer`. Memory flushing,
//! conversation compaction, task results and transcript exports hand history to a text
//! consumer: the document body cannot travel with that prompt, but the fact that a file was
//! part of the conversation must not disappear. Base64 payloads are described by size
//! instead of being inlined — one attachment would otherwise consume the whole prompt budget
//! and put a multi-megabyte base64 blob into every sink that keeps it.

use crate::message::{ContentBlock, DocumentBlock, Msg, Source};

/// Renders a message as text with one reference appended per document.
///
/// Sinks that store or prompt on plain text would otherwise reduce an attachment-only turn
/// to an empty string and drop it entirely. `include_tool_results` lets a sink that already
/// renders tool results on its own (a transcript export, for instance) pass `false` so the
/// same document is not reported twice.
///
/// Mirrors `DocumentReferenceRenderer.renderMessageText`.
pub fn render_message_text(msg: &Msg, include_tool_results: bool) -> String {
    let mut text = msg.text_content();
    let mut documents: Vec<&DocumentBlock> = msg
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Document(document) => Some(document),
            _ => None,
        })
        .collect();
    if include_tool_results {
        // A tool result carries its own output blocks. They are part of the turn and used to
        // be invisible to every text sink, so include them here rather than in each caller.
        for block in &msg.content {
            if let ContentBlock::ToolResult(result) = block {
                documents.extend(result.output.iter().filter_map(|output| match output {
                    ContentBlock::Document(document) => Some(document),
                    _ => None,
                }));
            }
        }
    }
    for document in documents {
        if !text.is_empty() && !text.chars().next_back().is_some_and(char::is_whitespace) {
            text.push('\n');
        }
        text.push_str(&render(document));
    }
    text
}

/// Renders a single document reference, for example
/// `[document: report.pdf, application/pdf, url: https://example.com/report.pdf]`.
///
/// Mirrors `DocumentReferenceRenderer.render`.
pub fn render(document: &DocumentBlock) -> String {
    let mut reference = String::from("[document: ");
    let label = document
        .name
        .as_deref()
        .map(single_line)
        .filter(|label| !label.trim().is_empty())
        .or_else(|| {
            let id = single_line(&document.id);
            (!id.trim().is_empty()).then_some(id)
        })
        .unwrap_or_else(|| "unnamed".to_string());
    reference.push_str(&label);

    // The source often knows its own type; prefer it over losing the type entirely.
    let source_mime_type = match &document.source {
        Source::Base64(base64) => Some(base64.media_type.as_str()),
        Source::Url(url) => url.mime_type.as_deref(),
        Source::FileId(_) => None,
    };
    let mime_type = document
        .mime_type
        .as_deref()
        .or(source_mime_type)
        .map(single_line)
        .filter(|mime_type| !mime_type.trim().is_empty());
    if let Some(mime_type) = mime_type {
        reference.push_str(", ");
        reference.push_str(&mime_type);
    }

    match &document.source {
        Source::FileId(file) => {
            reference.push_str(", provider file id: ");
            reference.push_str(&single_line(&file.file_id));
            reference.push_str(" (");
            reference.push_str(&single_line(&file.provider));
            reference.push(')');
        }
        Source::Url(url) => {
            reference.push_str(", url: ");
            reference.push_str(&single_line(&url.url));
        }
        Source::Base64(base64) => {
            let bytes = document
                .size_bytes
                .unwrap_or_else(|| decoded_size(&base64.data));
            reference.push_str(&format!(", inline base64 (~{bytes} bytes)"));
        }
    }
    reference.push(']');
    reference
}

/// Flattens a reference field onto one line.
///
/// The renderer promises a one-line reference: a name copied from an untrusted header or a
/// multipart filename can carry CR/LF, which would break the line the caller trusted.
fn single_line(value: &str) -> String {
    value.replace("\r\n", " ").replace(['\r', '\n'], " ")
}

/// Estimates the decoded size of a Base64 payload, subtracting the padding characters.
fn decoded_size(data: &str) -> u64 {
    let padding = data.chars().rev().take_while(|c| *c == '=').count() as u64;
    (data.len() as u64 * 3 / 4).saturating_sub(padding)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{
        Base64Source, FileIdSource, MsgRole, TextBlock, ToolResultBlock, URLSource,
    };

    fn url_document(url: &str) -> DocumentBlock {
        DocumentBlock::builder()
            .source(Source::Url(URLSource::new(url)))
            .build()
    }

    #[test]
    fn a_provider_file_id_renders_with_its_provider() {
        let document = DocumentBlock::builder()
            .source(Source::FileId(FileIdSource::new("dashscope", "file-42")))
            .name("report.pdf")
            .mime_type("application/pdf")
            .build();
        assert_eq!(
            render(&document),
            "[document: report.pdf, application/pdf, provider file id: file-42 (dashscope)]"
        );
    }

    #[test]
    fn a_url_renders_instead_of_the_document_body() {
        let document = DocumentBlock::builder()
            .source(Source::Url(URLSource::new(
                "https://example.test/report.pdf",
            )))
            .name("report.pdf")
            .mime_type("application/pdf")
            .build();
        assert_eq!(
            render(&document),
            "[document: report.pdf, application/pdf, url: https://example.test/report.pdf]"
        );
    }

    #[test]
    fn the_generated_id_is_the_label_when_no_name_is_set() {
        let document = url_document("https://example.test/report.pdf");
        assert_eq!(
            render(&document),
            format!(
                "[document: {}, url: https://example.test/report.pdf]",
                document.id
            )
        );
    }

    #[test]
    fn a_blank_name_and_id_render_as_unnamed() {
        let document = DocumentBlock::builder()
            .source(Source::Url(URLSource::new(
                "https://example.test/report.pdf",
            )))
            .name(" \n")
            .id("\t\r\n")
            .build();
        assert_eq!(
            render(&document),
            "[document: unnamed, url: https://example.test/report.pdf]"
        );
    }

    #[test]
    fn an_inline_payload_is_described_by_size_never_inlined() {
        let document = DocumentBlock::builder()
            .source(Source::Base64(Base64Source::new(
                "application/pdf",
                "JVBERi0xLjQK",
            )))
            .name("report.pdf")
            .build();
        let reference = render(&document);
        assert_eq!(
            reference,
            "[document: report.pdf, application/pdf, inline base64 (~9 bytes)]"
        );
        assert!(!reference.contains("JVBERi0xLjQK"));
    }

    #[test]
    fn a_declared_size_wins_over_the_payload_estimate() {
        let document = DocumentBlock::builder()
            .source(Source::Base64(Base64Source::new(
                "application/pdf",
                "JVBERi0xLjQK",
            )))
            .name("report.pdf")
            .size_bytes(8_388_608)
            .build();
        assert_eq!(
            render(&document),
            "[document: report.pdf, application/pdf, inline base64 (~8388608 bytes)]"
        );
    }

    #[test]
    fn base64_padding_is_subtracted_from_the_size_estimate() {
        let document = DocumentBlock::builder()
            .source(Source::Base64(Base64Source::new("text/plain", "aGVsbG8=")))
            .name("notes.txt")
            .build();
        assert_eq!(
            render(&document),
            "[document: notes.txt, text/plain, inline base64 (~5 bytes)]"
        );
    }

    #[test]
    fn the_url_source_supplies_the_mime_type_when_the_block_has_none() {
        let document = DocumentBlock::builder()
            .source(Source::Url(URLSource {
                url: "https://example.test/report.pdf".into(),
                mime_type: Some("application/pdf".into()),
            }))
            .name("report.pdf")
            .build();
        assert_eq!(
            render(&document),
            "[document: report.pdf, application/pdf, url: https://example.test/report.pdf]"
        );
    }

    #[test]
    fn a_message_renders_as_text_plus_one_reference_per_document() {
        let msg = Msg::builder()
            .role(MsgRole::User)
            .content(vec![
                ContentBlock::Text(TextBlock::new("Please review both files.")),
                ContentBlock::Document(
                    DocumentBlock::builder()
                        .source(Source::Url(URLSource::new("https://example.test/a.pdf")))
                        .name("a.pdf")
                        .mime_type("application/pdf")
                        .build(),
                ),
                ContentBlock::Document(
                    DocumentBlock::builder()
                        .source(Source::FileId(FileIdSource::new("dashscope", "file-7")))
                        .name("b.docx")
                        .mime_type("application/msword")
                        .build(),
                ),
            ])
            .build();
        assert_eq!(
            render_message_text(&msg, true),
            "Please review both files.\n\
             [document: a.pdf, application/pdf, url: https://example.test/a.pdf]\n\
             [document: b.docx, application/msword, provider file id: file-7 (dashscope)]"
        );
    }

    #[test]
    fn a_document_only_message_has_no_leading_blank_line() {
        let msg = Msg::builder()
            .role(MsgRole::User)
            .content(vec![ContentBlock::Document(
                DocumentBlock::builder()
                    .source(Source::Url(URLSource::new("https://example.test/a.pdf")))
                    .name("a.pdf")
                    .mime_type("application/pdf")
                    .build(),
            )])
            .build();
        assert_eq!(
            render_message_text(&msg, true),
            "[document: a.pdf, application/pdf, url: https://example.test/a.pdf]"
        );
    }

    #[test]
    fn a_document_carried_by_a_tool_result_is_included_on_request() {
        let msg = Msg::builder()
            .role(MsgRole::Tool)
            .content(vec![ContentBlock::ToolResult(ToolResultBlock::new(
                "call-1",
                "fetch_report",
                vec![ContentBlock::Document(
                    DocumentBlock::builder()
                        .source(Source::Url(URLSource::new(
                            "https://example.test/report.pdf",
                        )))
                        .name("report.pdf")
                        .mime_type("application/pdf")
                        .build(),
                )],
            ))])
            .build();
        assert_eq!(
            render_message_text(&msg, true),
            "[document: report.pdf, application/pdf, url: https://example.test/report.pdf]"
        );
        // A sink that renders tool results on its own must not report the document twice.
        assert_eq!(render_message_text(&msg, false), "");
    }

    #[test]
    fn a_plain_text_message_is_returned_unchanged() {
        assert_eq!(render_message_text(&Msg::user("hello"), true), "hello");
    }

    #[test]
    fn a_reference_stays_on_one_line_when_fields_carry_line_breaks() {
        let document = DocumentBlock::builder()
            .source(Source::Url(URLSource::new(
                "https://example.test/report.pdf",
            )))
            .name("re\r\nport.pdf")
            .mime_type("application/pdf\n")
            .build();
        let reference = render(&document);
        assert!(!reference.contains('\n') && !reference.contains('\r'));
        assert!(reference.contains("re port.pdf"));
    }
}
