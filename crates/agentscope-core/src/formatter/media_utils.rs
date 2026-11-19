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

/// Strips an optional `file://` scheme, returning the local path it denotes.
///
/// `is_local_file` treats `file://` URLs as local, so every consumer that opens
/// the path must strip the scheme first — otherwise the `strip_prefix("file://")`
/// branch in the callers is unreachable and the path never resolves.
pub fn file_url_to_path(url: &str) -> &str {
    url.strip_prefix("file://").unwrap_or(url)
}

/// Returns `true` when `url` is a local file path (no remote protocol scheme).
///
/// A `file://` URL is local too: the scheme only says "this is a path on this
/// machine", so it is resolved like a bare path.
pub fn is_local_file(url: &str) -> bool {
    if url.is_empty() {
        return false;
    }
    let path = file_url_to_path(url);
    !path.starts_with("http://")
        && !path.starts_with("https://")
        && !path.starts_with("ftp://")
        && !path.starts_with("oss://")
}

/// Returns `true` when `path` points to an existing local file.
pub fn is_file_exists(path: &str) -> bool {
    if !is_local_file(path) {
        return false;
    }
    Path::new(file_url_to_path(path)).exists()
}

/// Extracts the file extension (without the leading dot) from a path or URL.
pub fn get_extension(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }

    let file_name = if is_local_file(path) {
        Path::new(file_url_to_path(path))
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
///
/// The body is accumulated chunk by chunk so `MAX_SIZE_BYTES` aborts the
/// download instead of being checked after the whole payload is in memory: an
/// unbounded `bytes().await` let a runaway/hostile URL allocate gigabytes before
/// the size check ever ran.
pub async fn download_url_to_base64(url: &str) -> Result<String> {
    tracing::debug!("Downloading remote URL for base64 encoding: {url}");
    let bytes = download_url_bytes(url).await?;
    Ok(BASE64.encode(bytes))
}

/// Downloads `url`, aborting as soon as the body exceeds [`MAX_SIZE_BYTES`].
async fn download_url_bytes(url: &str) -> Result<Vec<u8>> {
    crate::tls::ensure_crypto_provider();

    let client = crate::tls::apply_tls_defaults(reqwest::Client::builder())
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

    // A lying or absent `content-length` is caught by the incremental check below.
    if let Some(len) = response.content_length() {
        if len > MAX_SIZE_BYTES {
            return Err(too_large_error(len, url));
        }
        if len > WARN_SIZE_BYTES {
            tracing::warn!("Large download detected: {len} bytes from {url}");
        }
    }

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    use futures::StreamExt as _;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| Error::Http(e.to_string()))?;
        if body.len() as u64 + chunk.len() as u64 > MAX_SIZE_BYTES {
            // `body` is dropped here: the oversized payload is never materialised.
            return Err(too_large_error(body.len() as u64 + chunk.len() as u64, url));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn too_large_error(size: u64, url: &str) -> Error {
    Error::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("Downloaded content too large: {size} bytes (max: {MAX_SIZE_BYTES}) for {url}"),
    ))
}

/// How long a probe result is reused.
///
/// A probe costs one network round trip, and the same history is re-serialized on
/// every turn; without a memo a long conversation with one remote image would re-probe
/// it on every single request.
const PROBE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// Bound on the memo, so a long-lived process cannot accumulate URLs forever.
const PROBE_CACHE_CAPACITY: usize = 128;

/// `(url, probed_at, verdict)` triples, oldest first.
static PROBE_CACHE: std::sync::OnceLock<
    std::sync::Mutex<Vec<(String, std::time::Instant, Option<String>)>>,
> = std::sync::OnceLock::new();

/// Cached [`probe_remote_media`]: `Some(verdict)` on a fresh hit.
fn cached_probe_result(url: &str) -> Option<Option<String>> {
    let cache = PROBE_CACHE.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    let mut entries = cache.lock().ok()?;
    entries.retain(|(_, at, _)| at.elapsed() < PROBE_CACHE_TTL);
    entries
        .iter()
        .find(|(key, _, _)| key == url)
        .map(|(_, _, verdict)| verdict.clone())
}

fn store_probe_result(url: &str, verdict: &Option<String>) {
    let cache = PROBE_CACHE.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    if let Ok(mut entries) = cache.lock() {
        entries.retain(|(key, _, _)| key != url);
        if entries.len() >= PROBE_CACHE_CAPACITY {
            entries.remove(0);
        }
        entries.push((url.to_string(), std::time::Instant::now(), verdict.clone()));
    }
}

/// Probes `url` through the memo above.
async fn probe_remote_media_memoized(url: &str) -> Option<String> {
    if let Some(verdict) = cached_probe_result(url) {
        return verdict;
    }
    let verdict = probe_remote_media(url).await;
    store_probe_result(url, &verdict);
    verdict
}

