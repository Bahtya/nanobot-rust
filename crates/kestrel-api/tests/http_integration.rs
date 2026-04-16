//! HTTP integration tests for the kestrel API server.
//!
//! Uses `tower::ServiceExt` to send requests directly to the Axum router
//! without spawning a real HTTP server.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use kestrel_api::ApiServer;
use kestrel_bus::MessageBus;
use kestrel_config::Config;
use kestrel_core::Usage;
use kestrel_heartbeat::{BusHealthCheck, HeartbeatService, ProviderHealthCheck};
use kestrel_providers::base::{
    BoxStream, CompletionChunk, CompletionRequest, CompletionResponse, LlmProvider,
};
use kestrel_providers::ProviderRegistry;
use kestrel_session::SessionManager;
use kestrel_tools::ToolRegistry;
use tower::ServiceExt;

/// Mock provider for integration tests.
struct MockProvider;

#[async_trait::async_trait]
impl LlmProvider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }
    async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<CompletionResponse> {
        Ok(CompletionResponse {
            content: Some("Mock integration response".to_string()),
            tool_calls: None,
            usage: Some(Usage {
                prompt_tokens: Some(10),
                completion_tokens: Some(6),
                total_tokens: Some(16),
            }),
            finish_reason: Some("stop".to_string()),
        })
    }
    async fn complete_stream(&self, req: CompletionRequest) -> anyhow::Result<BoxStream> {
        let resp = self.complete(req).await?;
        let chunk = CompletionChunk {
            delta: resp.content,
            tool_call_deltas: None,
            usage: resp.usage,
            done: true,
        };
        Ok(Box::pin(futures::stream::once(async move { Ok(chunk) })))
    }
    fn supports_model(&self, _model: &str) -> bool {
        true
    }
}

fn make_app() -> axum::Router {
    let config = Config::default();
    let bus = MessageBus::new();
    let tmp = tempfile::tempdir().unwrap();
    let session_manager = SessionManager::new(tmp.path().to_path_buf()).unwrap();
    let providers = ProviderRegistry::new();
    let tools = ToolRegistry::new();

    let server =
        ApiServer::with_registries(config, bus, session_manager, providers, tools, Some(8080));
    server.router()
}

fn make_app_with_provider() -> axum::Router {
    let mut config = Config::default();
    config.agent.model = "mock-model".to_string();
    let bus = MessageBus::new();
    let tmp = tempfile::tempdir().unwrap();
    let session_manager = SessionManager::new(tmp.path().to_path_buf()).unwrap();
    let mut reg = ProviderRegistry::new();
    reg.register("mock", MockProvider);
    reg.set_default("mock");
    let tools = ToolRegistry::new();

    let server = ApiServer::with_registries(config, bus, session_manager, reg, tools, Some(8080));
    server.router()
}

// ─── Health ──────────────────────────────────────────────

#[tokio::test]
async fn test_health_check() {
    let app = make_app();
    let req = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["status"], "starting"); // No health snapshot set yet
    assert_eq!(v["version"], kestrel_core::VERSION);
}

#[tokio::test]
async fn test_health_check_with_heartbeat_wiring() {
    let mut config = Config::default();
    config.agent.model = "mock-model".to_string();

    let bus = MessageBus::new();
    let tmp = tempfile::tempdir().unwrap();
    let session_manager = SessionManager::new(tmp.path().to_path_buf()).unwrap();

    let mut providers = ProviderRegistry::new();
    providers.register("mock", MockProvider);
    providers.set_default("mock");

    let tools = ToolRegistry::new();
    let server = ApiServer::with_registries(
        config.clone(),
        bus.clone(),
        session_manager.clone(),
        providers.clone(),
        tools.clone(),
        Some(8080),
    );

    let mut heartbeat =
        HeartbeatService::with_registries(config, providers.clone(), tools, session_manager);
    heartbeat.set_bus(bus.clone());
    heartbeat.register_check(std::sync::Arc::new(ProviderHealthCheck::new(
        std::sync::Arc::new(providers),
    )));
    heartbeat.register_check(std::sync::Arc::new(BusHealthCheck::new(bus)));
    heartbeat.add_snapshot_sink(server.health_snapshot_lock());
    heartbeat.run_checks().await.unwrap();

    let req = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let resp = server.router().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_ne!(v["status"], "starting");
    assert_eq!(v["status"], "healthy");
    assert!(v["checks"].as_array().unwrap().len() >= 2);
}

// ─── Models ──────────────────────────────────────────────

#[tokio::test]
async fn test_list_models() {
    let app = make_app();
    let req = Request::builder()
        .uri("/v1/models")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["object"], "list");
    assert!(v["data"].is_array());
}

#[tokio::test]
async fn test_list_models_with_provider() {
    let app = make_app_with_provider();
    let req = Request::builder()
        .uri("/v1/models")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let ids = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["id"].as_str().map(String::from))
        .collect::<Vec<_>>();
    assert!(ids.contains(&"mock".to_string()));
    assert!(ids.contains(&"mock-model".to_string()));
}

// ─── Validation ──────────────────────────────────────────

