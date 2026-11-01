//! Proxy configuration for the `openai-official` provider.
//!
//! Mirrors `io.agentscope.core.model.transport.ProxyConfig` plus the
//! `OpenAISdkClientFactory#applyProxy` wiring. Java builds an OkHttp client with
//! an HTTP/SOCKS proxy, optional basic auth and a `nonProxyHosts` bypass list;
//! Rust builds an equivalent `reqwest::Client` and hands it to
//! [`crate::common::ReqwestTransport::with_client`].
//!
//! ## Supported / unsupported (never silently dropped)
//! | Java feature | Rust |
//! |---|---|
//! | `HTTP` proxy + basic auth | supported (`reqwest::Proxy::all` + `basic_auth`)
//! | `nonProxyHosts` bypass | supported — resolved against the model base URL host, exactly like Java
//! | `SOCKS4` / `SOCKS5` | **explicit error**: this workspace builds `reqwest` without the `socks` feature
//! | authenticated SOCKS | **explicit error** (same as Java)

use agentscope_core::{Error, Result};
use serde_json::Value;

/// Proxy protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProxyType {
    /// Plain HTTP CONNECT proxy.
    #[default]
    Http,
    /// SOCKS4 proxy (no authentication).
    Socks4,
    /// SOCKS5 proxy.
    Socks5,
}

impl ProxyType {
    /// Parses a proxy type name (`http`, `socks4`, `socks5`, case-insensitive).
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "http" | "https" => Some(ProxyType::Http),
            "socks4" => Some(ProxyType::Socks4),
            "socks5" | "socks" => Some(ProxyType::Socks5),
            _ => None,
        }
    }
}

/// Proxy endpoint plus optional credentials and bypass rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyConfig {
    /// Proxy protocol.
    pub proxy_type: ProxyType,
    /// Proxy host or IP.
    pub host: String,
    /// Proxy port (1-65535).
    pub port: u16,
    /// Optional basic-auth user (HTTP proxies only).
    pub username: Option<String>,
    /// Optional basic-auth password (HTTP proxies only).
    pub password: Option<String>,
    /// Hosts that must bypass the proxy: exact names, `*.suffix` or `prefix.*`.
    pub non_proxy_hosts: Vec<String>,
}

impl ProxyConfig {
    /// Plain HTTP proxy without authentication.
    pub fn http(host: impl Into<String>, port: u16) -> Self {
        Self {
            proxy_type: ProxyType::Http,
            host: host.into(),
            port,
            username: None,
            password: None,
            non_proxy_hosts: Vec::new(),
        }
    }

