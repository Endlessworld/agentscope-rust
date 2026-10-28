//! DashScope request encryption (`DashScopeEncryptionUtils` + client glue).
//!
//! Java encrypts the `input` member of the request body with **AES-GCM-256**
//! (12-byte IV, 128-bit tag), wraps the per-request AES key with the DashScope
//! **RSA** public key, and sends both in the `X-DashScope-EncryptionKey` header
//! (`{"public_key_id", "encrypt_key", "iv"}`). The encrypted response carries
//! `output` as a base64 string that is decrypted with the same AES key/IV.
//!
//! ## Layering
//!
//! The protocol (which body member is encrypted, key/IV sizes, header shape,
//! response handling) lives here as pure functions over an injectable
//! [`DashScopeCryptoProvider`]. The primitives are bundled in
//! [`AesRsaCryptoProvider`](crate::dashscope::crypto::AesRsaCryptoProvider)
//! (`aes-gcm` + `rsa`, exactly the JCE algorithms Java uses) and installed by
//! default, so enabling encryption just works.
//!
//! [`UnavailableCryptoProvider`] is kept for callers that enable the flag but
//! deliberately withhold crypto: it fails loudly instead of ever sending a
//! plaintext request.

use agentscope_core::{Error, Result};
use serde_json::{json, Map, Value};

use super::dto::PublicKeyData;

/// AES-GCM authentication tag length, in bits (128 bits = 16 bytes).
pub const GCM_TAG_LENGTH_BITS: usize = 128;
/// AES-GCM initialization-vector length, in bytes.
pub const GCM_IV_LENGTH: usize = 12;
/// AES key size, in bytes (256 bits).
pub const AES_KEY_SIZE: usize = 32;
/// Header carrying `{public_key_id, encrypt_key, iv}` for encrypted requests.
pub const ENCRYPTION_KEY_HEADER: &str = "X-DashScope-EncryptionKey";

/// Primitive crypto operations the DashScope encryption protocol needs.
///
/// Implementations must be `Send + Sync` so a model can hold one behind an `Arc`.
/// The key/IV generators are part of the seam because Rust has no built-in CSPRNG
/// dependency here (Java uses `SecureRandom`).
pub trait DashScopeCryptoProvider: Send + Sync + std::fmt::Debug {
    /// Generates a fresh 256-bit AES key.
    fn generate_aes_key(&self) -> [u8; AES_KEY_SIZE];
    /// Generates a fresh 12-byte GCM IV.
    fn generate_iv(&self) -> [u8; GCM_IV_LENGTH];
    /// AES-GCM encrypts `plaintext` (UTF-8), returning base64 ciphertext+tag.
    fn aes_gcm_encrypt(&self, key: &[u8], iv: &[u8], plaintext: &str) -> Result<String>;
    /// AES-GCM decrypts base64 `ciphertext` into UTF-8 plaintext.
    fn aes_gcm_decrypt(&self, key: &[u8], iv: &[u8], ciphertext: &str) -> Result<String>;
    /// RSA-encrypts the (base64 encoded) AES key with a base64 X.509 public key.
    fn rsa_encrypt(&self, public_key_b64: &str, plaintext_b64: &str) -> Result<String>;
}

/// Deliberately inert provider: every operation fails with an actionable
/// message instead of silently sending plaintext.
///
/// It is no longer the default ([`AesRsaCryptoProvider`] is, see
/// [`crate::dashscope::crypto`]); it remains for callers that want the failure
/// mode rather than the primitives.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableCryptoProvider;

impl DashScopeCryptoProvider for UnavailableCryptoProvider {
    fn generate_aes_key(&self) -> [u8; AES_KEY_SIZE] {
        [0u8; AES_KEY_SIZE]
    }

    fn generate_iv(&self) -> [u8; GCM_IV_LENGTH] {
        [0u8; GCM_IV_LENGTH]
    }

    fn aes_gcm_encrypt(&self, _key: &[u8], _iv: &[u8], _plaintext: &str) -> Result<String> {
        Err(unavailable())
    }

    fn aes_gcm_decrypt(&self, _key: &[u8], _iv: &[u8], _ciphertext: &str) -> Result<String> {
        Err(unavailable())
    }

    fn rsa_encrypt(&self, _public_key_b64: &str, _plaintext_b64: &str) -> Result<String> {
        Err(unavailable())
    }
}

fn incomplete_key_response() -> Error {
    Error::Model("Invalid public key response: data is missing or incomplete".to_string())
}

