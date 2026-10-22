//! `DingTalkCallbackCrypto` — DingTalk HTTP-callback request verification and callback-body
//! decryption/encryption.
//!
//! Rust port of `io.agentscope.extensions.channel.dingtalk.DingTalkCallbackCrypto`.
//!
//! Two authentication schemes are supported:
//!
//! 1. **Java 2026 scheme (primary).** The callback carries `timestamp` and `sign` headers where
//!    `sign = Base64(HmacSHA256(appSecret, timestamp + "\n" + appSecret))`. The timestamp is a
//!    millisecond epoch: at most one hour old, and at most five minutes ahead of local time.
//!    This is byte-for-byte the Java [`DingTalkCallbackCrypto.verify`].
//! 2. **Classic `msg_signature` scheme (optional).** When a `token` is configured, callbacks may
//!    instead carry `msg_signature = SHA1(sort(token, timestamp, nonce, encrypt).join(""))` as
//!    lowercase hex. Java has no `token` property; this path exists so deployments still on the
//!    older DingTalk callback configuration keep working.
//!
//! Encrypted callback bodies use `AES-256-CBC` with the 32-byte key decoded from the 43-character
//! `aesKey` (`"="` appended before decoding); the first 16 key bytes are the CBC IV. The plaintext
//! layout is:
//!
//! ```text
//! | 16 bytes random | 4 bytes msg_len (big-endian) | msg (msg_len bytes) | optional trailer |
//! ```
//!
//! Decryption ignores bytes after `msg` (DingTalk's event variant appends a receive id). Padding is
//! PKCS#7 to a **32-byte** block with strict validation, exactly like the Java helper (a valid pad
//! value spans 1..=32, not 1..=16).

use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha1::{Digest, Sha1};
use sha2::Sha256;

use crate::common::crypto::{aes_256_cbc_decrypt, AES_KEY_SIZE};
use crate::common::{ChannelError, Result};

type HmacSha256 = Hmac<Sha256>;

/// Java's `Base64.getDecoder()` ignores non-zero bits in the final symbol; the `base64` crate's
/// `STANDARD` engine rejects them. The 43-character `aesKey` + `"="` path needs that leniency.
const JAVA_BASE64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::general_purpose::GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true),
    );

/// Maximum age of the request timestamp, per DingTalk docs (Java `MAX_TIMESTAMP_AGE_MS`).
pub const MAX_TIMESTAMP_AGE_MS: i64 = 3_600_000;
/// Maximum tolerated clock skew for future-dated timestamps (Java `MAX_FUTURE_TIMESTAMP_MS`).
pub const MAX_FUTURE_TIMESTAMP_MS: i64 = 300_000;

/// DingTalk callback signing + payload encryption/decryption.
#[derive(Clone)]
pub struct DingTalkCallbackCrypto {
    app_secret: String,
    aes_key: Option<[u8; AES_KEY_SIZE]>,
    token: Option<String>,
}

