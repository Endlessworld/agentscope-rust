//! Media utilities for multimodal content, mirroring `io.agentscope.core.formatter.MediaUtils`.
//!
//! Provides helpers to resolve MIME types, validate media extensions and convert between
//! local files / remote URLs and base64-encoded content.

use std::path::Path;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;

use crate::error::{Error, Result};
use crate::message::Source;

/// Warn threshold for a single media file (10 MiB).
pub const WARN_SIZE_BYTES: u64 = 10 * 1024 * 1024;
/// Hard limit for a single media file (50 MiB).
pub const MAX_SIZE_BYTES: u64 = 50 * 1024 * 1024;

/// Supported image extensions.
pub const SUPPORTED_IMAGE_EXTENSIONS: &[&str] =
    &["png", "jpg", "jpeg", "gif", "webp", "heic", "heif"];
/// Supported audio extensions.
pub const SUPPORTED_AUDIO_EXTENSIONS: &[&str] = &["wav", "mp3"];
/// Supported video extensions.
pub const SUPPORTED_VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mpeg", "mpg", "mov", "avi", "webm", "wmv", "flv", "3gp", "3gpp",
];

/// Returns `true` when `url` is a local file path (no recognized protocol scheme).
pub fn is_local_file(url: &str) -> bool {
    if url.is_empty() {
        return false;
    }
    !url.starts_with("http://")
        && !url.starts_with("https://")
        && !url.starts_with("ftp://")
        && !url.starts_with("file://")
        && !url.starts_with("oss://")
}

/// Returns `true` when `path` points to an existing local file.
pub fn is_file_exists(path: &str) -> bool {
    if !is_local_file(path) {
        return false;
    }
    Path::new(path).exists()
}

/// Extracts the file extension (without the leading dot) from a path or URL.
pub fn get_extension(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }

    let file_name = if is_local_file(path) {
        Path::new(path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
    } else {
        // URL: strip query/fragment, then take the segment after the last '/'.
        let cleaned = path.split(['?', '#']).next().unwrap_or_default();
        cleaned.rsplit('/').next().map(|s| s.to_string())
    };

    let Some(file_name) = file_name else {
        return String::new();
    };
    match file_name.rfind('.') {
        Some(idx) if idx < file_name.len() - 1 => file_name[idx + 1..].to_string(),
        _ => String::new(),
    }
}

/// Resolves the MIME type from a file extension.
pub fn determine_media_type(path: &str) -> String {
    let ext = get_extension(path).to_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "mp3" => "audio/mp3",
        "wav" => "audio/wav",
        "aiff" => "audio/aiff",
        "aac" => "audio/aac",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        "mp4" => "video/mp4",
        "mpeg" | "mpg" => "video/mpeg",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "webm" => "video/webm",
        "wmv" => "video/x-ms-wmv",
        "flv" => "video/x-flv",
        "3gpp" | "3gp" => "video/3gpp",
        // Document formats
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Resolves the MIME type from a [`Source`], preferring explicit hints over inference.
pub fn resolve_mime_type(source: &Source) -> Result<String> {
    match source {
        Source::Base64(b64) => Ok(b64.media_type.clone()),
        Source::Url(url_source) => {
            if let Some(hint) = &url_source.mime_type {
                if !hint.trim().is_empty() {
                    return Ok(hint.clone());
                }
            }
            let inferred = determine_media_type(&url_source.url);
            if inferred != "application/octet-stream" {
                return Ok(inferred);
            }
            Err(Error::Other(format!(
                "Cannot determine MIME type for URL '{}'; set URLSource.mimeType explicitly",
                url_source.url
            )))
        }
        Source::FileId(file) => Err(Error::Other(format!(
            "Provider file id '{}' carries no MIME type; set DocumentBlock.mimeType explicitly",
            file.file_id
        ))),
    }
}

/// Validates that an image path/URL has a supported extension.
pub fn validate_image_extension(url: &str) -> Result<()> {
    let ext = get_extension(url).to_lowercase();
    if !SUPPORTED_IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        return Err(Error::Other(format!(
            "\"{url}\" should end with one of {SUPPORTED_IMAGE_EXTENSIONS:?}"
        )));
    }
    Ok(())
}

/// Validates that an audio path/URL has a supported extension.
pub fn validate_audio_extension(url: &str) -> Result<()> {
    let ext = get_extension(url).to_lowercase();
    if !SUPPORTED_AUDIO_EXTENSIONS.contains(&ext.as_str()) {
        return Err(Error::Other(format!(
            "Unsupported audio file extension: {ext}, {SUPPORTED_AUDIO_EXTENSIONS:?} are supported"
        )));
    }
    Ok(())
}

