//! HTTP integration tests for the nanobot API server.
//!
//! Uses `tower::ServiceExt` to send requests directly to the Axum router
//! without spawning a real HTTP server.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use nanobot_api::ApiServer;
use nanobot_bus::MessageBus;
use nanobot_config::Config;
use nanobot_providers::ProviderRegistry;
use nanobot_session::SessionManager;
use nanobot_tools::ToolRegistry;
use tower::ServiceExt;

fn make_app() -> axum::Router {
    let config = Config::default();
    let bus = MessageBus::new();
    let tmp = tempfile::tempdir().unwrap();
    let session_manager = SessionManager::new(tmp.path().to_path_buf()).unwrap();
    let providers = ProviderRegistry::new();
    let tools = ToolRegistry::new();

    let server = ApiServer::with_registries(config, bus, session_manager, providers, tools, 8080);
    server.router()
}

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
    assert_eq!(v["status"], "ok");
    assert_eq!(v["version"], nanobot_core::VERSION);
}

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
    assert!(v["data"][0]["id"].is_string());
}

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
    // No provider configured → 500
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("No provider"));
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
