//! The queue message this adapter produces.

use serde::{Deserialize, Serialize};

use crate::envelope::Envelope;
use crate::error::EnvelopeError;
use crate::route::Route;

/// One RocketMQ message.
///
/// `tag`/`key` are filter and trace metadata; delivery correctness rests on `body`, which is an
/// [`Envelope`]. Serialisation is derived so a host that logs or persists a message out of band
/// can rebuild it without this crate's help.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    /// Topic the message belongs to (from [`Route::topic`]).
    pub topic: String,
    /// Tag used for subscription filtering (from [`Route::tag`]).
    pub tag: String,
    /// Business key, the durable session storage key (from [`Route::key`]).
    pub key: String,
    /// Encoded [`Envelope`].
    #[serde(with = "serde_bytes_hex")]
    pub body: Vec<u8>,
}

impl Message {
    /// Build a message from raw parts (used by hosts that persist messages themselves).
    pub fn new(
        topic: impl Into<String>,
        tag: impl Into<String>,
        key: impl Into<String>,
        body: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            topic: topic.into(),
            tag: tag.into(),
            key: key.into(),
            body: body.into(),
        }
    }

    /// Seal an envelope for `route`.
    ///
    /// TODO: RocketMQ caps a single message (Java's adapter uses 1 MiB, the broker default is
    /// 4 MiB); the Java module offloads a larger body to a shared `ObjectStore`. This crate has no
    /// object store contract to offload to, and truncating or compressing would change the bytes a
    /// consumer hashes, so an oversized event is left to fail at the broker instead of being
    /// silently reshaped here.
    pub fn seal(route: &Route, envelope: &Envelope) -> Result<Self, EnvelopeError> {
        Ok(Self {
            topic: route.topic.clone(),
            tag: route.tag.clone(),
            key: route.key.clone(),
            body: envelope.encode()?,
        })
    }
}

/// Bodies are bytes on the wire, but a JSON rendering is more useful in host logs and in any
/// records a host keeps outside the broker. Hex keeps the JSON round-trip exact and avoids the
/// base64 dependency.
mod serde_bytes_hex {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        let mut hex = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
        }
        serializer.serialize_str(&hex)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let hex = String::deserialize(deserializer)?;
        if !hex.is_ascii() {
            return Err(serde::de::Error::custom("message body hex is not ASCII"));
        }
        if hex.len() % 2 != 0 {
            return Err(serde::de::Error::custom("message body hex has an odd length"));
        }
        (0..hex.len())
            .step_by(2)
            .map(|index| {
                u8::from_str_radix(&hex[index..index + 2], 16).map_err(serde::de::Error::custom)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_survives_json_out_of_band() {
        let message = Message::new("t", "g", "k", vec![0, 1, 254, 255]);
        let json = serde_json::to_string(&message).expect("serialise");
        let restored: Message = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(restored, message);
    }

    #[test]
    fn an_odd_length_body_hex_is_rejected() {
        let json = r#"{"topic":"t","tag":"g","key":"k","body":"abc"}"#;
        assert!(serde_json::from_str::<Message>(json).is_err());
    }

    #[test]
    fn a_non_ascii_body_hex_is_rejected() {
        let json = r#"{"topic":"t","tag":"g","key":"k","body":"00éé"}"#;
        assert!(serde_json::from_str::<Message>(json).is_err());
    }
}
