//! Integration tests for Telegram channel with mock HTTP server.

use kestrel_channels::base::BaseChannel;
use kestrel_channels::platforms::telegram::TelegramChannel;
use kestrel_core::Platform;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Start a mock HTTP server that responds to a single request.
async fn start_mock_telegram_server(
    response_body: &str,
) -> Option<(u16, tokio::task::JoinHandle<()>)> {
    let body = response_body.to_string();
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => listener,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return None,
        Err(e) => panic!("failed to bind mock Telegram server: {e}"),
    };
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

    Some((port, handle))
}

async fn start_mock_telegram_server_with_responses(
    responses: Vec<String>,
    captured_requests: Arc<Mutex<Vec<String>>>,
) -> Option<(u16, tokio::task::JoinHandle<()>)> {
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => listener,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return None,
        Err(e) => panic!("failed to bind mock Telegram server: {e}"),
    };
    let port = listener.local_addr().unwrap().port();

    let handle = tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

        for body in responses {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut buf_reader = BufReader::new(reader);

            let mut line = String::new();
            let mut content_length = 0usize;
            let mut request = String::new();
            loop {
                line.clear();
                if buf_reader.read_line(&mut line).await.unwrap() == 0 {
                    break;
                }
                request.push_str(&line);
                let lower = line.to_ascii_lowercase();
                if let Some((_, value)) = lower.split_once("content-length:") {
                    content_length = value.trim().parse().unwrap_or(0);
                }
                if line == "\r\n" {
                    break;
                }
            }

            if content_length > 0 {
                let mut body_buf = vec![0u8; content_length];
                buf_reader.read_exact(&mut body_buf).await.unwrap();
                request.push_str(std::str::from_utf8(&body_buf).unwrap());
            }

            captured_requests.lock().await.push(request);

            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = writer.write_all(resp.as_bytes()).await;
        }
    });

    Some((port, handle))
}

#[tokio::test]
async fn test_telegram_send_message_with_mock() {
    let mock_response = r#"{"ok":true,"result":{"message_id":42,"chat":{"id":123,"type":"private"},"text":"Hello!"}}"#;
    let Some((port, _handle)) = start_mock_telegram_server(mock_response).await else {
        return;
    };

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
    let Some((port, _handle)) = start_mock_telegram_server(mock_response).await else {
        return;
    };

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
    let Some((port, _handle)) = start_mock_telegram_server(mock_response).await else {
        return;
    };

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
    let Some((port, _handle)) = start_mock_telegram_server(mock_response).await else {
        return;
    };

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
    let Some((port, _handle)) = start_mock_telegram_server(mock_response).await else {
        return;
    };

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
    let Some((port, _handle)) = start_mock_telegram_server(mock_response).await else {
        return;
    };

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

#[tokio::test]
async fn test_telegram_send_reaction_with_mock() {
    let mock_response = r#"{"ok":true}"#;
    let Some((port, _handle)) = start_mock_telegram_server(mock_response).await else {
        return;
    };

    let channel = TelegramChannel::with_token_and_url(
        "test-token".to_string(),
        format!("http://127.0.0.1:{}", port),
    );

    let result = channel.send_reaction("123", "42", "👀").await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_telegram_send_typing_with_mock() {
    let mock_response = r#"{"ok":true}"#;
    let Some((port, _handle)) = start_mock_telegram_server(mock_response).await else {
        return;
    };

    let channel = TelegramChannel::with_token_and_url(
        "test-token".to_string(),
        format!("http://127.0.0.1:{}", port),
    );

    let result = channel.send_typing("123").await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_telegram_connect_sends_online_notification_once() {
    let captured_requests = Arc::new(Mutex::new(Vec::new()));
    let responses = vec![
        r#"{"ok":true,"result":{"id":1,"is_bot":true,"first_name":"Kestrel","username":"kestrel_bot"}}"#.to_string(),
        r#"{"ok":true,"result":{"message_id":42,"chat":{"id":123,"type":"private"},"text":"online"}}"#.to_string(),
    ];
    let Some((port, _handle)) =
        start_mock_telegram_server_with_responses(responses, captured_requests.clone()).await
    else {
        return;
    };

    let config = kestrel_config::schema::TelegramConfig {
        token: "test-token".to_string(),
        enabled: true,
        allowed_users: vec![],
        admin_users: vec![],
        streaming: false,
        proxy: None,
    };
    let notifications = kestrel_config::schema::NotificationsConfig {
        online_notify: true,
        notify_chat_id: Some("123".to_string()),
        online_message: "Kestrel v{version} online - {channel} connected".to_string(),
    };
    let mut channel = TelegramChannel::with_config_and_url(
        &config,
        &notifications,
        format!("http://127.0.0.1:{port}"),
    );

    let connected = channel.connect().await.unwrap();
    assert!(connected);

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let requests = captured_requests.lock().await;
    assert_eq!(requests.len(), 2);
    assert!(requests[0].contains("GET /getMe HTTP/1.1"));
    assert!(requests[1].contains("POST /sendMessage HTTP/1.1"));
    assert!(requests[1].contains("\"chat_id\":123"));
    assert!(requests[1].contains(&format!(
        "\"text\":\"Kestrel v{} online - Telegram connected\"",
        env!("CARGO_PKG_VERSION")
    )));
}
