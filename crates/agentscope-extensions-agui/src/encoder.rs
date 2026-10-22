//! SSE encoding for AG-UI events, mirroring `AguiEventEncoder`.
//!
//! Wire format (one frame per event, blank line terminated):
//!
//! ```text
//! data: {"type":"TEXT_MESSAGE_CONTENT",...}\n\n
//! ```
//!
//! Multi-line JSON is split into one `data:` line per line, as required by the SSE
//! specification. Java relies on `ObjectMapper`'s compact single-line output, so this
//! never triggers there; splitting keeps the frame valid when a JSON serializer emits
//! pretty-printed output.

use crate::error::{AguiError, AguiResult};
use crate::event::AguiEvent;

/// Encodes AG-UI events into Server-Sent Events frames.
#[derive(Debug, Clone, Copy, Default)]
pub struct AguiEventEncoder;

impl AguiEventEncoder {
    /// Creates an encoder.
    pub fn new() -> Self {
        Self
    }

    /// Encodes an event as a complete SSE frame (`data: <json>\n\n`).
    pub fn encode(&self, event: &AguiEvent) -> AguiResult<String> {
        let json = serde_json::to_string(event).map_err(AguiError::encoding)?;
        Ok(frame(&json))
    }

    /// Encodes an event to JSON only, with the leading space Java uses to reconcile
    /// Spring WebFlux's `data:` prefix with clients expecting `data: {...}`.
    pub fn encode_to_json(&self, event: &AguiEvent) -> AguiResult<String> {
        let json = serde_json::to_string(event).map_err(AguiError::encoding)?;
        Ok(format!(" {json}"))
    }

    /// Encodes an event as an `axum` SSE event (payload only, no `event:`/`id:` fields).
    pub fn to_sse_event(&self, event: &AguiEvent) -> AguiResult<axum::response::sse::Event> {
        let json = serde_json::to_string(event).map_err(AguiError::encoding)?;
        Ok(axum::response::sse::Event::default().data(json))
    }

    /// Encodes an SSE comment (used for keep-alives).
    pub fn encode_comment(&self, comment: &str) -> String {
        format!(": {comment}\n\n")
    }

    /// Encodes the standard keep-alive comment.
    pub fn keep_alive(&self) -> String {
        ": keep-alive\n\n".to_string()
    }
}

/// Builds an SSE `data` frame, splitting multi-line payloads into multiple `data:` lines.
fn frame(json: &str) -> String {
    let mut out = String::with_capacity(json.len() + 8);
    for line in json.split('\n') {
        out.push_str("data: ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::AguiEvent;

    fn content_event(delta: &str) -> AguiEvent {
        AguiEvent::TextMessageContent {
            thread_id: "t".to_string(),
            run_id: "r".to_string(),
            message_id: "m".to_string(),
            delta: delta.to_string(),
            timestamp: None,
            raw_event: None,
        }
    }

    #[test]
    fn encode_wraps_the_json_in_a_single_sse_frame() {
        let encoder = AguiEventEncoder::new();
        let encoded = encoder.encode(&content_event("hi")).unwrap();
        assert!(encoded.starts_with("data: {"));
        assert!(encoded.ends_with("\n\n"));
        assert_eq!(encoded.matches("\n\n").count(), 1);
        assert_eq!(encoded.matches("data: ").count(), 1);
        let json = encoded
            .trim_end_matches("\n\n")
            .strip_prefix("data: ")
            .unwrap();
        let decoded: AguiEvent = serde_json::from_str(json).unwrap();
        assert_eq!(decoded, content_event("hi"));
        assert_eq!(
            decoded.event_type().unwrap().as_str(),
            "TEXT_MESSAGE_CONTENT"
        );
    }

    #[test]
    fn encode_to_json_keeps_the_leading_space_for_sse_clients() {
        let encoder = AguiEventEncoder::new();
        let json = encoder.encode_to_json(&content_event("x")).unwrap();
        assert!(json.starts_with(" {\"type\""));
        assert!(!json.ends_with('\n'));
        assert_eq!(
            serde_json::from_str::<AguiEvent>(json.trim_start()).unwrap(),
            content_event("x")
        );
    }

    #[test]
    fn multi_line_payloads_become_one_data_line_per_line() {
        let encoder = AguiEventEncoder::new();
        // A delta containing a newline is escaped by serde_json, so the frame must stay
        // single-line; verify by forcing a multi-line payload through `frame` directly.
        let encoded = encoder.encode(&content_event("a\nb")).unwrap();
        assert_eq!(encoded.matches("data: ").count(), 1);
        assert_eq!(
            encoded.matches('\n').count(),
            2,
            "one line break plus the blank separator"
        );

        let multi = frame("{\n\"a\": 1\n}");
        assert_eq!(multi, "data: {\ndata: \"a\": 1\ndata: }\n\n");
    }

    #[test]
    fn comments_and_keep_alives_use_the_sse_comment_form() {
        let encoder = AguiEventEncoder::new();
        assert_eq!(encoder.encode_comment("ping"), ": ping\n\n");
        assert_eq!(encoder.keep_alive(), ": keep-alive\n\n");
    }

    #[test]
    fn axum_sse_event_carries_the_json_payload() {
        let encoder = AguiEventEncoder::new();
        let event = encoder.to_sse_event(&content_event("z")).unwrap();
        let rendered = format!("{event:?}");
        assert!(rendered.contains("TEXT_MESSAGE_CONTENT"), "got {rendered}");
    }

    #[test]
    fn frames_are_newline_terminated_even_for_empty_payloads() {
        assert_eq!(frame(""), "data: \n\n");
    }
}
