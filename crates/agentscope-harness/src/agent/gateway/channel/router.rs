//! Resolves the target agent id and stable `MsgContext` for an `InboundMessage` by
//! evaluating `ChannelBinding` rules in deterministic priority order, mirroring
//! `ChannelRouter` and `RouteResult`.
//!
//! Binding evaluation tiers (highest to lowest priority):
//! `explicit` → `peer` → `parentPeer` → `guild+roles` → `guild` → `team` →
//! `account` → `channel` → `channel-default` → `global-default`.

use std::collections::BTreeMap;

use crate::agent::gateway::harness_gateway::OutboundAddress;
use crate::agent::gateway::msg_context::MsgContext;

use super::binding::ChannelBinding;
use super::config::ChannelConfig;
use super::dm_scope::DmScope;
use super::inbound::InboundMessage;

/// Output of `ChannelRouter::resolve_route`: the resolved agent id, the `MsgContext`
/// to pass to the gateway, a diagnostic `matched_by` label, and the outbound address.
#[derive(Debug, Clone)]
pub struct RouteResult {
    pub agent_id: String,
    pub context: MsgContext,
    pub matched_by: String,
    pub outbound_address: OutboundAddress,
}

/// Evaluates bindings in priority order and returns a `RouteResult` ready for gateway
/// execution.
pub struct ChannelRouter {
    global_default_agent_id: String,
}

/// Binding evaluation tiers, highest to lowest priority.
const TIERS: [&str; 7] = [
    "peer",
    "parentPeer",
    "guildRoles",
    "guild",
    "team",
    "account",
    "channel",
];

impl ChannelRouter {
    /// `global_default_agent_id` is the fallback agent id when no binding and no
    /// channel-level default match; typically the id of the agent registered via
    /// `bindMainAgent`.
    pub fn new(global_default_agent_id: Option<&str>) -> Self {
        Self {
            global_default_agent_id: global_default_agent_id.unwrap_or("main").to_string(),
        }
    }

    /// Evaluates bindings in priority order and returns a `RouteResult` ready for
    /// gateway execution.
    pub fn resolve_route(&self, config: &ChannelConfig, msg: &InboundMessage) -> RouteResult {
        let explicit = msg
            .preferred_agent_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let matched_binding = self.find_matched_binding(&config.bindings, msg);

        let (agent_id, matched_by) = if let Some(e) = explicit {
            (e.to_string(), "explicit".to_string())
        } else if let Some(b) = &matched_binding {
            (
                b.agent_id.clone(),
                self.detect_matched_tier(&config.bindings, msg),
            )
        } else if let Some(def) = &config.default_agent_id {
            (def.clone(), "channel-default".to_string())
        } else {
            (
                self.global_default_agent_id.clone(),
                "global-default".to_string(),
            )
        };

        let effective_scope = match &matched_binding {
            Some(b) => b.session_scope.unwrap_or(config.dm_scope),
            None => config.dm_scope,
        };

        let user_id = msg.sender_id.clone().or_else(|| {
            if msg.peer.kind.is_direct() {
                Some(msg.peer.id.clone())
            } else {
                None
            }
        });

        let context = self.build_context(msg, effective_scope, &agent_id, user_id.as_deref());
        let outbound = build_outbound_address(msg);
        RouteResult {
            agent_id,
            context,
            matched_by,
            outbound_address: outbound,
        }
    }

    // -----------------------------------------------------------------
    //  Binding evaluation - 7 tiers
    // -----------------------------------------------------------------

    fn find_matched_binding(
        &self,
        bindings: &[ChannelBinding],
        msg: &InboundMessage,
    ) -> Option<ChannelBinding> {
        for tier in TIERS {
            if let Some(b) = self.find_first_binding(bindings, msg, tier) {
                return Some(b);
            }
        }
        None
    }

    fn detect_matched_tier(&self, bindings: &[ChannelBinding], msg: &InboundMessage) -> String {
        for tier in TIERS {
            if self.find_first_binding(bindings, msg, tier).is_some() {
                return if tier == "guildRoles" {
                    "guild+roles".to_string()
                } else {
                    tier.to_string()
                };
            }
        }
        "none".to_string()
    }

    fn find_first_binding(
        &self,
        bindings: &[ChannelBinding],
        msg: &InboundMessage,
        tier: &str,
    ) -> Option<ChannelBinding> {
        bindings
            .iter()
            .find(|b| self.matches(b, msg, tier))
            .cloned()
    }

