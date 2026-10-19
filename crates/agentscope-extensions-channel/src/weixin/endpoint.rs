//! iLink provider-endpoint allowlist.
//!
//! Rust port of `io.agentscope.extensions.channel.weixin.WeixinEndpointPolicy`: the checks that run
//! **before** iLink credentials are sent, so a misconfigured (or hostile) `baseUrl` can never make
//! the adapter hand a bot token to an arbitrary host.
//!
//! The policy is deliberately conservative and entirely textual: HTTPS (or plaintext HTTP on
//! literal loopback), official `*.weixin.qq.com` hosts on port 443, and no userinfo / query /
//! fragment / path. Loopback is matched on the literal host, never on DNS resolution, so a name
//! that merely *points* at `127.0.0.1` is not an approved endpoint.

use crate::common::{ChannelError, Result};

/// Official provider hosts, matched case-insensitively after the explicit allowlist entries.
pub const OFFICIAL_HOST: &str = "ilinkai.weixin.qq.com";
/// Second explicit official host (the login flow may return it).
pub const OFFICIAL_WEIXIN_HOST: &str = "weixin.qq.com";
/// Suffix shared by every official sub-domain.
pub const OFFICIAL_HOST_SUFFIX: &str = ".weixin.qq.com";
/// Default iLink base URL.
pub const DEFAULT_BASE_URL: &str = "https://ilinkai.weixin.qq.com";

/// Trim, infer `https://`, strip trailing slashes and validate the result.
///
/// Mirrors the Java `normalizeBaseUrl`; the JVM `IllegalArgumentException` becomes
/// [`ChannelError::Config`].
pub fn normalize_base_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ChannelError::Config(
            "weixin.baseUrl must not be blank".to_string(),
        ));
    }
    let mut value = trimmed.trim_end_matches('/').to_string();
    if !value.starts_with("http://") && !value.starts_with("https://") {
        value = format!("https://{value}");
    }
    let parsed = parse_absolute_url(&value)?;
    if !parsed.path.is_empty() {
        return Err(ChannelError::Config(
            "Invalid iLink base URL".to_string(),
        ));
    }
    if !is_secure_or_loopback(&parsed) || !is_allowed_host(&parsed.host, parsed.port) {
        return Err(ChannelError::Config(
            "iLink base URL is not an approved provider endpoint".to_string(),
        ));
    }
    Ok(value)
}

/// Validate that `candidate` may replace `current` for provider polling.
///
/// Allowed: the same host/port, any two official hosts, or any two loopback endpoints. Anything
/// else is refused so a redirect cannot move the long poll (and the bot token) off the provider.
/// Mirrors the Java `validateProviderEndpoint`, returning the normalized candidate.
pub fn validate_provider_endpoint(current: &str, candidate: &str) -> Result<String> {
    let from = parse_absolute_url(&normalize_base_url(current)?)?;
    let to = parse_absolute_url(&normalize_base_url(candidate)?)?;
    let loopback = is_loopback(&from.host) && is_loopback(&to.host);
    let same_official_provider = is_official_host(&from.host) && is_official_host(&to.host);
    let same_host = from.host.eq_ignore_ascii_case(&to.host) && from.port == to.port;
    if !is_secure_or_loopback(&to)
        || !is_allowed_host(&to.host, to.port)
        || !(same_host || same_official_provider || loopback)
    {
        return Err(ChannelError::Unsupported(
            "Untrusted iLink endpoint".to_string(),
        ));
    }
    Ok(to.to_string())
}

/// Whether `host` is one of the official provider hosts (or a sub-domain of it).
pub fn is_official_host(host: &str) -> bool {
    let lower = host.to_ascii_lowercase();
    lower == OFFICIAL_HOST
        || lower == OFFICIAL_WEIXIN_HOST
        || lower.ends_with(OFFICIAL_HOST_SUFFIX)
}

/// Whether `host` is the literal loopback host (`localhost`, `127.0.0.1`, `::1`).
pub fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

fn is_secure_or_loopback(url: &Url) -> bool {
    url.scheme == "https" || (url.scheme == "http" && is_loopback(&url.host))
}

fn is_allowed_host(host: &str, port: Option<u16>) -> bool {
    is_loopback(host)
        || (is_official_host(host) && (port.is_none() || port == Some(443)))
}

/// The subset of a URL the policy needs. Written by hand (no `url` crate dependency) because the
/// policy only ever inspects absolute `http(s)` URLs with no path, query or fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Url {
    scheme: String,
    host: String,
    port: Option<u16>,
    path: String,
}

impl Url {
    fn to_string(&self) -> String {
        match self.port {
            Some(port) => format!("{}://{}:{port}", self.scheme, self.host),
            None => format!("{}://{}", self.scheme, self.host),
        }
    }
}

