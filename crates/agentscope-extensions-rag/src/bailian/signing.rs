//! ACS4-HMAC-SHA256 request signing for the Alibaba Cloud OpenAPI "pop" gateway.
//!
//! The Java `BailianClient` did not sign anything itself — it delegated to the
//! `com.aliyun.bailian20231229` SDK, which routes the call through
//! `com.aliyun.gateway.pop.Client` with `authType = "AK"` / `style = "ROA"` and applies the
//! `ACS4-HMAC-SHA256` scheme. This module reimplements exactly that scheme so the Rust port
//! can speak the same wire protocol without pulling in an Alibaba Cloud SDK.
//!
//! The algorithm was reverse-engineered from the SDK bytecode and pinned by golden-vector
//! tests (see the module tests):
//!
//! ```text
//! signingKey  = HMAC(HMAC(HMAC(HMAC("aliyun_v4" + secret, shortDate), region), product), "aliyun_v4_request")
//! canonicalQuery    = sorted "k=v" pairs, percent-encoded
//! canonicalHeaders  = for each signed header (sorted, lowercase): "name:trimmedValue\n"
//! stringToSign      = method \n path \n canonicalQuery \n canonicalHeaders \n signedHeaders \n hashedPayload
//! signature         = hex(HMAC(signingKey, "ACS4-HMAC-SHA256" \n hex(SHA256(stringToSign))))
//! Authorization     = "ACS4-HMAC-SHA256 Credential={AK}/{shortDate}/{region}/{product}/aliyun_v4_request"
//!                     + ",SignedHeaders=" + signedHeaders + ",Signature=" + signature
//! ```
//!
//! Headers participate in the signature when their lowercased name starts with `x-acs-` or
//! is `host` / `content-type` — mirroring `com.aliyun.gateway.pop.Client.getSignedHeaders`.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

/// Signature algorithm identifier (matches `Client._sha256`).
pub const SIGNATURE_ALGORITHM: &str = "ACS4-HMAC-SHA256";
/// API version sent as `x-acs-version` (`RetrieveRequest`'s `version` parameter).
///
/// The SDK passes `"2023-12-29"`, which is also the bailian20231229 artifact version.
pub const API_VERSION: &str = "2023-12-29";
/// API action sent as `x-acs-action`.
pub const API_ACTION: &str = "Retrieve";
/// OpenAPI product id (the `productId` passed to `Config.setEndpoint`).
///
/// The Bailian SDK's `Client` calls `client.doROARequestWithForm(...)` with product id
/// `bailian`; it is the third path segment of the `Credential` scope.
pub const PRODUCT: &str = "bailian";

/// Prefix prepended to the access key secret before the first HMAC round.
const SECRET_PREFIX: &str = "aliyun_v4";
/// Terminal scope component of the derived signing key.
const REQUEST_SUFFIX: &str = "aliyun_v4_request";

/// Lowercase hex encoding.
///
/// Kept local because `crate::hash`'s encoder is private and MD5-only scoped; a dependency on
/// the `hex` crate for four lines would not be justified.
pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Percent-encodes a value exactly like Alibaba Cloud's `Encoder.percentEncode`.
///
/// That helper is `URLEncoder.encode(value, "UTF-8")` followed by `+` → `%20`, `*` → `%2A`
/// and `%7E` → `~`. The effect is that `A-Za-z0-9`, `.`, `-`, `_` and `~` stay literal, a
/// space becomes `%20`, `*` becomes `%2A`, and everything else is `%XX` with uppercase hex.
/// It is also what `openapiutil.Client.getEncodeParam` does for the workspace-id path segment
/// (verified: `getEncodeParam("a b/c") == "a%20b%2Fc"`).
pub fn percent_encode(value: &str) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(DIGITS[(byte >> 4) as usize] as char);
            out.push(DIGITS[(byte & 0x0f) as usize] as char);
        }
    }
    out
}

/// Hex SHA-256 of the request body, sent as `x-acs-content-sha256`.
pub fn hashed_payload(body: &str) -> String {
    hex_lower(&Sha256::digest(body.as_bytes()))
}

/// Derives the region from the endpoint, reproducing `Client.getRegion`.
///
/// The SDK strips a `.aliyuncs.com` suffix and takes the second dot-separated component when
/// exactly two remain; otherwise it falls back to `"center"`. For the default endpoint
/// `bailian.cn-beijing.aliyuncs.com` this yields `cn-beijing`.
pub fn resolve_region(product: &str, endpoint: &str) -> String {
    if product.trim().is_empty() || endpoint.trim().is_empty() {
        return "center".to_string();
    }
    let stripped = endpoint.replace(".aliyuncs.com", "");
    let parts: Vec<&str> = stripped.split('.').collect();
    if parts.len() == 2 {
        parts[1].to_string()
    } else {
        "center".to_string()
    }
}