impl std::fmt::Debug for DingTalkCallbackCrypto {
    /// Never render key material in logs / test output.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DingTalkCallbackCrypto")
            .field("app_secret", &"<redacted>")
            .field("aes_key", &self.aes_key.as_ref().map(|_| "<redacted>"))
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl DingTalkCallbackCrypto {
    /// Build the Java-equivalent helper: required `appSecret`, optional `aesKey`.
    ///
    /// Mirrors the Java constructor's `IllegalArgumentException` branches as
    /// [`ChannelError::Config`].
    pub fn new(app_secret: impl Into<String>, aes_key: Option<&str>) -> Result<Self> {
        let app_secret = app_secret.into();
        if app_secret.trim().is_empty() {
            return Err(ChannelError::Config("appSecret is required".to_string()));
        }

        let aes_key = match aes_key {
            None => None,
            Some(key) => {
                if key.chars().count() != 43 {
                    return Err(ChannelError::Config(format!(
                        "aesKey must be 43 characters (got {})",
                        key.chars().count()
                    )));
                }
                // Java: `Base64.getDecoder().decode(aesKey + "=")`.
                let decoded = base64::Engine::decode(&JAVA_BASE64, format!("{key}="))
                    .map_err(|e| ChannelError::Config(format!("aesKey is not base64: {e}")))?;
                if decoded.len() != AES_KEY_SIZE {
                    return Err(ChannelError::Config(format!(
                        "Decoded AES key must be {AES_KEY_SIZE} bytes (got {})",
                        decoded.len()
                    )));
                }
                let mut bytes = [0u8; AES_KEY_SIZE];
                bytes.copy_from_slice(&decoded);
                Some(bytes)
            }
        };

        Ok(Self {
            app_secret,
            aes_key,
            token: None,
        })
    }

    /// Enable the classic `msg_signature` (SHA-1 over sorted token/timestamp/nonce/encrypt) scheme.
    ///
    /// A blank token leaves the scheme disabled. This is a Rust-only extension; the Java 2026
    /// implementation only verifies the `timestamp`/`sign` headers.
    pub fn with_token(mut self, token: Option<String>) -> Self {
        self.token = token.filter(|value| !value.trim().is_empty());
        self
    }

    /// The configured classic callback token, when enabled.
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// Returns whether this channel is configured with an `aesKey` (encrypted callbacks).
    pub fn has_aes_key(&self) -> bool {
        self.aes_key.is_some()
    }

    /// The CBC IV: the first 16 bytes of the AES key (Java `Arrays.copyOf(aesKey, 16)`).
    fn iv(&self) -> Option<[u8; 16]> {
        self.aes_key.map(|key| {
            let mut iv = [0u8; 16];
            iv.copy_from_slice(&key[..16]);
            iv
        })
    }

    /// Verify the Java-scheme `timestamp`/`sign` headers against the current clock.
    pub fn verify(&self, timestamp: Option<&str>, sign: Option<&str>) -> bool {
        self.verify_at(timestamp, sign, now_ms())
    }

    /// [`Self::verify`] with an injected clock (deterministic tests).
    ///
    /// Order matches Java exactly: trim the timestamp once, parse it as `i64`, apply the freshness
    /// window to the normalized value, then HMAC that same normalized value.
    pub fn verify_at(&self, timestamp: Option<&str>, sign: Option<&str>, now_ms: i64) -> bool {
        let (Some(timestamp), Some(sign)) = (timestamp, sign) else {
            return false;
        };
        let normalized = timestamp.trim();
        let Ok(ts) = normalized.parse::<i64>() else {
            return false;
        };
        let age = now_ms.saturating_sub(ts);
        // `-age > MAX_FUTURE` written as `age < -MAX_FUTURE` to avoid overflow at i64::MIN.
        if age > MAX_TIMESTAMP_AGE_MS || age < -MAX_FUTURE_TIMESTAMP_MS {
            return false;
        }
        let Ok(expected) = self.sign(normalized) else {
            return false;
        };
        constant_time_eq(expected.as_bytes(), sign.as_bytes())
    }

    /// `Base64(HmacSHA256(appSecret, timestamp + "\n" + appSecret))`.
    pub fn sign(&self, timestamp: &str) -> Result<String> {
        let mut mac = HmacSha256::new_from_slice(self.app_secret.as_bytes())
            .map_err(|e| ChannelError::Crypto(format!("HMAC init failed: {e}")))?;
        mac.update(timestamp.as_bytes());
        mac.update(b"\n");
        mac.update(self.app_secret.as_bytes());
        let digest = mac.finalize().into_bytes();
        Ok(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            digest,
        ))
    }

    /// Compute the classic `msg_signature` for `encrypt` (lowercase SHA-1 hex).
    ///
    /// Returns `None` when no token is configured. Inputs are sorted lexicographically and
    /// concatenated with no separator, matching the Java/Python SDK helper.
    pub fn sign_legacy(&self, timestamp: &str, nonce: &str, encrypt: &str) -> Option<String> {
        let token = self.token.as_deref()?;
        let mut parts = [token, timestamp, nonce, encrypt];
        parts.sort_unstable();
        let mut digest = Sha1::new();
        for part in parts {
            digest.update(part.as_bytes());
        }
        Some(hex_lower(&digest.finalize()))
    }

