//! `WeComCrypto` — WeCom callback AES-256-CBC decryption, completing the crypto pair whose SHA-1
//! signature half already lives in [`crate::wecom::signature`].
//!
//! Rust port of `io.agentscope.extensions.channel.wecom.WeComCrypto`.
//! See <https://developer.work.weixin.qq.com/document/path/90968>.
//!
//! Inputs mirror the Java constructor:
//! - `token` — the developer-supplied callback token (used for signature verification)
//! - `encoding_aes_key` — 43 base64 characters (no padding); `"="` is appended before decoding to
//!   yield a 32-byte AES key. The first 16 bytes of that key are also the CBC IV.
//! - `receive_id` — the corp id (self-built apps) or suite id (ISV apps). Decryption verifies the
//!   trailing receive-id of the plaintext against this value.
//!
//! WeCom plaintext layout after AES-CBC decrypt + PKCS#7 unpad:
//!
//! ```text
//! | 16 bytes random | 4 bytes msg_len (big-endian) | msg (XML, msg_len bytes) | receive_id |
//! ```

use crate::common::crypto::{aes_256_cbc_decrypt, pkcs7_unpad, AES_KEY_SIZE};
use crate::common::{ChannelError, Result};
use crate::wecom::signature::WeComSignatureVerifier;

/// Equivalent of Java's `Base64.getDecoder()`.
///
/// The JCE decoder is "forgiving": it ignores non-zero bits left over in the unused space of the
/// final symbol, whereas the `base64` crate's [`STANDARD`](base64::engine::general_purpose::STANDARD)
/// engine rejects those as `InvalidLastSymbol`. WeCom's 43-character `encodingAesKey` (with `=`
/// appended before decoding) exercises exactly that path, so the leniency is load-bearing here.
const JAVA_BASE64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::general_purpose::GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true),
    );

/// WeCom callback signing + payload decryption.
#[derive(Clone)]
pub struct WeComCrypto {
    signature: WeComSignatureVerifier,
    aes_key: [u8; AES_KEY_SIZE],
    receive_id: String,
}

impl std::fmt::Debug for WeComCrypto {
    /// Never render key material in logs / test output.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WeComCrypto")
            .field("aes_key", &"<redacted>")
            .field("receive_id", &self.receive_id)
            .finish()
    }
}

impl WeComCrypto {
    /// 43 characters is the only accepted `encodingAesKey` length (Java constructor guard).
    pub const ENCODING_AES_KEY_LEN: usize = 43;

    /// Validate and build. Mirrors the Java constructor's `IllegalArgumentException` branches.
    pub fn new(
        token: impl Into<String>,
        encoding_aes_key: &str,
        receive_id: impl Into<String>,
    ) -> Result<Self> {
        let token = token.into();
        if token.is_empty() {
            return Err(ChannelError::Config("WeCom token is required".to_string()));
        }
        if encoding_aes_key.chars().count() != Self::ENCODING_AES_KEY_LEN {
            return Err(ChannelError::Config(format!(
                "encodingAesKey must be {} characters (got {})",
                Self::ENCODING_AES_KEY_LEN,
                encoding_aes_key.chars().count()
            )));
        }
        let receive_id = receive_id.into();
        if receive_id.is_empty() {
            return Err(ChannelError::Config(
                "WeCom receiveId is required".to_string(),
            ));
        }

        // Java: `Base64.getDecoder().decode(encodingAesKey + "=")`.
        let decoded = base64::Engine::decode(&JAVA_BASE64, format!("{encoding_aes_key}="))
        .map_err(|e| ChannelError::Config(format!("wecom.encodingAesKey is not base64: {e}")))?;
        if decoded.len() != AES_KEY_SIZE {
            return Err(ChannelError::Config(format!(
                "Decoded AES key must be {AES_KEY_SIZE} bytes (got {})",
                decoded.len()
            )));
        }
        let mut aes_key = [0u8; AES_KEY_SIZE];
        aes_key.copy_from_slice(&decoded);

        Ok(Self {
            signature: WeComSignatureVerifier::new(token)?,
            aes_key,
            receive_id,
        })
    }

    /// The decoded 32-byte AES key (also the source of the CBC IV).
    pub fn aes_key(&self) -> &[u8; AES_KEY_SIZE] {
        &self.aes_key
    }

    /// The CBC IV: the first 16 bytes of the AES key (Java `Arrays.copyOf(aesKey, 16)`).
    pub fn iv(&self) -> [u8; 16] {
        let mut iv = [0u8; 16];
        iv.copy_from_slice(&self.aes_key[..16]);
        iv
    }

    /// The configured corp / suite id.
    pub fn receive_id(&self) -> &str {
        &self.receive_id
    }

    /// Compute the URL-handshake signature (delegates to [`WeComSignatureVerifier::sign`]).
    pub fn sign(&self, timestamp: &str, nonce: &str, encrypt: &str) -> String {
        self.signature.sign(timestamp, nonce, encrypt)
    }