/// The HTTP client shared by every probe.
///
/// One client keeps the connection pool (and its TLS sessions) across probes;
/// building a client per call threw away both and re-ran the TLS setup.
static PROBE_CLIENT: std::sync::OnceLock<Option<reqwest::Client>> = std::sync::OnceLock::new();

fn probe_client() -> Option<&'static reqwest::Client> {
    PROBE_CLIENT
        .get_or_init(|| {
            crate::tls::ensure_crypto_provider();
            crate::tls::apply_tls_defaults(reqwest::Client::builder())
                .connect_timeout(std::time::Duration::from_secs(5))
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .ok()
        })
        .as_ref()
}

/// Checks that a remote media URL is reachable, returning `None` when it is.
///
/// Providers fetch media URLs themselves and answer an unreachable one with a
/// 400 for the whole request ("Failed to download multimodal content"), which
/// loses the turn. A `HEAD` first (cheap, no body) and a 1KiB ranged `GET` as a
/// fallback covers servers that reject `HEAD`. Only a definitive negative is
/// reported: an inconclusive probe never downgrades a working URL.
pub async fn probe_remote_media(url: &str) -> Option<String> {
    let Some(client) = probe_client() else {
        return Some("probe client unavailable".to_string());
    };
    match client.head(url).send().await {
        Ok(response) if response.status().is_success() => return None,
        Ok(response) => {
            let status = response.status();
            // 405/501 mean "no HEAD here", not "unreachable"; anything else in
            // 4xx/5xx is a definitive answer, but the ranged GET still gets a vote.
            if status.is_client_error() && !matches!(status.as_u16(), 405 | 501) {
                return Some(format!("HTTP {status}"));
            }
        }
        Err(e) => {
            if e.is_connect() {
                return Some(e.to_string());
            }
        }
    }
    let ranged = client
        .get(url)
        .header(reqwest::header::RANGE, "bytes=0-1023")
        .send()
        .await;
    match ranged {
        Ok(response) if response.status().is_success() || response.status().is_redirection() => {
            None
        }
        Ok(response) => Some(format!("HTTP {}", response.status())),
        Err(e) => Some(e.to_string()),
    }
}

/// Text placeholder replacing a media block whose bytes could not be fetched.
pub fn unavailable_media_placeholder(kind: &str, url: &str, reason: &str) -> String {
    format!("[{kind} unavailable: {url} ({reason})]")
}

/// Replaces every remote media block in `messages` whose URL cannot be fetched
/// with a text placeholder, so one dead URL cannot 400 the whole request.
///
/// Returns the number of replaced blocks.
pub async fn degrade_unreachable_media(messages: &mut [crate::message::Msg]) -> usize {
    let mut checked: std::collections::BTreeMap<String, Option<String>> =
        std::collections::BTreeMap::new();
    for msg in messages.iter() {
        for url in remote_media_urls(msg) {
            if !checked.contains_key(&url) {
                let reason = probe_remote_media_memoized(&url).await;
                checked.insert(url, reason);
            }
        }
    }
    apply_media_degradation(messages, &checked)
}

/// The distinct `http(s)` media URLs referenced by `msg`, in content order.
fn remote_media_urls(msg: &crate::message::Msg) -> Vec<String> {
    use crate::message::ContentBlock;
    let mut urls = Vec::new();
    for block in msg.content.iter() {
        let source = match block {
            ContentBlock::Image(block) => Some(&block.source),
            ContentBlock::Audio(block) => Some(&block.source),
            ContentBlock::Video(block) => Some(&block.source),
            _ => None,
        };
        if let Some(Source::Url(url)) = source {
            let value = &url.url;
            if (value.starts_with("http://") || value.starts_with("https://"))
                && !urls.contains(value)
            {
                urls.push(value.clone());
            }
        }
    }
    urls
}

/// Applies pre-computed probe results (`url -> Some(reason)` means unavailable).
///
/// Split out from [`degrade_unreachable_media`] so the replacement rule is
/// testable without network access.
fn apply_media_degradation(
    messages: &mut [crate::message::Msg],
    unavailable: &std::collections::BTreeMap<String, Option<String>>,
) -> usize {
    use crate::message::{ContentBlock, TextBlock};
    let mut replaced = 0;
    for msg in messages.iter_mut() {
        for block in msg.content_mut().iter_mut() {
            let placeholder = match block {
                ContentBlock::Image(block) => {
                    media_placeholder("Image", &block.source, unavailable)
                }
                ContentBlock::Audio(block) => {
                    media_placeholder("Audio", &block.source, unavailable)
                }
                ContentBlock::Video(block) => {
                    media_placeholder("Video", &block.source, unavailable)
                }
                _ => None,
            };
            if let Some(placeholder) = placeholder {
                tracing::warn!(
                    "remote media is unreachable; downgraded to a text placeholder: {placeholder}"
                );
                *block = ContentBlock::Text(TextBlock::new(placeholder));
                replaced += 1;
            }
        }
    }
    replaced
}