    /// Constant-time verification of the classic `msg_signature`.
    ///
    /// Returns `false` when the scheme is disabled (no token) or the signature is absent.
    pub fn verify_legacy(
        &self,
        msg_signature: Option<&str>,
        timestamp: &str,
        nonce: &str,
        encrypt: &str,
    ) -> bool {
        let Some(signature) = msg_signature else {
            return false;
        };
        let Some(expected) = self.sign_legacy(timestamp, nonce, encrypt) else {
            return false;
        };
        constant_time_eq(expected.as_bytes(), signature.trim().as_bytes())
    }

    /// Decrypt an `encrypt` value and return the inner JSON payload as a UTF-8 string.
    ///
    /// Rejects (with the same conditions as Java): missing `aesKey`, invalid base64, a ciphertext
    /// that is not a whole number of AES blocks, invalid PKCS#7 padding, a plaintext shorter than
    /// 20 bytes, and an out-of-range `msg_len`.
    pub fn decrypt(&self, encrypt_base64: &str) -> Result<String> {
        let Some(aes_key) = self.aes_key else {
            return Err(ChannelError::Crypto(
                "aesKey is not configured for this channel".to_string(),
            ));
        };
        let iv = self.iv().expect("aes key checked above");

        let cipher_bytes =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encrypt_base64)
                .map_err(|e| {
                    ChannelError::Crypto(format!("DingTalk decrypt failed: invalid base64: {e}"))
                })?;

        let plain = aes_256_cbc_decrypt(&aes_key, &iv, &cipher_bytes)
            .map_err(|e| ChannelError::Crypto(format!("DingTalk decrypt failed: {e}")))?;
        let unpadded = pkcs7_unpad_strict(&plain)?;

        if unpadded.len() < 20 {
            return Err(ChannelError::Crypto(
                "Decrypted payload too short".to_string(),
            ));
        }
        let msg_len =
            u32::from_be_bytes([unpadded[16], unpadded[17], unpadded[18], unpadded[19]]) as usize;
        // Java: `msgLen < 0 || 20 + msgLen > unpad.length`; the first clause is impossible for a
        // u32, so only the upper bound applies.
        if 20 + msg_len > unpadded.len() {
            return Err(ChannelError::Crypto(format!(
                "Invalid msg_len in decrypted payload: {msg_len}"
            )));
        }

        // Java's `new String(bytes, UTF_8)` substitutes U+FFFD for malformed sequences rather than
        // rejecting the payload, so use the same lossy conversion.
        Ok(String::from_utf8_lossy(&unpadded[20..20 + msg_len]).into_owned())
    }

    /// Encrypt a JSON payload into the DingTalk callback envelope.
    ///
    /// `trailer` is appended after the message (DingTalk's event variant appends a receive id) and
    /// is ignored on decryption, exactly like the Java test fixture / SDK helper.
    pub fn encrypt(&self, json: &str, trailer: Option<&str>) -> Result<String> {
        let random = random_16();
        self.encrypt_with_random(json, trailer, &random)
    }

    /// [`Self::encrypt`] with an explicit 16-byte prefix, so tests can use a fixed gold vector.
    pub fn encrypt_with_random(
        &self,
        json: &str,
        trailer: Option<&str>,
        random: &[u8],
    ) -> Result<String> {
        use aes::cipher::{BlockEncryptMut, KeyIvInit};

        let (Some(aes_key), Some(iv)) = (self.aes_key, self.iv()) else {
            return Err(ChannelError::Crypto(
                "aesKey is not configured for this channel".to_string(),
            ));
        };
        if random.len() != 16 {
            return Err(ChannelError::Crypto(format!(
                "random prefix must be 16 bytes (got {})",
                random.len()
            )));
        }

        let msg = json.as_bytes();
        let mut payload = Vec::with_capacity(20 + msg.len() + trailer.map_or(0, str::len) + 32);
        payload.extend_from_slice(random);
        payload.extend_from_slice(&(msg.len() as u32).to_be_bytes());
        payload.extend_from_slice(msg);
        if let Some(trailer) = trailer {
            payload.extend_from_slice(trailer.as_bytes());
        }
        // Java test fixture: pad to a 32-byte multiple (a full 32-byte pad when already aligned).
        let pad = 32 - (payload.len() % 32);
        payload.extend(std::iter::repeat(pad as u8).take(pad));

        let encryptor = cbc::Encryptor::<aes::Aes256>::new_from_slices(&aes_key, &iv)
            .map_err(|e| ChannelError::Crypto(format!("DingTalk encrypt failed: {e}")))?;
        let len = payload.len();
        let out = encryptor
            .encrypt_padded_mut::<aes::cipher::block_padding::NoPadding>(&mut payload, len)
            .map_err(|e| ChannelError::Crypto(format!("DingTalk encrypt failed: {e}")))?;
        Ok(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            out,
        ))
    }
}

