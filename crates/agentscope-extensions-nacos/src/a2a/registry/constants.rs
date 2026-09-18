//! Constants for the Nacos A2A registry, mirroring
//! `io.agentscope.core.nacos.a2a.registry.constants.Constants`.

/// HTTP protocol constant.
pub const PROTOCOL_TYPE_HTTP: &str = "http";

/// HTTPS protocol constant.
pub const PROTOCOL_TYPE_HTTPS: &str = "https";

/// Default protocol returned when none is configured (mirrors
/// `AiConstants.A2a.A2A_ENDPOINT_DEFAULT_PROTOCOL`).
pub const TRANSPORT_DEFAULT_PROTOCOL: &str = "http";

/// Prefix of environment variables for transport properties.
pub const PROPERTIES_ENV_PREFIX: &str = "NACOS_A2A_AGENT_";

/// Environment-variable transport attributes, mirroring
/// `Constants.TransportPropertiesAttribute`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportPropertiesAttribute {
    Host,
    Port,
    Path,
    Protocol,
    Query,
    SupportTls,
}

impl TransportPropertiesAttribute {
    /// The env key segment used after `{TRANSPORT}_`.
    pub fn env_key(&self) -> &'static str {
        match self {
            Self::Host => "HOST",
            Self::Port => "PORT",
            Self::Path => "PATH",
            Self::Protocol => "PROTOCOL",
            Self::Query => "QUERY",
            Self::SupportTls => "SUPPORTTLS",
        }
    }

    /// The property key exposed in structured properties.
    pub fn property_key(&self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Port => "port",
            Self::Path => "path",
            Self::Protocol => "protocol",
            Self::Query => "query",
            Self::SupportTls => "supportTls",
        }
    }

    /// Resolves an attribute from its (upper-cased) env key.
    pub fn from_env_key(env_key: &str) -> Option<Self> {
        match env_key {
            "HOST" => Some(Self::Host),
            "PORT" => Some(Self::Port),
            "PATH" => Some(Self::Path),
            "PROTOCOL" => Some(Self::Protocol),
            "QUERY" => Some(Self::Query),
            "SUPPORTTLS" => Some(Self::SupportTls),
            _ => None,
        }
    }
}
