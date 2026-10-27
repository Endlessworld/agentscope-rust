//! Metadata key contract for inbound media (image / audio / video / file) messages.
//!
//! Rust port of `io.agentscope.extensions.channel.common.ChannelMediaMetadata`. Channel adapters
//! deliberately do not decode provider media into content blocks: material identifiers
//! (`media_id` / `image_key` / `file_key` / `downloadCode`) are not URLs and can only be exchanged
//! for bytes through a token-authenticated download whose artifacts are short-lived. Deciding
//! whether and when to fetch, where to buffer, and how to degrade is application policy.
//!
//! Mappers therefore surface a media event as a user `Msg` whose text content is a neutral
//! [`marker_text`] marker and whose metadata carries the keys defined here, so the application can
//! observe the event, download the material through its own path, or reply with guidance.
//!
//! All values are strings. Keys absent on a given platform are omitted; readers must treat the
//! metadata as a sparse map and never assume a key's presence.

use serde_json::{Map, Value};

/// Maximum length, in code points, of the file-name copy embedded in [`marker_text`].
const MARKER_FILE_NAME_MAX_LENGTH: usize = 80;

/// Normalized media kind: image (photo / picture).
pub const KIND_IMAGE: &str = "image";
/// Normalized media kind: audio (voice message).
pub const KIND_AUDIO: &str = "audio";
/// Normalized media kind: video, including video-bearing composite messages.
pub const KIND_VIDEO: &str = "video";
/// Normalized media kind: generic file attachment.
pub const KIND_FILE: &str = "file";

/// Normalized media kind — one of [`KIND_IMAGE`], [`KIND_AUDIO`], [`KIND_VIDEO`], [`KIND_FILE`].
pub const KIND: &str = "channelMediaKind";
/// Original provider type token, verbatim (WeCom `MsgType`, Feishu `message_type`, DingTalk
/// `msgtype`) — e.g. `voice`, `picture`, `media`.
pub const PROVIDER_TYPE: &str = "channelMediaProviderType";
/// Primary provider material identifier (`MediaId` / `image_key` / `file_key` / `downloadCode`).
pub const ID: &str = "channelMediaId";
/// Secondary material identifier when the platform provides one (thumbnail / cover image /
/// picture-specific download code).
pub const SECONDARY_ID: &str = "channelMediaSecondaryId";
/// Directly fetchable URL when the platform provides one alongside the material id.
pub const URL: &str = "channelMediaUrl";
/// File name, when the platform reports one.
pub const FILE_NAME: &str = "channelMediaFileName";
/// Container / format token (WeCom voice `Format`, DingTalk video `videoType`).
pub const FORMAT: &str = "channelMediaFormat";
/// Playback duration in milliseconds, as reported by the platform.
pub const DURATION_MS: &str = "channelMediaDurationMs";
/// Platform speech-to-text result for audio messages, when the platform provides one.
pub const RECOGNITION: &str = "channelMediaRecognition";

/// Neutral marker text used as the `Msg` text content of a mapped media message: `[image]`,
/// `[audio]`, `[video]`, or `[file: report.pdf]` when a file name is available.
///
/// The marker keeps the event visible to text-only agents; applications are free to rewrite the
/// text before dispatching. File names are provider-controlled input, so the embedded copy is
/// sanitized ([`sanitize_marker_file_name`]); the raw name travels unmodified under [`FILE_NAME`].
pub fn marker_text(kind: &str, file_name: Option<&str>) -> String {
    let Some(file_name) = file_name else {
        return format!("[{kind}]");
    };
    if file_name.trim().is_empty() {
        return format!("[{kind}]");
    }
    let safe = sanitize_marker_file_name(file_name);
    if safe.is_empty() {
        return format!("[{kind}]");
    }
    format!("[{kind}: {safe}]")
}

