//! `GitHubSignatureVerifier` — verifies the `X-Hub-Signature-256` webhook header.

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::common::{constant_time_eq, to_hex, ChannelError, Result};

const PREFIX: &str = "sha256=";

/// Verifies the `sha256=<hex(HMAC-SHA256(secret, body))>` signature GitHub sends.
pub struct GitHubSignatureVerifier {
    secret: Vec<u8>,
}

impl GitHubSignatureVerifier {
    /// `webhook_secret` is required (mirrors the Java constructor validation).
    pub fn new(webhook_secret: &str) -> Result<Self> {
        if webhook_secret.trim().is_empty() {
            return Err(ChannelError::Config(
                "webhookSecret is required".to_string(),
            ));
        }
        Ok(Self {
            secret: webhook_secret.as_bytes().to_vec(),
        })
    }

    /// Constant-time verification of the header against the raw body.
    pub fn verify(&self, header: Option<&str>, raw_body: &[u8]) -> bool {
        let Some(header) = header else { return false };
        let Some(got) = header.strip_prefix(PREFIX) else {
            return false;
        };
        let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(&self.secret) else {
            return false;
        };
        mac.update(raw_body);
        let expected = to_hex(&mac.finalize().into_bytes());
        constant_time_eq(&expected, got)
    }

    /// Compute the `sha256=<hex>` header value for `body` (used by tests / callers).
    pub fn sign(&self, body: &[u8]) -> String {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.secret).expect("HMAC accepts any key length");
        mac.update(body);
        format!("{PREFIX}{}", to_hex(&mac.finalize().into_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_blank_secret() {
        assert!(GitHubSignatureVerifier::new("").is_err());
        assert!(GitHubSignatureVerifier::new("   ").is_err());
    }

    #[test]
    fn verifies_a_correct_signature() {
        let v = GitHubSignatureVerifier::new("topsecret").unwrap();
        let body = br#"{"action":"created"}"#;
        let header = v.sign(body);
        assert!(header.starts_with("sha256="));
        assert!(v.verify(Some(&header), body));
    }

    #[test]
    fn rejects_wrong_signature_and_bad_inputs() {
        let v = GitHubSignatureVerifier::new("topsecret").unwrap();
        let body = b"payload";
        assert!(!v.verify(Some("sha256=deadbeef"), body));
        assert!(!v.verify(Some("deadbeef"), body));
        assert!(!v.verify(None, body));
        assert!(!v.verify(Some(&v.sign(body)), b"different"));
    }
}