    /// HTTP proxy with basic authentication.
    pub fn http_with_auth(
        host: impl Into<String>,
        port: u16,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            username: Some(username.into()),
            password: Some(password.into()),
            ..Self::http(host, port)
        }
    }

    /// SOCKS4 proxy (no authentication).
    pub fn socks4(host: impl Into<String>, port: u16) -> Self {
        Self {
            proxy_type: ProxyType::Socks4,
            ..Self::http(host, port)
        }
    }

    /// SOCKS5 proxy without authentication.
    pub fn socks5(host: impl Into<String>, port: u16) -> Self {
        Self {
            proxy_type: ProxyType::Socks5,
            ..Self::http(host, port)
        }
    }

    /// Sets the bypass list (`nonProxyHosts`).
    pub fn with_non_proxy_hosts<I, S>(mut self, hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.non_proxy_hosts = hosts.into_iter().map(Into::into).collect();
        self
    }

    /// Whether username **and** password are present (Java's `hasAuthentication`).
    pub fn has_authentication(&self) -> bool {
        matches!(self.username.as_deref(), Some(u) if !u.is_empty())
            && matches!(self.password.as_deref(), Some(p) if !p.is_empty())
    }

    /// Whether `hostname` must bypass the proxy.
    ///
    /// Same patterns as Java: exact match, `*.suffix` (suffix match), `prefix.*`
    /// (prefix match) and any other `*` wildcard.
    pub fn should_bypass(&self, hostname: &str) -> bool {
        if hostname.is_empty() {
            return false;
        }
        let hostname = hostname.to_ascii_lowercase();
        self.non_proxy_hosts.iter().any(|pattern| {
            let pattern = pattern.trim().to_ascii_lowercase();
            if pattern.is_empty() {
                return false;
            }
            if pattern == hostname {
                return true;
            }
            if let Some(suffix) = pattern.strip_prefix('*') {
                // `*.example.com` -> suffix match (including the leading dot).
                if suffix.starts_with('.') {
                    return hostname.ends_with(suffix);
                }
            }
            if let Some(prefix) = pattern.strip_suffix('*') {
                // `192.168.*` -> prefix match.
                if prefix.ends_with('.') || prefix.is_empty() {
                    return hostname.starts_with(prefix);
                }
            }
            if pattern.contains('*') {
                // General glob: split on `*` and match the parts in order.
                let parts: Vec<&str> = pattern.split('*').collect();
                let mut rest = hostname.as_str();
                for (index, part) in parts.iter().enumerate() {
                    if part.is_empty() {
                        continue;
                    }
                    match rest.find(part) {
                        Some(pos) if index == 0 && pos != 0 => return false,
                        Some(pos) => rest = &rest[pos + part.len()..],
                        None => return false,
                    }
                }
                return true;
            }
            false
        })
    }

    /// Validates the configuration, failing fast on anything that cannot be
    /// honoured (authenticated SOCKS, SOCKS at all, bad port).
    pub fn validate(&self) -> Result<()> {
        if self.host.trim().is_empty() {
            return Err(Error::Model("proxy host must not be blank".to_string()));
        }
        if self.port == 0 {
            return Err(Error::Model(
                "proxy port must be between 1 and 65535".to_string(),
            ));
        }
        if self.has_authentication() && self.proxy_type != ProxyType::Http {
            return Err(Error::Model(
                "Authenticated SOCKS proxies are not supported by the openai-official provider"
                    .to_string(),
            ));
        }
        if self.proxy_type != ProxyType::Http {
            return Err(Error::Model(format!(
                "SOCKS proxies ({:?}) are not supported: this build of reqwest is compiled \
                 without the `socks` feature. Configure an HTTP proxy instead.",
                self.proxy_type
            )));
        }
        Ok(())
    }

    /// The proxy URL (`http://host:port`).
    pub fn proxy_url(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    /// Parses a `proxy` option object:
    /// `{"type", "host", "port", "username", "password", "nonProxyHosts"}`.
    pub fn from_json(value: &Value) -> Result<Self> {
        let object = value.as_object().ok_or_else(|| {
            Error::Model("proxy must be a JSON object with host/port (optionally type)".to_string())
        })?;
        let kind = object
            .get("type")
            .and_then(Value::as_str)
            .map(|value| {
                ProxyType::parse(value).ok_or_else(|| {
                    Error::Model(format!(
                        "Invalid proxy type '{value}'; expected http, socks4 or socks5"
                    ))
                })
            })
            .transpose()?
            .unwrap_or(ProxyType::Http);
        let host = object
            .get("host")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|host| !host.is_empty())
            .ok_or_else(|| Error::Model("proxy host must not be blank".to_string()))?
            .to_string();
        let port = object
            .get("port")
            .and_then(|port| {
                port.as_u64()
                    .or_else(|| port.as_str().and_then(|s| s.trim().parse().ok()))
            })
            .and_then(|port| u16::try_from(port).ok())
            .ok_or_else(|| {
                Error::Model("proxy port must be an integer between 1 and 65535".to_string())
            })?;
        let non_proxy_hosts = object
            .get("nonProxyHosts")
            .and_then(Value::as_array)
            .map(|hosts| {
                hosts
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            proxy_type: kind,
            host,
            port,
            username: object
                .get("username")
                .and_then(Value::as_str)
                .map(str::to_string),
            password: object
                .get("password")
                .and_then(Value::as_str)
                .map(str::to_string),
            non_proxy_hosts,
        })
    }
}

