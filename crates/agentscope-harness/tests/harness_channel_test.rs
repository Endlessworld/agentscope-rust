//! Integration tests for the gateway channel routing and Chat UI adapter.
use std::sync::Arc;

use async_trait::async_trait;
use agentscope_core::error::Result;
use agentscope_core::message::Msg;
use agentscope_harness::{
    ChannelManager, ChannelSink, ChatUiChannel, GatewayAgent, GatewayTurnContext, HarnessGateway,
    InboundMessage, OutboundAddress, SendOptions,
};

/// Echo agent: replies with a fixed assistant message, counting input messages.
struct EchoAgent;

#[async_trait]
impl GatewayAgent for EchoAgent {
    async fn call(&self, messages: Vec<Msg>, _ctx: &GatewayTurnContext) -> Result<Msg> {
        Ok(Msg::assistant(format!("echo {} messages", messages.len())))
    }
}

async fn gateway_with_echo() -> Arc<HarnessGateway> {
    let gw = Arc::new(HarnessGateway::create_standalone());
    gw.bind_main_agent(Arc::new(EchoAgent)).await;
    gw
}

#[test]
fn send_options_session_key_resolution() {
    assert_eq!(SendOptions::user_id("u1").effective_session_key(), "u1");
    assert_eq!(SendOptions::of("u1", "s1").effective_session_key(), "s1");
    assert_eq!(
        SendOptions::user_id("u1").with_agent_id("support").agent_id.as_deref(),
        Some("support")
    );
}

#[test]
fn preview_route_defaults_to_global_main() {
    let chat = ChatUiChannel::create();
    let inbound = InboundMessage::dm("chatui", "u_1", vec![Msg::user("hi")]);
    let route = chat.preview_route(&inbound);
    assert_eq!(route.agent_id, "main");
    assert_eq!(route.matched_by, "global-default");
    assert_eq!(route.outbound_address.channel_id, "chatui");
    assert_eq!(route.outbound_address.to, "chatui:DIRECT:u_1");
}

#[tokio::test]
async fn chatui_send_routes_through_gateway() {
    let gw = gateway_with_echo().await;
    let chat = ChatUiChannel::with_gateway(gw);
    let reply = chat.send("hello").await.unwrap();
    assert!(reply.text_content().contains("echo 1 messages"));
}

#[tokio::test]
async fn chatui_send_to_subagent_and_peer() {
    let gw = gateway_with_echo().await;
    let chat = ChatUiChannel::with_gateway(gw);
    let reply = chat.send_peer("u_9", "ping").await.unwrap();
    assert!(reply.text_content().contains("echo 1 messages"));
}

#[tokio::test]
async fn chatui_outbound_enqueued_via_channel_manager() {
    let cm = Arc::new(ChannelManager::new());
    let chat = Arc::new(ChatUiChannel::create());
    cm.register(chat.clone() as Arc<dyn ChannelSink>).await;

    let addr = OutboundAddress::direct("chatui", "chatui:DIRECT:u_1");
    cm.deliver(&addr, vec![Msg::assistant("proactive")]).await;
    assert_eq!(chat.outbound_queue_size(), 1);

    let envelopes = chat.poll_outbound();
    assert_eq!(envelopes.len(), 1);
    assert_eq!(envelopes[0].address.channel_id, "chatui");
    assert_eq!(envelopes[0].messages.len(), 1);
    assert_eq!(chat.outbound_queue_size(), 0);
}

#[tokio::test]
async fn chatui_dispatch_via_channel_trait() {
    use agentscope_harness::agent::gateway::channel::Channel;
    use agentscope_harness::Peer;
    let gw = gateway_with_echo().await;
    let chat = ChatUiChannel::with_gateway(gw);
    let inbound = InboundMessage::builder("chatui", Peer::direct("u_5"), vec![Msg::user("hey")])
        .build();
    let reply = chat.dispatch(inbound).await.unwrap();
    assert!(reply.text_content().contains("echo 1 messages"));
}
