//! `FeishuCrypto` — Feishu callback payload decryption + `X-Lark-Signature` verification.
//!
//! Rust port of `io.agentscope.extensions.channel.feishu.FeishuCrypto`.
//!
//! Feishu encrypts the callback body when an Encrypt Key is configured in the developer console.
//! The wire format is JSON `{"encrypt":"<base64-ciphertext>"}`. The AES key is derived as
//! `SHA-256(encryptKey)` (32 bytes). The ciphertext layout is:
//!
//! ```text
//! | 16 bytes IV | AES-256-CBC + PKCS#7 padded body |
//! ```
//!
//! The signature header `X-Lark-Signature` is
//! `hex(SHA-256(timestamp + nonce + encryptKey + rawBody))`, where `rawBody` is the raw HTTP
//! request body (the JSON-stringified envelope, not the decrypted plaintext).
//!
//! See <https://open.feishu.cn/document/server-docs/event-subscription-guide/event-subscription-configure-/encrypt-key-encryption-configuration-case>.

use sha2::{Digest, Sha256};

use crate::common::crypto::{aes_256_cbc_decrypt, pkcs7_unpad, AES_BLOCK_SIZE};
use crate::common::{constant_time_eq, to_hex, ChannelError, Result};

/// Feishu AES-256-CBC payload decryption + SHA-256 signature verification.
#[derive(Clone)]
pub struct FeishuCrypto {
    encrypt_key: String,
    /// 32 bytes, `SHA-256(encryptKey)`.
    aes_key: [u8; 32],
}

impl std::fmt::Debug for FeishuCrypto {
    /// Never render key material in logs / test output.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeishuCrypto")
            .field("encrypt_key", &"<redacted>")
            .finish()
    }
}

impl FeishuCrypto {
    /// Build from the console Encrypt Key. Mirrors the Java constructor validation
    /// (`encryptKey == null || encryptKey.isBlank()` -> `IllegalArgumentException`).
    pub fn new(encrypt_key: impl Into<String>) -> Result<Self> {
        let encrypt_key = encrypt_key.into();
        if encrypt_key.trim().is_empty() {
            return Err(ChannelError::Config("encryptKey is required".to_string()));
        }
        let digest = Sha256::digest(encrypt_key.as_bytes());
        let mut aes_key = [0u8; 32];
        aes_key.copy_from_slice(&digest);
        Ok(Self {
            encrypt_key,
            aes_key,
        })
    }

    /// The derived AES key (exposed for round-trip tests and diagnostics).
    pub fn aes_key(&self) -> &[u8; 32] {
        &self.aes_key
    }

    /// The `X-Lark-Signature` value for the given timestamp / nonce / raw body.
    ///
    /// Mirrors `verifySignature`'s digest computation, but returns the hex string instead of
    /// comparing, so tests and callers can construct the header.
    pub fn sign(&self, timestamp: &str, nonce: &str, body: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(timestamp.as_bytes());
        hasher.update(nonce.as_bytes());
        hasher.update(self.encrypt_key.as_bytes());
        hasher.update(body.as_bytes());
        to_hex(&hasher.finalize())
    }

    /// Constant-time verification of `signature` against the computed digest.
    ///
    /// Returns `false` when any argument is `None`, matching the Java null guard.
    pub fn verify_signature(
        &self,
        signature: Option<&str>,
        timestamp: Option<&str>,
        nonce: Option<&str>,
        body: Option<&str>,
    ) -> bool {
        let (Some(signature), Some(timestamp), Some(nonce), Some(body)) =
            (signature, timestamp, nonce, body)
        else {
            return false;
        };
        constant_time_eq(&self.sign(timestamp, nonce, body), signature)
    }

    /// Decrypt the `encrypt` field of a Feishu callback into the JSON plaintext.
    ///
    /// `encrypt_base64` is base64-decoded, the leading 16 bytes are the CBC IV and the remainder
    /// is `AES-256-CBC` + PKCS#7 padded plaintext.
    pub fn decrypt(&self, encrypt_base64: &str) -> Result<String> {
        let cipher_bytes =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encrypt_base64)
                .map_err(|e| {
                    ChannelError::Crypto(format!("Feishu decrypt failed: bad base64: {e}"))
                })?;

