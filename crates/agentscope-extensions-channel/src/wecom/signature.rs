//! WeCom callback SHA-1 signature verification (the non-AES half of `WeComCrypto`).

use sha1::{Digest, Sha1};

use crate::common::{constant_time_eq, to_hex, ChannelError, Result};

/// Verifies the WeCom URL-handshake `signature`.
///
/// The signature is `SHA1(sort([token, timestamp, nonce, encrypt]).concat())` hex-encoded.
#[derive(Clone)]
pub struct WeComSignatureVerifier {
    token: String,
}

impl WeComSignatureVerifier {
    pub fn new(token: impl Into<String>) -> Result<Self> {
        let token = token.into();
        if token.is_empty() {
            return Err(ChannelError::Config("token is required".to_string()));
        }
        Ok(Self { token })
    }

    /// The hex signature for the given handshake parameters (exposed for tests / callers).
    pub fn sign(&self, timestamp: &str, nonce: &str, encrypt: &str) -> String {
        let mut parts = [self.token.as_str(), timestamp, nonce, encrypt];
        parts.sort_unstable();
        let mut hasher = Sha1::new();
        for part in parts {
            hasher.update(part.as_bytes());
        }
        to_hex(&hasher.finalize())
    }

    /// Constant-time verification against the caller-supplied `signature` query parameter.
    pub fn verify(
        &self,
        signature: Option<&str>,
        timestamp: &str,
        nonce: &str,
        encrypt: &str,
    ) -> bool {
        match signature {
            Some(signature) => constant_time_eq(&self.sign(timestamp, nonce, encrypt), signature),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_blank_token() {
        assert!(WeComSignatureVerifier::new("").is_err());
    }

    #[test]
    fn verifies_the_sorted_sha1_signature() {
        let v = WeComSignatureVerifier::new("tok").unwrap();
        let sig = v.sign("1700000000", "nonce1", "CIPHER");
        assert_eq!(sig.len(), 40);
        assert!(v.verify(Some(&sig), "1700000000", "nonce1", "CIPHER"));
        assert!(!v.verify(Some(&sig), "1700000001", "nonce1", "CIPHER"));
        assert!(!v.verify(None, "1700000000", "nonce1", "CIPHER"));
        assert!(!v.verify(Some("deadbeef"), "1700000000", "nonce1", "CIPHER"));
    }

    #[test]
    fn signature_is_order_independent_of_argument_order() {
        let v = WeComSignatureVerifier::new("tok").unwrap();
        assert_eq!(
            v.sign("b", "a", "ENC"),
            v.sign("a", "b", "ENC")
        );
    }
}
