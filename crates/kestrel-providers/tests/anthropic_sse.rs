//! Integration tests for Anthropic Claude provider.
//!
//! Tests non-streaming, SSE streaming, tool use, and retry with backoff
//! against a minimal mock HTTP server.

use futures::StreamExt;
use kestrel_providers::anthropic::{AnthropicConfig, AnthropicProvider};
use kestrel_providers::{LlmProvider, RetryConfig};

/// Helper: start a mock HTTP server on a random port.
async fn start_mock_server(
    response_body: &str,
    content_type: &str,
) -> Option<(u16, tokio::task::JoinHandle<()>)> {
    let body = response_body.to_string();
    let ct = content_type.to_string();
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => listener,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return None,
        Err(e) => panic!("failed to bind mock Anthropic server: {e}"),
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
            "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            ct,
            body.len(),
            body
        );
        let _ = writer.write_all(resp.as_bytes()).await;
    });

    Some((port, handle))
}

/// Helper: start a mock server that returns `error_count` error responses,
/// then a 200 with the given success body.
async fn start_mock_retry_server(
    error_status: u16,
    error_count: usize,
    success_body: &str,
) -> Option<(u16, tokio::task::JoinHandle<()>)> {
    let body = success_body.to_string();
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => listener,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return None,
        Err(e) => panic!("failed to bind mock Anthropic retry server: {e}"),
    };
    let port = listener.local_addr().unwrap().port();
    let err_count = error_count;

    let handle = tokio::spawn(async move {
        for i in 0..=err_count {
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

            if i < err_count {
                let resp = format!(
                    "HTTP/1.1 {} Error\r\nContent-Type: application/json\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    error_status
                );
                let _ = writer.write_all(resp.as_bytes()).await;
            } else {
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = writer.write_all(resp.as_bytes()).await;
            }
        }
    });

    Some((port, handle))
}

/// Build a no-proxy HTTP client for tests.
fn test_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .no_proxy()
        .build()
        .unwrap()
}

fn make_config(port: u16) -> AnthropicConfig {
    AnthropicConfig {
        api_key: "sk-test-key".to_string(),
        model: "claude-sonnet-4-20250514".to_string(),
        api_version: Some("2023-06-01".to_string()),
        base_url: Some(format!("http://127.0.0.1:{}", port)),
    }
}

fn make_provider(port: u16) -> AnthropicProvider {
    AnthropicProvider::with_client(make_config(port), test_client())
}

fn make_provider_with_retry(port: u16, retry: RetryConfig) -> AnthropicProvider {
    AnthropicProvider::with_client_and_retry(make_config(port), test_client(), retry)
}

fn make_request(stream: bool) -> kestrel_providers::CompletionRequest {
    kestrel_providers::CompletionRequest {
        model: "claude-sonnet-4-20250514".to_string(),
        messages: vec![kestrel_core::Message {
            role: kestrel_core::MessageRole::User,
            content: "Hi".to_string(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
        tools: None,
        max_tokens: Some(100),
        temperature: Some(0.7),
        stream,
    }
}

/// Test non-streaming Anthropic completion against a mock server.
#[tokio::test]
async fn test_anthropic_non_streaming_with_mock() {
    let response_body = r#"{"id":"msg_123","type":"message","role":"assistant","content":[{"type":"text","text":"Hello from Claude!"}],"model":"claude-sonnet-4-20250514","stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}}"#;

    let Some((port, _handle)) = start_mock_server(response_body, "application/json").await else {
        return;
    };
    let provider = make_provider(port);

    let response = provider.complete(make_request(false)).await.unwrap();
    assert_eq!(response.content.as_deref(), Some("Hello from Claude!"));
    assert!(response.tool_calls.is_none());
    let usage = response.usage.unwrap();
    assert_eq!(usage.prompt_tokens, Some(10));
    assert_eq!(usage.completion_tokens, Some(5));
}

/// Test non-streaming Anthropic completion with tool_use blocks.
#[tokio::test]
async fn test_anthropic_non_streaming_tool_calls() {
    let response_body = r#"{"id":"msg_456","type":"message","role":"assistant","content":[{"type":"text","text":"Let me check the weather."},{"type":"tool_use","id":"toolu_abc","name":"get_weather","input":{"city":"Berlin"}}],"model":"claude-sonnet-4-20250514","stop_reason":"tool_use","usage":{"input_tokens":20,"output_tokens":15}}"#;

    let Some((port, _handle)) = start_mock_server(response_body, "application/json").await else {
        return;
    };
    let provider = make_provider(port);

    let response = provider.complete(make_request(false)).await.unwrap();
    assert_eq!(
        response.content.as_deref(),
        Some("Let me check the weather.")
    );
    let tool_calls = response.tool_calls.unwrap();
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].id, "toolu_abc");
    assert_eq!(tool_calls[0].call_type, "function");
    assert_eq!(tool_calls[0].function.name, "get_weather");
    assert_eq!(response.finish_reason.as_deref(), Some("tool_use"));
}

/// Test Anthropic SSE streaming with content_block_delta events.
#[tokio::test]
async fn test_anthropic_sse_streaming_with_mock() {
    let sse_body = concat!(
        r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-20250514","usage":{"input_tokens":10,"output_tokens":0}}}"#,
        "\n\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "\n\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#,
        "\n\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo!"}}"#,
        "\n\n",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "\n\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#,
        "\n\n",
        r#"data: {"type":"message_stop"}"#,
        "\n\n",
    );

    let Some((port, _handle)) = start_mock_server(sse_body, "text/event-stream").await else {
        return;
    };
    let provider = make_provider(port);

    let mut stream = provider.complete_stream(make_request(true)).await.unwrap();

    let mut chunks = Vec::new();
    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.unwrap();
        chunks.push(chunk);
        if chunks.len() > 20 {
            break;
        }
    }

    let text_deltas: Vec<String> = chunks.iter().filter_map(|c| c.delta.clone()).collect();
    assert!(
        text_deltas.iter().any(|t| t == "Hel"),
        "Expected 'Hel' delta, got: {:?}",
        text_deltas
    );
    assert!(
        text_deltas.iter().any(|t| t == "lo!"),
        "Expected 'lo!' delta, got: {:?}",
        text_deltas
    );
}

