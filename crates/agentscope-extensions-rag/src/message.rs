//! Bridges retrieved RAG documents into the core message model.
//!
//! Ports two Java core classes:
//! * `io.agentscope.core.rag.KnowledgeRetrievalTools` — the agentic-mode tool output
//!   (`formatDocumentsForTool`).
//! * `io.agentscope.core.rag.GenericRAGHook` — the automatic retrieval hook, which injects
//!   `<retrieved_knowledge>...</retrieved_knowledge>` as an extra **user** message.
//!
//! Because core Rust has no `RagMode`/hook layer of its own, both halves live in this crate:
//! [`crate::tool::KnowledgeRetrievalTool`] (agentic) and [`crate::hook`] (generic). This module
//! keeps only the pure mapping functions the two share, so callers can wire RAG however they
//! prefer.

use agentscope_core::message::{ContentBlock, Msg, MsgRole, TextBlock};

use crate::model::Document;

/// Opening tag of the injected knowledge block (kept byte-identical to Java).
pub const RETRIEVED_KNOWLEDGE_OPEN: &str =
    "<retrieved_knowledge>Use the following content from the knowledge base(s) if it is helpful:\n\n";

/// Closing tag of the injected knowledge block.
pub const RETRIEVED_KNOWLEDGE_CLOSE: &str = "\n</retrieved_knowledge>";

/// Renders documents in the agentic-tool textual format.
///
/// Mirrors Java `KnowledgeRetrievalTools.formatDocumentsForTool`: a header with the number
/// of documents, then for each document `Document {n} (Score: {score}):\n{content}\n\n`.
/// Scores are formatted with three decimals; documents without a score omit the
/// `(Score: ...)` part.
pub fn format_documents_for_tool(documents: &[Document]) -> String {
    if documents.is_empty() {
        return "No relevant documents found in the knowledge base.".to_string();
    }

    let mut out = String::new();
    out.push_str(&format!(
        "Retrieved {} relevant document(s):\n\n",
        documents.len()
    ));

    for (i, doc) in documents.iter().enumerate() {
        out.push_str(&format!("Document {}", i + 1));
        if let Some(score) = doc.score {
            out.push_str(&format!(" (Score: {score:.3})"));
        }
        out.push_str(":\n");
        out.push_str(&doc.metadata.content_text());
        out.push_str("\n\n");
    }

    out
}

/// Builds the `<retrieved_knowledge>` user message injected by the generic RAG hook.
///
/// Mirrors Java `GenericRAGHook.createEnhancedMessages`: the message is a `USER` message
/// named `"user"` whose blocks are the opening tag, then per document a score text block
/// followed by the document's own content block (preserving multimodal blocks), then the
/// closing tag.
pub fn build_retrieved_knowledge_msg(documents: &[Document]) -> Msg {
    let mut blocks: Vec<ContentBlock> = Vec::with_capacity(documents.len() * 2 + 2);

    blocks.push(ContentBlock::Text(TextBlock::new(RETRIEVED_KNOWLEDGE_OPEN)));

    for doc in documents {
        let score = doc.score.unwrap_or(0.0);
        blocks.push(ContentBlock::Text(TextBlock::new(format!(
            "- Score: {score:.3}, Content: "
        ))));
        blocks.push(doc.metadata.content.clone());
    }

    blocks.push(ContentBlock::Text(TextBlock::new(
        RETRIEVED_KNOWLEDGE_CLOSE,
    )));

    Msg::builder()
        .name("user")
        .role(MsgRole::User)
        .content(blocks)
        .build()
}

/// Finds the query text from the last `USER` message in a history.
///
/// Mirrors Java `GenericRAGHook.extractQueryFromMessages`: scans backwards and returns the
/// first `USER` message's text content; empty string when none is present.
pub fn extract_query_from_messages(messages: &[Msg]) -> String {
    for msg in messages.iter().rev() {
        if msg.role() == MsgRole::User {
            return msg.text_content();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DocumentMetadata;

    fn doc(text: &str, score: Option<f64>) -> Document {
        let mut d = Document::new(DocumentMetadata::text(text, "d", text));
        d.score = score;
        d
    }

    #[test]
    fn empty_documents_use_java_placeholder() {
        assert_eq!(
            format_documents_for_tool(&[]),
            "No relevant documents found in the knowledge base."
        );
    }

    #[test]
    fn tool_format_matches_java_layout() {
        let out = format_documents_for_tool(&[doc("alpha", Some(0.876)), doc("beta", None)]);
        assert_eq!(
            out,
            "Retrieved 2 relevant document(s):\n\nDocument 1 (Score: 0.876):\nalpha\n\nDocument 2:\nbeta\n\n"
        );
    }

    #[test]
    fn retrieved_knowledge_message_has_expected_shape() {
        let msg = build_retrieved_knowledge_msg(&[doc("alpha", Some(0.123))]);
        assert_eq!(msg.role(), MsgRole::User);
        assert_eq!(msg.name(), Some("user"));
        let text = msg.text_content();
        assert!(text.starts_with("<retrieved_knowledge>"));
        assert!(text.contains("- Score: 0.123, Content: "));
        assert!(text.contains("alpha"));
        assert!(text.trim_end().ends_with("</retrieved_knowledge>"));
    }

    #[test]
    fn missing_score_renders_as_zero() {
        let msg = build_retrieved_knowledge_msg(&[doc("alpha", None)]);
        assert!(msg.text_content().contains("- Score: 0.000, Content: "));
    }

    #[test]
    fn query_extraction_picks_last_user_message() {
        let history = vec![
            Msg::user("first"),
            Msg::assistant("reply"),
            Msg::user("second"),
            Msg::assistant("tool-ish reply"),
        ];
        assert_eq!(extract_query_from_messages(&history), "second");
    }

    #[test]
    fn query_extraction_is_empty_without_user_message() {
        let history = vec![Msg::assistant("only assistant")];
        assert_eq!(extract_query_from_messages(&history), "");
        assert_eq!(extract_query_from_messages(&[]), "");
    }
}
