//! Bundled [`DashScopeCryptoProvider`]: the real AES-GCM-256 + RSA primitives.
//!
//! Mirrors the JCE calls in `DashScopeEncryptionUtils.java`:
//!
//! | Java | Here |
//! |---|---|
//! | `KeyGenerator.getInstance("AES")` + 256 bit | [`AesRsaCryptoProvider::generate_aes_key`] via `OsRng` |
//! | `new SecureRandom().nextBytes(new byte[12])` | [`AesRsaCryptoProvider::generate_iv`] |
//! | `Cipher.getInstance("AES/GCM/NoPadding")` + `GCMParameterSpec(128, iv)` | `Aes256Gcm` (12-byte nonce, 128-bit tag) |
//! | `Base64.getEncoder().encodeToString(doFinal(...))` | ciphertext‖tag, standard base64 with padding |
//! | `Cipher.getInstance("RSA")` (JCE default `RSA/ECB/PKCS1Padding`) | `Pkcs1v15Encrypt` |
//! | `X509EncodedKeySpec` + `KeyFactory.getInstance("RSA")` | `RsaPublicKey::from_public_key_der` (SPKI DER) |
//!
//! Layout notes that matter for interoperability:
//!
//! * `AES/GCM/NoPadding` returns `ciphertext || tag`, and so does `aes-gcm`'s
//!   `Aead::encrypt`; both use a 12-byte IV as the GCM nonce directly (no GHASH
//!   preprocessing of the IV), so the bytes match.
//! * Java RSA-encrypts the **base64 text** of the raw AES key (not the raw key),
//!   then base64-encodes the RSA ciphertext once more. That double encoding is
//!   what lands in `X-DashScope-EncryptionKey.encrypt_key`.
//! * PKCS#1 v1.5 encryption is randomized, so a wrapped key is not reproducible
//!   byte-for-byte; the gold test unwraps it with the matching private key.

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use rand::rngs::OsRng;
use rand::RngCore;
use rsa::pkcs1v15::Pkcs1v15Encrypt;
use rsa::pkcs8::DecodePublicKey;
use rsa::RsaPublicKey;

use agentscope_core::{Error, Result};

use super::encryption::{DashScopeCryptoProvider, AES_KEY_SIZE, GCM_IV_LENGTH};

/// The default provider: real AES-256-GCM + RSA PKCS#1 v1.5.
///
/// Install it explicitly with
/// [`DashScopeChatModel::with_encryption`](super::model::DashScopeChatModel::with_encryption)
/// or rely on the default installed by
/// [`DashScopeChatModel::with_encryption_config`](super::model::DashScopeChatModel::with_encryption_config).
#[derive(Debug, Default, Clone, Copy)]
pub struct AesRsaCryptoProvider;

impl AesRsaCryptoProvider {
    /// Creates the provider (it carries no state).
    pub const fn new() -> Self {
        Self
    }
}

fn cipher_error(context: &str, detail: impl std::fmt::Display) -> Error {
    Error::Model(format!("{context}: {detail}"))
}

/// Rejects wrong key sizes instead of letting `Nonce::from_slice` panic.
fn aes_cipher(key: &[u8]) -> Result<Aes256Gcm> {
    if key.len() != AES_KEY_SIZE {
        return Err(cipher_error(
            "Invalid AES key for DashScope encryption",
            format!("expected {AES_KEY_SIZE} bytes, got {}", key.len()),
        ));
    }
    Aes256Gcm::new_from_slice(key).map_err(|e| cipher_error("Failed to initialise AES-GCM", e))
}

fn gcm_nonce(iv: &[u8]) -> Result<&Nonce<aes_gcm::aead::consts::U12>> {
    if iv.len() != GCM_IV_LENGTH {
        return Err(cipher_error(
            "Invalid AES-GCM IV for DashScope encryption",
            format!("expected {GCM_IV_LENGTH} bytes, got {}", iv.len()),
        ));
    }
    Ok(Nonce::from_slice(iv))
}

fn encode_base64(bytes: &[u8]) -> String {
    BASE64.encode(bytes)
}

fn decode_base64(value: &str, what: &str) -> Result<Vec<u8>> {
    BASE64
        .decode(value.as_bytes())
        .map_err(|e| cipher_error(&format!("Invalid base64 {what}"), e))
}

impl DashScopeCryptoProvider for AesRsaCryptoProvider {
    fn generate_aes_key(&self) -> [u8; AES_KEY_SIZE] {
        let mut key = [0u8; AES_KEY_SIZE];
        OsRng.fill_bytes(&mut key);
        key
    }

    fn generate_iv(&self) -> [u8; GCM_IV_LENGTH] {
        let mut iv = [0u8; GCM_IV_LENGTH];
        OsRng.fill_bytes(&mut iv);
        iv
    }

