//! Integration tests for Discord channel with mock HTTP server.

use nanobot_channels::base::BaseChannel;
use nanobot_channels::platforms::discord::DiscordChannel;
use nanobot_core::Platform;

/// HTTP method for the mock server to handle.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
enum MockMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

/// Start a mock HTTP server that responds to a single request with the given method.
async fn start_mock_discord_server(
    response_body: &str,
    status_code: u16,
    expected_method: MockMethod,
) -> Option<(u16, tokio::task::JoinHandle<()>)> {
    let body = response_body.to_string();
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => listener,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return None,
        Err(e) => panic!("failed to bind mock Discord server: {e}"),
    };
    let port = listener.local_addr().unwrap().port();

    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let (reader, mut writer) = stream.into_split();
        let mut buf_reader = BufReader::new(reader);
        let mut request_line = String::new();
        let _ = buf_reader.read_line(&mut request_line).await.unwrap();

        // Read remaining headers
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

        // Verify method matches (best-effort, don't fail the test here)
        let method_matches = match expected_method {
            MockMethod::Get => request_line.starts_with("GET"),
            MockMethod::Post => request_line.starts_with("POST"),
            MockMethod::Put => request_line.starts_with("PUT"),
            MockMethod::Patch => request_line.starts_with("PATCH"),
            MockMethod::Delete => request_line.starts_with("DELETE"),
        };

        let status_text = if status_code == 200 {
            "OK"
        } else if status_code == 204 {
            "No Content"
        } else {
            "Error"
        };

        let response = if !method_matches && status_code < 400 {
            // Method mismatch — return error to catch it in test assertions
            "HTTP/1.1 405 Method Not Allowed\r\nContent-Type: application/json\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
        } else {
            format!(
                "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                status_code,
                status_text,
                body.len(),
                body
            )
        };
        let _ = writer.write_all(response.as_bytes()).await;
    });

    Some((port, handle))
}

#[tokio::test]
async fn test_discord_send_message_with_mock() {
    let mock_response = r#"{"id":"111222333","channel_id":"12345","content":"Hello!"}"#;
    let Some((port, _handle)) =
        start_mock_discord_server(mock_response, 200, MockMethod::Post).await
    else {
        return;
    };

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
    let Some((port, _handle)) =
        start_mock_discord_server(mock_response, 200, MockMethod::Post).await
    else {
        return;
    };

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
    let Some((port, _handle)) =
        start_mock_discord_server(mock_response, 429, MockMethod::Post).await
    else {
        return;
    };

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
    let Some((port, _handle)) =
        start_mock_discord_server(mock_response, 200, MockMethod::Post).await
    else {
        return;
    };

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

#[tokio::test]
async fn test_discord_edit_message_with_mock() {
    let mock_response = r#"{"id":"111222333","channel_id":"12345","content":"edited!"}"#;
    let Some((port, _handle)) =
        start_mock_discord_server(mock_response, 200, MockMethod::Patch).await
    else {
        return;
    };

    let channel = DiscordChannel::with_token_and_url(
        "test-bot-token".to_string(),
        format!("http://127.0.0.1:{}/", port),
    );

    let result = channel
        .edit_message("12345", "111222333", "edited!")
        .await
        .unwrap();
    assert!(result.success);
    assert_eq!(result.message_id.as_deref(), Some("111222333"));
    assert!(result.error.is_none());
}

#[tokio::test]
async fn test_discord_edit_message_not_found() {
    let mock_response = r#"{"message":"Unknown Message","code":10008}"#;
    let Some((port, _handle)) =
        start_mock_discord_server(mock_response, 404, MockMethod::Patch).await
    else {
        return;
    };

    let channel = DiscordChannel::with_token_and_url(
        "test-bot-token".to_string(),
        format!("http://127.0.0.1:{}/", port),
    );

    let result = channel
        .edit_message("12345", "nonexistent", "new text")
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.error.is_some());
    assert!(!result.retryable);
}

#[tokio::test]
async fn test_discord_edit_message_forbidden() {
    let mock_response =
        r#"{"message":"Cannot edit a message authored by another user.","code":50005}"#;
    let Some((port, _handle)) =
        start_mock_discord_server(mock_response, 403, MockMethod::Patch).await
    else {
        return;
    };

    let channel = DiscordChannel::with_token_and_url(
        "test-bot-token".to_string(),
        format!("http://127.0.0.1:{}/", port),
    );

    let result = channel
        .edit_message("12345", "other_user_msg", "new text")
        .await
        .unwrap();
    assert!(!result.success);
    assert!(!result.retryable);
}

#[tokio::test]
async fn test_discord_delete_message_with_mock() {
    // DELETE returns 204 No Content with empty body
    let Some((port, _handle)) = start_mock_discord_server("", 204, MockMethod::Delete).await else {
        return;
    };

    let channel = DiscordChannel::with_token_and_url(
        "test-bot-token".to_string(),
        format!("http://127.0.0.1:{}/", port),
    );

    let result = channel.delete_message("12345", "111222333").await.unwrap();
    assert!(result.success);
    assert_eq!(result.message_id.as_deref(), Some("111222333"));
    assert!(result.error.is_none());
}

#[tokio::test]
async fn test_discord_delete_message_not_found() {
    let mock_response = r#"{"message":"Unknown Message","code":10008}"#;
    let Some((port, _handle)) =
        start_mock_discord_server(mock_response, 404, MockMethod::Delete).await
    else {
        return;
    };

    let channel = DiscordChannel::with_token_and_url(
        "test-bot-token".to_string(),
        format!("http://127.0.0.1:{}/", port),
    );

    let result = channel
        .delete_message("12345", "nonexistent")
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.error.is_some());
}

#[tokio::test]
async fn test_discord_delete_message_forbidden() {
    let mock_response = r#"{"message":"Missing Access","code":50001}"#;
    let Some((port, _handle)) =
        start_mock_discord_server(mock_response, 403, MockMethod::Delete).await
    else {
        return;
    };

    let channel = DiscordChannel::with_token_and_url(
        "test-bot-token".to_string(),
        format!("http://127.0.0.1:{}/", port),
    );

    let result = channel.delete_message("12345", "other_msg").await.unwrap();
    assert!(!result.success);
    assert!(!result.retryable);
}

#[tokio::test]
async fn test_discord_send_reaction_with_mock() {
    // PUT /channels/{id}/messages/{id}/reactions/{emoji}/@me → 204 No Content
    let Some((port, _handle)) = start_mock_discord_server("", 204, MockMethod::Put).await else {
        return;
    };

    let channel = DiscordChannel::with_token_and_url(
        "test-bot-token".to_string(),
        format!("http://127.0.0.1:{}/", port),
    );

    let result = channel.send_reaction("12345", "111222333", "✅").await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_discord_send_typing_with_mock() {
    // POST /channels/{id}/typing → 204 No Content
    let Some((port, _handle)) = start_mock_discord_server("", 204, MockMethod::Post).await else {
        return;
    };

    let channel = DiscordChannel::with_token_and_url(
        "test-bot-token".to_string(),
        format!("http://127.0.0.1:{}/", port),
    );

    let result = channel.send_typing("12345").await;
    assert!(result.is_ok());
}
