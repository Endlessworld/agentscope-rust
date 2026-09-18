//! Outbound-address parsing shared by every platform outbound client.
//!
//! Harness `ChannelRouter` emits `OutboundAddress::to` as `"<channelId>:<KIND>:<peerId>"` (see
//! `agentscope_harness::agent::gateway::channel::router::build_outbound_address`). The Java
//! outbound clients re-parse that string to recover the `PeerKind` and the platform-specific
//! peer id; this module centralises that identical logic.

use agentscope_harness::agent::PeerKind;

/// A parsed outbound target: the peer kind plus the platform-specific recipient id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerTarget {
    /// Resolved peer kind (defaults to `Direct` when the address is unparseable).
    pub kind: PeerKind,
    /// Platform-specific recipient identifier.
    pub id: String,
}

impl PeerTarget {
    pub fn new(kind: PeerKind, id: impl Into<String>) -> Self {
        Self {
            kind,
            id: id.into(),
        }
    }
}

/// Parse `"<channelId>[:<KIND>]:<peerId>"` into a [`PeerTarget`].
///
/// Mirrors the Java `parseAddress(...)` helpers: an unrecognised or missing kind falls back to
/// `Direct`, and the peer id keeps any further `:` separators verbatim.
pub fn parse_peer_target(to: &str) -> PeerTarget {
    let Some(sep) = to.find(':') else {
        return PeerTarget::new(PeerKind::Direct, to);
    };
    let rest = &to[sep + 1..];
    match rest.find(':') {
        None => PeerTarget::new(PeerKind::Direct, rest),
        Some(sep2) => {
            let kind_raw = &rest[..sep2];
            let id = &rest[sep2 + 1..];
            let kind = match kind_raw.to_ascii_uppercase().as_str() {
                "DIRECT" => PeerKind::Direct,
                "CHANNEL" => PeerKind::Channel,
                "GROUP" => PeerKind::Group,
                "THREAD" => PeerKind::Thread,
                _ => PeerKind::Direct,
            };
            PeerTarget::new(kind, id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_routed_channel_kind_peer_format() {
        let t = parse_peer_target("dingtalk:GROUP:conv-42");
        assert_eq!(t.kind, PeerKind::Group);
        assert_eq!(t.id, "conv-42");

        let t = parse_peer_target("github:THREAD:octo/repo#7");
        assert_eq!(t.kind, PeerKind::Thread);
        assert_eq!(t.id, "octo/repo#7");
    }

    #[test]
    fn peer_id_may_contain_colons() {
        let t = parse_peer_target("gitlab:THREAD:grp/proj#9:MergeRequest");
        assert_eq!(t.kind, PeerKind::Thread);
        assert_eq!(t.id, "grp/proj#9:MergeRequest");
    }

    #[test]
    fn unknown_kind_falls_back_to_direct() {
        let t = parse_peer_target("channel:WAT:id");
        assert_eq!(t.kind, PeerKind::Direct);
        assert_eq!(t.id, "id");

        // Lower-case kinds are accepted too (Java upper-cases before `valueOf`).
        let t = parse_peer_target("feishu:group:chat-1");
        assert_eq!(t.kind, PeerKind::Group);
        assert_eq!(t.id, "chat-1");
    }

    #[test]
    fn address_without_separator_is_the_whole_id() {
        let t = parse_peer_target("just-an-id");
        assert_eq!(t.kind, PeerKind::Direct);
        assert_eq!(t.id, "just-an-id");

        // Only a channel prefix, no kind segment: the remainder is the peer id.
        let t = parse_peer_target("wecom:user-1");
        assert_eq!(t.kind, PeerKind::Direct);
        assert_eq!(t.id, "user-1");
    }
}