/// Parse `scheme://host[:port][/path]` and reject userinfo, query or fragment.
fn parse_absolute_url(value: &str) -> Result<Url> {
    fn invalid() -> ChannelError {
        ChannelError::Config("Invalid iLink base URL".to_string())
    }
    let scheme_end = value.find("://").ok_or_else(invalid)?;
    let scheme = value[..scheme_end].to_ascii_lowercase();
    let rest = &value[scheme_end + 3..];
    if rest.contains('@') || rest.contains('?') || rest.contains('#') {
        return Err(invalid());
    }
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return Err(invalid());
    }
    // IPv6 literals (`[::1]`) keep their brackets off the host so `is_loopback` matches.
    let (host, port_text) = match authority.rfind(':') {
        Some(index) if !authority[index..].contains(']') => {
            (&authority[..index], Some(&authority[index + 1..]))
        }
        _ => (authority, None),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        return Err(invalid());
    }
    let port = match port_text {
        Some(text) if !text.is_empty() => Some(text.parse::<u16>().map_err(|_| invalid())?),
        Some(_) => return Err(invalid()),
        None => None,
    };
    Ok(Url {
        scheme,
        host: host.to_string(),
        port,
        path: path.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_https_and_strips_trailing_slashes() {
        assert_eq!(
            normalize_base_url("ilinkai.weixin.qq.com").unwrap(),
            "https://ilinkai.weixin.qq.com"
        );
        assert_eq!(
            normalize_base_url(" https://ilinkai.weixin.qq.com/// ").unwrap(),
            "https://ilinkai.weixin.qq.com"
        );
        assert_eq!(
            normalize_base_url("http://127.0.0.1:8089/").unwrap(),
            "http://127.0.0.1:8089"
        );
        assert_eq!(
            normalize_base_url("localhost:8089").unwrap(),
            "https://localhost:8089"
        );
    }

    #[test]
    fn rejects_blank_base_url() {
        assert!(normalize_base_url("").is_err());
        assert!(normalize_base_url("   ").is_err());
        assert!(normalize_base_url("///").is_err());
    }

    #[test]
    fn rejects_userinfo_query_fragment_and_path() {
        assert!(normalize_base_url("https://user:pass@ilinkai.weixin.qq.com").is_err());
        assert!(normalize_base_url("https://ilinkai.weixin.qq.com?debug=1").is_err());
        assert!(normalize_base_url("https://ilinkai.weixin.qq.com#fragment").is_err());
        assert!(normalize_base_url("https://ilinkai.weixin.qq.com/ilink").is_err());
    }

    #[test]
    fn rejects_hosts_outside_the_official_provider_and_loopback() {
        assert!(normalize_base_url("https://evil.example.com").is_err());
        assert!(normalize_base_url("https://ilinkai.weixin.qq.com.evil.com").is_err());
        assert!(normalize_base_url("https://evilweixin.qq.com").is_err());
    }

    #[test]
    fn rejects_hostnames_that_only_look_like_loopback() {
        assert!(normalize_base_url("http://localhost.attacker.example").is_err());
        assert!(normalize_base_url("http://127.0.0.1.attacker.example").is_err());
        assert!(normalize_base_url("https://localhost.attacker.example").is_err());
    }

    #[test]
    fn rejects_plaintext_http_outside_loopback() {
        assert!(normalize_base_url("http://ilinkai.weixin.qq.com").is_err());
        assert!(normalize_base_url("http://weixin.qq.com").is_err());
    }

    #[test]
    fn allows_redirects_within_the_official_provider_or_loopback_only() {
        assert_eq!(
            validate_provider_endpoint("https://ilinkai.weixin.qq.com", "https://weixin.qq.com")
                .unwrap(),
            "https://weixin.qq.com"
        );
        assert_eq!(
            validate_provider_endpoint("http://127.0.0.1:8089", "http://127.0.0.1:9999").unwrap(),
            "http://127.0.0.1:9999"
        );
        // Loopback -> official provider and official -> third party are both refused.
        assert!(
            validate_provider_endpoint("http://127.0.0.1:8089", "https://ilinkai.weixin.qq.com")
                .is_err()
        );
        assert!(
            validate_provider_endpoint("https://ilinkai.weixin.qq.com", "https://evil.example.com")
                .is_err()
        );
    }

    #[test]
    fn official_host_matching_is_suffix_bounded() {
        assert!(is_official_host("ilinkai.weixin.qq.com"));
        assert!(is_official_host("weixin.qq.com"));
        assert!(is_official_host("other.weixin.qq.com"));
        assert!(is_official_host("ILINKAI.WEIXIN.QQ.COM"));
        assert!(!is_official_host("notweixin.qq.com"));
        assert!(!is_official_host("weixin.qq.com.evil.com"));
    }
}
