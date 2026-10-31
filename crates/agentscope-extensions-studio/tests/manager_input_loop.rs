//! STUDIO-01 regression: the manager's **real assembly entry point** must hand the Socket.IO
//! transport's own `PendingInputRegistry` to the `StudioWebSocketClient` it installs.
//!
//! Before the fix `StudioManager::initialize_with_transports` built the client with
//! `StudioWebSocketClient::with_transport`, which allocates a *fresh* registry, while
//! `SocketIoTransport` resolved `forwardUserInput` into the registry it was constructed with.
//! The two tables never met, so `wait_for_input` on `StudioManager::web_socket_client()` waited
//! out its timeout with the answer sitting in the other table.
//!
//! Everything is offline: a local `tokio-tungstenite` server plays Studio (Engine.IO open packet,
//! namespace ACK, then one `42["forwardUserInput", ...]` event), and the REST seam is a stub that
//! answers `200 OK` to `registerRun`.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use agentscope_core::message::ContentBlock;
use agentscope_extensions_studio::studio::socket_io::SocketIoTransport;
use agentscope_extensions_studio::{
    PendingInputRegistry, StudioConfig, StudioHttpRequest, StudioHttpResponse, StudioHttpTransport,
    StudioManager, StudioResult,
};
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

/// Answers every Studio REST call with `200 OK` — `registerRun` must succeed without network.
struct AlwaysOkHttp;

#[async_trait]
impl StudioHttpTransport for AlwaysOkHttp {
    async fn execute(&self, _request: StudioHttpRequest) -> StudioResult<StudioHttpResponse> {
        Ok(StudioHttpResponse::new(200, "{}"))
    }
}

/// Plays the Studio server side of the Socket.IO handshake for one connection.
async fn serve_studio_socket(listener: TcpListener, push_input: tokio::sync::oneshot::Receiver<()>) {
    let (stream, _) = listener.accept().await.expect("accept the socket.io client");
    let mut ws = tokio_tungstenite::accept_async(stream)
        .await
        .expect("websocket handshake");

    // Engine.IO v4 open packet.
    ws.send(Message::Text(
        "0{\"sid\":\"s-1\",\"pingInterval\":25000,\"pingTimeout\":20000}"
            .to_string()
            .into(),
    ))
    .await
    .expect("send the engine.io open packet");

    // Wait for the namespace join (`40/python,{...}`) before acknowledging it.
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(text))) => {
                if text.to_string().starts_with("40") {
                    break;
                }
            }
            None => panic!("client disconnected before joining the namespace"),
            Some(Err(err)) => panic!("client read error: {err}"),
            Some(Ok(_)) => {}
        }
    }
    ws.send(Message::Text(
        "40/python,{\"sid\":\"s-1\"}".to_string().into(),
    ))
    .await
    .expect("acknowledge the namespace join");

    // Push the user's reply only once the host is actually waiting for it.
    push_input.await.expect("the test triggers the event");
    ws.send(Message::Text(
        "42[\"forwardUserInput\",[\"req-1\",[{\"type\":\"text\",\"text\":\"typed\"}],{}]]"
            .to_string()
            .into(),
    ))
    .await
    .expect("send the forwardUserInput event");
}

#[tokio::test]
async fn the_manager_assembly_delivers_user_input_to_wait_for_input() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a local studio double");
    let port = listener.local_addr().expect("local address").port();
    let (push_tx, push_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(serve_studio_socket(listener, push_rx));

    let config = StudioConfig::builder()
        .studio_url(format!("http://127.0.0.1:{port}"))
        .run_id("run-input-loop")
        .reconnect_attempts(0)
        .build();
    let transport = Arc::new(
        SocketIoTransport::new(&config, Arc::new(PendingInputRegistry::new()))
            .expect("build the socket.io transport"),
    );

    // The real entry point: register the run, connect the socket, install the clients.
    StudioManager::initialize_with_transports(config, Arc::new(AlwaysOkHttp), transport)
        .await
        .expect("the manager initializes");
    let client = StudioManager::web_socket_client().expect("the installed websocket client");

    // Register the pending request before the event is pushed: `wait_for_input` registers on
    // its first poll, so poll it once and then release the server.
    let mut wait = Box::pin(client.wait_for_input("req-1", Duration::from_secs(5)));
    let first = std::future::poll_fn(|cx| std::task::Poll::Ready(wait.as_mut().poll(cx))).await;
    assert!(
        first.is_pending(),
        "wait_for_input must wait for the socket event"
    );
    push_tx.send(()).expect("release the server");

    let data = wait
        .await
        .expect("the socket event must resolve the pending request");
    assert_eq!(data.blocks_input.len(), 1);
    match &data.blocks_input[0] {
        ContentBlock::Text(text) => assert_eq!(text.text, "typed"),
        other => panic!("unexpected content block: {other:?}"),
    }
    // `{}` decodes to an empty structured-input map, exactly like Studio's form payload.
    assert_eq!(data.structured_input.map(|map| map.len()), Some(0));

    StudioManager::shutdown();
    server.abort();
}
