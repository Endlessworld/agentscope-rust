//! Presigned URL generation (`q-sign-algorithm=sha1` over the query string).
//!
//! A presigned URL carries the same signature as [`crate::signature::sign_request`]
//! but puts every `q-*` key-value pair into the query string instead of the
//! `Authorization` header, so a plain browser/`curl` can use it:
//!
//! ```text
//! https://<bucket>.cos.<region>.myqcloud.com/<key>
//!   ?q-sign-algorithm=sha1&q-ak=<SecretId>
//!   &q-sign-time=<start>;<end>&q-key-time=<start>;<end>
//!   &q-header-list=host&q-url-param-list=<signed params>
//!   &q-signature=<hmac-sha1>
//! ```
//!
//! The signing algorithm is **not** duplicated here: [`presign_url`] calls
//! [`crate::signature::sign_request`] and reuses its `Authorization` value verbatim
//! (it is already the `k=v&k=v` chain the URL needs). Only `host` is signed as a
//! header, because that is the single header every HTTP client sends implicitly —
//! signing `date` or `content-type` would make the URL unusable by an ordinary
//! browser.
//!
//! Temporary credentials (STS) travel as the `x-cos-security-token` **query
//! parameter**, so it is part of the signed parameter list and needs no header on
//! the receiving side.

use crate::http_object_store::query_string;
use crate::signature::{self, CosCredentials};
use crate::{CosError, Result};

/// Longest validity COS accepts for a signed URL (7 days).
pub const MAX_PRESIGN_EXPIRE_SECS: u64 = 7 * 24 * 60 * 60;

/// Clamps/validates the requested validity in seconds.
///
/// `None` means "use the client default" (`CosHttpConfig::sign_expire_secs`, 15
/// minutes by default, matching the COS SDK).
pub fn resolve_expire_secs(requested: Option<u64>, default_secs: u64) -> Result<u64> {
    let expire = requested.unwrap_or(default_secs);
    if expire == 0 {
        return Err(CosError::Config(
            "presigned URL expiry must be at least 1 second".to_string(),
        ));
    }
    if expire > MAX_PRESIGN_EXPIRE_SECS {
        return Err(CosError::Config(format!(
            "presigned URL expiry must not exceed {MAX_PRESIGN_EXPIRE_SECS} seconds (COS limit), \
             got {expire}"
        )));
    }
    Ok(expire)
}

/// Builds a presigned URL for `method` + `key`.
///
/// * `base_url` — scheme + host, without a trailing slash (see
///   [`crate::CosHttpConfig::base_url`]).
/// * `host` — the `host[:port]` value that ends up in `q-header-list`.
/// * `params` — extra parameters to sign (e.g. `x-cos-security-token`,
///   `response-content-type`); they are percent-encoded and appended to the query
///   string in the given order.
/// * `key_time` — `"<start>;<end>"`; controls the validity window.
///
/// The returned URL is absolute and already percent-encoded.
pub fn presign_url(
    base_url: &str,
    host: &str,
    credentials: &CosCredentials,
    method: &str,
    key: &str,
    params: &[(String, String)],
    key_time: &str,
) -> String {
    let path = object_path(key);
    let headers = vec![("host".to_string(), host.to_string())];
    let signed = signature::sign_request(credentials, method, &path, params, &headers, key_time);

    let mut url = format!(
        "{}{}?",
        base_url.trim_end_matches('/'),
        signature::uri_encode_path(&path)
    );
    if !params.is_empty() {
        url.push_str(&query_string(params));
        url.push('&');
    }
    // `authorization` is exactly `q-sign-algorithm=..&..&q-signature=..`.
    url.push_str(&signed.authorization);
    url
}

