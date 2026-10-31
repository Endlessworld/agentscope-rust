//! GitHub outbound HTTP client: `POST /repos/{owner}/{repo}/issues/{number}/comments`.

use std::sync::Arc;

use agentscope_core::message::Msg;
use agentscope_harness::agent::OutboundAddress;
use serde_json::json;

use crate::common::{ensure_success, ChannelError, HttpTransport, PreparedRequest, Result};

/// GitHub outbound client built on the pluggable [`HttpTransport`] seam.
pub struct GitHubOutboundClient {
    api_base: String,
    token: String,
    transport: Arc<dyn HttpTransport>,
}

/// Parsed `owner/repo#number` outbound target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadTarget {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl GitHubOutboundClient {
    pub fn new(
        api_base: impl Into<String>,
        token: impl Into<String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            api_base: api_base.into(),
            token: token.into(),
            transport,
        }
    }

    /// Parse `"<channelId>:thread:owner/repo#number"`; errors on malformed input.
    pub fn parse_address(to: &str) -> Result<ThreadTarget> {
        let first_colon = to
            .find(':')
            .ok_or_else(|| ChannelError::Unsupported(format!("unparseable peerId '{to}'")))?;
        let rest = &to[first_colon + 1..];
        let peer_id = match rest.find(':') {
            Some(second) => &rest[second + 1..],
            None => rest,
        };
        let hash = peer_id.find('#');
        let slash = peer_id.find('/');
        let (Some(hash), Some(slash)) = (hash, slash) else {
            return Err(ChannelError::Unsupported(format!(
                "unparseable peerId '{to}'"
            )));
        };
        if slash > hash {
            return Err(ChannelError::Unsupported(format!(
                "unparseable peerId '{to}'"
            )));
        }
        let owner = peer_id[..slash].to_string();
        let repo = peer_id[slash + 1..hash].to_string();
        let number = peer_id[hash + 1..]
            .trim()
            .parse::<u64>()
            .map_err(|_| ChannelError::Unsupported(format!("unparseable peerId '{to}'")))?;
        if owner.trim().is_empty() || repo.trim().is_empty() || number == 0 {
            return Err(ChannelError::Unsupported(format!(
                "unparseable peerId '{to}'"
            )));
        }
        Ok(ThreadTarget {
            owner,
            repo,
            number,
        })
    }

    /// Build one request per message (blank texts are skipped, mirroring Java `Mono.empty()`).
    pub fn prepare(
        &self,
        address: &OutboundAddress,
        messages: &[Msg],
    ) -> Result<Vec<PreparedRequest>> {
        let target = Self::parse_address(&address.to)?;
        let mut requests = Vec::new();
        for msg in messages {
            let text = msg.text_content();
            if text.trim().is_empty() {
                continue;
            }
            let url = format!(
                "{}/repos/{}/{}/issues/{}/comments",
                self.api_base.trim_end_matches('/'),
                target.owner,
                target.repo,
                target.number
            );
            requests.push(
                PreparedRequest::post_json(url, json!({ "body": text }))
                    .header("Authorization", format!("Bearer {}", self.token))
                    .header("Accept", "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", "2022-11-28")
                    .header("User-Agent", "agentscope-builder"),
            );
        }
        Ok(requests)
    }

    /// Execute one request per message.
    ///
    /// A non-2xx answer is an error (Java `WebClient.retrieve()`): a bad token, an unknown repo or
    /// a rejected comment must not look like a delivered reply (CHANNEL-N04).
    pub async fn send(&self, address: &OutboundAddress, messages: &[Msg]) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        for request in self.prepare(address, messages)? {
            let response = self.transport.execute(request).await?;
            ensure_success(&response)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testing::RecordingTransport;
    use crate::common::PreparedResponse;

    fn client(transport: Arc<RecordingTransport>) -> GitHubOutboundClient {
        GitHubOutboundClient::new("https://api.github.com", "pat", transport)
    }

    #[test]
    fn parses_thread_address() {
        let t = GitHubOutboundClient::parse_address("github:THREAD:octo/repo#7").unwrap();
        assert_eq!(t.owner, "octo");
        assert_eq!(t.repo, "repo");
        assert_eq!(t.number, 7);
    }

    #[test]
    fn rejects_malformed_addresses() {
        assert!(GitHubOutboundClient::parse_address("no-colon").is_err());
        assert!(GitHubOutboundClient::parse_address("github:THREAD:octo#7").is_err());
        assert!(GitHubOutboundClient::parse_address("github:THREAD:octo/repo").is_err());
        assert!(GitHubOutboundClient::parse_address("github:THREAD:octo/repo#abc").is_err());
        assert!(GitHubOutboundClient::parse_address("github:THREAD:octo/repo#0").is_err());
        assert!(GitHubOutboundClient::parse_address("github:THREAD:/repo#1").is_err());
    }

    #[test]
    fn builds_comment_request_with_github_headers() {
        let transport = Arc::new(RecordingTransport::new());
        let c = client(transport);
        let address = OutboundAddress::direct("github", "github:THREAD:octo/repo#7");
        let reqs = c.prepare(&address, &[Msg::user("hello")]).unwrap();
        assert_eq!(reqs.len(), 1);
        let req = &reqs[0];
        assert_eq!(
            req.url,
            "https://api.github.com/repos/octo/repo/issues/7/comments"
        );
        assert_eq!(req.header_value("Authorization"), Some("Bearer pat"));
        assert_eq!(
            req.header_value("Accept"),
            Some("application/vnd.github+json")
        );
        assert_eq!(req.header_value("X-GitHub-Api-Version"), Some("2022-11-28"));
        assert_eq!(req.header_value("User-Agent"), Some("agentscope-builder"));
        assert_eq!(req.body.as_ref().unwrap()["body"], json!("hello"));
    }

    #[test]
    fn skips_blank_messages() {
        let transport = Arc::new(RecordingTransport::new());
        let c = client(transport);
        let address = OutboundAddress::direct("github", "github:THREAD:octo/repo#7");
        let reqs = c
            .prepare(&address, &[Msg::user("  "), Msg::user("real")])
            .unwrap();
        assert_eq!(reqs.len(), 1);
    }

    #[tokio::test]
    async fn send_posts_one_comment_per_message() {
        let transport = Arc::new(RecordingTransport::new());
        let c = client(transport.clone());
        let address = OutboundAddress::direct("github", "github:THREAD:octo/repo#7");
        c.send(&address, &[Msg::user("a"), Msg::user("b")])
            .await
            .unwrap();
        assert_eq!(transport.request_count(), 2);
    }

    /// CHANNEL-N04: the Java client goes through `retrieve()`, so a rejected comment must be an
    /// error instead of a silent "delivered".
    #[tokio::test]
    async fn a_rejected_comment_is_an_error() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(401, r#"{"message":"Bad credentials"}"#),
        ]));
        let c = client(transport);
        let address = OutboundAddress::direct("github", "github:THREAD:octo/repo#7");

        let err = c
            .send(&address, &[Msg::user("a")])
            .await
            .expect_err("a 401 must not look like a delivered comment");
        assert!(
            matches!(err, ChannelError::Remote { status: 401, .. }),
            "{err}"
        );
        assert!(err.to_string().contains("Bad credentials"), "{err}");
    }
}