#[tokio::test]
async fn test_chat_completions_bad_request_no_user() {
    let app = make_app();
    let req_body = serde_json::json!({
        "model": "test-model",
        "messages": [
            {"role": "system", "content": "You are helpful"}
        ]
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&req_body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("No user message"));
}

#[tokio::test]
async fn test_chat_completions_empty_messages() {
    let app = make_app();
    let req_body = serde_json::json!({
        "model": "test-model",
        "messages": []
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&req_body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("non-empty"));
}

#[tokio::test]
async fn test_chat_completions_invalid_temperature() {
    let app = make_app_with_provider();
    let req_body = serde_json::json!({
        "model": "mock-model",
        "messages": [{"role": "user", "content": "Hi"}],
        "temperature": 3.5
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&req_body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Temperature"));
}

#[tokio::test]
async fn test_chat_completions_zero_max_tokens() {
    let app = make_app_with_provider();
    let req_body = serde_json::json!({
        "model": "mock-model",
        "messages": [{"role": "user", "content": "Hi"}],
        "max_tokens": 0
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&req_body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("max_tokens"));
}

#[tokio::test]
async fn test_chat_completions_no_provider_configured() {
    let app = make_app();
    let req_body = serde_json::json!({
        "model": "test-model",
        "messages": [
            {"role": "user", "content": "Hello"}
        ]
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&req_body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    // No provider configured → model not found (404)
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not found"));
    assert_eq!(v["error"]["code"].as_str(), Some("model_not_found"));
}

#[tokio::test]
async fn test_chat_completions_invalid_json() {
    let app = make_app();
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from("not valid json"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ─── Success paths ───────────────────────────────────────

#[tokio::test]
async fn test_chat_completions_success() {
    let app = make_app_with_provider();
    let req_body = serde_json::json!({
        "model": "mock-model",
        "messages": [
            {"role": "system", "content": "You are helpful"},
            {"role": "user", "content": "Hello"}
        ],
        "temperature": 0.7,
        "max_tokens": 100
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&req_body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["object"], "chat.completion");
    assert_eq!(v["model"], "mock-model");
    assert_eq!(v["choices"][0]["message"]["role"], "assistant");
    assert_eq!(
        v["choices"][0]["message"]["content"],
        "Mock integration response"
    );
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
    assert_eq!(v["usage"]["prompt_tokens"], 10);
    assert_eq!(v["usage"]["completion_tokens"], 6);
    assert_eq!(v["usage"]["total_tokens"], 16);
    assert!(v["id"].as_str().unwrap().starts_with("chatcmpl-"));
}

// ─── Streaming ───────────────────────────────────────────

#[tokio::test]
async fn test_chat_completions_streaming() {
    let app = make_app_with_provider();
    let req_body = serde_json::json!({
        "model": "mock-model",
        "messages": [{"role": "user", "content": "Hello"}],
        "stream": true
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&req_body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(ct.contains("text/event-stream"));
}

#[tokio::test]
async fn test_chat_completions_streaming_three_chunks() {
    let app = make_app_with_provider();
    let req_body = serde_json::json!({
        "model": "mock-model",
        "messages": [{"role": "user", "content": "Hello"}],
        "stream": true
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&req_body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let body_str = String::from_utf8(body.to_vec()).unwrap();

    // Should have exactly 4 SSE data events: role, content, stop, [DONE]
    let data_count = body_str.matches("data:").count();
    assert_eq!(
        data_count, 4,
        "Expected 4 SSE chunks, got {}: {}",
        data_count, body_str
    );

    // First chunk: role announcement
    assert!(
        body_str.contains("\"role\":\"assistant\"") || body_str.contains("\"role\": \"assistant\"")
    );

    // Second chunk: content
    assert!(body_str.contains("Mock integration response"));

    // Third chunk: finish_reason + usage
    assert!(
        body_str.contains("\"finish_reason\":\"stop\"")
            || body_str.contains("\"finish_reason\": \"stop\"")
    );
    assert!(body_str.contains("\"prompt_tokens\""));

    // Fourth event: [DONE] sentinel
    assert!(
        body_str.contains("data: [DONE]"),
        "SSE stream should end with [DONE]"
    );
}

#[tokio::test]
async fn test_chat_completions_streaming_consistent_id() {
    let app = make_app_with_provider();
    let req_body = serde_json::json!({
        "model": "mock-model",
        "messages": [{"role": "user", "content": "Hello"}],
        "stream": true
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&req_body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let body_str = String::from_utf8(body.to_vec()).unwrap();

    // Extract IDs from all SSE data lines
    let ids: Vec<String> = body_str
        .lines()
        .filter(|l| l.starts_with("data:"))
        .filter_map(|l| {
            let json_str = l.trim_start_matches("data:").trim();
            let v: serde_json::Value = serde_json::from_str(json_str).ok()?;
            v.get("id")?.as_str().map(|s| s.to_string())
        })
        .collect();

    assert!(ids.len() >= 2, "Should have at least 2 chunks with IDs");
    let first = &ids[0];
    assert!(ids.iter().all(|id| id == first), "All IDs must be the same");
    assert!(first.starts_with("chatcmpl-"));
}

// ─── HTTP method / routing ───────────────────────────────

#[tokio::test]
async fn test_404_for_unknown_route() {
    let app = make_app();
    let req = Request::builder()
        .uri("/v1/unknown")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_method_not_allowed_on_models() {
    let app = make_app();
    let req = Request::builder()
        .method("POST")
        .uri("/v1/models")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn test_method_not_allowed_on_completions() {
    let app = make_app();
    let req = Request::builder()
        .method("GET")
        .uri("/v1/chat/completions")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}