    /// Constant-time signature verification, mirroring the Java `verifySignature` null guard.
    pub fn verify_signature(
        &self,
        signature: Option<&str>,
        timestamp: &str,
        nonce: &str,
        encrypt: &str,
    ) -> bool {
        self.signature.verify(signature, timestamp, nonce, encrypt)
    }

    /// Decrypt an `<Encrypt>` value and return the inner XML payload.
    ///
    /// Rejects (with the same messages as Java): too-short plaintext, an out-of-range `msg_len`,
    /// and a trailing receive-id that does not match the configured one.
    pub fn decrypt(&self, encrypt_base64: &str) -> Result<String> {
        let cipher_bytes = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            encrypt_base64,
        )
        .map_err(|e| ChannelError::Crypto(format!("WeCom decrypt failed: bad base64: {e}")))?;

        let iv = self.iv();
        let plain = aes_256_cbc_decrypt(&self.aes_key, &iv, &cipher_bytes)
            .map_err(|e| ChannelError::Crypto(format!("WeCom decrypt failed: {e}")))?;
        let unpadded = pkcs7_unpad(&plain);
        if unpadded.len() < 20 {
            return Err(ChannelError::Crypto(
                "WeCom decrypt failed: Decrypted payload too short".to_string(),
            ));
        }

        let msg_len = u32::from_be_bytes([
            unpadded[16],
            unpadded[17],
            unpadded[18],
            unpadded[19],
        ]) as usize;
        if 20 + msg_len > unpadded.len() {
            return Err(ChannelError::Crypto(format!(
                "WeCom decrypt failed: Invalid msg_len in decrypted payload: {msg_len}"
            )));
        }