/// `yyyyMMdd` form of an `x-acs-date` timestamp (`"2026-09-19T00:00:00Z"` → `"20260919"`).
///
/// Mirrors `Client.modifyRequest`: `timestamp.substring(0, 10).replace("-", "")`.
pub fn short_date(api_date: &str) -> String {
    api_date.chars().take(10).filter(|c| *c != '-').collect()
}

/// Renders an instant as the `yyyy-MM-ddTHH:mm:ssZ` UTC string the SDK sends in `x-acs-date`.
pub fn api_date(now: chrono::DateTime<chrono::Utc>) -> String {
    now.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    // HMAC accepts keys of any length, so this cannot fail.
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Derives the ACS4 signing key.
///
/// Reproduces `Client.getSigningkey` for the `ACS4-HMAC-SHA256` branch. Note the
/// argument order of the SDK helper: `HmacSHA256Sign(data, key)`, i.e. the running key is the
/// HMAC *key* and the next scope component is the *data*.
pub fn signing_key(secret: &str, product: &str, region: &str, short_date: &str) -> Vec<u8> {
    let seed = format!("{SECRET_PREFIX}{secret}");
    let k_date = hmac_sha256(seed.as_bytes(), short_date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_product = hmac_sha256(&k_region, product.as_bytes());
    hmac_sha256(&k_product, REQUEST_SUFFIX.as_bytes())
}

/// The canonical query string: sorted `key=value` pairs, percent-encoded.
///
/// Reproduces `Client.buildCanonicalizedResource`. A parameter with an empty value is emitted
/// as a bare `key` (the SDK only appends `=value` when the value is non-empty).
fn canonical_query(pairs: &[(String, String)]) -> String {
    let mut sorted: Vec<&(String, String)> = pairs.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = String::new();
    for (index, (key, value)) in sorted.iter().enumerate() {
        if index > 0 {
            out.push('&');
        }
        out.push_str(&percent_encode(key));
        if !value.is_empty() {
            out.push('=');
            out.push_str(&percent_encode(value));
        }
    }
    out
}

/// Lowercased header names that participate in the signature, sorted ascending.
///
/// Reproduces `Client.getSignedHeaders`: keep `x-acs-*`, `host` and `content-type`.
fn signed_headers(headers: &[(String, String)]) -> Vec<String> {
    let mut names: Vec<String> = headers
        .iter()
        .map(|(name, _)| name.to_ascii_lowercase())
        .filter(|name| name.starts_with("x-acs-") || name == "host" || name == "content-type")
        .collect();
    names.sort();
    names.dedup();
    names
}

/// `"name:trimmedValue\n"` for every signed header, in sorted order.
///
/// Reproduces `Client.buildCanonicalizedHeaders`. Note that the SDK looks the value up by the
/// *lowercased* name, so header names are normalised before signing.
fn canonical_headers(headers: &[(String, String)], signed: &[String]) -> String {
    let lowered: Vec<(String, String)> = headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
        .collect();

    let mut out = String::new();
    for name in signed {
        let value = lowered
            .iter()
            .find(|(existing, _)| existing == name)
            .map(|(_, value)| value.as_str())
            .unwrap_or("");
        out.push_str(name);
        out.push(':');
        out.push_str(value);
        out.push('\n');
    }
    out
}

/// Every input required to produce an `Authorization` header.
#[derive(Debug, Clone)]
pub struct SignInput<'a> {
    /// HTTP method (uppercase, e.g. `POST`).
    pub method: &'a str,
    /// Canonical path, e.g. `/llm-xxxx/index/retrieve`.
    pub path: &'a str,
    /// Query parameters; sorted internally. Bailian sends none, so this is normally empty.
    pub query: &'a [(String, String)],
    /// Request headers; names are lowercased before signing.
    pub headers: &'a [(String, String)],
    /// Hex SHA-256 of the serialized body ([`hashed_payload`]).
    pub hashed_payload: &'a str,
    /// Alibaba Cloud access key id.
    pub access_key_id: &'a str,
    /// Alibaba Cloud access key secret.
    pub access_key_secret: &'a str,
    /// OpenAPI product id ([`PRODUCT`]).
    pub product: &'a str,
    /// Region derived from the endpoint ([`resolve_region`]).
    pub region: &'a str,
    /// `yyyyMMdd` compact date ([`short_date`]).
    pub short_date: &'a str,
}

impl SignInput<'_> {
    /// The `stringToSign` the HMAC is computed over (exposed for tests/diagnostics).
    pub fn string_to_sign(&self) -> String {
        let signed = signed_headers(self.headers);
        let path = if self.path.is_empty() { "/" } else { self.path };
        format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            self.method,
            path,
            canonical_query(self.query),
            canonical_headers(self.headers, &signed),
            signed.join(";"),
            self.hashed_payload,
        )
    }

    /// The lowercase signed-header names, joined with `;`.
    pub fn signed_headers_joined(&self) -> String {
        signed_headers(self.headers).join(";")
    }

    /// The `ACS4-HMAC-SHA256` signature (hex) over [`Self::string_to_sign`].
    pub fn signature(&self) -> String {
        let canonical_request_hash = hex_lower(&Sha256::digest(self.string_to_sign().as_bytes()));
        let string_to_sign = format!("{SIGNATURE_ALGORITHM}\n{canonical_request_hash}");
        let key = signing_key(
            self.access_key_secret,
            self.product,
            self.region,
            self.short_date,
        );
        hex_lower(&hmac_sha256(&key, string_to_sign.as_bytes()))
    }
}