/// Validates that a video path/URL has a supported extension.
pub fn validate_video_extension(url: &str) -> Result<()> {
    let ext = get_extension(url).to_lowercase();
    if !SUPPORTED_VIDEO_EXTENSIONS.contains(&ext.as_str()) {
        return Err(Error::Other(format!(
            "Unsupported video file extension: {ext}, {SUPPORTED_VIDEO_EXTENSIONS:?} are supported"
        )));
    }
    Ok(())
}

/// Resolves the audio format string ("wav" or "mp3") from a file extension.
pub fn determine_audio_format(path: &str) -> String {
    if get_extension(path).to_lowercase() == "wav" {
        "wav".to_string()
    } else {
        "mp3".to_string()
    }
}

/// Infers the audio format string from a MIME type, defaulting to "mp3".
pub fn infer_audio_format_from_media_type(media_type: Option<&str>) -> String {
    match media_type {
        Some(m) if m.contains("wav") => "wav".to_string(),
        Some(m) if m.contains("opus") => "opus".to_string(),
        Some(m) if m.contains("flac") => "flac".to_string(),
        _ => "mp3".to_string(),
    }
}

/// Reads a local file and returns its base64-encoded content.
pub fn file_to_base64(path: &str) -> Result<String> {
    let file_path = Path::new(path);
    if !file_path.exists() {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("File does not exist: {path}"),
        )));
    }
    check_file_size(file_path)?;
    let bytes = std::fs::read(file_path)?;
    Ok(BASE64.encode(bytes))
}

/// Validates that a file does not exceed the configured size limits.
fn check_file_size(path: &Path) -> Result<()> {
    let size = std::fs::metadata(path)?.len();
    if size > MAX_SIZE_BYTES {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("File too large: {size} bytes (max: {MAX_SIZE_BYTES})"),
        )));
    }
    if size > WARN_SIZE_BYTES {
        tracing::warn!("Large file detected: {} bytes at {}", size, path.display());
    }
    Ok(())
}

/// Converts a local file path into a `file://` protocol URL.
pub fn to_file_protocol_url(path: &str) -> Result<String> {
    let p = Path::new(path);
    if !p.exists() {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("File does not exist: {path}"),
        )));
    }
    let absolute = std::path::absolute(p)?;
    Ok(format!("file://{}", absolute.display()))
}

/// Converts a URL to a `file://` protocol URL for existing local files, otherwise leaves it as-is.
pub fn url_to_protocol_url(url: &str) -> Result<String> {
    if is_file_exists(url) {
        to_file_protocol_url(url)
    } else {
        Ok(url.to_string())
    }
}

/// Downloads a remote URL and returns its base64-encoded content.
pub async fn download_url_to_base64(url: &str) -> Result<String> {
    tracing::debug!("Downloading remote URL for base64 encoding: {url}");
    crate::tls::ensure_crypto_provider();

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| Error::Http(e.to_string()))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Http(e.to_string()))?;

    let status = response.status();
    if !status.is_success() {
        return Err(Error::HttpStatus {
            status: status.as_u16(),
            message: format!("Failed to download URL: HTTP {status} for {url}"),
        });
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| Error::Http(e.to_string()))?;

    if bytes.len() as u64 > MAX_SIZE_BYTES {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "Downloaded content too large: {} bytes (max: {MAX_SIZE_BYTES})",
                bytes.len()
            ),
        )));
    }
    if bytes.len() as u64 > WARN_SIZE_BYTES {
        tracing::warn!("Large download detected: {} bytes from {url}", bytes.len());
    }

    Ok(BASE64.encode(bytes))
}