        let xml = std::str::from_utf8(&unpadded[20..20 + msg_len]).map_err(|e| {
            ChannelError::Crypto(format!("WeCom decrypt failed: message is not UTF-8: {e}"))
        })?;
        let trailing = std::str::from_utf8(&unpadded[20 + msg_len..]).map_err(|e| {
            ChannelError::Crypto(format!("WeCom decrypt failed: receive_id is not UTF-8: {e}"))
        })?;
        if trailing != self.receive_id {
            return Err(ChannelError::Crypto(format!(
                "WeCom decrypt failed: receive_id mismatch (expected '{}', got '{}')",
                self.receive_id, trailing
            )));
        }
        Ok(xml.to_string())
    }

    /// Encrypt a payload using the same scheme (the Java side has both directions; the handshake
    /// verification path and tests need it, and it keeps the layout definition in one place).
    pub fn encrypt(&self, xml: &str, receive_id: &str) -> Result<String> {
        // Java uses a 16-byte random prefix; callers inject it so this stays deterministic/testable.
        self.encrypt_with_random(xml, receive_id, &[0u8; 16])
    }

    /// [`WeComCrypto::encrypt`] with an explicit 16-byte random prefix.
    pub fn encrypt_with_random(
        &self,
        xml: &str,
        receive_id: &str,
        random: &[u8],
    ) -> Result<String> {
        use aes::cipher::{BlockEncryptMut, KeyIvInit};
        if random.len() != 16 {
            return Err(ChannelError::Crypto(format!(
                "random prefix must be 16 bytes (got {})",
                random.len()
            )));
        }
        let msg = xml.as_bytes();
        let mut payload = Vec::with_capacity(20 + msg.len() + receive_id.len() + 32);
        payload.extend_from_slice(random);
        payload.extend_from_slice(&(msg.len() as u32).to_be_bytes());
        payload.extend_from_slice(msg);
        payload.extend_from_slice(receive_id.as_bytes());

        let pad = 16 - (payload.len() % 16);
        payload.extend(std::iter::repeat(pad as u8).take(pad));

        let iv = self.iv();
        let enc = cbc::Encryptor::<aes::Aes256>::new_from_slices(&self.aes_key, &iv)
            .map_err(|e| ChannelError::Crypto(format!("WeCom encrypt failed: {e}")))?;
        let len = payload.len();
        let out = enc
            .encrypt_padded_mut::<aes::cipher::block_padding::NoPadding>(&mut payload, len)
            .map_err(|e| ChannelError::Crypto(format!("WeCom encrypt failed: {e}")))?;
        Ok(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            out,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 43-character fixture key (base64 alphabet, no padding).
    const ENCODING_AES_KEY: &str = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG";
    const TOKEN: &str = "wecom-token";
    const RECEIVE_ID: &str = "corp-1";
    const MSG_XML: &str = "<xml><ToUserName>corp-1</ToUserName><FromUserName>user-1</FromUserName><MsgType>text</MsgType><Content>hi</Content><MsgId>9988776655443322</MsgId><AgentID>1</AgentID></xml>";
    /// `base64(AES-256-CBC(key, iv, random16 || msgLen || MSG_XML || RECEIVE_ID))` with
    /// `random16 = "RANDOM_16_BYTES!"`, produced by an independent Node `crypto` implementation.
    const KNOWN_CIPHERTEXT_B64: &str = "Dysz8zbp0q/9hL/wJiqQsubpmn9m1I2/K69Uos1Ub86GZgajiHkwtTHsqkS7wTZiSznr268i7j3dnXCLMCf+X1XFEx/67MTcmjX5ZAy1K0v5kWjoCo34FZKoZjXUolyJg8nJuGzELCBI22W22Vc1WG+wqzHGM48owoTlf5QE8oizaj/X07eC8BXdZW27ZH6zDT8C+x0hyx58i1Zna96E1qKLMDjlzn7Hef4595J5nTp2a1tHFm+gDNou8j4uac46CtXE/WkP9LfO03xgPk/0BA==";

    fn crypto() -> WeComCrypto {
        WeComCrypto::new(TOKEN, ENCODING_AES_KEY, RECEIVE_ID).unwrap()
    }

    #[test]
    fn rejects_bad_constructor_inputs() {
        assert!(WeComCrypto::new("", ENCODING_AES_KEY, RECEIVE_ID).is_err());
        assert!(WeComCrypto::new(TOKEN, "too-short", RECEIVE_ID).is_err());
        assert!(WeComCrypto::new(TOKEN, "", RECEIVE_ID).is_err());
        assert!(WeComCrypto::new(TOKEN, ENCODING_AES_KEY, "").is_err());
        // 43 characters but not decodable into exactly 32 bytes.
        let bad = "!".repeat(43);
        assert!(WeComCrypto::new(TOKEN, &bad, RECEIVE_ID).is_err());
    }

    #[test]
    fn derives_32_byte_key_and_16_byte_iv_from_encoding_aes_key() {
        let c = crypto();
        assert_eq!(c.aes_key().len(), 32);
        assert_eq!(
            crate::common::to_hex(c.aes_key()),
            "69b71d79f8218a39259a7a29aabb2dbafc31cb3d35db7e39ebbf3d0010831051"
        );
        assert_eq!(c.iv().as_slice(), &c.aes_key()[..16]);
        assert_eq!(c.receive_id(), RECEIVE_ID);
    }

    #[test]
    fn decrypts_known_vector_from_independent_implementation() {
        assert_eq!(crypto().decrypt(KNOWN_CIPHERTEXT_B64).unwrap(), MSG_XML);
    }

    #[test]
    fn encrypt_decrypt_round_trip() {
        let c = crypto();
        for xml in [
            "<xml><Content>hi</Content></xml>",
            // Exactly one AES block of payload prefix padding exercise.
            "<xml/>",
        ] {
            let encrypted = c.encrypt_with_random(xml, RECEIVE_ID, b"RANDOM_16_BYTES!").unwrap();
            assert_eq!(c.decrypt(&encrypted).unwrap(), xml);
        }
        // The fixed-`encrypt` wrapper must also round-trip.
        let encrypted = c.encrypt(MSG_XML, RECEIVE_ID).unwrap();
        assert_eq!(c.decrypt(&encrypted).unwrap(), MSG_XML);
    }

    #[test]
    fn encrypt_with_random_rejects_wrong_prefix_length() {
        assert!(crypto()
            .encrypt_with_random(MSG_XML, RECEIVE_ID, &[0u8; 15])
            .is_err());
    }

    #[test]
    fn decrypt_rejects_receive_id_mismatch() {
        let c = crypto();
        // Encrypt with a different receive id, then try to decrypt with the configured one.
        let encrypted = c.encrypt_with_random(MSG_XML, "other-corp", b"RANDOM_16_BYTES!").unwrap();
        let err = c.decrypt(&encrypted).unwrap_err();
        assert!(err.to_string().contains("receive_id mismatch"), "{err}");
    }

    #[test]
    fn decrypt_rejects_bad_base64_short_and_corrupt_payloads() {
        let c = crypto();
        assert!(c.decrypt("!!!not-base64!!!").is_err());
        assert!(c.decrypt("").is_err());
        // Valid base64, but decrypting yields fewer than 20 bytes after unpad.
        let tiny = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [0u8; 16]);
        assert!(c.decrypt(&tiny).is_err());
        // Ciphertext lengths that are not block multiples are rejected by the AES layer.
        let odd = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [0u8; 17]);
        assert!(c.decrypt(&odd).is_err());
    }

    #[test]
    fn signature_matches_independent_sha1_vector() {
        let c = crypto();
        let sig = c.sign("1700000000", "nonce-2", KNOWN_CIPHERTEXT_B64);
        assert_eq!(sig, "c4b452f51436f842e545324d34ca081a3751653a");
        assert!(c.verify_signature(
            Some(&sig),
            "1700000000",
            "nonce-2",
            KNOWN_CIPHERTEXT_B64
        ));
        assert!(!c.verify_signature(
            Some(&sig),
            "1700000001",
            "nonce-2",
            KNOWN_CIPHERTEXT_B64
        ));
        assert!(!c.verify_signature(None, "1700000000", "nonce-2", KNOWN_CIPHERTEXT_B64));
    }

    #[test]
    fn debug_does_not_leak_key_material() {
        let rendered = format!("{:?}", crypto());
        assert!(rendered.contains("redacted"));
        assert!(!rendered.contains("69b71d79"));
    }
}
