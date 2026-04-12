//! Integration tests for OpenAI-compatible SSE streaming.
//!
//! Starts a minimal HTTP server that serves SSE chunks and verifies
//! that the provider correctly parses them.

use futures::StreamExt;
use nanobot_providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use nanobot_providers::LlmProvider;

/// Helper: start a mock HTTP server on a random port, return (port, JoinHandle).
/// The server reads the request, then writes `response_body` as the HTTP response.
async fn start_mock_http_server(
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
        // Read request headers
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

/// Build a provider that connects to localhost (no system proxy).
fn make_provider(port: u16) -> OpenAiCompatProvider {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .no_proxy()
        .build()
        .unwrap();
    let config = OpenAiCompatConfig {
        api_key: "test-key".to_string(),
        base_url: format!("http://127.0.0.1:{}", port),
        model: "gpt-4".to_string(),
        organization: None,
        no_proxy: false,
    };
    OpenAiCompatProvider::with_client(config, client)
}

fn make_request(stream: bool) -> nanobot_providers::CompletionRequest {
    nanobot_providers::CompletionRequest {
        model: "gpt-4".to_string(),
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

/// Test that a non-streaming completion parses correctly against a mock server.
#[tokio::test]
async fn test_openai_non_streaming_with_mock() {
    let response_body = r#"{"choices":[{"message":{"content":"Hello!","tool_calls":null},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#;

    let (port, _handle) = start_mock_http_server(response_body, "application/json").await;
    let provider = make_provider(port);

    let response = provider.complete(make_request(false)).await.unwrap();
    assert_eq!(response.content.as_deref(), Some("Hello!"));
    assert!(response.tool_calls.is_none());
    let usage = response.usage.unwrap();
    assert_eq!(usage.prompt_tokens, Some(5));
    assert_eq!(usage.completion_tokens, Some(2));
    assert_eq!(usage.total_tokens, Some(7));
}

/// Test SSE streaming with a mock server that sends OpenAI-format chunks.
#[tokio::test]
async fn test_openai_sse_streaming_with_mock() {
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"},\"index\":0}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"lo!\"},\"index\":0}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"\"},\"index\":0,\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\n",
        "data: [DONE]\n\n",
    );

    let (port, _handle) = start_mock_http_server(sse_body, "text/event-stream").await;
    let provider = make_provider(port);

    let mut stream = provider.complete_stream(make_request(true)).await.unwrap();

    let mut chunks = Vec::new();
    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.unwrap();
        chunks.push(chunk);
        if chunks.len() > 10 {
            break;
        } // safety limit
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

/// Test that streaming tool calls are correctly accumulated.
#[tokio::test]
async fn test_openai_sse_tool_calls() {
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_123\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]},\"index\":0}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\"}}]},\"index\":0}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"Berlin\\\"}\"}}]},\"index\":0,\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    );

    let (port, _handle) = start_mock_http_server(sse_body, "text/event-stream").await;
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
    assert_eq!(all_deltas[0].id.as_deref(), Some("call_123"));
    assert_eq!(all_deltas[0].function_name.as_deref(), Some("get_weather"));
    assert_eq!(
        all_deltas[0].function_arguments.as_deref(),
        Some("{\"city\":\"Berlin\"}")
    );
}
