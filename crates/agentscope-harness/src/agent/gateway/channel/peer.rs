//! Conversation peer identity, mirroring `Peer` and `PeerKind`.

/// Classification of the conversation peer, used for session key construction and
/// DM-scope resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PeerKind {
    Direct,
    Channel,
    Group,
    Thread,
}

impl PeerKind {
    /// Lowercase wire value (e.g. `"direct"`), used in `Peer::key()`.
    pub fn value(self) -> &'static str {
        match self {
            PeerKind::Direct => "direct",
            PeerKind::Channel => "channel",
            PeerKind::Group => "group",
            PeerKind::Thread => "thread",
        }
    }

    /// Uppercase enum name (e.g. `"DIRECT"`), used in outbound address `to`.
    pub fn name(self) -> &'static str {
        match self {
            PeerKind::Direct => "DIRECT",
            PeerKind::Channel => "CHANNEL",
            PeerKind::Group => "GROUP",
            PeerKind::Thread => "THREAD",
        }
    }

    /// Whether this peer kind represents a direct (one-to-one) message.
    pub fn is_direct(self) -> bool {
        self == PeerKind::Direct
    }

    /// Whether this peer kind represents a thread nested inside another peer.
    pub fn is_thread(self) -> bool {
        self == PeerKind::Thread
    }
}

/// Identifies the conversation peer: a user DM, a channel, a group, or a thread.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Peer {
    pub kind: PeerKind,
    pub id: String,
}

impl Peer {
    pub fn new(kind: PeerKind, id: impl Into<String>) -> Self {
        Self {
            kind,
            id: id.into(),
        }
    }

    /// Composite key `"<kind>:<id>"` used in binding matching.
    pub fn key(&self) -> String {
        format!("{}:{}", self.kind.value(), self.id)
    }

    pub fn direct(id: impl Into<String>) -> Self {
        Self::new(PeerKind::Direct, id)
    }

    pub fn channel(id: impl Into<String>) -> Self {
        Self::new(PeerKind::Channel, id)
    }

    pub fn group(id: impl Into<String>) -> Self {
        Self::new(PeerKind::Group, id)
    }

    pub fn thread(id: impl Into<String>) -> Self {
        Self::new(PeerKind::Thread, id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_key_uses_lowercase_kind() {
        assert_eq!(Peer::direct("u_42").key(), "direct:u_42");
        assert_eq!(Peer::channel("c_ops").key(), "channel:c_ops");
    }

    #[test]
    fn peer_kind_flags() {
        assert!(PeerKind::Direct.is_direct());
        assert!(!PeerKind::Channel.is_direct());
        assert!(PeerKind::Thread.is_thread());
        assert!(!PeerKind::Group.is_thread());
        assert_eq!(PeerKind::Direct.value(), "direct");
        assert_eq!(PeerKind::Direct.name(), "DIRECT");
    }
}