    fn matches(&self, b: &ChannelBinding, msg: &InboundMessage, tier: &str) -> bool {
        match tier {
            "peer" => b.peer.as_deref() == Some(&msg.peer.key()),
            "parentPeer" => {
                b.parent_peer.is_some()
                    && msg
                        .parent_peer
                        .as_ref()
                        .map(|p| b.parent_peer.as_deref() == Some(&p.key()))
                        .unwrap_or(false)
            }
            "guildRoles" => {
                b.guild.is_some()
                    && !b.roles.is_empty()
                    && b.guild.as_deref() == msg.guild.as_deref()
                    && msg.roles.iter().any(|r| b.roles.contains(r))
            }
            "guild" => {
                b.guild.is_some()
                    && b.roles.is_empty()
                    && b.guild.as_deref() == msg.guild.as_deref()
            }
            "team" => b.team.is_some() && b.team.as_deref() == msg.team.as_deref(),
            "account" => b.account.is_some() && b.account.as_deref() == msg.account_id.as_deref(),
            "channel" => b.channel.is_some() && b.channel.as_deref() == Some(&msg.channel_id),
            _ => false,
        }
    }

    // -----------------------------------------------------------------
    //  MsgContext construction from routing result + dmScope
    // -----------------------------------------------------------------

    fn build_context(
        &self,
        msg: &InboundMessage,
        dm_scope: DmScope,
        agent_id: &str,
        user_id: Option<&str>,
    ) -> MsgContext {
        let mut extra = BTreeMap::new();
        extra.insert("agentId".to_string(), agent_id.to_string());
        let channel = msg.channel_id.clone();
        let peer = &msg.peer;

        let ctx = if peer.kind.is_thread() {
            let parent_room = msg.parent_peer.as_ref().map(|p| p.id.clone());
            MsgContext {
                channel: Some(channel),
                group: msg.guild.clone(),
                room: parent_room,
                thread_id: Some(peer.id.clone()),
                thread_ts: None,
                extra,
                user_id: None,
            }
        } else if peer.kind.is_direct() {
            Self::build_dm_context(
                channel,
                &peer.id,
                msg.account_id.as_deref(),
                dm_scope,
                extra,
            )
        } else {
            MsgContext {
                channel: Some(channel),
                group: msg.guild.clone(),
                room: Some(peer.id.clone()),
                thread_id: None,
                thread_ts: None,
                extra,
                user_id: None,
            }
        };

        match user_id {
            Some(u) => ctx.with_user_id(u),
            None => ctx,
        }
    }

    fn build_dm_context(
        channel: String,
        peer_id: &str,
        account_id: Option<&str>,
        dm_scope: DmScope,
        extra: BTreeMap<String, String>,
    ) -> MsgContext {
        match dm_scope {
            DmScope::Main => MsgContext {
                channel: Some(channel),
                group: None,
                room: None,
                thread_id: None,
                thread_ts: None,
                extra,
                user_id: None,
            },
            DmScope::PerPeer | DmScope::PerChannelPeer => MsgContext {
                channel: Some(channel),
                group: None,
                room: Some(peer_id.to_string()),
                thread_id: None,
                thread_ts: None,
                extra,
                user_id: None,
            },
            DmScope::PerAccountChannelPeer => MsgContext {
                channel: Some(channel),
                group: account_id.map(str::to_string),
                room: Some(peer_id.to_string()),
                thread_id: None,
                thread_ts: None,
                extra,
                user_id: None,
            },
        }
    }
}

