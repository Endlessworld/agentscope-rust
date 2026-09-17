//! `TeamsMiddleware` — injects the agent-team context into the system prompt and
//! drains the member's mailbox into the reasoning input before each model call,
//! mirroring `io.agentscope.harness.agent.middleware.TeamsMiddleware`.
//!
//! The role-clipped [`TeamTool`](crate::tool::TeamTool) itself is registered at
//! build time; this middleware owns the per-turn prompt/mailbox wiring.
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::{ContentBlock, Msg, MsgRole};
use agentscope_core::middleware::{EventStream, Middleware, Next, ReasoningInput};

use super::subagents::prepend_to_system_message;
use crate::agent::team::{TeamClient, TeamContext, TeamRole};

/// Middleware binding one agent to one team.
pub struct TeamsMiddleware {
    client: Arc<dyn TeamClient>,
    team_id: String,
    /// This agent's member name on the team.
    member: String,
    role: TeamRole,
}

impl TeamsMiddleware {
    pub fn new(client: Arc<dyn TeamClient>, team_id: impl Into<String>, member: impl Into<String>) -> Self {
        Self {
            client,
            team_id: team_id.into(),
            member: member.into(),
            role: TeamRole::Member,
        }
    }

    /// Overrides this member's role (drives TeamTool clipping via runtime metadata).
    pub fn with_role(mut self, role: TeamRole) -> Self {
        self.role = role;
        self
    }

    /// Builds the middleware from a [`TeamContext`], defaulting the member to `fallback_member`
    /// when the context does not name one. Mirrors Java `new TeamsMiddleware(client, context)`.
    pub fn from_context(
        client: Arc<dyn TeamClient>,
        context: &TeamContext,
        fallback_member: impl Into<String>,
    ) -> Self {
        let team_id = context.team_id.clone().unwrap_or_default();
        let member = context
            .member
            .clone()
            .unwrap_or_else(|| fallback_member.into());
        let role = context.role.unwrap_or(TeamRole::Member);
        Self {
            client,
            team_id,
            member,
            role,
        }
    }

    fn team_section(&self) -> Option<String> {
        let info = self.client.team_info(&self.team_id).ok()?;
        let mut members: Vec<String> = info
            .members
            .iter()
            .map(|m| format!("- {} [{}]{}", m.name, m.role.as_str(), if m.active { "" } else { " (inactive)" }))
            .collect();
        members.sort();
        let ctx = TeamContext::for_role(&self.team_id, self.role);
        let ops = ctx.available_operations.join(", ");
        Some(format!(
            "<agent-teams>\nYou are `{}` in team `{}` ({}) with role {}.\nTeam members:\n{}\n\
             Coordinate with the `team` tool. Available operations for your role: {}.\n\
             Check the board and your messages regularly; broadcast decisions that \
             affect other members.\n</agent-teams>",
            self.member,
            info.id,
            info.name,
            self.role.as_str(),
            members.join("\n"),
            ops
        ))
    }
}

impl Middleware for TeamsMiddleware {
    fn on_reasoning(
        &self,
        _ctx: &RuntimeContext,
        input: ReasoningInput,
        next: Next<ReasoningInput>,
    ) -> EventStream {
        // System prompt injection: team context section.
        let section = self.team_section();
        let mut input = input;
        if let Some(section) = &section {
            input.messages = prepend_to_system_message(input.messages, section);
        }

        // Mailbox drain: deliver pending team messages as an assistant message.
        let mailbox = self.client.drain_mailbox(&self.team_id, &self.member);
        if !mailbox.is_empty() {
            let blocks: Vec<ContentBlock> = mailbox
                .iter()
                .map(|m| {
                    ContentBlock::Text(agentscope_core::message::TextBlock::new(format!(
                        "<team-message from=\"{}\" id=\"{}\">{}</team-message>",
                        m.from, m.id, m.content
                    )))
                })
                .collect();
            input.messages.push(
                Msg::builder()
                    .role(MsgRole::Assistant)
                    .content(blocks)
                    .build(),
            );
        }

        next(input)
    }
}
