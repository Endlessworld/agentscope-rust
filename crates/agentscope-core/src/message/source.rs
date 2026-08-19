use serde::{Deserialize, Serialize};

/// Media source for binary content blocks (URL or Base64).
///
/// Mirrors the `Source` polymorphic hierarchy in the Java SDK: serialized with a `type`
/// discriminator (`"url"` / `"base64"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Source {
    /// URL-based media source.
    Url(URLSource),
    /// Base64-encoded media source.
    Base64(Base64Source),
}

/// URL-based media content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct URLSource {
    /// The URL pointing to the media content.
    pub url: String,
    /// Optional MIME type hint (e.g. `image/jpeg`) for extension-less URLs.
    #[serde(rename = "mime_type", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

impl URLSource {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            mime_type: None,
        }
    }

    pub fn builder() -> URLSourceBuilder {
        URLSourceBuilder::default()
    }
}

#[derive(Debug, Default)]
pub struct URLSourceBuilder {
    url: Option<String>,
    mime_type: Option<String>,
}

impl URLSourceBuilder {
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    pub fn mime_type(mut self, mime_type: impl Into<String>) -> Self {
        self.mime_type = Some(mime_type.into());
        self
    }

    pub fn build(self) -> URLSource {
        URLSource {
            url: self.url.unwrap_or_default(),
            mime_type: self.mime_type,
        }
    }
}

/// Base64-encoded media content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Base64Source {
    /// MIME type of the media content (e.g. `image/jpeg`).
    #[serde(rename = "media_type")]
    pub media_type: String,
    /// Base64-encoded media data.
    pub data: String,
}

impl Base64Source {
    pub fn new(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            media_type: media_type.into(),
            data: data.into(),
        }
    }

    pub fn builder() -> Base64SourceBuilder {
        Base64SourceBuilder::default()
    }
}

#[derive(Debug, Default)]
pub struct Base64SourceBuilder {
    media_type: Option<String>,
    data: Option<String>,
}

impl Base64SourceBuilder {
    pub fn media_type(mut self, media_type: impl Into<String>) -> Self {
        self.media_type = Some(media_type.into());
        self
    }

    pub fn data(mut self, data: impl Into<String>) -> Self {
        self.data = Some(data.into());
        self
    }

    pub fn build(self) -> Base64Source {
        Base64Source {
            media_type: self.media_type.unwrap_or_default(),
            data: self.data.unwrap_or_default(),
        }
    }
}
