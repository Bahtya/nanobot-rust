//! Integration tests for Discord channel with mock HTTP server.

use nanobot_channels::base::BaseChannel;
use nanobot_channels::platforms::discord::DiscordChannel;
use nanobot_core::Platform;

/// Start a mock HTTP server that responds to a single request.
async fn start_mock_discord_server(
    response_body: &str,
    status_code: u16,
) -> (u16, tokio::task::JoinHandle<()>) {
    let body = response_body.to_string();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let (reader, mut writer) = stream.into_split();
        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            if buf_reader.read_line(&mut line).await.unwrap() == 0 {
                break;
            }
            if line == "\r\n" {
                break;
            }
        }

        let status_text = if status_code == 200 { "OK" } else { "Error" };
        let resp = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            status_code,
            status_text,
            body.len(),
            body
        );
        let _ = writer.write_all(resp.as_bytes()).await;
    });

    (port, handle)
}

#[tokio::test]
async fn test_discord_send_message_with_mock() {
    let mock_response = r#"{"id":"111222333","channel_id":"12345","content":"Hello!"}"#;
    let (port, _handle) = start_mock_discord_server(mock_response, 200).await;

    let channel = DiscordChannel::with_token_and_url(
        "test-bot-token".to_string(),
        format!("http://127.0.0.1:{}/", port),
    );

    let result = channel.send_message("12345", "Hello!", None).await.unwrap();
    assert!(result.success);
    assert_eq!(result.message_id.as_deref(), Some("111222333"));
    assert!(result.error.is_none());
}

#[tokio::test]
async fn test_discord_send_message_with_reply() {
    let mock_response = r#"{"id":"999888","channel_id":"12345","content":"Reply!"}"#;
    let (port, _handle) = start_mock_discord_server(mock_response, 200).await;

    let channel = DiscordChannel::with_token_and_url(
        "test-bot-token".to_string(),
        format!("http://127.0.0.1:{}/", port),
    );

    let result = channel
        .send_message("12345", "Reply!", Some("111222"))
        .await
        .unwrap();
    assert!(result.success);
    assert_eq!(result.message_id.as_deref(), Some("999888"));
}

#[tokio::test]
async fn test_discord_send_message_rate_limited() {
    let mock_response = r#"{"message":"You are being rate limited.","retry_after":5}"#;
    let (port, _handle) = start_mock_discord_server(mock_response, 429).await;

    let channel = DiscordChannel::with_token_and_url(
        "test-bot-token".to_string(),
        format!("http://127.0.0.1:{}/", port),
    );

    let result = channel.send_message("12345", "test", None).await.unwrap();
    assert!(!result.success);
    assert!(result.error.is_some());
    assert!(result.retryable);
}

#[tokio::test]
async fn test_discord_send_image_with_mock() {
    let mock_response = r#"{"id":"555666","channel_id":"12345","content":""}"#;
    let (port, _handle) = start_mock_discord_server(mock_response, 200).await;

    let channel = DiscordChannel::with_token_and_url(
        "test-bot-token".to_string(),
        format!("http://127.0.0.1:{}/", port),
    );

    let result = channel
        .send_image("12345", "https://example.com/img.png", Some("caption"))
        .await
        .unwrap();
    assert!(result.success);
    assert_eq!(result.message_id.as_deref(), Some("555666"));
}

#[tokio::test]
async fn test_discord_channel_name_and_platform() {
    let channel = DiscordChannel::with_token_and_url(
        "test-token".to_string(),
        "http://localhost:0/".to_string(),
    );
    assert_eq!(channel.name(), "discord");
    assert_eq!(channel.platform(), Platform::Discord);
    assert!(!channel.is_connected());
}
