//! GitLab outbound HTTP client: `POST /projects/{id}/{issues|merge_requests}/{iid}/notes`.

use std::sync::Arc;

use agentscope_core::message::Msg;
use agentscope_harness::agent::OutboundAddress;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde_json::json;

use crate::common::{ensure_success, ChannelError, HttpTransport, PreparedRequest, Result};

/// Matches Java `URLEncoder.encode(path, UTF-8)`: everything except `A-Za-z0-9.-_*` is escaped.
const JAVA_URL_ENCODE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'*');

/// GitLab outbound client built on the pluggable [`HttpTransport`] seam.
pub struct GitLabOutboundClient {
    api_base: String,
    token: String,
    transport: Arc<dyn HttpTransport>,
}

/// Parsed `path/with/namespace#iid:Type` outbound target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteTarget {
    pub path: String,
    pub iid: u64,
    pub noteable_type: String,
}

impl GitLabOutboundClient {
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

    /// Parse `"<channelId>:thread:path/with/namespace#iid:Issue|MergeRequest"`.
    pub fn parse_address(to: &str) -> Result<NoteTarget> {
        fn err(to: &str) -> ChannelError {
            ChannelError::Unsupported(format!("unparseable peerId '{to}'"))
        }
        let first_colon = to.find(':').ok_or_else(|| err(to))?;
        let rest = &to[first_colon + 1..];
        let peer_id = match rest.find(':') {
            Some(second) => &rest[second + 1..],
            None => rest,
        };
        let hash = peer_id.find('#').ok_or_else(|| err(to))?;
        let path = peer_id[..hash].to_string();
        let tail = &peer_id[hash + 1..];
        let colon = tail.rfind(':').ok_or_else(|| err(to))?;
        let iid = tail[..colon].trim().parse::<u64>().map_err(|_| err(to))?;
        let noteable_type = tail[colon + 1..].to_string();
        if path.trim().is_empty()
            || iid == 0
            || !(noteable_type == "Issue" || noteable_type == "MergeRequest")
        {
            return Err(err(to));
        }
        Ok(NoteTarget {
            path,
            iid,
            noteable_type,
        })
    }

    /// Build one request per message (blank texts are skipped).
    pub fn prepare(
        &self,
        address: &OutboundAddress,
        messages: &[Msg],
    ) -> Result<Vec<PreparedRequest>> {
        let target = Self::parse_address(&address.to)?;
        let encoded_path = utf8_percent_encode(&target.path, JAVA_URL_ENCODE).to_string();
        let type_segment = if target.noteable_type == "Issue" {
            "issues"
        } else {
            "merge_requests"
        };
        let mut requests = Vec::new();
        for msg in messages {
            let text = msg.text_content();
            if text.trim().is_empty() {
                continue;
            }
            let url = format!(
                "{}/projects/{}/{}/{}/notes",
                self.api_base.trim_end_matches('/'),
                encoded_path,
                type_segment,
                target.iid
            );
            requests.push(
                PreparedRequest::post_json(url, json!({ "body": text }))
                    .header("PRIVATE-TOKEN", self.token.clone()),
            );
        }
        Ok(requests)
    }

    /// Execute one request per message.
    ///
    /// A non-2xx answer is an error (Java `WebClient.retrieve()`): a bad private token, an unknown
    /// project or a rejected note must not look like a delivered reply (CHANNEL-N04).
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

    fn client(transport: Arc<RecordingTransport>) -> GitLabOutboundClient {
        GitLabOutboundClient::new("https://gitlab.com/api/v4", "pat", transport)
    }

    #[test]
    fn parses_thread_address() {
        let t =
            GitLabOutboundClient::parse_address("gitlab:THREAD:grp/proj#9:MergeRequest").unwrap();
        assert_eq!(t.path, "grp/proj");
        assert_eq!(t.iid, 9);
        assert_eq!(t.noteable_type, "MergeRequest");
    }

    #[test]
    fn rejects_malformed_addresses() {
        assert!(GitLabOutboundClient::parse_address("no-colon").is_err());
        assert!(GitLabOutboundClient::parse_address("gitlab:THREAD:grp/proj#9").is_err());
        assert!(GitLabOutboundClient::parse_address("gitlab:THREAD:grp/proj#x:Issue").is_err());
        assert!(GitLabOutboundClient::parse_address("gitlab:THREAD:grp/proj#0:Issue").is_err());
        assert!(GitLabOutboundClient::parse_address("gitlab:THREAD:grp/proj#9:Snippet").is_err());
    }

    #[test]
    fn builds_issue_note_request_with_encoded_project_id() {
        let transport = Arc::new(RecordingTransport::new());
        let c = client(transport);
        let address = OutboundAddress::direct("gitlab", "gitlab:THREAD:grp/proj#9:Issue");
        let reqs = c.prepare(&address, &[Msg::user("nice")]).unwrap();
        assert_eq!(reqs.len(), 1);
        let req = &reqs[0];
        assert_eq!(
            req.url,
            "https://gitlab.com/api/v4/projects/grp%2Fproj/issues/9/notes"
        );
        assert_eq!(req.header_value("PRIVATE-TOKEN"), Some("pat"));
        assert_eq!(req.body.as_ref().unwrap()["body"], json!("nice"));
    }

    #[test]
    fn builds_merge_request_note_request() {
        let transport = Arc::new(RecordingTransport::new());
        let c = client(transport);
        let address = OutboundAddress::direct("gitlab", "gitlab:THREAD:grp/proj#9:MergeRequest");
        let reqs = c.prepare(&address, &[Msg::user("lgtm")]).unwrap();
        assert_eq!(
            reqs[0].url,
            "https://gitlab.com/api/v4/projects/grp%2Fproj/merge_requests/9/notes"
        );
    }

    #[tokio::test]
    async fn send_posts_note_per_message() {
        let transport = Arc::new(RecordingTransport::new());
        let c = client(transport.clone());
        let address = OutboundAddress::direct("gitlab", "gitlab:THREAD:grp/proj#9:Issue");
        c.send(&address, &[Msg::user("a"), Msg::user("b")])
            .await
            .unwrap();
        assert_eq!(transport.request_count(), 2);
    }

    /// CHANNEL-N04: the Java client goes through `retrieve()`, so a rejected note must be an error
    /// instead of a silent "delivered".
    #[tokio::test]
    async fn a_rejected_note_is_an_error() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(403, r#"{"message":"403 Forbidden"}"#),
        ]));
        let c = client(transport);
        let address = OutboundAddress::direct("gitlab", "gitlab:THREAD:grp/proj#9:Issue");

        let err = c
            .send(&address, &[Msg::user("lgtm")])
            .await
            .expect_err("a 403 must not look like a delivered note");
        assert!(
            matches!(err, ChannelError::Remote { status: 403, .. }),
            "{err}"
        );
        assert!(err.to_string().contains("403 Forbidden"), "{err}");
    }
}