fn build_outbound_address(msg: &InboundMessage) -> OutboundAddress {
    let to = format!(
        "{}:{}:{}",
        msg.channel_id,
        msg.peer.kind.name(),
        msg.peer.id
    );
    let thread_id = if msg.peer.kind.is_thread() {
        Some(msg.peer.id.clone())
    } else {
        None
    };
    OutboundAddress {
        channel_id: msg.channel_id.clone(),
        account_id: msg.account_id.clone(),
        to,
        thread_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::gateway::channel::peer::Peer;

    fn msg(text: &str) -> Vec<agentscope_core::message::Msg> {
        vec![agentscope_core::message::Msg::user(text.to_string())]
    }

    #[test]
    fn explicit_agent_short_circuits_bindings() {
        let config = ChannelConfig::of("chatui");
        let router = ChannelRouter::new(Some("main"));
        let inbound = InboundMessage::dm_for("chatui", "u_1", "support", msg("hi"));
        let r = router.resolve_route(&config, &inbound);
        assert_eq!(r.agent_id, "support");
        assert_eq!(r.matched_by, "explicit");
    }

    #[test]
    fn peer_binding_matches_highest_tier() {
        let config = ChannelConfig::builder("chatui")
            .binding(ChannelBinding::for_guild("g1", "guild-agent"))
            .binding(ChannelBinding::for_peer("direct:u_42", "peer-agent"))
            .build();
        let router = ChannelRouter::new(Some("main"));
        let inbound = InboundMessage::dm("chatui", "u_42", msg("hi"));
        let r = router.resolve_route(&config, &inbound);
        assert_eq!(r.agent_id, "peer-agent");
        assert_eq!(r.matched_by, "peer");
    }

    #[test]
    fn guild_roles_binding_takes_precedence_over_guild() {
        let config = ChannelConfig::builder("chatui")
            .binding(ChannelBinding::for_guild("g1", "guild-agent"))
            .binding(ChannelBinding::for_guild_roles(
                "g1",
                ["admin"],
                "role-agent",
            ))
            .build();
        let router = ChannelRouter::new(Some("main"));
        // sender has "admin" role -> guild+roles tier wins over plain guild tier
        let inbound = InboundMessage::builder("chatui", Peer::channel("c1"), msg("hi"))
            .guild("g1")
            .roles(["admin"])
            .build();
        let r = router.resolve_route(&config, &inbound);
        assert_eq!(r.agent_id, "role-agent");
        assert_eq!(r.matched_by, "guild+roles");
    }

    #[test]
    fn falls_back_to_channel_default_then_global() {
        let config = ChannelConfig::of_with_default("chatui", "channel-def");
        let router = ChannelRouter::new(Some("main"));
        let inbound = InboundMessage::channel_room("chatui", "c1", None::<String>, msg("hi"));
        let r = router.resolve_route(&config, &inbound);
        assert_eq!(r.agent_id, "channel-def");
        assert_eq!(r.matched_by, "channel-default");

        let config2 = ChannelConfig::of("chatui");
        let r2 = router.resolve_route(&config2, &inbound);
        assert_eq!(r2.agent_id, "main");
        assert_eq!(r2.matched_by, "global-default");
    }

    #[test]
    fn main_dm_scope_single_session() {
        let config = ChannelConfig::of("chatui"); // default Main
        let router = ChannelRouter::new(Some("main"));
        let a = router.resolve_route(&config, &InboundMessage::dm("chatui", "u_1", msg("a")));
        let b = router.resolve_route(&config, &InboundMessage::dm("chatui", "u_2", msg("b")));
        // All DMs share one session (room is None).
        assert_eq!(a.context.room, None);
        assert_eq!(a.context.canonical_key(), b.context.canonical_key());
    }

    #[test]
    fn per_peer_dm_scope_separates_sessions() {
        let config = ChannelConfig::builder("chatui")
            .dm_scope(DmScope::PerPeer)
            .build();
        let router = ChannelRouter::new(Some("main"));
        let a = router.resolve_route(&config, &InboundMessage::dm("chatui", "u_1", msg("a")));
        let b = router.resolve_route(&config, &InboundMessage::dm("chatui", "u_2", msg("b")));
        assert_eq!(a.context.room.as_deref(), Some("u_1"));
        assert_ne!(a.context.canonical_key(), b.context.canonical_key());
    }

    #[test]
    fn thread_context_uses_parent_room_and_thread_id() {
        let config = ChannelConfig::of("chatui");
        let router = ChannelRouter::new(Some("main"));
        let inbound = InboundMessage::builder("chatui", Peer::thread("t_9"), msg("hi"))
            .parent_peer(Peer::channel("c_ops"))
            .guild("g1")
            .build();
        let r = router.resolve_route(&config, &inbound);
        assert_eq!(r.context.room.as_deref(), Some("c_ops"));
        assert_eq!(r.context.thread_id.as_deref(), Some("t_9"));
        assert_eq!(r.context.group.as_deref(), Some("g1"));
        // Outbound thread id mirrors the thread peer id.
        assert_eq!(r.outbound_address.thread_id.as_deref(), Some("t_9"));
        assert_eq!(r.outbound_address.to, "chatui:THREAD:t_9");
    }

    #[test]
    fn channel_peer_sets_user_id_from_sender() {
        let config = ChannelConfig::of("chatui");
        let router = ChannelRouter::new(Some("main"));
        let inbound = InboundMessage::builder("chatui", Peer::channel("c1"), msg("hi"))
            .sender_id("alice")
            .guild("g1")
            .build();
        let r = router.resolve_route(&config, &inbound);
        assert_eq!(r.context.user_id.as_deref(), Some("alice"));
        assert_eq!(r.context.room.as_deref(), Some("c1"));
        assert_eq!(r.context.group.as_deref(), Some("g1"));
        assert_eq!(
            r.context.extra.get("agentId").map(String::as_str),
            Some("main")
        );
    }
}
