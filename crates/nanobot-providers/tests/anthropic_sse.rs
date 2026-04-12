//! Integration tests for Anthropic Claude SSE streaming.
//!
//! Starts a minimal HTTP server that serves Anthropic-format SSE events
//! and verifies that the provider correctly parses them.

use futures::StreamExt;
use nanobot_providers::anthropic::{AnthropicConfig, AnthropicProvider};
use nanobot_providers::LlmProvider;

/// Helper: start a mock HTTP server on a random port.
async fn start_mock_server(
    response_body: &str,
    content_type: &str,
) -> (u16, tokio::task::JoinHandle<()>) {
    let body = response_body.to_string();
    let ct = content_type.to_string();
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
            "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            ct,
            body.len(),
            body
        );
        let _ = writer.write_all(resp.as_bytes()).await;
    });

    (port, handle)
}

/// Build a provider pointing at localhost (no system proxy).
fn make_provider(port: u16) -> AnthropicProvider {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .no_proxy()
        .build()
        .unwrap();
    let config = AnthropicConfig {
        api_key: "sk-test-key".to_string(),
        model: "claude-sonnet-4-20250514".to_string(),
        api_version: Some("2023-06-01".to_string()),
        base_url: Some(format!("http://127.0.0.1:{}", port)),
    };
    AnthropicProvider::with_client(config, client)
}

fn make_request(stream: bool) -> nanobot_providers::CompletionRequest {
    nanobot_providers::CompletionRequest {
        model: "claude-sonnet-4-20250514".to_string(),
        messages: vec![nanobot_core::Message {
            role: nanobot_core::MessageRole::User,
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

    let (port, _handle) = start_mock_server(response_body, "application/json").await;
    let provider = make_provider(port);

    let response = provider.complete(make_request(false)).await.unwrap();
    assert_eq!(response.content.as_deref(), Some("Hello from Claude!"));
    assert!(response.tool_calls.is_none());
    let usage = response.usage.unwrap();
    assert_eq!(usage.prompt_tokens, Some(10));
    assert_eq!(usage.completion_tokens, Some(5));
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

    let (port, _handle) = start_mock_server(sse_body, "text/event-stream").await;
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

    let (port, _handle) = start_mock_server(sse_body, "text/event-stream").await;
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