/// The raw, *unencoded* object path used for signing (the URL carries the encoded
/// form).
///
/// This mirrors `CosHttpObjectStore::send`, which prefixes exactly one `/`: a key
/// that already starts with `/` therefore keeps it, so a presigned URL always
/// addresses the same object as [`crate::CosHttpObjectStore::get`].
fn object_path(key: &str) -> String {
    if key.is_empty() {
        "/".to_string()
    } else {
        format!("/{key}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_TIME: &str = "1700000000;1700000900";
    const BASE: &str = "https://examplebucket-1250000000.cos.ap-beijing.myqcloud.com";
    const HOST: &str = "examplebucket-1250000000.cos.ap-beijing.myqcloud.com";

    fn credentials() -> CosCredentials {
        CosCredentials::new("AKIDexample", "secret-key")
    }

    /// The URL carries the whole `q-*` chain that `sign_request` produced, with
    /// only `host` in the header list.
    #[test]
    fn url_reuses_the_authorization_chain() {
        let creds = credentials();
        let url = presign_url(BASE, HOST, &creds, "GET", "k", &[], KEY_TIME);
        let signed = signature::sign_request(
            &creds,
            "GET",
            "/k",
            &[],
            &[("host".to_string(), HOST.to_string())],
            KEY_TIME,
        );

        assert_eq!(
            url,
            format!("{BASE}/k?{}", signed.authorization),
            "the query string must be the Authorization value verbatim"
        );
        let expected_prefix = format!(
            "{BASE}/k?q-sign-algorithm=sha1&q-ak=AKIDexample\
             &q-sign-time={KEY_TIME}&q-key-time={KEY_TIME}\
             &q-header-list=host&q-url-param-list=&q-signature="
        );
        assert!(
            url.starts_with(&expected_prefix),
            "unexpected presigned URL: {url}"
        );
        assert_eq!(&url[expected_prefix.len()..], signed.signature);
        assert!(
            !url.contains(' '),
            "a presigned URL must not contain spaces"
        );
    }

    /// `host` is the only signed header — signing `date` would break browser use.
    #[test]
    fn only_host_is_signed() {
        let url = presign_url(BASE, HOST, &credentials(), "GET", "k", &[], KEY_TIME);
        assert!(url.contains("&q-header-list=host&"));
    }

    /// The method participates in the signature, so a PUT URL cannot be replayed
    /// as a GET.
    #[test]
    fn method_changes_the_signature() {
        let creds = credentials();
        let get = presign_url(BASE, HOST, &creds, "GET", "k", &[], KEY_TIME);
        let put = presign_url(BASE, HOST, &creds, "PUT", "k", &[], KEY_TIME);
        assert_ne!(get, put);
        assert_eq!(
            get.split("q-signature=").nth(1),
            Some(
                signature::sign_request(
                    &creds,
                    "GET",
                    "/k",
                    &[],
                    &[("host".to_string(), HOST.to_string())],
                    KEY_TIME
                )
                .signature
                .as_str()
            )
        );
    }

    /// Extra parameters are encoded, listed in `q-url-param-list`, and placed in
    /// front of the `q-*` chain.
    #[test]
    fn extra_parameters_are_signed_and_encoded() {
        let params = vec![
            (
                "response-content-type".to_string(),
                "application/octet-stream".to_string(),
            ),
            ("x-cos-security-token".to_string(), "tok/en=".to_string()),
        ];
        let url = presign_url(BASE, HOST, &credentials(), "GET", "k", &params, KEY_TIME);
        assert!(url.contains(
            "/k?response-content-type=application%2Foctet-stream\
             &x-cos-security-token=tok%2Fen%3D&q-sign-algorithm=sha1"
        ));
        assert!(url.contains("&q-url-param-list=response-content-type;x-cos-security-token&"));
    }

    /// Paths are percent-encoded in the URL but signed raw, and they match the
    /// path `CosHttpObjectStore::send` would use for the same key.
    #[test]
    fn object_paths_are_encoded_for_the_url() {
        let url = presign_url(
            BASE,
            HOST,
            &credentials(),
            "GET",
            "a b/中文.txt",
            &[],
            KEY_TIME,
        );
        assert!(url.contains("/a%20b/%E4%B8%AD%E6%96%87.txt?"));

        // Bucket-level presign (empty key) still has a valid path.
        let url = presign_url(BASE, HOST, &credentials(), "GET", "", &[], KEY_TIME);
        assert!(url.starts_with(&format!("{BASE}/?q-sign-algorithm=sha1")));

        // A leading slash is kept verbatim: `send` also prefixes exactly one `/`,
        // so the presigned URL addresses the same object as `get`.
        let url = presign_url(BASE, HOST, &credentials(), "GET", "/k", &[], KEY_TIME);
        let signed = signature::sign_request(
            &credentials(),
            "GET",
            "//k",
            &[],
            &[("host".to_string(), HOST.to_string())],
            KEY_TIME,
        );
        assert_eq!(url, format!("{BASE}//k?{}", signed.authorization));
    }

    #[test]
    fn expiry_is_validated_and_defaults_to_the_client_setting() {
        assert_eq!(resolve_expire_secs(None, 900).unwrap(), 900);
        assert_eq!(resolve_expire_secs(Some(60), 900).unwrap(), 60);
        assert_eq!(
            resolve_expire_secs(Some(MAX_PRESIGN_EXPIRE_SECS), 900).unwrap(),
            MAX_PRESIGN_EXPIRE_SECS
        );
        assert!(resolve_expire_secs(Some(0), 900).is_err());
        let err = resolve_expire_secs(Some(MAX_PRESIGN_EXPIRE_SECS + 1), 900).unwrap_err();
        assert!(err.to_string().contains("must not exceed"));
    }
}