        // Java: `cipherBytes.length < 16 + 16` -> "Encrypted payload too short".
        if cipher_bytes.len() < AES_BLOCK_SIZE * 2 {
            return Err(ChannelError::Crypto(
                "Feishu decrypt failed: Encrypted payload too short".to_string(),
            ));
        }

        let (iv, body) = cipher_bytes.split_at(AES_BLOCK_SIZE);
        let plain = aes_256_cbc_decrypt(&self.aes_key, iv, body)
            .map_err(|e| ChannelError::Crypto(format!("Feishu decrypt failed: {e}")))?;
        let unpadded = pkcs7_unpad(&plain);
        String::from_utf8(unpadded)
            .map_err(|e| ChannelError::Crypto(format!("Feishu decrypt failed: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::{BlockEncryptMut, KeyIvInit};
    use aes::Aes256;

    /// Test Encrypt Key from the fixture generator (see the tracker §4.1 for provenance).
    const ENCRYPT_KEY: &str = "test-encrypt-key-32bytes-padding!!";
    /// Fixed IV used by the fixture so the ciphertext is reproducible offline.
    const FIXED_IV: [u8; 16] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f,
    ];
    /// Plaintext fixture produced by an independent AES-256-CBC implementation (Node `crypto`).
    const KNOWN_PLAINTEXT: &str =
        "{\"schema\":\"2.0\",\"type\":\"event\",\"header\":{\"event_id\":\"evt-1\"}}";
    /// `base64(FIXED_IV || AES-256-CBC(SHA-256(ENCRYPT_KEY), FIXED_IV, KNOWN_PLAINTEXT))`.
    const KNOWN_CIPHERTEXT_B64: &str = "AAECAwQFBgcICQoLDA0ODzHihg42cwfWJCjauuRSGR8TyuGewEqESDaowCrNTUbgpRyYKN7iMVkMvTOl7JpNcJsL2/c4j5clo5vef77f+60=";

    /// Independently encrypt a plaintext the way Feishu does, so the round-trip test does not
    /// simply mirror the production decryption path.
    fn encrypt(encrypt_key: &str, iv: &[u8; 16], plaintext: &[u8]) -> Vec<u8> {
        let key = Sha256::digest(encrypt_key.as_bytes());
        // PKCS#7 pad to the AES block size (the Java side unpads by hand, so we pad by hand too).
        let pad = AES_BLOCK_SIZE - (plaintext.len() % AES_BLOCK_SIZE);
        let mut padded = plaintext.to_vec();
        padded.extend(std::iter::repeat(pad as u8).take(pad));
        let enc = cbc::Encryptor::<Aes256>::new_from_slices(&key, iv).expect("valid key/iv");
        let mut out = vec![0u8; padded.len()];
        out.copy_from_slice(&padded);
        enc.encrypt_padded_mut::<aes::cipher::block_padding::NoPadding>(&mut out, padded.len())
            .expect("padded length is a block multiple");
        let mut framed = iv.to_vec();
        framed.extend_from_slice(&out);
        framed
    }

    fn b64(bytes: &[u8]) -> String {
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes)
    }

    #[test]
    fn rejects_blank_encrypt_key() {
        assert!(FeishuCrypto::new("").is_err());
        assert!(FeishuCrypto::new("   ").is_err());
    }

    #[test]
    fn aes_key_is_sha256_of_encrypt_key() {
        let crypto = FeishuCrypto::new(ENCRYPT_KEY).unwrap();
        let expected = Sha256::digest(ENCRYPT_KEY.as_bytes());
        assert_eq!(crypto.aes_key().as_slice(), expected.as_slice());
        assert_eq!(crypto.aes_key().len(), 32);
    }

    #[test]
    fn decrypts_known_vector_from_independent_implementation() {
        let crypto = FeishuCrypto::new(ENCRYPT_KEY).unwrap();
        let plain = crypto
            .decrypt(KNOWN_CIPHERTEXT_B64)
            .expect("known vector decrypts");
        assert_eq!(plain, KNOWN_PLAINTEXT);
    }

    #[test]
    fn decrypt_round_trips_arbitrary_plaintext() {
        let crypto = FeishuCrypto::new(ENCRYPT_KEY).unwrap();
        for plaintext in [
            "a",
            "{\"type\":\"url_verification\",\"challenge\":\"c-1\"}",
            // Exactly one AES block: PKCS#7 must add a whole extra padding block.
            "0123456789abcdef",
            "0123456789abcdefg",
        ] {
            let framed = encrypt(ENCRYPT_KEY, &FIXED_IV, plaintext.as_bytes());
            assert_eq!(crypto.decrypt(&b64(&framed)).unwrap(), plaintext);
        }
    }

    #[test]
    fn decrypt_rejects_short_bad_base64_and_bad_padding() {
        let crypto = FeishuCrypto::new(ENCRYPT_KEY).unwrap();
        // Shorter than IV + one block.
        assert!(crypto.decrypt(&b64(&[0u8; 16])).is_err());
        assert!(crypto.decrypt("not-base64!!").is_err());
        // A block-aligned body that is not valid UTF-8 after unpad.
        let mut framed = FIXED_IV.to_vec();
        framed.extend_from_slice(&[0x00u8; 32]);
        assert!(crypto.decrypt(&b64(&framed)).is_err());
    }

    #[test]
    fn signature_is_sha256_of_timestamp_nonce_key_and_body() {
        let crypto = FeishuCrypto::new(ENCRYPT_KEY).unwrap();
        let sig = crypto.sign("1700000000", "nonce-1", "{\"encrypt\":\"x\"}");
        assert_eq!(
            sig,
            "b73e3c426efa7f383a17af3c5f3036d28f4c76295b5502a653ce5f6e02262c5f"
        );
        assert!(crypto.verify_signature(
            Some(&sig),
            Some("1700000000"),
            Some("nonce-1"),
            Some("{\"encrypt\":\"x\"}")
        ));
    }

    #[test]
    fn signature_verification_rejects_mismatches_and_nulls() {
        let crypto = FeishuCrypto::new(ENCRYPT_KEY).unwrap();
        let body = "{\"encrypt\":\"x\"}";
        let sig = crypto.sign("1700000000", "nonce-1", body);
        // Wrong timestamp / nonce / body / signature.
        assert!(!crypto.verify_signature(
            Some(&sig),
            Some("1700000001"),
            Some("nonce-1"),
            Some(body)
        ));
        assert!(!crypto.verify_signature(
            Some(&sig),
            Some("1700000000"),
            Some("nonce-2"),
            Some(body)
        ));
        assert!(!crypto.verify_signature(
            Some(&sig),
            Some("1700000000"),
            Some("nonce-1"),
            Some("other")
        ));
        assert!(!crypto.verify_signature(
            Some("deadbeef"),
            Some("1700000000"),
            Some("nonce-1"),
            Some(body)
        ));
        // Any missing argument -> false (Java null guard).
        assert!(!crypto.verify_signature(None, Some("1700000000"), Some("nonce-1"), Some(body)));
        assert!(!crypto.verify_signature(Some(&sig), None, Some("nonce-1"), Some(body)));
        assert!(!crypto.verify_signature(Some(&sig), Some("1700000000"), None, Some(body)));
        assert!(!crypto.verify_signature(Some(&sig), Some("1700000000"), Some("nonce-1"), None));
    }

    #[test]
    fn debug_does_not_leak_key_material() {
        let crypto = FeishuCrypto::new(ENCRYPT_KEY).unwrap();
        let rendered = format!("{crypto:?}");
        assert!(rendered.contains("redacted"));
        assert!(!rendered.contains(ENCRYPT_KEY));
    }
}