    fn aes_gcm_encrypt(&self, key: &[u8], iv: &[u8], plaintext: &str) -> Result<String> {
        let cipher = aes_cipher(key)?;
        let nonce = gcm_nonce(iv)?;
        let ciphertext = cipher
            .encrypt(nonce, plaintext.as_bytes())
            .map_err(|e| cipher_error("Failed to encrypt data with AES-GCM", e))?;
        Ok(encode_base64(&ciphertext))
    }

    fn aes_gcm_decrypt(&self, key: &[u8], iv: &[u8], ciphertext: &str) -> Result<String> {
        let cipher = aes_cipher(key)?;
        let nonce = gcm_nonce(iv)?;
        let raw = decode_base64(ciphertext, "AES-GCM ciphertext")?;
        let plaintext = cipher
            .decrypt(nonce, raw.as_slice())
            .map_err(|e| cipher_error("Failed to decrypt data with AES-GCM", e))?;
        String::from_utf8(plaintext)
            .map_err(|e| cipher_error("Failed to decrypt data with AES-GCM", e))
    }

    fn rsa_encrypt(&self, public_key_b64: &str, plaintext_b64: &str) -> Result<String> {
        let der = decode_base64(public_key_b64, "RSA public key")?;
        let public_key = RsaPublicKey::from_public_key_der(&der)
            .map_err(|e| cipher_error("Invalid RSA public key for DashScope encryption", e))?;
        let wrapped = public_key
            .encrypt(&mut OsRng, Pkcs1v15Encrypt, plaintext_b64.as_bytes())
            .map_err(|e| cipher_error("Failed to encrypt AES key with RSA", e))?;
        Ok(encode_base64(&wrapped))
    }
}