fn unavailable() -> Error {
    Error::Model(
        "DashScope request encryption is enabled but no DashScopeCryptoProvider with real crypto \
         primitives is installed (UnavailableCryptoProvider). Use \
         DashScopeChatModel::with_encryption_config(config) for the bundled AesRsaCryptoProvider, \
         or pass your own provider to with_encryption(config, provider)."
            .to_string(),
    )
}

/// The RSA public key pair DashScope hands out for encrypted requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptionConfig {
    /// Server-side key identifier (`public_key_id`).
    pub public_key_id: String,
    /// Base64-encoded X.509 RSA public key.
    pub public_key: String,
}

impl EncryptionConfig {
    /// Creates a config, rejecting blank values (Java treats blank as disabled,
    /// but an explicit config with an empty key is a configuration bug).
    pub fn new(public_key_id: impl Into<String>, public_key: impl Into<String>) -> Result<Self> {
        let public_key_id = public_key_id.into();
        let public_key = public_key.into();
        if public_key_id.trim().is_empty() {
            return Err(Error::Model(
                "DashScope encryption requires a non-empty public key id".to_string(),
            ));
        }
        if public_key.trim().is_empty() {
            return Err(Error::Model(
                "DashScope encryption requires a non-empty RSA public key".to_string(),
            ));
        }
        Ok(Self {
            public_key_id,
            public_key,
        })
    }

    /// Parses the body returned by `GET /api/v1/public-keys/latest`.
    ///
    /// Mirrors `DashScopeHttpClient#fetchPublicKey` validation: an error payload
    /// or a `data` object missing either field is rejected.
    pub fn from_public_key_response(value: &Value) -> Result<Self> {
        // DashScope reports failures with `code` (int or string) + `message`.
        // The typed DTO models `code` as a string, so the envelope is inspected
        // as JSON and only `data` is decoded through the DTO.
        if value.get("code").is_some_and(|code| !code.is_null()) {
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            let code = value
                .get("code")
                .map(|code| code.to_string().trim_matches('"').to_string())
                .unwrap_or_default();
            return Err(Error::Model(format!(
                "Failed to fetch public key: {message} (code {code})"
            )));
        }
        let data = value.get("data").ok_or_else(incomplete_key_response)?;
        let data: PublicKeyData = serde_json::from_value(data.clone())
            .map_err(|e| Error::Model(format!("Failed to parse public key response: {e}")))?;
        let public_key_id = data.public_key_id.filter(|v| !v.trim().is_empty());
        let public_key = data.public_key.filter(|v| !v.trim().is_empty());
        match (public_key_id, public_key) {
            (Some(public_key_id), Some(public_key)) => Self::new(public_key_id, public_key),
            _ => Err(incomplete_key_response()),
        }
    }
}

/// Per-request encryption state needed later to decrypt the response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptionContext {
    /// AES key used for this request.
    pub aes_key: Vec<u8>,
    /// GCM IV used for this request.
    pub iv: Vec<u8>,
    /// RSA-wrapped, base64-encoded AES key.
    pub encrypted_aes_key: String,
}

/// Renders the `X-DashScope-EncryptionKey` header value.
pub fn encryption_header_value(public_key_id: &str, context: &EncryptionContext) -> Result<String> {
    let mut header: Map<String, Value> = Map::new();
    header.insert("public_key_id".to_string(), json!(public_key_id));
    header.insert("encrypt_key".to_string(), json!(context.encrypted_aes_key));
    let iv = String::from_utf8_lossy(&base64_encode(&context.iv)).to_string();
    header.insert("iv".to_string(), json!(iv));
    serde_json::to_string(&Value::Object(header))
        .map_err(|e| Error::Model(format!("Failed to build encryption header: {e}")))
}

/// Encrypts the `input` member of `body` in place.
///
/// Returns `Ok(None)` when the body has no `input` member (nothing to encrypt),
/// matching Java's `encryptRequestBodyIfNeeded`.
pub fn encrypt_request_body(
    body: &mut Value,
    config: &EncryptionConfig,
    provider: &dyn DashScopeCryptoProvider,
) -> Result<Option<EncryptionContext>> {
    let Some(map) = body.as_object_mut() else {
        return Ok(None);
    };
    let Some(input) = map.get("input") else {
        return Ok(None);
    };
    let input_json = serde_json::to_string(input)
        .map_err(|e| Error::Model(format!("Failed to serialize input for encryption: {e}")))?;

    let aes_key = provider.generate_aes_key();
    let iv = provider.generate_iv();
    let encrypted_input = provider.aes_gcm_encrypt(&aes_key, &iv, &input_json)?;
    let aes_key_b64 = String::from_utf8_lossy(&base64_encode(&aes_key)).to_string();
    let encrypted_aes_key = provider.rsa_encrypt(&config.public_key, &aes_key_b64)?;

    map.insert("input".to_string(), json!(encrypted_input));
    Ok(Some(EncryptionContext {
        aes_key: aes_key.to_vec(),
        iv: iv.to_vec(),
        encrypted_aes_key,
    }))
}

