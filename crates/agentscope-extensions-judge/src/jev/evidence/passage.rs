//! Immutable, versioned retrieval evidence (`JevPassage`).
//!
//! Attributes are preserved for the host but never sent to the judge.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::JevError;

/// One retrieval passage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JevPassage {
    /// The passage id; bounded and non-blank.
    pub id: String,
    /// The passage text.
    pub text: String,
    /// The passage version; bounded and non-blank.
    pub version: String,
    /// Host attributes (ACL labels, source, score, ...); never sent to the judge.
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
}

impl JevPassage {
    /// Creates and validates a passage (`JevPassage` compact constructor).
    pub fn new(
        id: impl Into<String>,
        text: impl Into<String>,
        version: impl Into<String>,
        attributes: BTreeMap<String, String>,
    ) -> Result<Self, JevError> {
        let id = id.into();
        let version = version.into();
        if id.trim().is_empty()
            || id.chars().count() > 512
            || version.trim().is_empty()
            || version.chars().count() > 512
        {
            return Err(JevError::invalid("bounded passage id/version required"));
        }
        Ok(Self {
            id,
            text: text.into(),
            version,
            attributes,
        })
    }

    /// Creates a passage with no attributes.
    pub fn plain(
        id: impl Into<String>,
        text: impl Into<String>,
        version: impl Into<String>,
    ) -> Result<Self, JevError> {
        Self::new(id, text, version, BTreeMap::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passages_require_bounded_identifiers() {
        assert_eq!(
            JevPassage::plain("", "text", "v1").unwrap_err().message(),
            "bounded passage id/version required"
        );
        assert_eq!(
            JevPassage::plain("id", "text", "  ")
                .unwrap_err()
                .message(),
            "bounded passage id/version required"
        );
        assert_eq!(
            JevPassage::plain("x".repeat(513), "text", "v1")
                .unwrap_err()
                .message(),
            "bounded passage id/version required"
        );
        let passage = JevPassage::plain("id", "text", "v1").unwrap();
        assert!(passage.attributes.is_empty());
        assert_eq!(passage.id, "id");
        assert_eq!(
            serde_json::from_str::<JevPassage>(r#"{"id":"a","text":"t","version":"v"}"#)
                .unwrap()
                .text,
            "t"
        );
    }
}
