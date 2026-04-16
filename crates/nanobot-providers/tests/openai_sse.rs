//! Integration tests for OpenAI-compatible provider.
//!
//! Tests non-streaming, SSE streaming, tool calls, and retry with backoff
//! against a minimal mock HTTP server.

use futures::StreamExt;
use nanobot_providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use nanobot_providers::{LlmProvider, RetryConfig};

/// Helper: start a mock HTTP server on a random port, return (port, JoinHandle).
/// The server reads the request, then writes `response_body` as the HTTP response.
async fn start_mock_http_server(
    response_body: &str,
    content_type: &str,
) -> Option<(u16, tokio::task::JoinHandle<()>)> {
    let body = response_body.to_string();
    let ct = content_type.to_string();
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => listener,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return None,
        Err(e) => panic!("failed to bind mock OpenAI server: {e}"),
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

/// Helper: start a mock server that returns `error_count` 429 responses,
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
        Err(e) => panic!("failed to bind mock OpenAI retry server: {e}"),
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
                    "HTTP/1.1 {} Rate Limited\r\nContent-Type: application/json\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
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

fn make_config(port: u16) -> OpenAiCompatConfig {
    OpenAiCompatConfig {
        api_key: "test-key".to_string(),
        base_url: format!("http://127.0.0.1:{}", port),
        model: "gpt-4".to_string(),
        organization: None,
        no_proxy: false,
    }
}

fn make_provider(port: u16) -> OpenAiCompatProvider {
    OpenAiCompatProvider::with_client(make_config(port), test_client())
}

fn make_provider_with_retry(port: u16, retry: RetryConfig) -> OpenAiCompatProvider {
    OpenAiCompatProvider::with_client_and_retry(make_config(port), test_client(), retry)
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

    let Some((port, _handle)) = start_mock_http_server(response_body, "application/json").await
    else {
        return;
    };
    let provider = make_provider(port);

    let response = provider.complete(make_request(false)).await.unwrap();
    assert_eq!(response.content.as_deref(), Some("Hello!"));
    assert!(response.tool_calls.is_none());
    let usage = response.usage.unwrap();
    assert_eq!(usage.prompt_tokens, Some(5));
    assert_eq!(usage.completion_tokens, Some(2));
    assert_eq!(usage.total_tokens, Some(7));
}

/// Test non-streaming completion with tool calls in the response.
#[tokio::test]
async fn test_openai_non_streaming_tool_calls() {
    let response_body = r#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"call_abc","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Berlin\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":15,"total_tokens":25}}"#;

    let Some((port, _handle)) = start_mock_http_server(response_body, "application/json").await
    else {
        return;
    };
    let provider = make_provider(port);

    let response = provider.complete(make_request(false)).await.unwrap();
    assert!(response.content.is_none());
    let tool_calls = response.tool_calls.unwrap();
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].id, "call_abc");
    assert_eq!(tool_calls[0].call_type, "function");
    assert_eq!(tool_calls[0].function.name, "get_weather");
    assert_eq!(tool_calls[0].function.arguments, r#"{"city":"Berlin"}"#);
    assert_eq!(response.finish_reason.as_deref(), Some("tool_calls"));
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

    let Some((port, _handle)) = start_mock_http_server(sse_body, "text/event-stream").await else {
        return;
    };
    let provider = make_provider(port);

    let mut stream = provider.complete_stream(make_request(true)).await.unwrap();

    let mut chunks = Vec::new();
    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.unwrap();
        chunks.push(chunk);
        if chunks.len() > 10 {
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

/// Test that streaming tool calls are correctly accumulated.
#[tokio::test]
async fn test_openai_sse_tool_calls() {
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_123\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]},\"index\":0}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\"}}]},\"index\":0}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"Berlin\\\"}\"}}]},\"index\":0,\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    );

    let Some((port, _handle)) = start_mock_http_server(sse_body, "text/event-stream").await else {
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
    assert_eq!(all_deltas[0].id.as_deref(), Some("call_123"));
    assert_eq!(all_deltas[0].function_name.as_deref(), Some("get_weather"));
    assert_eq!(
        all_deltas[0].function_arguments.as_deref(),
        Some("{\"city\":\"Berlin\"}")
    );
}

/// Test that the provider retries on 429 and eventually succeeds.
#[tokio::test]
async fn test_openai_retry_on_429_then_success() {
    let success_body = r#"{"choices":[{"message":{"content":"retry ok!","tool_calls":null},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;

    // Server returns 429 once, then 200.
    let Some((port, _handle)) = start_mock_retry_server(429, 1, success_body).await else {
        return;
    };
    let retry = RetryConfig::default().with_max_retries(3);
    let provider = make_provider_with_retry(port, retry);

    let response = provider.complete(make_request(false)).await.unwrap();
    assert_eq!(response.content.as_deref(), Some("retry ok!"));
}

/// Test that the provider retries on 503 and eventually succeeds.
#[tokio::test]
async fn test_openai_retry_on_503_then_success() {
    let success_body = r#"{"choices":[{"message":{"content":"server recovered","tool_calls":null},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;

    let Some((port, _handle)) = start_mock_retry_server(503, 2, success_body).await else {
        return;
    };
    let retry = RetryConfig::default().with_max_retries(3);
    let provider = make_provider_with_retry(port, retry);

    let response = provider.complete(make_request(false)).await.unwrap();
    assert_eq!(response.content.as_deref(), Some("server recovered"));
}

/// Test that the provider returns error when retries are exhausted.
#[tokio::test]
async fn test_openai_retry_exhausted() {
    // Server always returns 429 (more times than we'll retry).
    let Some((port, _handle)) = start_mock_retry_server(429, 5, "").await else {
        return;
    };
    let retry = RetryConfig::default().with_max_retries(2);
    let provider = make_provider_with_retry(port, retry);

    let result = provider.complete(make_request(false)).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("429"));
}
