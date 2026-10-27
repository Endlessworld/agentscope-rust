//! GitHub bot-identity resolution (`GET /user`), used for bot-loop self-detection.
//!
//! Rust port of `io.agentscope.extensions.channel.github.GitHubBotIdentityResolver`.
//!
//! The Java class resolves the bot's own numeric user id for the configured PAT and caches it for
//! the channel lifetime. When a webhook delivers a comment authored by the bot itself, the channel
//! must drop it instead of looping forever.
//!
//! The lookup is **best-effort**: a network failure never blocks startup and simply leaves the
//! identity unresolved (`bot_user_id() == None`). That mirrors the Java `refresh()` which catches
//! every exception and logs at WARN.

use std::sync::{Arc, RwLock};

use crate::common::{HttpTransport, PreparedRequest, Result};

/// Marks an unresolved identity; the Java class uses `-1`.
pub const UNRESOLVED_USER_ID: i64 = -1;

/// Resolved bot identity: numeric user id plus login handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotIdentity {
    pub user_id: i64,
    pub login: Option<String>,
}

impl BotIdentity {
    /// The Java `Identity.UNKNOWN` sentinel.
    pub fn unknown() -> Self {
        Self {
            user_id: UNRESOLVED_USER_ID,
            login: None,
        }
    }

    /// Whether the identity carries a usable numeric id.
    pub fn is_resolved(&self) -> bool {
        self.user_id > 0
    }
}

impl Default for BotIdentity {
    fn default() -> Self {
        Self::unknown()
    }
}

/// Resolves and caches the GitHub bot identity for a PAT.
pub struct GitHubBotIdentityResolver {
    api_base: String,
    token: String,
    transport: Arc<dyn HttpTransport>,
    identity: RwLock<BotIdentity>,
}

impl std::fmt::Debug for GitHubBotIdentityResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubBotIdentityResolver")
            .field("api_base", &self.api_base)
            .field("identity", &self.identity())
            .finish()
    }
}

impl GitHubBotIdentityResolver {
    /// Build with an optional `botUserLogin` override.
    ///
    /// Mirrors the Java constructor: a non-blank `bot_user_login_override` pre-seeds the identity
    /// with `user_id = -1` (still unresolved) and the supplied login.
    pub fn new(
        api_base: impl Into<String>,
        token: impl Into<String>,
        bot_user_login_override: Option<String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        let identity = match bot_user_login_override {
            Some(login) if !login.trim().is_empty() => BotIdentity {
                user_id: UNRESOLVED_USER_ID,
                login: Some(login),
            },
            _ => BotIdentity::unknown(),
        };
        Self {
            api_base: api_base.into(),
            token: token.into(),
            transport,
            identity: RwLock::new(identity),
        }
    }