/// Builds a `reqwest::Client` honouring `config`.
///
/// `base_url` supplies the host used for the `nonProxyHosts` bypass decision:
/// the model talks to a single base URL for its whole lifetime, exactly like the
/// Java client factory.
pub fn build_client(config: &ProxyConfig, base_url: &str) -> Result<reqwest::Client> {
    config.validate()?;
    let host = host_of(base_url);
    agentscope_core::tls::ensure_crypto_provider();
    let mut builder = reqwest::Client::builder();
    if !config.should_bypass(&host) {
        let mut proxy = reqwest::Proxy::all(config.proxy_url())
            .map_err(|e| Error::Model(format!("Failed to configure proxy: {e}")))?;
        if let (Some(username), Some(password)) = (&config.username, &config.password) {
            if config.has_authentication() {
                proxy = proxy.basic_auth(username, password);
            }
        }
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|e| Error::Model(format!("Failed to construct HTTP client: {e}")))
}

/// Extracts the host from a base URL (`https://api.openai.com/v1` -> `api.openai.com`).
fn host_of(url: &str) -> String {
    let without_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(
            without_scheme
                .split(['/', '?', '#'])
                .next()
                .unwrap_or_default(),
        )
        .split(':')
        .next()
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bypass_patterns_match_java_semantics() {
        let config = ProxyConfig::http("proxy.local", 8080).with_non_proxy_hosts(vec![
            "localhost",
            "*.internal.com",
            "192.168.*",
        ]);
        assert!(config.should_bypass("localhost"));
        assert!(config.should_bypass("api.internal.com"));
        assert!(config.should_bypass("192.168.1.10"));
        assert!(!config.should_bypass("api.openai.com"));
        assert!(!config.should_bypass("notlocalhost"));
        assert!(!config.should_bypass(""));
        // No bypass list => never bypass.
        assert!(!ProxyConfig::http("proxy.local", 8080).should_bypass("localhost"));
    }

    #[test]
    fn http_proxy_builds_a_client_and_bypass_skips_the_proxy() {
        let config = ProxyConfig::http("127.0.0.1", 3128);
        assert!(build_client(&config, "https://api.openai.com/v1").is_ok());

        let bypass = ProxyConfig::http_with_auth("127.0.0.1", 3128, "user", "pass")
            .with_non_proxy_hosts(vec!["api.openai.com"]);
        // Credentials + bypass still builds fine (the proxy is simply not applied).
        assert!(build_client(&bypass, "https://api.openai.com/v1").is_ok());
    }

    #[test]
    fn unsupported_configurations_fail_loudly() {
        let socks = ProxyConfig::socks5("127.0.0.1", 1080);
        let err = build_client(&socks, "https://api.openai.com/v1").unwrap_err();
        assert!(err.to_string().contains("socks"), "{err}");

        let auth_socks = ProxyConfig {
            username: Some("u".to_string()),
            password: Some("p".to_string()),
            ..ProxyConfig::socks4("127.0.0.1", 1080)
        };
        assert!(auth_socks
            .validate()
            .unwrap_err()
            .to_string()
            .contains("Authenticated SOCKS"));

        assert!(ProxyConfig::http("", 8080).validate().is_err());
        assert!(ProxyConfig::http("host", 0).validate().is_err());
    }

    #[test]
    fn parses_the_proxy_option_object() {
        let config = ProxyConfig::from_json(&serde_json::json!({
            "type": "http",
            "host": "proxy.test",
            "port": 8080,
            "username": "u",
            "password": "p",
            "nonProxyHosts": ["localhost"],
        }))
        .unwrap();
        assert_eq!(config.host, "proxy.test");
        assert_eq!(config.port, 8080);
        assert!(config.has_authentication());
        assert_eq!(config.non_proxy_hosts, vec!["localhost".to_string()]);

        assert!(ProxyConfig::from_json(&serde_json::json!({"host": "h"})).is_err());
        assert!(ProxyConfig::from_json(&serde_json::json!({
            "type": "ftp",
            "host": "h",
            "port": 1
        }))
        .is_err());
    }

    #[test]
    fn host_extraction_handles_paths_and_ports() {
        assert_eq!(host_of("https://api.openai.com/v1"), "api.openai.com");
        assert_eq!(host_of("http://localhost:8080"), "localhost");
        assert_eq!(host_of("api.openai.com"), "api.openai.com");
    }
}
