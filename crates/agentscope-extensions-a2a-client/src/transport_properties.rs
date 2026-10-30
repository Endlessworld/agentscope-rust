//! Transport endpoint properties used to synthesize A2A [`AgentInterface`] entries.
//!
//! Mirrors Java `io.agentscope.core.a2a.server.transport.TransportProperties`.
//!
//! Lives in the client crate (2026-10-29) so the server-side card builder and any
//! client-side consumer share one definition instead of re-declaring it per crate.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::model::AgentInterface;

const HTTPS: &str = "https";
const HTTP: &str = "http";

/// Transport endpoint properties used to synthesize [`AgentInterface`] entries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportProperties {
    pub transport_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default)]
    pub support_tls: bool,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl TransportProperties {
    /// Returns the Java-style fluent builder for `transport_type`.
    pub fn builder(transport_type: impl Into<String>) -> TransportPropertiesBuilder {
        TransportPropertiesBuilder::new(transport_type)
    }

    /// The effective port: the configured one, else the scheme default (443/80).
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or(if self.support_tls { 443 } else { 80 })
    }

    /// The path with a leading `/` when non-empty, mirroring Java `getPath`.
    pub fn normalized_path(&self) -> String {
        let path = self.path.as_deref().unwrap_or("");
        if path.is_empty() {
            String::new()
        } else if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        }
    }

    /// Renders `%s://%s:%d%s`, mirroring Java `AgentScopeAgentCardConverter#createAdditionalInterface`.
    pub fn to_url(&self) -> String {
        let scheme = if self.support_tls { HTTPS } else { HTTP };
        format!(
            "{}://{}:{}{}",
            scheme,
            format_host_for_url(self.host.as_deref().unwrap_or_default()),
            self.effective_port(),
            self.normalized_path()
        )
    }

    /// Converts this transport into an [`AgentInterface`].
    pub fn to_agent_interface(&self) -> AgentInterface {
        AgentInterface {
            transport: self.transport_type.clone(),
            url: self.to_url(),
        }
    }
}

/// Formats a host for use in a URL authority, adding brackets to unbracketed IPv6 literals.
///
/// Mirrors Java `NetworkUtils#formatHostForUrl`: host names, IPv4 addresses and already
/// bracketed IPv6 hosts are returned unchanged; an unbracketed IPv6 literal such as `::1`
/// becomes `[::1]`, so `scheme://host:port` stays a valid HTTP authority.
pub fn format_host_for_url(host: &str) -> String {
    let needs_brackets = host.contains(':') && !(host.starts_with('[') && host.ends_with(']'));
    if needs_brackets {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// Builder for [`TransportProperties`].
#[derive(Debug, Clone)]
pub struct TransportPropertiesBuilder {
    transport_type: String,
    host: Option<String>,
    port: Option<u16>,
    path: Option<String>,
    support_tls: bool,
    extra: Map<String, Value>,
}

impl TransportPropertiesBuilder {
    fn new(transport_type: impl Into<String>) -> Self {
        Self {
            transport_type: transport_type.into(),
            host: None,
            port: None,
            path: None,
            support_tls: false,
            extra: Map::new(),
        }
    }

    pub fn host(mut self, host: impl Into<String>) -> Self {
        self.host = Some(host.into());
        self
    }

    pub fn port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    pub fn support_tls(mut self, support_tls: bool) -> Self {
        self.support_tls = support_tls;
        self
    }

    pub fn extra(mut self, key: impl Into<String>, value: Value) -> Self {
        self.extra.insert(key.into(), value);
        self
    }

    pub fn build(self) -> TransportProperties {
        TransportProperties {
            transport_type: self.transport_type,
            host: self.host,
            port: self.port,
            path: self.path,
            support_tls: self.support_tls,
            extra: self.extra,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Java: NetworkUtilsTest#formatHostForUrl — host names, IPv4 and bracketed IPv6 keep their
    // existing form, an unbracketed IPv6 literal gains brackets.
    #[test]
    fn ipv6_hosts_are_bracketed_and_other_hosts_are_untouched() {
        assert_eq!(format_host_for_url("::1"), "[::1]");
        assert_eq!(format_host_for_url("2001:db8::1"), "[2001:db8::1]");
        assert_eq!(format_host_for_url("[::1]"), "[::1]");
        assert_eq!(format_host_for_url("agent.example.com"), "agent.example.com");
        assert_eq!(format_host_for_url("127.0.0.1"), "127.0.0.1");
        assert_eq!(format_host_for_url(""), "");
    }

    // Java: AgentScopeAgentCardConverterTest — the synthesized interface URL must stay a valid
    // authority for an IPv6 host.
    #[test]
    fn ipv6_transport_renders_a_bracketed_authority() {
        let ipv6 = TransportProperties::builder("JSONRPC")
            .host("::1")
            .port(8080)
            .path("a2a")
            .build();
        assert_eq!(ipv6.to_url(), "http://[::1]:8080/a2a");
        assert_eq!(
            ipv6.to_agent_interface(),
            AgentInterface {
                transport: "JSONRPC".to_string(),
                url: "http://[::1]:8080/a2a".to_string(),
            }
        );

        let hostname = TransportProperties::builder("JSONRPC")
            .host("localhost")
            .port(8080)
            .build();
        assert_eq!(hostname.to_url(), "http://localhost:8080");
    }
}