/// Fixed vectors used by the unit and end-to-end tests.
///
/// Everything here was produced by the **Java reference implementation**
/// (`javax.crypto`, i.e. the same JCE calls `DashScopeEncryptionUtils` makes),
/// not by this Rust code:
///
/// * AES key = the ASCII bytes of `0123456789abcdef0123456789abcdef` (256 bit).
/// * IV = the ASCII bytes of `0123456789ab` (12 byte).
/// * `GOLD_*` = `Cipher.getInstance("AES/GCM/NoPadding")` +
///   `GCMParameterSpec(128, iv)` + `Base64.getEncoder()`.
/// * `RSA_*` = a throwaway 2048-bit key pair generated once with
///   `KeyPairGenerator.getInstance("RSA")`; `RSA_PRIVATE_KEY_PKCS8_B64` is the
///   PKCS#8 DER of the private key and only exists so tests can prove that the
///   wrapped AES key is really decryptable by the DashScope side.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use rsa::pkcs1v15::Pkcs1v15Encrypt;
    use rsa::pkcs8::DecodePrivateKey;
    use rsa::RsaPrivateKey;

    pub(crate) const AES_KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
    pub(crate) const IV: &[u8] = b"0123456789ab";

    /// `{"messages":[{"role":"user","content":"hi"}]}`
    pub(crate) const GOLD_PLAINTEXT: &str = r#"{"messages":[{"role":"user","content":"hi"}]}"#;
    /// JCE ciphertext‖tag, base64: 61-byte payload (45 plaintext bytes + 16-byte tag).
    pub(crate) const GOLD_CIPHERTEXT_B64: &str =
        "4ouKXzC5+XyQdgGH6H9eVS3qse0t6h8YQf3tNeub9sdeGgkN0MgHrQnMAjy2WdDnujA0FaOwQhAyITjGJA==";

    /// Same input JSON with a multi-byte UTF-8 payload (你好，世界) to pin the
    /// UTF-8 encoding Java uses for `plaintext.getBytes(StandardCharsets.UTF_8)`.
    pub(crate) const GOLD_PLAINTEXT_UTF8: &str =
        "{\"messages\":[{\"content\":\"\u{4f60}\u{597d}\u{ff0c}\u{4e16}\u{754c}\",\"role\":\"user\"}]}";
    pub(crate) const GOLD_CIPHERTEXT_UTF8_B64: &str = "4ouKXzC5+XyQdgGH6H9eRC3ooKp5vEhRBmtyuSxdJEaW84PBZBWwSULCXROk5Q78hE7p28djW1kuopcayBv0gZNOmCI5AgcPnB0=";

    /// Base64 of the X.509/SPKI DER of the throwaway test public key.
    pub(crate) const RSA_PUBLIC_KEY_B64: &str = "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAnNZYp27XbV2HkN599CTanJvBWi+yGW/1feLbQH8NaImXLxdUPExAdK7G66BiVGWr7PFlks39h1DAQUmB4sX2+mCQ6gGR4pouePTC1szyBVYypg8hERGVjoGjRB7GFzeYiUQlJbROuLZ+5ZFZgbt1omlBBr1XxUEWADZCqCBMD/6ZTzfoRNOC+9LE2i/7xXiI16Su1T6lNWgS+fdCTWgUv4ED9l64XyEaD8mGbiyEcKXElwXDkB5XU2VYmFspDIfxSqbE71ZogebkxH4rtK2eyaUmjSJzXo+zbsyRYL53dw9/9fa6E/zhSYnx2wpbMXGb+YWv8HnLuFugFafkcZFLnwIDAQAB";

    /// Base64 of the PKCS#8 DER of the matching private key.
    pub(crate) const RSA_PRIVATE_KEY_PKCS8_B64: &str = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCc1linbtdtXYeQ3n30JNqcm8FaL7IZb/V94ttAfw1oiZcvF1Q8TEB0rsbroGJUZavs8WWSzf2HUMBBSYHixfb6YJDqAZHimi549MLWzPIFVjKmDyEREZWOgaNEHsYXN5iJRCUltE64tn7lkVmBu3WiaUEGvVfFQRYANkKoIEwP/plPN+hE04L70sTaL/vFeIjXpK7VPqU1aBL590JNaBS/gQP2XrhfIRoPyYZuLIRwpcSXBcOQHldTZViYWykMh/FKpsTvVmiB5uTEfiu0rZ7JpSaNInNej7NuzJFgvnd3D3/19roT/OFJifHbClsxcZv5ha/wecu4W6AVp+RxkUufAgMBAAECggEASW+qEjf5whlFtWUgh9VL0YGyvsZPd86vHbM/40q6XgTy19duF0/C7H+736946ziyEF+VpnAyco4yfz9qhGMaenEF2bRDeV2lPfipVIehGTObwpqZ6pKXuyLjbi2UpmFfWIKifC2ZoK6+60IsLW03rwTmsD+vHcsUgKPngS6iPuhFHlFVS1BNvRnyIccupXWTf25sbLAOv7gY5Dg0rtpdJwfdrJaAVt5hSV5v52qOY2+DZL0ZRAM5X8R/bd7tdvC+EtDNXqYXlBL68Ryhh7sJO3dju7zkLOIVLW4nFAMrgoWA37WF5mfXNYNPTTGCh3UXjC/q6MVT662XPL4EvZJkkQKBgQDVb7G9fcGsmiiT7ghHcH7nJTmxPrx//7Ab8BYY5GKCyQ4t4I67os2s1sQ0uYBUhUSBT8HaJFcJixT0F30nKhZHkW+Ui+gDXHKq8ytTmKjX1SSAnV0yixbwZ3nSukOqsh37UsK8XCwF5tdPnTRiCyzkFFCEE9I2ZUqAsl7qFf/ATwKBgQC8HSu/KcfaJh6B9pFoZvFfe6aJL2APB3gumLayB5G7NnSqbOwyogywnF5xeIUUWhn7eiZmgakq8c5h7CkQyhYnnU2HS8wlO8pWFuLojsW85wY5u/V03jbxl36OEAe8mZ7+48/Y/shr/HHH/9YiUdTjVQFBbgHJ1OmdEnYiu/sbsQKBgDAwmNPs6eabo9/Klanzl2N+/yiqvoqz40JbkdGq9ycKWB45nmB1ILUOhJxW6cerk00TAfTL0a8A7QkzqHlIQmGET8odUJQ5slYwsy6bwDuDD0vOu/UmfFZpNpNCpPaMYsKU7Az1TqdCeIisOOCOH2ObxpYbl5HaitjF7TIzJ1x3AoGATAYmBe3mvP0m2zNA9MaVyhb46Pxif/Bu27YQL8brWJPbdnzqwFPJeR7Y9FbWwbDgS0fEeSzxp09uLxj4R7bEWWIzUPiLGokdbsl1DrOUqpFbbL3Pp1mfLugg110XI1uabDy5ZWMisLGnckKs0dXsiatzBHRm+KC5gSdS4xrBkyECgYEApYda8tCY6KyB2UdDPb+5TpuTOl0EzHhIoi0brNU+OX0sxDVfLktmINyg1XlpohQ9nxb89Jn9cJMqDd/153AT2KxQprIXdgWJFL5T14zBTS6enKYEo3SbwZdXdJaVkzS/8iyqKVlKsYCIVLY5zv34ap2F+bKNfwgTMKIbVIILutU=";

    /// Base64-decodes a header field (`iv`).
    pub(crate) fn decode_b64(value: &str) -> Vec<u8> {
        BASE64.decode(value).expect("valid base64")
    }

    /// Unwraps `encrypt_key` exactly like the DashScope server would
    /// (`Cipher.getInstance("RSA")` in decrypt mode), returning the raw AES key.
    pub(crate) fn unwrap_aes_key(wrapped_b64: &str) -> Vec<u8> {
        let der = BASE64.decode(RSA_PRIVATE_KEY_PKCS8_B64).unwrap();
        let private_key = RsaPrivateKey::from_pkcs8_der(&der).unwrap();
        let wrapped = BASE64.decode(wrapped_b64).unwrap();
        // 2048-bit modulus -> 256-byte PKCS#1 v1.5 block.
        assert_eq!(wrapped.len(), 256, "PKCS#1 v1.5 block size");
        let inner = private_key
            .decrypt(Pkcs1v15Encrypt, &wrapped)
            .expect("wrapped key must decrypt with the matching private key");
        BASE64.decode(inner).expect("inner payload is base64 text")
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn aes_gcm_matches_the_jce_gold_vector() {
        let provider = AesRsaCryptoProvider::new();
        // Byte-exact against `Cipher.getInstance("AES/GCM/NoPadding")` with the
        // same 256-bit key and 12-byte GCM IV.
        assert_eq!(
            provider
                .aes_gcm_encrypt(AES_KEY, IV, GOLD_PLAINTEXT)
                .unwrap(),
            GOLD_CIPHERTEXT_B64
        );
        // Ciphertext‖tag layout: 45 plaintext bytes + 16-byte tag = 61 bytes.
        assert_eq!(BASE64.decode(GOLD_CIPHERTEXT_B64).unwrap().len(), 61);
        // ... and it decrypts back to the plaintext.
        assert_eq!(
            provider
                .aes_gcm_decrypt(AES_KEY, IV, GOLD_CIPHERTEXT_B64)
                .unwrap(),
            GOLD_PLAINTEXT
        );
    }

    #[test]
    fn aes_gcm_encodes_the_plaintext_as_utf8_like_java() {
        let provider = AesRsaCryptoProvider::new();
        assert_eq!(
            provider
                .aes_gcm_encrypt(AES_KEY, IV, GOLD_PLAINTEXT_UTF8)
                .unwrap(),
            GOLD_CIPHERTEXT_UTF8_B64
        );
    }

    #[test]
    fn aes_gcm_rejects_a_tampered_tag_and_wrong_sizes() {
        let provider = AesRsaCryptoProvider::new();
        let mut raw = BASE64.decode(GOLD_CIPHERTEXT_B64).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0x01;
        let tampered = BASE64.encode(&raw);
        let err = provider
            .aes_gcm_decrypt(AES_KEY, IV, &tampered)
            .unwrap_err();
        assert!(err.to_string().contains("Failed to decrypt"), "{err}");

        // Wrong key size / IV length must be a typed error, never a panic.
        let err = provider.aes_gcm_encrypt(&[0u8; 16], IV, "x").unwrap_err();
        assert!(err.to_string().contains("expected 32 bytes"), "{err}");
        let err = provider
            .aes_gcm_encrypt(AES_KEY, &[0u8; 16], "x")
            .unwrap_err();
        assert!(err.to_string().contains("expected 12 bytes"), "{err}");
    }

    #[test]
    fn rsa_wraps_the_base64_aes_key_with_pkcs1_v1_5() {
        let provider = AesRsaCryptoProvider::new();
        let aes_key_b64 = BASE64.encode(AES_KEY);
        let wrapped = provider
            .rsa_encrypt(RSA_PUBLIC_KEY_B64, &aes_key_b64)
            .unwrap();
        // Same 256-byte block Java produces for a 2048-bit key.
        assert_eq!(BASE64.decode(&wrapped).unwrap().len(), 256);
        // Double encoding: envelope is base64(RSA(base64(raw key))).
        assert_eq!(unwrap_aes_key(&wrapped), AES_KEY.to_vec());
    }

    #[test]
    fn rsa_rejects_a_malformed_public_key() {
        let provider = AesRsaCryptoProvider::new();
        let err = provider.rsa_encrypt("not base64!!", "AAAA").unwrap_err();
        assert!(err.to_string().contains("base64 RSA public key"), "{err}");
        let err = provider.rsa_encrypt("AAAA", "AAAA").unwrap_err();
        assert!(err.to_string().contains("Invalid RSA public key"), "{err}");
    }

    #[test]
    fn generated_keys_and_ivs_have_the_protocol_sizes() {
        let provider = AesRsaCryptoProvider::new();
        let key_a = provider.generate_aes_key();
        let key_b = provider.generate_aes_key();
        assert_eq!(key_a.len(), AES_KEY_SIZE);
        assert_eq!(provider.generate_iv().len(), GCM_IV_LENGTH);
        // Two draws from a CSPRNG must not coincide (probability ~2^-256).
        assert_ne!(key_a, key_b);
        assert_ne!(provider.generate_iv(), provider.generate_iv());
    }
}
