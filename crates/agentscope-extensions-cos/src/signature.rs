//! Tencent Cloud COS v5 request signing (`q-sign-algorithm=sha1`).
//!
//! The Java module hands signing to the official SDK (`com.qcloud:cos_api`). No
//! equivalent Rust SDK is used here, so the algorithm is implemented directly from
//! the published specification (Tencent Cloud COS API docs, “请求签名” —
//! <https://cloud.tencent.com/document/product/436/7778>):
//!
//! ```text
//! SignKey      = HMAC-SHA1(SecretKey, KeyTime)
//! HttpString   = lower(HttpMethod) \n UriPathname \n HttpParameters \n HttpHeaders \n
//! StringToSign = "sha1" \n KeyTime \n SHA1(HttpString) \n
//! Signature    = HMAC-SHA1(SignKey, StringToSign)
//! Authorization: q-sign-algorithm=sha1&q-ak=<SecretId>&q-sign-time=<KeyTime>
//!                &q-key-time=<KeyTime>&q-header-list=<HeaderList>
//!                &q-url-param-list=<UrlParamList>&q-signature=<Signature>
//! ```
//!
//! All digests are lowercase hex; both `SignKey` and `StringToSign` are hashed as
//! UTF-8 **strings** (not raw bytes), matching the spec and every official SDK.
//!
//! Everything here is pure (no clock, no I/O) so the golden vectors published in the
//! specification can be asserted directly; `sign_request` takes `KeyTime` as a
//! parameter and the HTTP adapter owns the clock.

use hmac::{Hmac, Mac};
use sha1::{Digest, Sha1};

type HmacSha1 = Hmac<Sha1>;

/// Credentials used to sign COS requests.
///
/// `Debug` masks the secrets so they cannot leak through logs.
#[derive(Clone, PartialEq, Eq)]
pub struct CosCredentials {
    secret_id: String,
    secret_key: String,
    security_token: Option<String>,
}

impl CosCredentials {
    /// Creates permanent-key credentials.
    pub fn new(secret_id: impl Into<String>, secret_key: impl Into<String>) -> Self {
        Self {
            secret_id: secret_id.into(),
            secret_key: secret_key.into(),
            security_token: None,
        }
    }

    /// Attaches a temporary-session token (`x-cos-security-token`).
    pub fn with_security_token(mut self, token: impl Into<String>) -> Self {
        let token = token.into();
        self.security_token = if token.trim().is_empty() {
            None
        } else {
            Some(token)
        };
        self
    }

    /// The SecretId, sent as `q-ak`.
    pub fn secret_id(&self) -> &str {
        &self.secret_id
    }

    /// The SecretKey. Not exposed publicly — it is only used to derive the SignKey.
    pub(crate) fn secret_key(&self) -> &str {
        &self.secret_key
    }

    /// The optional temporary-session token.
    pub fn security_token(&self) -> Option<&str> {
        self.security_token.as_deref()
    }
}

impl std::fmt::Debug for CosCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CosCredentials")
            .field("secret_id", &self.secret_id)
            .field("secret_key", &"***")
            .field(
                "security_token",
                &self.security_token.as_ref().map(|_| "***"),
            )
            .finish()
    }
}

/// The intermediate values produced while signing a request.
///
/// Exposed so callers can log/replay a signature and so tests can pin every stage
/// of the derivation against the published golden vectors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CosSignature {
    /// `method\npath\nparams\nheaders\n` (method lowercased, path raw).
    pub http_string: String,
    /// `sha1\nKeyTime\nSHA1(HttpString)\n`.
    pub string_to_sign: String,
    /// `HMAC-SHA1(SecretKey, KeyTime)`.
    pub sign_key: String,
    /// Lowercase-hex `HMAC-SHA1(SignKey, StringToSign)`.
    pub signature: String,
    /// Sorted, `;`-joined lowercase header names.
    pub header_list: String,
    /// Sorted, `;`-joined lowercase parameter names.
    pub url_param_list: String,
    /// The complete `Authorization` header value.
    pub authorization: String,
}

/// Builds the `KeyTime` string `"<start>;<end>"` from Unix seconds.
pub fn key_time(start: i64, end: i64) -> String {
    format!("{start};{end}")
}

/// Lowercase-hex SHA1 of `data`.
pub fn sha1_hex(data: impl AsRef<[u8]>) -> String {
    let mut hasher = Sha1::new();
    hasher.update(data.as_ref());
    hex_lower(&hasher.finalize())
}

/// Lowercase-hex HMAC-SHA1 of `message` keyed by `key`, both as raw bytes.
pub fn hmac_sha1_hex(key: impl AsRef<[u8]>, message: impl AsRef<[u8]>) -> String {
    let mut mac =
        HmacSha1::new_from_slice(key.as_ref()).expect("HMAC-SHA1 accepts keys of any length");
    mac.update(message.as_ref());
    hex_lower(&mac.finalize().into_bytes())
}