/// Builds the `Authorization` header value.
///
/// Format (reproduced from `Client.getAuthorization`):
/// `{alg} Credential={ak}/{shortDate}/{region}/{product}/aliyun_v4_request,SignedHeaders={...},Signature={...}`
pub fn build_authorization(input: &SignInput<'_>) -> String {
    format!(
        "{SIGNATURE_ALGORITHM} Credential={}/{}/{}/{}/{REQUEST_SUFFIX},SignedHeaders={},Signature={}",
        input.access_key_id,
        input.short_date,
        input.region,
        input.product,
        input.signed_headers_joined(),
        input.signature(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// Golden vector captured from the real `com.aliyun.gateway.pop.Client` (0.0.6) jars:
    /// every value below is verbatim what the Java SDK produced for the same inputs.
    fn golden_headers(hashed: &str) -> Vec<(String, String)> {
        vec![
            (
                "host".to_string(),
                "bailian.cn-beijing.aliyuncs.com".to_string(),
            ),
            ("x-acs-version".to_string(), "2023-12-29".to_string()),
            ("x-acs-action".to_string(), "Retrieve".to_string()),
            ("x-acs-date".to_string(), "2026-09-19T00:00:00Z".to_string()),
            (
                "x-acs-signature-nonce".to_string(),
                "abcdef0123456789".to_string(),
            ),
            ("accept".to_string(), "application/json".to_string()),
            (
                "content-type".to_string(),
                "application/json; charset=utf-8".to_string(),
            ),
            ("x-acs-content-sha256".to_string(), hashed.to_string()),
        ]
    }

    #[test]
    fn signing_key_matches_java_sdk_golden_vector() {
        let key = signing_key("testAccessKeySecret", "bailian", "cn-beijing", "20260919");
        assert_eq!(
            hex_lower(&key),
            "a1a9d4ddf2f67b6c96d9168d9726fe7ab5f2aa34e99dd84a26742899d41bc219"
        );
    }

    #[test]
    fn hashed_payload_matches_java_sdk_golden_vector() {
        let body = r#"{"IndexId":"idx-1","Query":"hello"}"#;
        assert_eq!(
            hashed_payload(body),
            "fe443f582f43b8ed7b4984b88e0255228c6cd7dd10e233676914ae25d7096a15"
        );
    }

    #[test]
    fn signed_headers_match_java_sdk_golden_vector() {
        let hashed = hashed_payload(r#"{"IndexId":"idx-1","Query":"hello"}"#);
        let headers = golden_headers(&hashed);
        assert_eq!(
            signed_headers(&headers).join(";"),
            "content-type;host;x-acs-action;x-acs-content-sha256;x-acs-date;\
             x-acs-signature-nonce;x-acs-version"
        );
        assert_eq!(
            canonical_headers(&headers, &signed_headers(&headers)),
            format!(
                "content-type:application/json; charset=utf-8\n\
                 host:bailian.cn-beijing.aliyuncs.com\n\
                 x-acs-action:Retrieve\n\
                 x-acs-content-sha256:{hashed}\n\
                 x-acs-date:2026-09-19T00:00:00Z\n\
                 x-acs-signature-nonce:abcdef0123456789\n\
                 x-acs-version:2023-12-29\n"
            )
        );
    }

    #[test]
    fn signature_matches_java_sdk_golden_vector() {
        let hashed = hashed_payload(r#"{"IndexId":"idx-1","Query":"hello"}"#);
        let headers = golden_headers(&hashed);
        let query = vec![("A".to_string(), "b".to_string())];
        let input = SignInput {
            method: "POST",
            path: "/llm-abc/index/retrieve",
            query: &query,
            headers: &headers,
            hashed_payload: &hashed,
            access_key_id: "testAccessKeyId",
            access_key_secret: "testAccessKeySecret",
            product: PRODUCT,
            region: "cn-beijing",
            short_date: "20260919",
        };

        assert_eq!(canonical_query(&query), "A=b");
        assert_eq!(
            input.signature(),
            "ef4539d43f3b91b6247367c7f33d770a0cffb0f1a351e06a756b88c193a6e5fd"
        );
        assert_eq!(
            build_authorization(&input),
            "ACS4-HMAC-SHA256 Credential=testAccessKeyId/20260919/cn-beijing/bailian/\
             aliyun_v4_request,SignedHeaders=content-type;host;x-acs-action;\
             x-acs-content-sha256;x-acs-date;x-acs-signature-nonce;x-acs-version,\
             Signature=ef4539d43f3b91b6247367c7f33d770a0cffb0f1a351e06a756b88c193a6e5fd"
        );
    }

    #[test]
    fn region_derivation_matches_java_sdk() {
        // Golden values captured from `com.aliyun.gateway.pop.Client.getRegion`.
        assert_eq!(
            resolve_region("bailian", "bailian.cn-beijing.aliyuncs.com"),
            "cn-beijing"
        );
        assert_eq!(
            resolve_region("bailian", "bailian.cn-shanghai-finance-1.aliyuncs.com"),
            "cn-shanghai-finance-1"
        );
        assert_eq!(
            resolve_region("bailian", "bailian-vpc.cn-beijing.aliyuncs.com"),
            "cn-beijing"
        );
        // A single component (no region part) and the blank inputs fall back to `center`.
        assert_eq!(resolve_region("bailian", "localhost"), "center");
        assert_eq!(
            resolve_region("", "bailian.cn-beijing.aliyuncs.com"),
            "center"
        );
        assert_eq!(resolve_region("bailian", ""), "center");
    }

    #[test]
    fn percent_encode_matches_java_encoder() {
        assert_eq!(percent_encode("llm-abc"), "llm-abc");
        assert_eq!(percent_encode("a b/c"), "a%20b%2Fc");
        assert_eq!(percent_encode("a*b~c"), "a%2Ab~c");
        assert_eq!(percent_encode("\u{4e2d}\u{6587}"), "%E4%B8%AD%E6%96%87");
    }

    #[test]
    fn short_date_strips_dashes_from_first_ten_chars() {
        assert_eq!(short_date("2026-09-19T00:00:00Z"), "20260919");
        assert_eq!(short_date("2026-12-31T23:59:59Z"), "20261231");
    }

    #[test]
    fn api_date_uses_utc_iso_seconds() {
        let instant = chrono::Utc.with_ymd_and_hms(2026, 9, 19, 0, 0, 0).unwrap();
        assert_eq!(api_date(instant), "2026-09-19T00:00:00Z");
    }

    #[test]
    fn empty_path_falls_back_to_slash() {
        let headers = golden_headers(&hashed_payload("{}"));
        let input = SignInput {
            method: "POST",
            path: "",
            query: &[],
            headers: &headers,
            hashed_payload: "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a",
            access_key_id: "ak",
            access_key_secret: "sk",
            product: PRODUCT,
            region: "cn-beijing",
            short_date: "20260919",
        };
        let string_to_sign = input.string_to_sign();
        let lines: Vec<&str> = string_to_sign.split('\n').collect();
        assert_eq!(lines[0], "POST");
        assert_eq!(lines[1], "/");
        assert_eq!(lines[2], "");
    }

    #[test]
    fn canonical_query_sorts_and_keeps_empty_values_bare() {
        let pairs = vec![
            ("b".to_string(), "2".to_string()),
            ("a".to_string(), "1".to_string()),
            ("solitary".to_string(), String::new()),
        ];
        assert_eq!(canonical_query(&pairs), "a=1&b=2&solitary");
    }

    #[test]
    fn non_signed_headers_are_excluded() {
        let headers = vec![
            ("Accept".to_string(), "application/json".to_string()),
            ("User-Agent".to_string(), "sdk".to_string()),
            ("HOST".to_string(), "example.com".to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
        ];
        // `accept` / `user-agent` are excluded; mixed-case `HOST` is normalised.
        assert_eq!(signed_headers(&headers).join(";"), "content-type;host");
        assert_eq!(
            canonical_headers(&headers, &signed_headers(&headers)),
            "content-type:application/json\nhost:example.com\n"
        );
    }
}