    /// The `GET /user` request, with the headers the Java client sends.
    pub fn build_request(&self) -> PreparedRequest {
        PreparedRequest::get(format!("{}/user", self.api_base.trim_end_matches('/')))
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "agentscope-builder")
    }

    /// Parse `GET /user` body: `id` must be positive and `login` non-null to be accepted,
    /// exactly like the Java `if (id > 0 && login != null)` guard.
    pub fn parse_identity(body: &str) -> Option<BotIdentity> {
        let node: serde_json::Value = serde_json::from_str(body).ok()?;
        let user_id = node
            .get("id")
            .and_then(|v| v.as_i64())
            .unwrap_or(UNRESOLVED_USER_ID);
        let login = node
            .get("login")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string());
        if user_id > 0 {
            if let Some(login) = login {
                return Some(BotIdentity {
                    user_id,
                    login: Some(login),
                });
            }
        }
        None
    }

    /// Store a parsed identity.
    pub fn set_identity(&self, identity: BotIdentity) {
        *self.identity.write().expect("identity lock poisoned") = identity;
    }

    /// The cached identity.
    pub fn identity(&self) -> BotIdentity {
        self.identity
            .read()
            .expect("identity lock poisoned")
            .clone()
    }

    /// The bot's numeric user id, or `-1` when unresolved.
    pub fn bot_user_id(&self) -> i64 {
        self.identity().user_id
    }

    /// The bot's login handle, or `None` when unresolved.
    pub fn bot_login(&self) -> Option<String> {
        self.identity().login
    }

    /// Best-effort `GET /user`. Never returns an error: a transport failure or an unusable body
    /// leaves the previous identity in place (Java `refresh()` logs at WARN and continues).
    pub async fn refresh(&self) -> Option<BotIdentity> {
        let response = match self.transport.execute(self.build_request()).await {
            Ok(response) => response,
            Err(err) => {
                tracing::warn!("Failed to resolve GitHub bot identity: {err}");
                return None;
            }
        };
        let parsed = Self::parse_identity(&response.body);
        if let Some(identity) = &parsed {
            self.set_identity(identity.clone());
            tracing::info!(
                "GitHub bot identity resolved: id={}, login={}",
                identity.user_id,
                identity.login.as_deref().unwrap_or("")
            );
        }
        parsed
    }

    /// [`GitHubBotIdentityResolver::refresh`] behind the crate [`Result`] type, for callers that
    /// want to observe the transport failure explicitly.
    pub async fn try_refresh(&self) -> Result<Option<BotIdentity>> {
        let response = self.transport.execute(self.build_request()).await?;
        let parsed = Self::parse_identity(&response.body);
        if let Some(identity) = &parsed {
            self.set_identity(identity.clone());
        }
        Ok(parsed)
    }

    /// Whether `commenter_id` is the bot itself. Mirrors the webhook check
    /// `botId > 0 && commenterId.isPresent() && commenterId == botId`.
    pub fn is_self(&self, commenter_id: Option<i64>) -> bool {
        let bot_id = self.bot_user_id();
        bot_id > 0 && commenter_id == Some(bot_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testing::RecordingTransport;
    use crate::common::PreparedResponse;

    fn resolver(transport: Arc<dyn HttpTransport>) -> GitHubBotIdentityResolver {
        GitHubBotIdentityResolver::new("https://api.github.com", "ghp_token", None, transport)
    }

    #[test]
    fn builds_expected_get_user_request() {
        let r = resolver(Arc::new(RecordingTransport::new()));
        let req = r.build_request();
        assert_eq!(req.method, crate::common::HttpMethod::Get);
        assert_eq!(req.url, "https://api.github.com/user");
        assert_eq!(req.header_value("Authorization"), Some("Bearer ghp_token"));
        assert_eq!(
            req.header_value("Accept"),
            Some("application/vnd.github+json")
        );
        assert_eq!(req.header_value("X-GitHub-Api-Version"), Some("2022-11-28"));
        assert_eq!(req.header_value("User-Agent"), Some("agentscope-builder"));
    }

    #[test]
    fn login_override_preseed_without_user_id() {
        let r = GitHubBotIdentityResolver::new(
            "https://api.github.com",
            "t",
            Some("my-bot".to_string()),
            Arc::new(RecordingTransport::new()),
        );
        assert_eq!(r.bot_user_id(), UNRESOLVED_USER_ID);
        assert_eq!(r.bot_login().as_deref(), Some("my-bot"));
        // A blank override behaves like no override.
        let r = GitHubBotIdentityResolver::new(
            "https://api.github.com",
            "t",
            Some("   ".to_string()),
            Arc::new(RecordingTransport::new()),
        );
        assert_eq!(r.bot_login(), None);
    }

    #[test]
    fn parse_identity_requires_positive_id_and_login() {
        assert_eq!(
            GitHubBotIdentityResolver::parse_identity(r#"{"id":42,"login":"octo"}"#),
            Some(BotIdentity {
                user_id: 42,
                login: Some("octo".to_string())
            })
        );
        // Missing login, missing id, non-positive id, blank login, malformed JSON.
        assert_eq!(
            GitHubBotIdentityResolver::parse_identity(r#"{"id":42}"#),
            None
        );
        assert_eq!(
            GitHubBotIdentityResolver::parse_identity(r#"{"login":"octo"}"#),
            None
        );
        assert_eq!(
            GitHubBotIdentityResolver::parse_identity(r#"{"id":0,"login":"octo"}"#),
            None
        );
        assert_eq!(
            GitHubBotIdentityResolver::parse_identity(r#"{"id":42,"login":"  "}"#),
            None
        );
        assert_eq!(GitHubBotIdentityResolver::parse_identity("not json"), None);
    }

    #[tokio::test]
    async fn refresh_caches_resolved_identity() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, r#"{"id":12345,"login":"agentscope-bot"}"#),
        ]));
        let r = resolver(transport.clone());
        let resolved = r.refresh().await.expect("resolved");
        assert_eq!(resolved.user_id, 12345);
        assert_eq!(r.bot_user_id(), 12345);
        assert_eq!(r.bot_login().as_deref(), Some("agentscope-bot"));
        assert_eq!(transport.request_count(), 1);
    }

    #[tokio::test]
    async fn refresh_is_best_effort_on_error_and_bad_body() {
        // Unusable body keeps the sentinel identity.
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, "{}"),
        ]));
        let r = resolver(transport);
        assert!(r.refresh().await.is_none());
        assert_eq!(r.bot_user_id(), UNRESOLVED_USER_ID);
        assert!(!r.identity().is_resolved());
    }

    #[test]
    fn self_detection_requires_resolved_bot_identity() {
        let r = resolver(Arc::new(RecordingTransport::new()));
        // Unresolved: never claims a match, even for the sentinel id.
        assert!(!r.is_self(Some(UNRESOLVED_USER_ID)));
        assert!(!r.is_self(Some(7)));
        assert!(!r.is_self(None));

        r.set_identity(BotIdentity {
            user_id: 7,
            login: Some("bot".to_string()),
        });
        assert!(r.is_self(Some(7)));
        assert!(!r.is_self(Some(8)));
        assert!(!r.is_self(None));
    }
}