/// `SignKey = HMAC-SHA1(SecretKey, KeyTime)`.
pub fn sign_key(secret_key: &str, key_time: &str) -> String {
    hmac_sha1_hex(secret_key.as_bytes(), key_time.as_bytes())
}

/// Percent-encodes a single URL component.
///
/// Implements the COS `UrlEncode` table: everything except RFC 3986 unreserved
/// characters (`A-Z a-z 0-9 - . _ ~`) is encoded as uppercase `%XX` over the UTF-8
/// bytes. This includes `/`, which is why paths use [`uri_encode_path`].
pub fn uri_encode(value: &str) -> String {
    encode(value, false)
}

/// Percent-encodes an object path, preserving `/` separators.
pub fn uri_encode_path(path: &str) -> String {
    encode(path, true)
}

fn encode(value: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Returns `(list, encoded_pairs)` for a parameter/header map.
///
/// Keys are lowercased and percent-encoded, values are percent-encoded, the pairs
/// are sorted lexicographically by encoded key and rendered as `k1=v1&k2=v2`. The
/// list is the `;`-joined (unencoded aside from the key encoding) name sequence.
pub fn canonical_pairs(pairs: &[(String, String)]) -> (String, String) {
    let mut normalized: Vec<(String, String)> = pairs
        .iter()
        .map(|(key, value)| (uri_encode(&key.to_lowercase()), uri_encode(value)))
        .collect();
    normalized.sort_by(|a, b| a.0.cmp(&b.0));

    let list = normalized
        .iter()
        .map(|(key, _)| key.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let encoded = normalized
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    (list, encoded)
}

/// Builds `HttpString = method\npath\nparams\nheaders\n`.
///
/// `path` is the **raw** (not percent-encoded) request path, exactly as the
/// specification’s worked examples compute it; the request line itself still
/// carries the percent-encoded path.
pub fn http_string(
    method: &str,
    path: &str,
    params: &[(String, String)],
    headers: &[(String, String)],
) -> String {
    let (_, encoded_params) = canonical_pairs(params);
    let (_, encoded_headers) = canonical_pairs(headers);
    format!(
        "{}\n{path}\n{encoded_params}\n{encoded_headers}\n",
        method.to_lowercase()
    )
}

/// Builds `StringToSign = sha1\nKeyTime\nSHA1(HttpString)\n`.
pub fn string_to_sign(key_time: &str, http_string: &str) -> String {
    format!("sha1\n{key_time}\n{}\n", sha1_hex(http_string.as_bytes()))
}

/// Signs a request and returns every intermediate value plus the `Authorization`
/// header value.
///
/// `path` is raw (unencoded); `params`/`headers` values are raw and encoded here.
/// Callers must send exactly the headers they signed (the adapter signs `host` plus
/// a small fixed set).
pub fn sign_request(
    credentials: &CosCredentials,
    method: &str,
    path: &str,
    params: &[(String, String)],
    headers: &[(String, String)],
    key_time: &str,
) -> CosSignature {
    let sign_key = sign_key(credentials.secret_key(), key_time);
    let http_string = http_string(method, path, params, headers);
    let string_to_sign = string_to_sign(key_time, &http_string);
    let signature = hmac_sha1_hex(sign_key.as_bytes(), string_to_sign.as_bytes());
    let (header_list, _) = canonical_pairs(headers);
    let (url_param_list, _) = canonical_pairs(params);
    let authorization = format!(
        "q-sign-algorithm=sha1&q-ak={}&q-sign-time={key_time}&q-key-time={key_time}\
         &q-header-list={header_list}&q-url-param-list={url_param_list}\
         &q-signature={signature}",
        credentials.secret_id()
    );

    CosSignature {
        http_string,
        string_to_sign,
        sign_key,
        signature,
        header_list,
        url_param_list,
        authorization,
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    //! Golden vectors come from the official COS “请求签名” page
    //! (<https://cloud.tencent.com/document/product/436/7778>, 示例一 upload / 示例二
    //! download). The page masks the last 6 hex characters of the sample
    //! `Signature` as `…1234`; the values asserted below are the 34-character
    //! prefixes printed there, extended by recomputing the HMAC over the published
    //! `StringToSign` with the published `SignKey`.

    use super::*;

    /// RFC 2202 §3 test cases pin the HMAC-SHA1 primitive itself.
    #[test]
    fn hmac_sha1_matches_rfc2202_vectors() {
        assert_eq!(
            hmac_sha1_hex(b"Jefe", b"what do ya want for nothing?"),
            "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79"
        );
        assert_eq!(
            hmac_sha1_hex([0x0bu8; 20], b"Hi There"),
            "b617318655057264e28bc0b6fb378c8ef146be00"
        );
    }

    /// FIPS 180-4 / RFC 3174 vectors pin SHA1.
    #[test]
    fn sha1_matches_rfc3174_vectors() {
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            sha1_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }

    /// `UrlEncode` table: `=`/`"`/`/`/`+` are encoded, unreserved characters are not.
    #[test]
    fn uri_encode_matches_cos_table() {
        assert_eq!(
            uri_encode("uin=\"100000000011\""),
            "uin%3D%22100000000011%22"
        );
        assert_eq!(
            uri_encode("mQ/fVh815F3k6TAUm8m0eg=="),
            "mQ%2FfVh815F3k6TAUm8m0eg%3D%3D"
        );
        assert_eq!(uri_encode("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(uri_encode("中文"), "%E4%B8%AD%E6%96%87");
        assert_eq!(uri_encode("/a/b"), "%2Fa%2Fb");
        assert_eq!(uri_encode_path("/a/中文"), "/a/%E4%B8%AD%E6%96%87");
    }

    /// Spec step 3 example: `/?prefix=example-folder%2F&delimiter=%2F&max-keys=10`.
    #[test]
    fn parameter_canonicalization_is_sorted_and_encoded_once() {
        let params = vec![
            ("prefix".to_string(), "example-folder/".to_string()),
            ("delimiter".to_string(), "/".to_string()),
            ("max-keys".to_string(), "10".to_string()),
        ];
        let (list, encoded) = canonical_pairs(&params);
        assert_eq!(list, "delimiter;max-keys;prefix");
        assert_eq!(
            encoded,
            "delimiter=%2F&max-keys=10&prefix=example-folder%2F"
        );

        // A valueless parameter signs as `k=` (spec: `/exampleobject?acl`).
        let (list, encoded) = canonical_pairs(&[("acl".to_string(), String::new())]);
        assert_eq!(list, "acl");
        assert_eq!(encoded, "acl=");
    }

    /// Spec step 4 example: header list is lowercased and lexicographically sorted.
    #[test]
    fn header_canonicalization_sorts_and_lowercases() {
        let headers = vec![
            (
                "Host".to_string(),
                "examplebucket-1250000000.cos.ap-shanghai.myqcloud.com".into(),
            ),
            (
                "Date".to_string(),
                "Thu, 16 May 2019 03:15:06 GMT".to_string(),
            ),
            ("x-cos-acl".to_string(), "private".to_string()),
            (
                "X-COS-GRANT-READ".to_string(),
                "uin=\"100000000011\"".to_string(),
            ),
        ];
        let (list, encoded) = canonical_pairs(&headers);
        assert_eq!(list, "date;host;x-cos-acl;x-cos-grant-read");
        assert_eq!(
            encoded,
            "date=Thu%2C%2016%20May%202019%2003%3A15%3A06%20GMT\
             &host=examplebucket-1250000000.cos.ap-shanghai.myqcloud.com\
             &x-cos-acl=private&x-cos-grant-read=uin%3D%22100000000011%22"
        );
    }

    /// Golden vector 1 (spec 示例一, PUT): full derivation chain, no parameters.
    #[test]
    fn official_put_example_golden_vector() {
        let key_time = "1557989151;1557996351";
        let path = "/exampleobject(腾讯云)";
        let headers = vec![
            (
                "Date".to_string(),
                "Thu, 16 May 2019 06:45:51 GMT".to_string(),
            ),
            (
                "Host".to_string(),
                "examplebucket-1250000000.cos.ap-beijing.myqcloud.com".into(),
            ),
            ("Content-Type".to_string(), "text/plain".to_string()),
            ("Content-Length".to_string(), "13".to_string()),
            (
                "Content-MD5".to_string(),
                "mQ/fVh815F3k6TAUm8m0eg==".to_string(),
            ),
            ("x-cos-acl".to_string(), "private".to_string()),
            (
                "x-cos-grant-read".to_string(),
                "uin=\"100000000011\"".to_string(),
            ),
        ];

        let http_string = http_string("PUT", path, &[], &headers);
        // `\n` is a real newline; the spec prints it as an escaped `\n`.
        assert_eq!(
            http_string,
            "put\n/exampleobject(腾讯云)\n\ncontent-length=13\
             &content-md5=mQ%2FfVh815F3k6TAUm8m0eg%3D%3D&content-type=text%2Fplain\
             &date=Thu%2C%2016%20May%202019%2006%3A45%3A51%20GMT\
             &host=examplebucket-1250000000.cos.ap-beijing.myqcloud.com\
             &x-cos-acl=private&x-cos-grant-read=uin%3D%22100000000011%22\n"
        );
        assert_eq!(
            sha1_hex(http_string.as_bytes()),
            "8b2751e77f43a0995d6e9eb9477f4b685cca4172"
        );

        let string_to_sign = string_to_sign(key_time, &http_string);
        assert_eq!(
            string_to_sign,
            "sha1\n1557989151;1557996351\n8b2751e77f43a0995d6e9eb9477f4b685cca4172\n"
        );

        // Published SignKey for this KeyTime; published Signature is
        // `3b8851a11a569213c17ba8fa7dcf2abec6931234` with the tail masked.
        let signature = hmac_sha1_hex(
            b"eb2519b498b02ac213cb1f3d1a3d27a3b3c9bc5f",
            string_to_sign.as_bytes(),
        );
        assert_eq!(signature, "3b8851a11a569213c17ba8fa7dcf2abec6935172");
        assert!(signature.starts_with("3b8851a11a569213c17ba8fa7dcf2abe"));
    }

    /// Golden vector 2 (spec 示例二, GET): includes URL parameters, whose names are
    /// sorted independently of the header list.
    #[test]
    fn official_get_example_golden_vector() {
        let key_time = "1557989753;1557996953";
        let path = "/exampleobject(腾讯云)";
        let params = vec![
            (
                "response-content-type".to_string(),
                "application/octet-stream".into(),
            ),
            (
                "response-cache-control".to_string(),
                "max-age=600".to_string(),
            ),
        ];
        let headers = vec![
            (
                "Host".to_string(),
                "examplebucket-1250000000.cos.ap-beijing.myqcloud.com".into(),
            ),
            (
                "Date".to_string(),
                "Thu, 16 May 2019 06:55:53 GMT".to_string(),
            ),
        ];

        let http_string = http_string("GET", path, &params, &headers);
        assert_eq!(
            http_string,
            "get\n/exampleobject(腾讯云)\n\
             response-cache-control=max-age%3D600\
             &response-content-type=application%2Foctet-stream\n\
             date=Thu%2C%2016%20May%202019%2006%3A55%3A53%20GMT\
             &host=examplebucket-1250000000.cos.ap-beijing.myqcloud.com\n"
        );
        assert_eq!(
            sha1_hex(http_string.as_bytes()),
            "54ecfe22f59d3514fdc764b87a32d8133ea611e6"
        );

        let string_to_sign = string_to_sign(key_time, &http_string);
        assert_eq!(
            string_to_sign,
            "sha1\n1557989753;1557996953\n54ecfe22f59d3514fdc764b87a32d8133ea611e6\n"
        );

        let signature = hmac_sha1_hex(
            b"937914bf490e9e8c189836aad2052e4feeb35eaf",
            string_to_sign.as_bytes(),
        );
        assert_eq!(signature, "01681b8c9d798a678e43b685a9f1bba0f6c0e012");
        assert!(signature.starts_with("01681b8c9d798a678e43b685a9f1bba0f6c0"));

        let (list, _) = canonical_pairs(&params);
        assert_eq!(list, "response-cache-control;response-content-type");
    }

    /// `sign_request` wires the chain together and renders the Authorization header.
    #[test]
    fn authorization_header_has_the_spec_shape() {
        let credentials = CosCredentials::new("AKIDexample", "secret-key");
        let signed = sign_request(
            &credentials,
            "GET",
            "/bucket-object",
            &[("acl".to_string(), String::new())],
            &[(
                "host".to_string(),
                "examplebucket-1250000000.cos.ap-beijing.myqcloud.com".into(),
            )],
            "1557989151;1557996351",
        );

        assert_eq!(signed.header_list, "host");
        assert_eq!(signed.url_param_list, "acl");
        assert_eq!(
            signed.sign_key,
            sign_key("secret-key", "1557989151;1557996351")
        );
        assert_eq!(
            signed.authorization,
            format!(
                "q-sign-algorithm=sha1&q-ak=AKIDexample\
                 &q-sign-time=1557989151;1557996351&q-key-time=1557989151;1557996351\
                 &q-header-list=host&q-url-param-list=acl&q-signature={}",
                signed.signature
            )
        );
        assert!(!signed.authorization.chars().any(|c| c.is_whitespace()));
    }

    /// The `Debug` impl must never print the secret key or session token.
    #[test]
    fn credentials_debug_masks_secrets() {
        let credentials =
            CosCredentials::new("AKIDexample", "top-secret").with_security_token("session-token");
        let rendered = format!("{credentials:?}");
        assert!(rendered.contains("AKIDexample"));
        assert!(!rendered.contains("top-secret"));
        assert!(!rendered.contains("session-token"));

        // A blank token is treated as absent, so no `x-cos-security-token` is signed.
        let credentials = CosCredentials::new("id", "key").with_security_token("   ");
        assert!(credentials.security_token().is_none());
    }
}