/// Marker-safe copy of a provider-reported file name: brackets, control characters, invisible
/// formatting characters and Unicode line/paragraph separators become spaces (so a `]` cannot
/// close the marker early and a line break cannot start a new line of agent-visible text),
/// whitespace runs collapse to one space, and the copy is truncated to at most
/// [`MARKER_FILE_NAME_MAX_LENGTH`] code points.
pub fn sanitize_marker_file_name(file_name: &str) -> String {
    let mut replaced = String::with_capacity(file_name.len());
    for ch in file_name.chars() {
        let unsafe_char = ch == '['
            || ch == ']'
            || ch.is_control()
            || is_format_char(ch)
            || ch == '\u{2028}'
            || ch == '\u{2029}';
        replaced.push(if unsafe_char { ' ' } else { ch });
    }

    // Java `\s+` only matches ASCII whitespace by default.
    let mut collapsed = String::with_capacity(replaced.len());
    let mut in_space = false;
    for ch in replaced.chars() {
        if matches!(ch, ' ' | '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r') {
            if !in_space {
                collapsed.push(' ');
                in_space = true;
            }
        } else {
            collapsed.push(ch);
            in_space = false;
        }
    }

    let truncated: String = collapsed
        .trim()
        .chars()
        .take(MARKER_FILE_NAME_MAX_LENGTH)
        .collect();
    truncated.trim_end().to_string()
}

/// Insert `value` under `key` only when it is neither absent nor blank; platform-optional fields
/// are omitted so the metadata map stays sparse.
pub fn put_if_present(metadata: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        if !value.trim().is_empty() {
            metadata.insert(key.to_string(), Value::String(value.to_string()));
        }
    }
}

/// Approximation of the Unicode `Cf` (format) general category used by the Java sanitizer: zero
/// width spaces / joiners, bidirectional overrides, soft hyphen, tag characters and friends. Rust
/// `std` exposes no general-category API, so the ranges are listed explicitly.
fn is_format_char(ch: char) -> bool {
    matches!(ch,
        '\u{00AD}'
        | '\u{0600}'..='\u{0605}'
        | '\u{061C}'
        | '\u{06DD}'
        | '\u{070F}'
        | '\u{0890}'..='\u{0891}'
        | '\u{08E2}'
        | '\u{180E}'
        | '\u{200B}'..='\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{206F}'
        | '\u{FEFF}'
        | '\u{FFF9}'..='\u{FFFB}'
        | '\u{110BD}'
        | '\u{110CD}'
        | '\u{13430}'..='\u{1343F}'
        | '\u{1BCA0}'..='\u{1BCA3}'
        | '\u{1D173}'..='\u{1D17A}'
        | '\u{E0001}'
        | '\u{E0020}'..='\u{E007F}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_text_without_a_file_name_is_the_bare_kind() {
        assert_eq!(marker_text(KIND_IMAGE, None), "[image]");
        assert_eq!(marker_text(KIND_AUDIO, Some("   ")), "[audio]");
        assert_eq!(marker_text(KIND_VIDEO, Some("")), "[video]");
    }

    #[test]
    fn marker_text_embeds_the_sanitized_file_name() {
        assert_eq!(
            marker_text(KIND_FILE, Some("report.pdf")),
            "[file: report.pdf]"
        );
        // A `]` cannot close the marker early and invisible characters are dropped.
        assert_eq!(
            marker_text(KIND_FILE, Some("ev]il\u{200B}.pdf")),
            "[file: ev il .pdf]"
        );
        // Control characters and Unicode line separators become spaces, then collapse.
        assert_eq!(
            marker_text(KIND_FILE, Some("a\u{0007}\u{2028}b")),
            "[file: a b]"
        );
        assert_eq!(marker_text(KIND_FILE, Some("  a   b  ")), "[file: a b]");
    }

    #[test]
    fn marker_file_name_is_truncated_to_80_code_points() {
        let long = "é".repeat(100);
        let marker = marker_text(KIND_FILE, Some(&long));
        assert_eq!(marker, format!("[file: {}]", "é".repeat(80)));
        // Truncation counts code points, not bytes.
        assert_eq!(marker.chars().count(), "[file: ".chars().count() + 80 + 1);
    }

    #[test]
    fn fully_sanitized_names_fall_back_to_the_bare_marker() {
        assert_eq!(marker_text(KIND_FILE, Some("[]\u{200B}")), "[file]");
    }

    #[test]
    fn put_if_present_keeps_the_map_sparse() {
        let mut metadata = Map::new();
        put_if_present(&mut metadata, FORMAT, Some("amr"));
        put_if_present(&mut metadata, FILE_NAME, Some("  "));
        put_if_present(&mut metadata, ID, None);
        assert_eq!(metadata.len(), 1);
        assert_eq!(
            metadata.get(FORMAT),
            Some(&Value::String("amr".to_string()))
        );
    }
}
