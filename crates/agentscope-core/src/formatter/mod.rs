//! Formatter layer: response format and JSON schema configuration.
//! Mirrors `io.agentscope.core.formatter`.

pub mod json_schema;
pub mod media_utils;
pub mod response_format;

pub use json_schema::{JsonSchema, JsonSchemaBuilder};
pub use media_utils::{
    determine_audio_format, determine_media_type, download_url_to_base64, file_to_base64,
    get_extension, infer_audio_format_from_media_type, is_file_exists, is_local_file,
    resolve_mime_type, to_file_protocol_url, url_to_base64_data_url, url_to_protocol_url,
    validate_audio_extension, validate_image_extension, validate_video_extension, MAX_SIZE_BYTES,
    SUPPORTED_AUDIO_EXTENSIONS, SUPPORTED_IMAGE_EXTENSIONS, SUPPORTED_VIDEO_EXTENSIONS,
    WARN_SIZE_BYTES,
};
pub use response_format::{ResponseFormat, ResponseFormatBuilder};
