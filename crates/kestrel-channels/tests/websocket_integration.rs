//! Integration test: full WebSocket connect → auth → message → streaming → disconnect cycle.
//!
//! Exercises the WebSocket channel end-to-end with a real TCP listener,
//! real WebSocket client, message bus, and streaming consumer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use kestrel_bus::events::StreamChunk;
use kestrel_bus::MessageBus;
use kestrel_channels::platforms::websocket::WsEnvelope;
use kestrel_channels::BaseChannel;
use kestrel_channels::WebSocketChannel;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Helper: bind a random port and return the address.
async fn random_addr() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    drop(listener);
    addr
}

/// Helper: drain the next text message from a WebSocket and parse it as JSON.
async fn drain_text(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> serde_json::Value {
    let msg = tokio::time::timeout(std::time::Duration::from_secs(3), ws.next())
        .await
        .expect("timeout waiting for ws message")
        .expect("stream ended")
        .expect("ws error");
    match msg {
        WsMessage::Text(text) => serde_json::from_str(&text).expect("invalid json"),
        other => panic!("Expected text message, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// Test 1: Full cycle without auth
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_full_cycle_no_auth() {
    let bus = Arc::new(MessageBus::new());
    let addr = random_addr().await;

    // Set up channel with bus handler.
    let mut channel = WebSocketChannel::with_addr(addr.clone());
    channel.set_message_handler(bus.inbound_sender());
    channel.connect().await.unwrap();

    // Start streaming consumer.
    let clients: Arc<dashmap::DashMap<String, tokio::sync::mpsc::UnboundedSender<String>>> =
        Arc::new(dashmap::DashMap::new());
    let streaming_running = Arc::new(AtomicBool::new(true));
    {
        let bus_c = bus.clone();
        let running_c = streaming_running.clone();
        tokio::spawn(async move {
            kestrel_channels::run_ws_stream_consumer(bus_c, clients, move || {
                running_c.load(Ordering::Relaxed)
            })
            .await;
        });
    }
    // Give streaming consumer time to subscribe.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Connect client.
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}", addr))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // 1. Receive welcome.
    let welcome = drain_text(&mut ws).await;
    assert_eq!(welcome["type"], "welcome");
    let client_id = welcome["client_id"].as_str().unwrap().to_string();

    // 2. Send a message.
    let env = WsEnvelope::message("hello from client");
    ws.send(WsMessage::Text(env.to_json().unwrap().into()))
        .await
        .unwrap();

    // Verify inbound message arrives on the bus.
    let mut inbound_rx = bus.consume_inbound().await.unwrap();
    let inbound = tokio::time::timeout(std::time::Duration::from_secs(2), inbound_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inbound.content, "hello from client");
    assert_eq!(inbound.chat_id, client_id);

    // 3. Send a response back via the channel.
    let result = channel
        .send_message(&client_id, "hello from server", None)
        .await
        .unwrap();
    assert!(result.success);

    let response = drain_text(&mut ws).await;
    assert_eq!(response["type"], "message");
    assert_eq!(response["content"], "hello from server");

    // 4. Send ping.
    let ping = r#"{"type":"ping"}"#;
    ws.send(WsMessage::Text(ping.into())).await.unwrap();
    let pong = drain_text(&mut ws).await;
    assert_eq!(pong["type"], "pong");

    // 5. Disconnect.
    channel.disconnect().await.unwrap();
    assert!(!channel.is_connected());

    streaming_running.store(false, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Test 2: Full cycle with auth
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_full_cycle_with_auth() {
    let bus = Arc::new(MessageBus::new());
    let addr = random_addr().await;

    let mut channel = WebSocketChannel::with_auth(addr.clone(), true, Some("my-token".to_string()));
    channel.set_message_handler(bus.inbound_sender());
    channel.connect().await.unwrap();

    // Connect without auth.
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}", addr))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // 1. Sending a message before auth should fail.
    let msg = WsEnvelope::message("premature");
    ws.send(WsMessage::Text(msg.to_json().unwrap().into()))
        .await
        .unwrap();

    let err = drain_text(&mut ws).await;
    assert_eq!(err["type"], "error");
    assert_eq!(err["code"], "auth_required");

    // 2. Connect another client and authenticate properly.
    let (mut ws2, _) = tokio_tungstenite::connect_async(format!("ws://{}", addr))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let auth_msg = r#"{"type":"auth","token":"my-token"}"#;
    ws2.send(WsMessage::Text(auth_msg.into())).await.unwrap();

    let welcome = drain_text(&mut ws2).await;
    assert_eq!(welcome["type"], "welcome");
    let client_id = welcome["client_id"].as_str().unwrap().to_string();

    // 3. Send message after auth.
    let env = WsEnvelope::message("authenticated hello");
    ws2.send(WsMessage::Text(env.to_json().unwrap().into()))
        .await
        .unwrap();

    let mut inbound_rx = bus.consume_inbound().await.unwrap();
    let inbound = tokio::time::timeout(std::time::Duration::from_secs(2), inbound_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inbound.content, "authenticated hello");
    assert_eq!(inbound.chat_id, client_id);

    // 4. Legacy format still works after auth.
    let legacy = r#"{"role":"user","content":"legacy after auth"}"#;
    ws2.send(WsMessage::Text(legacy.into())).await.unwrap();

    let inbound2 = tokio::time::timeout(std::time::Duration::from_secs(2), inbound_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inbound2.content, "legacy after auth");

    channel.disconnect().await.unwrap();
}

// ---------------------------------------------------------------------------
// Test 3: Streaming end-to-end
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_streaming_end_to_end() {
    let bus = Arc::new(MessageBus::new());

    // Set up the streaming consumer with a simulated client.
    let clients: Arc<dashmap::DashMap<String, tokio::sync::mpsc::UnboundedSender<String>>> =
        Arc::new(dashmap::DashMap::new());
    let (client_tx, mut client_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    clients.insert("ws-client-1".to_string(), client_tx);

    let running_flag = Arc::new(AtomicBool::new(true));
    {
        let bus_c = bus.clone();
        let clients_c = clients.clone();
        let running_c = running_flag.clone();
        tokio::spawn(async move {
            kestrel_channels::run_ws_stream_consumer(bus_c, clients_c, move || {
                running_c.load(Ordering::Relaxed)
            })
            .await;
        });
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Publish streaming chunks.
    bus.publish_stream_chunk(StreamChunk {
        session_key: "websocket:ws-client-1".to_string(),
        content: "Hello ".to_string(),
        done: false,
    });
    bus.publish_stream_chunk(StreamChunk {
        session_key: "websocket:ws-client-1".to_string(),
        content: "world!".to_string(),
        done: false,
    });
    bus.publish_stream_chunk(StreamChunk {
        session_key: "websocket:ws-client-1".to_string(),
        content: String::new(),
        done: true,
    });

    // Receive and verify chunks.
    let json1 = tokio::time::timeout(std::time::Duration::from_secs(2), client_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let chunk1: serde_json::Value = serde_json::from_str(&json1).unwrap();
    assert_eq!(chunk1["type"], "streaming");
    assert_eq!(chunk1["chunk"], "Hello ");
    assert_eq!(chunk1["done"], false);

    let json2 = tokio::time::timeout(std::time::Duration::from_secs(2), client_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let chunk2: serde_json::Value = serde_json::from_str(&json2).unwrap();
    assert_eq!(chunk2["type"], "streaming");
    assert_eq!(chunk2["chunk"], "world!");
    assert_eq!(chunk2["done"], false);

    let json3 = tokio::time::timeout(std::time::Duration::from_secs(2), client_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let chunk3: serde_json::Value = serde_json::from_str(&json3).unwrap();
    assert_eq!(chunk3["type"], "streaming");
    assert_eq!(chunk3["chunk"], "");
    assert_eq!(chunk3["done"], true);

    running_flag.store(false, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Test 4: Multiple clients with individual sessions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_multiple_clients_individual_sessions() {
    let bus = Arc::new(MessageBus::new());
    let addr = random_addr().await;

    let mut channel = WebSocketChannel::with_addr(addr.clone());
    channel.set_message_handler(bus.inbound_sender());
    channel.connect().await.unwrap();

    // Connect two clients.
    let (mut ws1, _) = tokio_tungstenite::connect_async(format!("ws://{}", addr))
        .await
        .unwrap();
    let (mut ws2, _) = tokio_tungstenite::connect_async(format!("ws://{}", addr))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Drain welcomes.
    let w1 = drain_text(&mut ws1).await;
    let w2 = drain_text(&mut ws2).await;
    let id1 = w1["client_id"].as_str().unwrap().to_string();
    let id2 = w2["client_id"].as_str().unwrap().to_string();
    assert_ne!(id1, id2);

    // Send from both clients.
    let env1 = WsEnvelope::message("from client 1");
    ws1.send(WsMessage::Text(env1.to_json().unwrap().into()))
        .await
        .unwrap();

    let env2 = WsEnvelope::message("from client 2");
    ws2.send(WsMessage::Text(env2.to_json().unwrap().into()))
        .await
        .unwrap();

    let mut inbound_rx = bus.consume_inbound().await.unwrap();

    let msg1 = tokio::time::timeout(std::time::Duration::from_secs(2), inbound_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let msg2 = tokio::time::timeout(std::time::Duration::from_secs(2), inbound_rx.recv())
        .await
        .unwrap()
        .unwrap();

    // Each client should have its own chat_id.
    let ids = vec![msg1.chat_id.clone(), msg2.chat_id.clone()];
    assert!(ids.contains(&id1));
    assert!(ids.contains(&id2));

    // Send targeted responses.
    channel
        .send_message(&id1, "reply to 1", None)
        .await
        .unwrap();
    channel
        .send_message(&id2, "reply to 2", None)
        .await
        .unwrap();

    let r1 = drain_text(&mut ws1).await;
    let r2 = drain_text(&mut ws2).await;
    assert_eq!(r1["content"], "reply to 1");
    assert_eq!(r2["content"], "reply to 2");

    channel.disconnect().await.unwrap();
}