/// The placeholder for `source` when its URL was reported unavailable.
fn media_placeholder(
    kind: &str,
    source: &Source,
    unavailable: &std::collections::BTreeMap<String, Option<String>>,
) -> Option<String> {
    let Source::Url(url) = source else {
        return None;
    };
    match unavailable.get(&url.url) {
        Some(Some(reason)) => Some(unavailable_media_placeholder(kind, &url.url, reason)),
        _ => None,
    }
}

/// Converts a local file or remote URL into a base64 data URL (`data:{mediaType};base64,...`).
pub async fn url_to_base64_data_url(url: &str) -> Result<String> {
    let base64 = if is_file_exists(url) {
        file_to_base64(file_url_to_path(url))?
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
    fn the_probe_memo_reuses_and_bounds_its_entries() {
        // The probe is a network round trip and the same history is re-serialized every
        // turn, so a repeated URL must not be probed again, and the memo must not grow
        // without bound.
        store_probe_result(
            "https://example.invalid/a.png",
            &Some("unreachable".to_string()),
        );
        assert_eq!(
            cached_probe_result("https://example.invalid/a.png"),
            Some(Some("unreachable".to_string()))
        );
        assert_eq!(cached_probe_result("https://example.invalid/b.png"), None);

        for index in 0..(PROBE_CACHE_CAPACITY + 8) {
            store_probe_result(&format!("https://example.invalid/{index}.png"), &None);
        }
        let entries = PROBE_CACHE
            .get()
            .expect("cache initialised")
            .lock()
            .unwrap();
        assert!(
            entries.len() <= PROBE_CACHE_CAPACITY,
            "memo grew to {} entries",
            entries.len()
        );
    }

    #[test]
    fn local_file_detection() {
        assert!(!is_local_file("https://example.com/a.png"));
        assert!(!is_local_file("http://example.com/a.png"));
        assert!(!is_local_file("ftp://example.com/a.png"));
        // A `file://` URL points at this machine, so it is local: excluding it made
        // every `strip_prefix("file://")` branch in the callers unreachable.
        assert!(is_local_file("file:///tmp/a.png"));
        assert!(!is_local_file("oss://bucket/a.png"));
        assert!(is_local_file("/tmp/a.png"));
        assert!(is_local_file("relative/path.png"));
        assert!(!is_local_file(""));
        assert_eq!(file_url_to_path("file:///tmp/a.png"), "/tmp/a.png");
        assert_eq!(file_url_to_path("/tmp/a.png"), "/tmp/a.png");
    }

    #[test]
    fn a_file_url_resolves_like_its_path() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("agentscope_media_{}.png", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"hi").unwrap();
        let file_url = format!("file://{}", path.display());
        assert!(is_file_exists(&file_url), "{file_url} must resolve");
        assert_eq!(get_extension(&file_url), "png");
        assert_eq!(determine_media_type(&file_url), "image/png");
        std::fs::remove_file(&path).unwrap();
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
        // The `file://` form must inline the same bytes instead of hitting the network.
        let file_url = format!("file://{}", path.display());
        let via_file_url = url_to_base64_data_url(&file_url).await.unwrap();
        assert_eq!(via_file_url, data_url);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn an_unreachable_media_url_becomes_a_text_placeholder() {
        use crate::message::{ContentBlock, ImageBlock, Msg, MsgRole, TextBlock, URLSource};
        use std::collections::BTreeMap;

        let image =
            |url: &str| ContentBlock::Image(ImageBlock::new(Source::Url(URLSource::new(url))));
        let mut messages = vec![Msg::builder()
            .role(MsgRole::User)
            .content(vec![
                ContentBlock::Text(TextBlock::new("look")),
                image("https://example.invalid/dead.png"),
                image("https://example.invalid/live.png"),
            ])
            .build()];

        let mut unavailable = BTreeMap::new();
        unavailable.insert(
            "https://example.invalid/dead.png".to_string(),
            Some("HTTP 404 Not Found".to_string()),
        );
        unavailable.insert("https://example.invalid/live.png".to_string(), None);
        // Only the definitively unreachable URL is replaced.
        assert_eq!(apply_media_degradation(&mut messages, &unavailable), 1);
        assert_eq!(messages[0].content.len(), 3);
        assert!(matches!(
            &messages[0].content[1],
            ContentBlock::Text(t)
                if t.text.contains("Image unavailable") && t.text.contains("dead.png")
        ));
        assert!(matches!(&messages[0].content[2], ContentBlock::Image(_)));

        // The scan only reports http(s) media URLs, so a local source is never probed.
        let msg = Msg::builder()
            .role(MsgRole::User)
            .content(vec![image("file:///tmp/a.png")])
            .build();
        assert!(remote_media_urls(&msg).is_empty());
    }
}
