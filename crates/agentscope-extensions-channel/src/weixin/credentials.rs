//! iLink credentials plus the credential-exchange seam.
//!
//! Rust port of `WeixinCredentials` / `WeixinCredentialProvider`. The token is never rendered by a
//! `Debug` or `Display` impl, mirroring the Java record's redacted `toString`.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::common::{ChannelError, Result};

/// Credentials issued by iLink for one authorized Personal Weixin account.
///
/// The field is private so the token can only be read through [`WeixinCredentials::bot_token`],
/// and both `Debug` and `Display` are redacted by hand.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WeixinCredentials {
    bot_token: String,
}

impl WeixinCredentials {
    /// Build credentials, rejecting a blank token (mirrors the Java compact constructor).
    pub fn new(bot_token: impl Into<String>) -> Result<Self> {
        let bot_token = bot_token.into();
        if bot_token.trim().is_empty() {
            return Err(ChannelError::Config(
                "weixin.botToken must not be blank".to_string(),
            ));
        }
        Ok(Self { bot_token })
    }

    /// The bearer token. Callers must not log or embed this value.
    pub fn bot_token(&self) -> &str {
        &self.bot_token
    }
}

/// Redacted so an accidental `{:?}` never prints the token.
impl std::fmt::Debug for WeixinCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WeixinCredentials[botToken=***]")
    }
}

/// Redacted, mirroring the Java record's `toString`.
impl std::fmt::Display for WeixinCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WeixinCredentials[botToken=***]")
    }
}

/// Supplies the current iLink credentials to a running Weixin channel.
///
/// The Java `@FunctionalInterface` becomes an object-safe trait so a host can rotate credentials
/// without rebuilding the channel.
pub trait WeixinCredentialProvider: Send + Sync {
    /// The credentials valid right now.
    fn current(&self) -> Result<WeixinCredentials>;
}

/// A provider that always returns the same credentials.
#[derive(Debug)]
pub struct FixedCredentialProvider {
    credentials: WeixinCredentials,
}

impl FixedCredentialProvider {
    pub fn new(credentials: WeixinCredentials) -> Self {
        Self { credentials }
    }

    /// Wrap a raw token, mirroring `WeixinCredentialProvider.fixed(String)`.
    pub fn from_token(bot_token: impl Into<String>) -> Result<Self> {
        Ok(Self::new(WeixinCredentials::new(bot_token)?))
    }

    /// Shared handle suitable for `WeixinOutboundClient`.
    pub fn shared(self) -> Arc<dyn WeixinCredentialProvider> {
        Arc::new(self)
    }
}

impl WeixinCredentialProvider for FixedCredentialProvider {
    fn current(&self) -> Result<WeixinCredentials> {
        Ok(self.credentials.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_reject_a_blank_token() {
        assert!(WeixinCredentials::new("").is_err());
        assert!(WeixinCredentials::new("   ").is_err());
        assert_eq!(
            WeixinCredentials::new("secret-token").unwrap().bot_token(),
            "secret-token"
        );
    }

    #[test]
    fn diagnostic_strings_never_expose_the_token() {
        let credentials = WeixinCredentials::new("secret-token").unwrap();
        assert!(!format!("{credentials:?}").contains("secret-token"));
        assert!(!credentials.to_string().contains("secret-token"));
        assert_eq!(credentials.to_string(), "WeixinCredentials[botToken=***]");
    }

    #[test]
    fn fixed_provider_returns_the_same_credentials() {
        let provider = FixedCredentialProvider::from_token("tok").unwrap();
        assert_eq!(provider.current().unwrap().bot_token(), "tok");
        assert_eq!(provider.current().unwrap().bot_token(), "tok");
        // `Arc<dyn ..>` keeps the trait object-safe.
        let shared: Arc<dyn WeixinCredentialProvider> = provider.shared();
        assert_eq!(shared.current().unwrap().bot_token(), "tok");
    }
}