/// Strict PKCS#7 unpad, byte-for-byte equivalent to the Java `pkcs7Unpad` used by
/// `DingTalkCallbackCrypto` (which throws instead of passing malformed input through).
fn pkcs7_unpad_strict(input: &[u8]) -> Result<Vec<u8>> {
    if input.is_empty() {
        return Err(ChannelError::Crypto(
            "Decrypted payload is empty".to_string(),
        ));
    }
    let pad = input[input.len() - 1] as usize;
    if pad < 1 || pad > 32 || pad > input.len() {
        return Err(ChannelError::Crypto(format!(
            "Invalid PKCS#7 padding length: {pad}"
        )));
    }
    if input[input.len() - pad..]
        .iter()
        .any(|byte| *byte as usize != pad)
    {
        return Err(ChannelError::Crypto(
            "Invalid PKCS#7 padding bytes".to_string(),
        ));
    }
    Ok(input[..input.len() - pad].to_vec())
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 16 bytes of process-local entropy for the envelope prefix.
///
/// The prefix only needs to be unpredictable enough to avoid identical ciphertexts for identical
/// payloads; it is not key material, so a std-only mix of the clock and a counter is sufficient.
fn random_16() -> [u8; 16] {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut out = [0u8; 16];
    for (chunk, slot) in out.chunks_exact_mut(8).enumerate() {
        let state = std::collections::hash_map::RandomState::new();
        let mut hasher = state.build_hasher();
        hasher.write_u128(nanos);
        hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        hasher.write_usize(chunk);
        slot.copy_from_slice(&hasher.finish().to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "test-app-secret";
    /// Java `DingTalkCallbackTestSupport.AES_KEY` (43 chars, base64 of the 32 ASCII-byte key).
    const AES_KEY: &str = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY";
    const TOKEN: &str = "test-token";
    const MSG_JSON: &str = r#"{"msgtype":"text","text":{"content":"hello"}}"#;

    /// `Base64(HmacSHA256("test-app-secret", "1700000000000\n" + secret))`, produced by an
    /// independent Node `crypto` implementation.
    const HMAC_GOLD: &str = "dhJB8TJ0cHPEbwpUauugV2Wz8v4qlWoO+C42nEhJW+c=";
    /// `SHA1(sort("test-token","1700000000","nonce-1","abc123").join(""))`, independent Node.
    const LEGACY_GOLD: &str = "a514a5a6a03a5f8a76b09bcba6693e42668a076d";
    /// `AES-256-CBC` envelope for `MSG_JSON` with a zero 16-byte prefix, independent Node.
    const CIPHERTEXT_GOLD: &str = "+DyaYNwM25ghn3nW1dsWNR+6CbpvD3E+l3/3XlD/FVFeUBO19fGNxnQT1U0XMjfpaKwTBIzOkZbjublPNVqCih4Aabqun8HGBD9ozkaazb8mFrXsvCFSRwyfexI9+6iovPvAZK3ODocgWjK0/Ulohw==";
    /// Same envelope with the trailing `"receive-id"` that event callbacks append.
    const CIPHERTEXT_GOLD_TRAILER: &str = "+DyaYNwM25ghn3nW1dsWNR+6CbpvD3E+l3/3XlD/FVFeUBO19fGNxnQT1U0XMjfpaKwTBIzOkZbjublPNVqCinqc7eNi/BBjxKtFwxxsNLk3QStEKosges+z1VxQ57AXNQlHltc3JRzf6ksCOpPYrA==";

    fn crypto() -> DingTalkCallbackCrypto {
        DingTalkCallbackCrypto::new(SECRET, Some(AES_KEY)).unwrap()
    }

    #[test]
    fn hmac_signature_matches_the_java_gold_vector() {
        let crypto = DingTalkCallbackCrypto::new(SECRET, None).unwrap();
        assert_eq!(crypto.sign("1700000000000").unwrap(), HMAC_GOLD);
        assert!(crypto.verify_at(Some("1700000000000"), Some(HMAC_GOLD), 1_700_000_000_000));
        // The header is trimmed once; the normalized value feeds both checks (Java test).
        assert!(crypto.verify_at(
            Some("  1700000000000  "),
            Some(HMAC_GOLD),
            1_700_000_000_000
        ));
    }

    #[test]
    fn freshness_window_matches_java_bounds() {
        let crypto = DingTalkCallbackCrypto::new(SECRET, None).unwrap();
        let now = 1_700_000_000_000;
        let bound_old = now - MAX_TIMESTAMP_AGE_MS;
        let sign = crypto.sign(&bound_old.to_string()).unwrap();
        assert!(crypto.verify_at(Some(&bound_old.to_string()), Some(&sign), now));
        let too_old = bound_old - 1;
        let sign = crypto.sign(&too_old.to_string()).unwrap();
        assert!(!crypto.verify_at(Some(&too_old.to_string()), Some(&sign), now));

        let bound_future = now + MAX_FUTURE_TIMESTAMP_MS;
        let sign = crypto.sign(&bound_future.to_string()).unwrap();
        assert!(crypto.verify_at(Some(&bound_future.to_string()), Some(&sign), now));
        let too_far = bound_future + 1;
        let sign = crypto.sign(&too_far.to_string()).unwrap();
        assert!(!crypto.verify_at(Some(&too_far.to_string()), Some(&sign), now));
    }

    #[test]
    fn verify_rejects_wrong_missing_and_non_numeric_inputs() {
        let crypto = DingTalkCallbackCrypto::new(SECRET, None).unwrap();
        let now = 1_700_000_000_000;
        assert!(!crypto.verify_at(Some("not-a-number"), Some(HMAC_GOLD), now));
        assert!(!crypto.verify_at(None, Some(HMAC_GOLD), now));
        assert!(!crypto.verify_at(Some("1700000000000"), None, now));
        assert!(!crypto.verify_at(Some("1700000000000"), Some("another-signature"), now));
        // Signature computed over a different timestamp.
        let other = crypto.sign("1700000000001").unwrap();
        assert!(!crypto.verify_at(Some("1700000000000"), Some(&other), now));
        // Extreme values must not overflow the freshness arithmetic.
        assert!(!crypto.verify_at(Some(&i64::MAX.to_string()), Some(HMAC_GOLD), now));
        assert!(!crypto.verify_at(Some(&i64::MIN.to_string()), Some(HMAC_GOLD), now));
    }

    #[test]
    fn classic_msg_signature_matches_the_gold_vector() {
        let crypto = crypto().with_token(Some(TOKEN.to_string()));
        assert_eq!(
            crypto
                .sign_legacy("1700000000", "nonce-1", "abc123")
                .as_deref(),
            Some(LEGACY_GOLD)
        );
        assert!(crypto.verify_legacy(Some(LEGACY_GOLD), "1700000000", "nonce-1", "abc123"));
        assert!(!crypto.verify_legacy(Some(LEGACY_GOLD), "1700000001", "nonce-1", "abc123"));
        assert!(!crypto.verify_legacy(None, "1700000000", "nonce-1", "abc123"));
        // No token -> scheme disabled.
        assert!(DingTalkCallbackCrypto::new(SECRET, None)
            .unwrap()
            .sign_legacy("1", "2", "3")
            .is_none());
    }

    #[test]
    fn decrypts_the_java_gold_ciphertext() {
        assert_eq!(crypto().decrypt(CIPHERTEXT_GOLD).unwrap(), MSG_JSON);
    }

    #[test]
    fn ignores_trailing_bytes_beyond_msg_length() {
        // DingTalk event callbacks append a receive id after the message (Java test).
        assert_eq!(crypto().decrypt(CIPHERTEXT_GOLD_TRAILER).unwrap(), MSG_JSON);
    }

    #[test]
    fn encrypt_decrypt_round_trip_including_trailer() {
        let crypto = crypto();
        for (json, trailer) in [
            (MSG_JSON, None),
            (r#"{"msgtype":"text"}"#, Some("receive-id")),
            (r#"{"msgtype":"text","text":{"content":"中文"}}"#, None),
        ] {
            let fixed = crypto
                .encrypt_with_random(json, trailer, &[7u8; 16])
                .unwrap();
            assert_eq!(crypto.decrypt(&fixed).unwrap(), json);
            // The random-prefix helper must round-trip too.
            let random = crypto.encrypt(json, trailer).unwrap();
            assert_eq!(crypto.decrypt(&random).unwrap(), json);
        }
    }

    #[test]
    fn encrypt_with_random_rejects_a_wrong_prefix_length() {
        assert!(crypto()
            .encrypt_with_random(MSG_JSON, None, &[0u8; 15])
            .is_err());
    }

    #[test]
    fn decrypt_rejects_bad_base64_short_and_unaligned_ciphertexts_explicitly() {
        let crypto = crypto();
        let err = crypto.decrypt("!!!not-base64!!!").unwrap_err();
        assert!(err.to_string().contains("invalid base64"), "{err}");

        // Valid base64, but 16 zero bytes decrypt to a block whose pad byte is 0 -> rejected.
        let tiny = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [0u8; 16]);
        let err = crypto.decrypt(&tiny).unwrap_err();
        assert!(err.to_string().contains("padding length"), "{err}");

        // A ciphertext whose length is not a multiple of the AES block size is rejected.
        let odd = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [0u8; 17]);
        assert!(crypto.decrypt(&odd).is_err());

        // Empty ciphertext never reaches a valid plaintext.
        assert!(crypto.decrypt("").is_err());
    }

    #[test]
    fn decrypt_rejects_tampered_framing_and_padding() {
        let crypto = crypto();
        let mutate = |index: usize| {
            let mut bytes =
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, CIPHERTEXT_GOLD)
                    .unwrap();
            bytes[index] ^= 0x55;
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes)
        };
        // Corrupting the first block garbles the msg_len header.
        assert!(crypto.decrypt(&mutate(0)).is_err());
        // Corrupting the last block garbles the padding.
        let last =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, CIPHERTEXT_GOLD)
                .unwrap()
                .len()
                - 1;
        assert!(crypto.decrypt(&mutate(last)).is_err());
    }

    #[test]
    fn decrypt_without_aes_key_fails_explicitly() {
        let crypto = DingTalkCallbackCrypto::new(SECRET, None).unwrap();
        let err = crypto.decrypt(CIPHERTEXT_GOLD).unwrap_err();
        assert!(
            err.to_string().contains("aesKey is not configured"),
            "{err}"
        );
        assert!(!crypto.has_aes_key());
    }

    #[test]
    fn constructor_rejects_wrong_key_material_and_blank_secret() {
        assert!(DingTalkCallbackCrypto::new(SECRET, Some("too-short")).is_err());
        assert!(DingTalkCallbackCrypto::new("", Some(AES_KEY)).is_err());
        assert!(DingTalkCallbackCrypto::new("   ", None).is_err());
        // 43 characters but not decodable to exactly 32 bytes.
        assert!(DingTalkCallbackCrypto::new(SECRET, Some(&"!".repeat(43))).is_err());
        assert!(DingTalkCallbackCrypto::new(SECRET, Some(AES_KEY))
            .unwrap()
            .has_aes_key());
    }

    #[test]
    fn debug_does_not_leak_key_material() {
        let rendered = format!("{:?}", crypto().with_token(Some(TOKEN.to_string())));
        assert!(rendered.contains("redacted"));
        assert!(!rendered.contains(SECRET));
        assert!(!rendered.contains(AES_KEY));
        assert!(!rendered.contains(TOKEN));
    }
}
