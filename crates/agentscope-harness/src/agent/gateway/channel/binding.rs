//! One routing rule mapping inbound conditions to a target agent, mirroring
//! `ChannelBinding`.

use std::collections::BTreeSet;

use super::dm_scope::DmScope;

/// One routing rule that maps a set of inbound-message conditions to a target
/// `agent_id`. Evaluated by `ChannelRouter` at the tier corresponding to the most
/// specific non-null condition field it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelBinding {
    pub agent_id: String,
    pub peer: Option<String>,
    pub parent_peer: Option<String>,
    pub guild: Option<String>,
    pub roles: BTreeSet<String>,
    pub team: Option<String>,
    pub account: Option<String>,
    pub channel: Option<String>,
    pub session_scope: Option<DmScope>,
}

impl ChannelBinding {
    pub fn new(agent_id: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            peer: None,
            parent_peer: None,
            guild: None,
            roles: BTreeSet::new(),
            team: None,
            account: None,
            channel: None,
            session_scope: None,
        }
    }

    /// Matches an exact peer (e.g. a specific DM user or channel id).
    pub fn for_peer(peer: impl Into<String>, agent_id: impl Into<String>) -> Self {
        let mut b = Self::new(agent_id);
        b.peer = Some(peer.into());
        b
    }

    /// Matches messages that are threads under a specific parent peer.
    pub fn for_parent_peer(parent_peer: impl Into<String>, agent_id: impl Into<String>) -> Self {
        let mut b = Self::new(agent_id);
        b.parent_peer = Some(parent_peer.into());
        b
    }

    /// Matches any message from a specific guild (server/workspace).
    pub fn for_guild(guild: impl Into<String>, agent_id: impl Into<String>) -> Self {
        let mut b = Self::new(agent_id);
        b.guild = Some(guild.into());
        b
    }

    /// Matches messages from a guild where the sender holds at least one role.
    pub fn for_guild_roles(
        guild: impl Into<String>,
        roles: impl IntoIterator<Item = impl Into<String>>,
        agent_id: impl Into<String>,
    ) -> Self {
        let mut b = Self::new(agent_id);
        b.guild = Some(guild.into());
        b.roles = roles.into_iter().map(Into::into).collect();
        b
    }

    /// Matches a specific team id.
    pub fn for_team(team: impl Into<String>, agent_id: impl Into<String>) -> Self {
        let mut b = Self::new(agent_id);
        b.team = Some(team.into());
        b
    }

    /// Matches a specific account id.
    pub fn for_account(account: impl Into<String>, agent_id: impl Into<String>) -> Self {
        let mut b = Self::new(agent_id);
        b.account = Some(account.into());
        b
    }

    /// Matches a specific channel id.
    pub fn for_channel(channel: impl Into<String>, agent_id: impl Into<String>) -> Self {
        let mut b = Self::new(agent_id);
        b.channel = Some(channel.into());
        b
    }

    /// Returns a copy with a per-binding DM session scope override.
    pub fn with_session_scope(mut self, scope: DmScope) -> Self {
        self.session_scope = Some(scope);
        self
    }
}