/// Decrypts the `output` member of `body` in place when it is an encrypted
/// string. Unknown/failed payloads are left untouched (Java logs and returns the
/// original body), so a decrypt failure never loses the upstream response.
pub fn decrypt_response_body(
    body: &mut Value,
    context: &EncryptionContext,
    provider: &dyn DashScopeCryptoProvider,
) {
    let Some(map) = body.as_object_mut() else {
        return;
    };
    let Some(Value::String(encrypted)) = map.get("output") else {
        return;
    };
    let encrypted = encrypted.clone();
    match provider.aes_gcm_decrypt(&context.aes_key, &context.iv, &encrypted) {
        Ok(plaintext) => match serde_json::from_str::<Value>(&plaintext) {
            Ok(decrypted) => {
                map.insert("output".to_string(), decrypted);
            }
            Err(e) => crate::common::diagnostics::warn(
                "encryption_decrypt_failed",
                "output",
                &format!("decrypted output is not JSON: {e}"),
            ),
        },
        Err(e) => crate::common::diagnostics::warn(
            "encryption_decrypt_failed",
            "output",
            &format!("failed to decrypt response body: {e}"),
        ),
    }
}

/// Minimal base64 encoder for the IV / AES key.
///
/// Kept local so this module compiles without pulling `base64` into the hot
/// path of every provider; the crate's `base64` dependency is already declared
/// but unused elsewhere (see `MODEL__GAP_TRACKER.md` §3.3).
fn base64_encode(input: &[u8]) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .encode(input)
        .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic, non-cryptographic stand-in: XOR + base64. Lets the
    /// protocol be exercised end-to-end without a crypto dependency.
    #[derive(Debug)]
    struct XorProvider;

    impl DashScopeCryptoProvider for XorProvider {
        fn generate_aes_key(&self) -> [u8; AES_KEY_SIZE] {
            [7u8; AES_KEY_SIZE]
        }

        fn generate_iv(&self) -> [u8; GCM_IV_LENGTH] {
            [3u8; GCM_IV_LENGTH]
        }

        fn aes_gcm_encrypt(&self, key: &[u8], _iv: &[u8], plaintext: &str) -> Result<String> {
            let bytes: Vec<u8> = plaintext
                .bytes()
                .enumerate()
                .map(|(i, b)| b ^ key[i % key.len()])
                .collect();
            Ok(String::from_utf8_lossy(&base64_encode(&bytes)).to_string())
        }

        fn aes_gcm_decrypt(&self, key: &[u8], _iv: &[u8], ciphertext: &str) -> Result<String> {
            use base64::Engine as _;
            let raw = base64::engine::general_purpose::STANDARD
                .decode(ciphertext.as_bytes())
                .map_err(|e| Error::Model(format!("bad base64: {e}")))?;
            let bytes: Vec<u8> = raw
                .iter()
                .enumerate()
                .map(|(i, b)| b ^ key[i % key.len()])
                .collect();
            String::from_utf8(bytes).map_err(|e| Error::Model(e.to_string()))
        }

        fn rsa_encrypt(&self, public_key_b64: &str, plaintext_b64: &str) -> Result<String> {
            Ok(format!("rsa({public_key_b64}|{plaintext_b64})"))
        }
    }

    fn config() -> EncryptionConfig {
        EncryptionConfig::new("1", "PUBKEY").unwrap()
    }

    #[test]
    fn encrypts_input_and_builds_the_header() {
        let provider = XorProvider;
        let mut body = json!({
            "model": "qwen-plus",
            "input": {"messages": [{"role": "user", "content": "hi"}]},
        });
        let context = encrypt_request_body(&mut body, &config(), &provider)
            .unwrap()
            .expect("input present");
        // The plaintext input must not survive in the body.
        assert!(body["input"].is_string(), "{body}");
        assert_eq!(body["model"], json!("qwen-plus"));
        assert_eq!(context.aes_key, vec![7u8; AES_KEY_SIZE]);
        assert_eq!(context.iv, vec![3u8; GCM_IV_LENGTH]);

        let header: Value =
            serde_json::from_str(&encryption_header_value("1", &context).unwrap()).unwrap();
        assert_eq!(header["public_key_id"], json!("1"));
        assert_eq!(header["encrypt_key"], json!(context.encrypted_aes_key));
        // IV is base64 of 12 zero-ish bytes: 16 chars, no padding needed.
        assert_eq!(header["iv"].as_str().unwrap().len(), 16);

        // Round-trip: decrypting the encrypted input recovers the JSON.
        let decrypted = provider
            .aes_gcm_decrypt(
                &context.aes_key,
                &context.iv,
                body["input"].as_str().unwrap(),
            )
            .unwrap();
        let parsed: Value = serde_json::from_str(&decrypted).unwrap();
        assert_eq!(parsed["messages"][0]["content"], json!("hi"));
    }

    #[test]
    fn header_gold_format_matches_the_dashscope_schema() {
        // IV is the JCE test vector `0123456789ab`: base64 = MDEyMzQ1Njc4OWFi.
        let context = EncryptionContext {
            aes_key: vec![7u8; AES_KEY_SIZE],
            iv: b"0123456789ab".to_vec(),
            encrypted_aes_key: "WRAPPED".to_string(),
        };
        let header = encryption_header_value("key-1", &context).unwrap();
        let parsed: Value = serde_json::from_str(&header).unwrap();
        // Exactly the three fields DashScope reads, with base64 IV
        // (Java: Base64.getEncoder().encodeToString(context.iv)).
        assert_eq!(
            parsed,
            json!({
                "public_key_id": "key-1",
                "encrypt_key": "WRAPPED",
                "iv": "MDEyMzQ1Njc4OWFi",
            })
        );
        assert_eq!(parsed.as_object().unwrap().len(), 3);
        // Compact JSON, no whitespace (Java serializes the Map the same way).
        assert!(!header.contains(char::is_whitespace), "{header}");
        assert!(header.starts_with('{') && header.ends_with('}'), "{header}");
    }

    #[test]
    fn body_without_input_is_left_alone() {
        let mut body = json!({"model": "qwen-plus"});
        assert!(encrypt_request_body(&mut body, &config(), &XorProvider)
            .unwrap()
            .is_none());
        assert_eq!(body, json!({"model": "qwen-plus"}));
    }

    #[test]
    fn decrypts_output_string_and_ignores_other_shapes() {
        let provider = XorProvider;
        let context = EncryptionContext {
            aes_key: vec![7u8; AES_KEY_SIZE],
            iv: vec![3u8; GCM_IV_LENGTH],
            encrypted_aes_key: "wrapped".to_string(),
        };
        let plaintext = json!({"choices": [{"message": {"content": "hello"}}]}).to_string();
        let encrypted = provider
            .aes_gcm_encrypt(&context.aes_key, &context.iv, &plaintext)
            .unwrap();
        let mut body = json!({"output": encrypted});
        decrypt_response_body(&mut body, &context, &provider);
        assert_eq!(
            body["output"]["choices"][0]["message"]["content"],
            json!("hello")
        );

        // Already-decrypted output is untouched.
        let mut plain = json!({"output": {"choices": []}});
        decrypt_response_body(&mut plain, &context, &provider);
        assert_eq!(plain, json!({"output": {"choices": []}}));
    }

    #[test]
    fn decrypt_failure_keeps_the_original_body() {
        let provider = XorProvider;
        let context = EncryptionContext {
            aes_key: vec![7u8; AES_KEY_SIZE],
            iv: vec![3u8; GCM_IV_LENGTH],
            encrypted_aes_key: "wrapped".to_string(),
        };
        let mut body = json!({"output": "not-base64!!"});
        decrypt_response_body(&mut body, &context, &provider);
        assert_eq!(body["output"], json!("not-base64!!"));
    }

    #[test]
    fn unavailable_provider_fails_loudly() {
        let mut body = json!({"input": {"messages": []}});
        let err =
            encrypt_request_body(&mut body, &config(), &UnavailableCryptoProvider).unwrap_err();
        assert!(
            err.to_string().contains("no DashScopeCryptoProvider"),
            "{err}"
        );
        // Body untouched: never silently send plaintext.
        assert!(body["input"].is_object());
    }

    #[test]
    fn config_rejects_blank_fields_and_parses_public_key_response() {
        assert!(EncryptionConfig::new("", "k").is_err());
        assert!(EncryptionConfig::new("1", "  ").is_err());

        let ok = json!({
            "data": {"public_key_id": "1", "public_key": "MIIB"},
        });
        assert_eq!(
            EncryptionConfig::from_public_key_response(&ok).unwrap(),
            EncryptionConfig::new("1", "MIIB").unwrap()
        );
        let missing = json!({"data": {"public_key_id": "1"}});
        assert!(EncryptionConfig::from_public_key_response(&missing).is_err());
        let no_data = json!({"code": 400, "message": "nope"});
        let err = EncryptionConfig::from_public_key_response(&no_data).unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
    }
}
