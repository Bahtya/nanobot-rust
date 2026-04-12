//! Integration tests for Telegram channel with mock HTTP server.

use nanobot_channels::base::BaseChannel;
use nanobot_channels::platforms::telegram::TelegramChannel;
use nanobot_core::Platform;

/// Start a mock HTTP server that responds to a single request.
async fn start_mock_telegram_server(response_body: &str) -> (u16, tokio::task::JoinHandle<()>) {
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

        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = writer.write_all(resp.as_bytes()).await;
    });

    (port, handle)
}

#[tokio::test]
async fn test_telegram_send_message_with_mock() {
    let mock_response = r#"{"ok":true,"result":{"message_id":42,"chat":{"id":123,"type":"private"},"text":"Hello!"}}"#;
    let (port, _handle) = start_mock_telegram_server(mock_response).await;

    let channel = TelegramChannel::with_token_and_url(
        "test-token".to_string(),
        format!("http://127.0.0.1:{}", port),
    );

    let result = channel.send_message("123", "Hello!", None).await.unwrap();
    assert!(result.success);
    assert_eq!(result.message_id.as_deref(), Some("42"));
    assert!(result.error.is_none());
}

#[tokio::test]
async fn test_telegram_send_message_with_reply() {
    let mock_response =
        r#"{"ok":true,"result":{"message_id":99,"chat":{"id":456},"text":"Reply!"}}"#;
    let (port, _handle) = start_mock_telegram_server(mock_response).await;

    let channel = TelegramChannel::with_token_and_url(
        "test-token".to_string(),
        format!("http://127.0.0.1:{}", port),
    );

    let result = channel
        .send_message("456", "Reply!", Some("50"))
        .await
        .unwrap();
    assert!(result.success);
    assert_eq!(result.message_id.as_deref(), Some("99"));
}

#[tokio::test]
async fn test_telegram_send_message_api_error() {
    let mock_response = r#"{"ok":false,"description":"Bad Request: chat not found"}"#;
    let (port, _handle) = start_mock_telegram_server(mock_response).await;

    let channel = TelegramChannel::with_token_and_url(
        "test-token".to_string(),
        format!("http://127.0.0.1:{}", port),
    );

    let result = channel.send_message("999", "test", None).await.unwrap();
    assert!(!result.success);
    assert!(result.error.is_some());
    assert!(result.error.unwrap().contains("chat not found"));
}

#[tokio::test]
async fn test_telegram_send_image_with_mock() {
    let mock_response =
        r#"{"ok":true,"result":{"message_id":100,"chat":{"id":123},"caption":"test img"}}"#;
    let (port, _handle) = start_mock_telegram_server(mock_response).await;

    let channel = TelegramChannel::with_token_and_url(
        "test-token".to_string(),
        format!("http://127.0.0.1:{}", port),
    );

    let result = channel
        .send_image("123", "https://example.com/img.png", Some("test img"))
        .await
        .unwrap();
    assert!(result.success);
    assert_eq!(result.message_id.as_deref(), Some("100"));
}

#[tokio::test]
async fn test_telegram_channel_name_and_platform() {
    let channel = TelegramChannel::with_token_and_url(
        "test-token".to_string(),
        "http://localhost:0".to_string(),
    );
    assert_eq!(channel.name(), "telegram");
    assert_eq!(channel.platform(), Platform::Telegram);
    assert!(!channel.is_connected());
}

#[tokio::test]
async fn test_telegram_edit_message_text_with_mock() {
    let mock_response =
        r#"{"ok":true,"result":{"message_id":42,"chat":{"id":123},"text":"edited!"}}"#;
    let (port, _handle) = start_mock_telegram_server(mock_response).await;

    let channel = TelegramChannel::with_token_and_url(
        "test-token".to_string(),
        format!("http://127.0.0.1:{}", port),
    );

    let result = channel
        .edit_message_text("123", "42", "edited!", None)
        .await
        .unwrap();
    assert!(result.success);
    assert_eq!(result.message_id.as_deref(), Some("42"));
}

#[tokio::test]
async fn test_telegram_edit_message_reply_markup_with_mock() {
    let mock_response =
        r#"{"ok":true,"result":{"message_id":42,"chat":{"id":123},"text":"old text"}}"#;
    let (port, _handle) = start_mock_telegram_server(mock_response).await;

    let channel = TelegramChannel::with_token_and_url(
        "test-token".to_string(),
        format!("http://127.0.0.1:{}", port),
    );

    let result = channel
        .edit_message_reply_markup("123", "42", None)
        .await
        .unwrap();
    assert!(result.success);
}
