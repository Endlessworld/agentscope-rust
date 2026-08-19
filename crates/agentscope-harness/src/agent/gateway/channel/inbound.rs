//! Normalized inbound message produced by a channel adapter before routing,
//! mirroring `InboundMessage`.

use std::collections::BTreeSet;

use agentscope_core::message::Msg;

use super::peer::Peer;

/// Normalized inbound message carrying all identity and context metadata needed by
/// `ChannelRouter` to resolve an agent and build a stable session key.
#[derive(Debug, Clone)]
pub struct InboundMessage {
    /// Identifier of the originating channel (e.g. `"chatui"`, `"slack"`).
    pub channel_id: String,
    /// Optional multi-account identifier (e.g. Slack app installation id).
    pub account_id: Option<String>,
    /// The conversation peer (DM user, channel, group, or thread).
    pub peer: Peer,
    /// Optional sender identity for user-level isolation.
    pub sender_id: Option<String>,
    /// Optional parent peer for thread-nested messages.
    pub parent_peer: Option<Peer>,
    /// Optional guild / server / workspace id.
    pub guild: Option<String>,
    /// Optional team id.
    pub team: Option<String>,
    /// Optional set of role ids the sender holds within the guild.
    pub roles: BTreeSet<String>,
    /// The actual message content to forward to the agent.
    pub messages: Vec<Msg>,
    /// Optional explicit agent override (tier-0 short-circuit before any binding tier).
    pub preferred_agent_id: Option<String>,
}

impl InboundMessage {
    pub fn new(channel_id: impl Into<String>, peer: Peer, messages: Vec<Msg>) -> Self {
        Self {
            channel_id: channel_id.into(),
            account_id: None,
            peer,
            sender_id: None,
            parent_peer: None,
            guild: None,
            team: None,
            roles: BTreeSet::new(),
            messages,
            preferred_agent_id: None,
        }
    }

    /// Single-turn DM with no guild/team/role metadata (typical for direct chat UI use).
    pub fn dm(channel_id: impl Into<String>, peer_id: impl Into<String>, messages: Vec<Msg>) -> Self {
        let peer_id = peer_id.into();
        Self {
            channel_id: channel_id.into(),
            account_id: None,
            peer: Peer::direct(peer_id.clone()),
            sender_id: Some(peer_id),
            parent_peer: None,
            guild: None,
            team: None,
            roles: BTreeSet::new(),
            messages,
            preferred_agent_id: None,
        }
    }

    /// Single-turn DM with an explicit `preferred_agent_id` override.
    pub fn dm_for(
        channel_id: impl Into<String>,
        peer_id: impl Into<String>,
        agent_id: impl Into<String>,
        messages: Vec<Msg>,
    ) -> Self {
        let peer_id = peer_id.into();
        Self {
            channel_id: channel_id.into(),
            account_id: None,
            peer: Peer::direct(peer_id.clone()),
            sender_id: Some(peer_id),
            parent_peer: None,
            guild: None,
            team: None,
            roles: BTreeSet::new(),
            messages,
            preferred_agent_id: Some(agent_id.into()),
        }
    }

    /// Channel / group message with an explicit sender and optional guild context.
    pub fn channel(
        channel_id: impl Into<String>,
        room_id: impl Into<String>,
        sender_id: impl Into<String>,
        guild: Option<impl Into<String>>,
        messages: Vec<Msg>,
    ) -> Self {
        Self {
            channel_id: channel_id.into(),
            account_id: None,
            peer: Peer::channel(room_id),
            sender_id: Some(sender_id.into()),
            parent_peer: None,
            guild: guild.map(Into::into),
            team: None,
            roles: BTreeSet::new(),
            messages,
            preferred_agent_id: None,
        }
    }

    /// Channel message with optional guild context (no explicit sender identity).
    pub fn channel_room(
        channel_id: impl Into<String>,
        room_id: impl Into<String>,
        guild: Option<impl Into<String>>,
        messages: Vec<Msg>,
    ) -> Self {
        Self {
            channel_id: channel_id.into(),
            account_id: None,
            peer: Peer::channel(room_id),
            sender_id: None,
            parent_peer: None,
            guild: guild.map(Into::into),
            team: None,
            roles: BTreeSet::new(),
            messages,
            preferred_agent_id: None,
        }
    }

    /// Whether this message originates from a direct / DM peer.
    pub fn is_dm(&self) -> bool {
        self.peer.kind.is_direct()
    }

    /// Whether this message is a thread-nested reply.
    pub fn is_thread(&self) -> bool {
        self.peer.kind.is_thread()
    }

    /// Returns a builder for constructing messages with all optional fields.
    pub fn builder(channel_id: impl Into<String>, peer: Peer, messages: Vec<Msg>) -> InboundMessageBuilder {
        InboundMessageBuilder {
            channel_id: channel_id.into(),
            account_id: None,
            peer,
            sender_id: None,
            parent_peer: None,
            guild: None,
            team: None,
            roles: BTreeSet::new(),
            messages,
            preferred_agent_id: None,
        }
    }
}

/// Builder for `InboundMessage`.
#[derive(Debug, Clone)]
pub struct InboundMessageBuilder {
    channel_id: String,
    account_id: Option<String>,
    peer: Peer,
    sender_id: Option<String>,
    parent_peer: Option<Peer>,
    guild: Option<String>,
    team: Option<String>,
    roles: BTreeSet<String>,
    messages: Vec<Msg>,
    preferred_agent_id: Option<String>,
}

impl InboundMessageBuilder {
    pub fn account_id(mut self, account_id: impl Into<String>) -> Self {
        self.account_id = Some(account_id.into());
        self
    }

    pub fn sender_id(mut self, sender_id: impl Into<String>) -> Self {
        self.sender_id = Some(sender_id.into());
        self
    }

    pub fn parent_peer(mut self, parent_peer: Peer) -> Self {
        self.parent_peer = Some(parent_peer);
        self
    }

    pub fn guild(mut self, guild: impl Into<String>) -> Self {
        self.guild = Some(guild.into());
        self
    }

    pub fn team(mut self, team: impl Into<String>) -> Self {
        self.team = Some(team.into());
        self
    }

    pub fn roles(mut self, roles: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.roles = roles.into_iter().map(Into::into).collect();
        self
    }

    pub fn preferred_agent_id(mut self, preferred_agent_id: impl Into<String>) -> Self {
        self.preferred_agent_id = Some(preferred_agent_id.into());
        self
    }

    pub fn build(self) -> InboundMessage {
        InboundMessage {
            channel_id: self.channel_id,
            account_id: self.account_id,
            peer: self.peer,
            sender_id: self.sender_id,
            parent_peer: self.parent_peer,
            guild: self.guild,
            team: self.team,
            roles: self.roles,
            messages: self.messages,
            preferred_agent_id: self.preferred_agent_id,
        }
    }
}