/// Test Anthropic SSE streaming with tool use (content_block_start + partial_json deltas).
#[tokio::test]
async fn test_anthropic_sse_tool_calls() {
    let sse_body = concat!(
        r#"data: {"type":"message_start","message":{"id":"msg_2","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-20250514","usage":{"input_tokens":10,"output_tokens":0}}}"#,
        "\n\n",
        r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_123","name":"get_weather"}}"#,
        "\n\n",
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}"#,
        "\n\n",
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"Berlin\"}"}}"#,
        "\n\n",
        r#"data: {"type":"content_block_stop","index":1}"#,
        "\n\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":20}}"#,
        "\n\n",
        r#"data: {"type":"message_stop"}"#,
        "\n\n",
    );

    let Some((port, _handle)) = start_mock_server(sse_body, "text/event-stream").await else {
        return;
    };
    let provider = make_provider(port);

    let mut stream = provider.complete_stream(make_request(true)).await.unwrap();

    let mut all_deltas = Vec::new();
    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.unwrap();
        if let Some(tc_deltas) = &chunk.tool_call_deltas {
            all_deltas.extend(tc_deltas.clone());
        }
        if chunk.done {
            break;
        }
        if all_deltas.len() > 10 {
            break;
        }
    }

    assert!(!all_deltas.is_empty(), "Expected tool call deltas");
    assert_eq!(all_deltas[0].id.as_deref(), Some("toolu_123"));
    assert_eq!(all_deltas[0].function_name.as_deref(), Some("get_weather"));
    assert_eq!(
        all_deltas[0].function_arguments.as_deref(),
        Some("{\"city\":\"Berlin\"}")
    );
}

/// Test that the Anthropic provider retries on 429 and eventually succeeds.
#[tokio::test]
async fn test_anthropic_retry_on_429_then_success() {
    let success_body = r#"{"id":"msg_retry","type":"message","role":"assistant","content":[{"type":"text","text":"retry ok!"}],"model":"claude-sonnet-4-20250514","stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;

    // Server returns 429 once, then 200.
    let Some((port, _handle)) = start_mock_retry_server(429, 1, success_body).await else {
        return;
    };
    let retry = RetryConfig::default().with_max_retries(3);
    let provider = make_provider_with_retry(port, retry);

    let response = provider.complete(make_request(false)).await.unwrap();
    assert_eq!(response.content.as_deref(), Some("retry ok!"));
}

/// Test that the Anthropic provider retries on 503 and eventually succeeds.
#[tokio::test]
async fn test_anthropic_retry_on_503_then_success() {
    let success_body = r#"{"id":"msg_retry2","type":"message","role":"assistant","content":[{"type":"text","text":"server recovered"}],"model":"claude-sonnet-4-20250514","stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;

    let Some((port, _handle)) = start_mock_retry_server(503, 2, success_body).await else {
        return;
    };
    let retry = RetryConfig::default().with_max_retries(3);
    let provider = make_provider_with_retry(port, retry);

    let response = provider.complete(make_request(false)).await.unwrap();
    assert_eq!(response.content.as_deref(), Some("server recovered"));
}

/// Test that the Anthropic provider returns error when retries are exhausted.
#[tokio::test]
async fn test_anthropic_retry_exhausted() {
    // Server always returns 429.
    let Some((port, _handle)) = start_mock_retry_server(429, 5, "").await else {
        return;
    };
    let retry = RetryConfig::default().with_max_retries(2);
    let provider = make_provider_with_retry(port, retry);

    let result = provider.complete(make_request(false)).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("429"));
}