/// Converts a local file or remote URL into a base64 data URL (`data:{mediaType};base64,...`).
pub async fn url_to_base64_data_url(url: &str) -> Result<String> {
    let base64 = if is_file_exists(url) {
        file_to_base64(url)?
    } else {
        download_url_to_base64(url).await?
    };
    let media_type = determine_media_type(url);
    Ok(format!("data:{media_type};base64,{base64}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Base64Source, URLSource};

    #[test]
    fn local_file_detection() {
        assert!(!is_local_file("https://example.com/a.png"));
        assert!(!is_local_file("http://example.com/a.png"));
        assert!(!is_local_file("ftp://example.com/a.png"));
        assert!(!is_local_file("file:///tmp/a.png"));
        assert!(!is_local_file("oss://bucket/a.png"));
        assert!(is_local_file("/tmp/a.png"));
        assert!(is_local_file("relative/path.png"));
        assert!(!is_local_file(""));
    }

    #[test]
    fn extension_extraction() {
        assert_eq!(get_extension("/tmp/image.PNG"), "PNG");
        assert_eq!(get_extension("https://example.com/a.jpeg?x=1"), "jpeg");
        assert_eq!(get_extension("noext"), "");
        assert_eq!(get_extension("trailing."), "");
        assert_eq!(get_extension(""), "");
    }

    #[test]
    fn media_type_mapping() {
        assert_eq!(determine_media_type("a.jpg"), "image/jpeg");
        assert_eq!(determine_media_type("a.png"), "image/png");
        assert_eq!(determine_media_type("a.mp3"), "audio/mp3");
        assert_eq!(determine_media_type("a.wav"), "audio/wav");
        assert_eq!(determine_media_type("a.mp4"), "video/mp4");
        // Documents
        assert_eq!(determine_media_type("document.pdf"), "application/pdf");
        assert_eq!(
            determine_media_type("a.unknown"),
            "application/octet-stream"
        );
    }

    #[test]
    fn resolve_mime_type_prefers_hint() {
        let b64 = Source::Base64(Base64Source::new("image/png", "AAAA"));
        assert_eq!(resolve_mime_type(&b64).unwrap(), "image/png");

        let url_hint = Source::Url(URLSource {
            url: "https://x/photo".into(),
            mime_type: Some("image/jpeg".into()),
        });
        assert_eq!(resolve_mime_type(&url_hint).unwrap(), "image/jpeg");

        let url_ext = Source::Url(URLSource {
            url: "https://x/photo.png".into(),
            mime_type: None,
        });
        assert_eq!(resolve_mime_type(&url_ext).unwrap(), "image/png");

        let url_unknown = Source::Url(URLSource {
            url: "https://x/photo".into(),
            mime_type: None,
        });
        assert!(resolve_mime_type(&url_unknown).is_err());
    }

    #[test]
    fn extension_validation() {
        assert!(validate_image_extension("a.png").is_ok());
        assert!(validate_image_extension("a.txt").is_err());
        assert!(validate_audio_extension("a.mp3").is_ok());
        assert!(validate_audio_extension("a.png").is_err());
        assert!(validate_video_extension("a.mp4").is_ok());
        assert!(validate_video_extension("a.txt").is_err());
    }

    #[test]
    fn audio_format_inference() {
        assert_eq!(determine_audio_format("a.wav"), "wav");
        assert_eq!(determine_audio_format("a.mp3"), "mp3");
        assert_eq!(determine_audio_format("a.ogg"), "mp3");
        assert_eq!(infer_audio_format_from_media_type(None), "mp3");
        assert_eq!(infer_audio_format_from_media_type(Some("audio/wav")), "wav");
        assert_eq!(
            infer_audio_format_from_media_type(Some("audio/opus")),
            "opus"
        );
        assert_eq!(
            infer_audio_format_from_media_type(Some("audio/flac")),
            "flac"
        );
    }

    #[test]
    fn file_to_base64_roundtrip() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("agentscope_media_{}.png", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"hello").unwrap();
        let encoded = file_to_base64(path.to_str().unwrap()).unwrap();
        assert_eq!(encoded, "aGVsbG8=");
        std::fs::remove_file(&path).unwrap();

        assert!(file_to_base64("/nonexistent/file.png").is_err());
    }

    #[test]
    fn file_protocol_url_conversion() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("agentscope_media_{}.png", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"x").unwrap();
        let proto = to_file_protocol_url(path.to_str().unwrap()).unwrap();
        assert!(proto.starts_with("file://"));
        let via_url = url_to_protocol_url(path.to_str().unwrap()).unwrap();
        assert!(via_url.starts_with("file://"));
        assert_eq!(
            url_to_protocol_url("https://example.com/a.png").unwrap(),
            "https://example.com/a.png"
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn data_url_for_local_file() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("agentscope_media_{}.png", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"hi").unwrap();
        let data_url = url_to_base64_data_url(path.to_str().unwrap())
            .await
            .unwrap();
        assert!(data_url.starts_with("data:image/png;base64,"));
        std::fs::remove_file(&path).unwrap();
    }
}
