//! GitLab bot-identity resolution (`GET /user`), used for bot-loop self-detection.
//!
//! Rust port of `io.agentscope.extensions.channel.gitlab.GitLabBotIdentityResolver`.
//! See [`crate::github::bot_identity`] for the shared design notes; the only differences are the
//! `PRIVATE-TOKEN` header and the `username` field name. The lookup is best-effort.

use std::sync::{Arc, RwLock};

use crate::common::{HttpTransport, PreparedRequest};
use crate::github::bot_identity::{BotIdentity, UNRESOLVED_USER_ID};

/// Resolves and caches the GitLab bot identity for a personal/project access token.
pub struct GitLabBotIdentityResolver {
    api_base: String,
    token: String,
    transport: Arc<dyn HttpTransport>,
    identity: RwLock<BotIdentity>,
}

impl std::fmt::Debug for GitLabBotIdentityResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitLabBotIdentityResolver")
            .field("api_base", &self.api_base)
            .field("identity", &self.identity())
            .finish()
    }
}

impl GitLabBotIdentityResolver {
    pub fn new(
        api_base: impl Into<String>,
        token: impl Into<String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            api_base: api_base.into(),
            token: token.into(),
            transport,
            identity: RwLock::new(BotIdentity::unknown()),
        }
    }

    /// The `GET /user` request, mirroring the Java `PRIVATE-TOKEN` header.
    pub fn build_request(&self) -> PreparedRequest {
        PreparedRequest::get(format!("{}/user", self.api_base.trim_end_matches('/')))
            .header("PRIVATE-TOKEN", self.token.clone())
            .header("Accept", "application/json")
    }

    /// Parse `GET /user`: `id` must be positive and `username` non-empty.
    pub fn parse_identity(body: &str) -> Option<BotIdentity> {
        let node: serde_json::Value = serde_json::from_str(body).ok()?;
        let user_id = node
            .get("id")
            .and_then(|v| v.as_i64())
            .unwrap_or(UNRESOLVED_USER_ID);
        let username = node
            .get("username")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string());
        if user_id > 0 {
            if let Some(username) = username {
                return Some(BotIdentity {
                    user_id,
                    login: Some(username),
                });
            }
        }
        None
    }

    pub fn set_identity(&self, identity: BotIdentity) {
        *self.identity.write().expect("identity lock poisoned") = identity;
    }

    pub fn identity(&self) -> BotIdentity {
        self.identity.read().expect("identity lock poisoned").clone()
    }

    /// The bot's numeric user id, or `-1` when unresolved.
    pub fn bot_user_id(&self) -> i64 {
        self.identity().user_id
    }

    /// The bot's username, or `None` when unresolved.
    pub fn bot_username(&self) -> Option<String> {
        self.identity().login
    }

    /// Best-effort `GET /user`; never blocks startup on failure.
    pub async fn refresh(&self) -> Option<BotIdentity> {
        let response = match self.transport.execute(self.build_request()).await {
            Ok(response) => response,
            Err(err) => {
                tracing::warn!("Failed to resolve GitLab bot identity: {err}");
                return None;
            }
        };
        let parsed = Self::parse_identity(&response.body);
        if let Some(identity) = &parsed {
            self.set_identity(identity.clone());
            tracing::info!(
                "GitLab bot identity resolved: id={}, username={}",
                identity.user_id,
                identity.login.as_deref().unwrap_or("")
            );
        }
        parsed
    }

    /// Whether `author_id` is the bot itself (webhook self-detection).
    pub fn is_self(&self, author_id: Option<i64>) -> bool {
        let bot_id = self.bot_user_id();
        bot_id > 0 && author_id == Some(bot_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testing::RecordingTransport;
    use crate::common::PreparedResponse;

    fn resolver(transport: Arc<dyn HttpTransport>) -> GitLabBotIdentityResolver {
        GitLabBotIdentityResolver::new("https://gitlab.com/api/v4", "glpat-token", transport)
    }

    #[test]
    fn builds_expected_private_token_request() {
        let r = resolver(Arc::new(RecordingTransport::new()));
        let req = r.build_request();
        assert_eq!(req.method, crate::common::HttpMethod::Get);
        assert_eq!(req.url, "https://gitlab.com/api/v4/user");
        assert_eq!(req.header_value("PRIVATE-TOKEN"), Some("glpat-token"));
        assert_eq!(req.header_value("Accept"), Some("application/json"));
    }

    #[test]
    fn parse_identity_requires_positive_id_and_username() {
        assert_eq!(
            GitLabBotIdentityResolver::parse_identity(r#"{"id":9,"username":"glab-bot"}"#),
            Some(BotIdentity {
                user_id: 9,
                login: Some("glab-bot".to_string())
            })
        );
        assert_eq!(GitLabBotIdentityResolver::parse_identity(r#"{"id":9}"#), None);
        assert_eq!(
            GitLabBotIdentityResolver::parse_identity(r#"{"id":-3,"username":"b"}"#),
            None
        );
        assert_eq!(GitLabBotIdentityResolver::parse_identity("nope"), None);
    }

    #[tokio::test]
    async fn refresh_caches_and_detects_self() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, r#"{"id":777,"username":"agentscope-bot"}"#),
        ]));
        let r = resolver(transport);
        assert_eq!(r.bot_user_id(), UNRESOLVED_USER_ID);
        assert!(!r.is_self(Some(777)));

        let resolved = r.refresh().await.expect("resolved");
        assert_eq!(resolved.user_id, 777);
        assert_eq!(r.bot_username().as_deref(), Some("agentscope-bot"));
        assert!(r.is_self(Some(777)));
        assert!(!r.is_self(Some(778)));
        assert!(!r.is_self(None));
    }

    #[tokio::test]
    async fn refresh_is_best_effort_on_unusable_body() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, r#"{"username":"nobody"}"#),
        ]));
        let r = resolver(transport);
        assert!(r.refresh().await.is_none());
        assert_eq!(r.bot_user_id(), UNRESOLVED_USER_ID);
    }
}
