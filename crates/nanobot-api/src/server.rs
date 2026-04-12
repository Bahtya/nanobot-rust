//! OpenAI-compatible HTTP API server using Axum.
//!
//! Provides `/v1/chat/completions` and `/v1/models` endpoints.
//! The completions endpoint runs the agent directly to produce responses.

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use nanobot_agent::AgentRunner;
use nanobot_bus::MessageBus;
use nanobot_config::Config;
use nanobot_core::{Message, MessageRole};
use nanobot_providers::ProviderRegistry;
use nanobot_session::SessionManager;
use nanobot_tools::ToolRegistry;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::{debug, info};

/// Shared state for the API server.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub bus: Arc<MessageBus>,
    pub session_manager: Arc<SessionManager>,
    pub provider_registry: Arc<ProviderRegistry>,
    pub tool_registry: Arc<ToolRegistry>,
}

/// The API server.
pub struct ApiServer {
    state: AppState,
    port: u16,
}

impl ApiServer {
    pub fn new(
        config: Config,
        bus: MessageBus,
        session_manager: SessionManager,
        port: u16,
    ) -> Self {
        let state = AppState {
            config: Arc::new(config),
            bus: Arc::new(bus),
            session_manager: Arc::new(session_manager),
            provider_registry: Arc::new(ProviderRegistry::new()),
            tool_registry: Arc::new(ToolRegistry::new()),
        };
        Self { state, port }
    }

    /// Create with pre-built provider and tool registries.
    pub fn with_registries(
        config: Config,
        bus: MessageBus,
        session_manager: SessionManager,
        provider_registry: ProviderRegistry,
        tool_registry: ToolRegistry,
        port: u16,
    ) -> Self {
        let state = AppState {
            config: Arc::new(config),
            bus: Arc::new(bus),
            session_manager: Arc::new(session_manager),
            provider_registry: Arc::new(provider_registry),
            tool_registry: Arc::new(tool_registry),
        };
        Self { state, port }
    }

    /// Build the Axum router.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/v1/chat/completions", post(chat_completions))
            .route("/v1/models", get(list_models))
            .route("/health", get(health))
            .layer(CorsLayer::permissive())
            .layer(TraceLayer::new_for_http())
            .with_state(self.state.clone())
    }

    /// Start the API server.
    pub async fn run(&self) -> anyhow::Result<()> {
        let app = self.router();
        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], self.port));
        info!("API server listening on {}", addr);

        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app).await?;
        Ok(())
    }
}

// ─── Request/Response Types ──────────────────────────────────

/// OpenAI-compatible chat completion request.
#[derive(Debug, Deserialize)]
pub struct ChatCompletionRequest {
    /// Model name to use for completion.
    pub model: String,
    /// Conversation messages.
    pub messages: Vec<ApiMessage>,
    /// Sampling temperature (0-2).
    #[serde(default)]
    pub temperature: Option<f32>,
    /// Maximum tokens to generate.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Whether to stream the response.
    #[serde(default)]
    pub stream: bool,
}

/// A message in the chat completion API.
#[derive(Debug, Deserialize, Serialize)]
pub struct ApiMessage {
    /// Role of the message author (system, user, assistant).
    pub role: String,
    /// Text content of the message.
    pub content: String,
}

/// OpenAI-compatible chat completion response.
#[derive(Debug, Serialize)]
pub struct ChatCompletionResponse {
    /// Unique completion ID.
    id: String,
    /// Object type (always "chat.completion").
    object: String,
    /// Unix timestamp of creation.
    created: u64,
    /// Model used for completion.
    model: String,
    /// Completion choices.
    choices: Vec<Choice>,
    /// Token usage statistics.
    usage: UsageInfo,
}

#[derive(Debug, Serialize)]
struct Choice {
    index: u32,
    message: ResponseMessage,
    finish_reason: String,
}

#[derive(Debug, Serialize)]
struct ResponseMessage {
    role: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct UsageInfo {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
}

#[derive(Debug, Serialize)]
struct ModelsResponse {
    object: String,
    data: Vec<ModelInfo>,
}

#[derive(Debug, Serialize)]
struct ModelInfo {
    id: String,
    object: String,
    created: u64,
    owned_by: String,
}

/// Error response matching OpenAI format.
#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: ErrorDetail,
}

#[derive(Debug, Serialize)]
struct ErrorDetail {
    message: String,
    r#type: String,
    code: Option<String>,
}

// ─── Handlers ──────────────────────────────────────────────

async fn chat_completions(
    State(state): State<AppState>,
    Json(req): Json<ChatCompletionRequest>,
) -> impl IntoResponse {
    debug!("Chat completion request for model: {}", req.model);

    // Extract the last user message
    let user_content = req
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.clone())
        .unwrap_or_default();

    if user_content.is_empty() {
        let error = ErrorResponse {
            error: ErrorDetail {
                message: "No user message found in request".to_string(),
                r#type: "invalid_request_error".to_string(),
                code: None,
            },
        };
        return (StatusCode::BAD_REQUEST, Json(error)).into_response();
    }

    // Build the system prompt from config
    let system_prompt = state
        .config
        .agent
        .system_prompt
        .clone()
        .unwrap_or_else(|| "You are a helpful AI assistant.".to_string());

    // Convert API messages to nanobot Messages
    let mut messages: Vec<Message> = req
        .messages
        .iter()
        .filter(|m| m.role != "system")
        .map(|m| Message {
            role: match m.role.as_str() {
                "assistant" => MessageRole::Assistant,
                _ => MessageRole::User,
            },
            content: m.content.clone(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        })
        .collect();

    // Ensure there's at least one user message
    if messages.is_empty() {
        messages.push(Message {
            role: MessageRole::User,
            content: user_content,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        });
    }

    // Build the agent runner and execute
    let runner = AgentRunner::new(
        state.config.clone(),
        state.provider_registry.clone(),
        state.tool_registry.clone(),
    );

    match runner.run(system_prompt, messages).await {
        Ok(result) => {
            let response = ChatCompletionResponse {
                id: format!("chatcmpl-{}", uuid::Uuid::new_v4()),
                object: "chat.completion".to_string(),
                created: chrono::Utc::now().timestamp() as u64,
                model: req.model.clone(),
                choices: vec![Choice {
                    index: 0,
                    message: ResponseMessage {
                        role: "assistant".to_string(),
                        content: result.content,
                    },
                    finish_reason: "stop".to_string(),
                }],
                usage: UsageInfo {
                    prompt_tokens: result.usage.prompt_tokens.unwrap_or(0),
                    completion_tokens: result.usage.completion_tokens.unwrap_or(0),
                    total_tokens: result.usage.total_tokens.unwrap_or(0),
                },
            };
            (StatusCode::OK, Json(response)).into_response()
        }
        Err(e) => {
            debug!("Agent error: {}", e);
            let error = ErrorResponse {
                error: ErrorDetail {
                    message: format!("Agent processing error: {}", e),
                    r#type: "server_error".to_string(),
                    code: None,
                },
            };
            (StatusCode::INTERNAL_SERVER_ERROR, Json(error)).into_response()
        }
    }
}

async fn list_models(State(state): State<AppState>) -> impl IntoResponse {
    let model = &state.config.agent.model;
    Json(ModelsResponse {
        object: "list".to_string(),
        data: vec![ModelInfo {
            id: model.clone(),
            object: "model".to_string(),
            created: 0,
            owned_by: "nanobot-rs".to_string(),
        }],
    })
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "version": nanobot_core::VERSION,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_state() -> AppState {
        let config = Config::default();
        let bus = MessageBus::new();
        let tmp = tempfile::tempdir().unwrap();
        let session_manager = SessionManager::new(tmp.path().to_path_buf()).unwrap();
        AppState {
            config: Arc::new(config),
            bus: Arc::new(bus),
            session_manager: Arc::new(session_manager),
            provider_registry: Arc::new(ProviderRegistry::new()),
            tool_registry: Arc::new(ToolRegistry::new()),
        }
    }

    fn test_router() -> Router {
        Router::new()
            .route("/v1/chat/completions", post(chat_completions))
            .route("/v1/models", get(list_models))
            .route("/health", get(health))
            .with_state(test_state())
    }

    #[tokio::test]
    async fn test_health_endpoint() {
        let app = test_router();
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
    async fn test_models_endpoint() {
        let app = test_router();
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
    async fn test_chat_completions_no_user_message() {
        let app = test_router();
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
    }

    #[tokio::test]
    async fn test_chat_completions_with_user_message() {
        // Without a configured provider, the agent will fail — we test the error path.
        let app = test_router();
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
        // No provider configured → 500 with agent error
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("No provider"));
    }

    #[tokio::test]
    async fn test_server_new_and_router() {
        let config = Config::default();
        let bus = MessageBus::new();
        let tmp = tempfile::tempdir().unwrap();
        let session_manager = SessionManager::new(tmp.path().to_path_buf()).unwrap();
        let server = ApiServer::new(config, bus, session_manager, 8080);
        let _router = server.router();
    }

    #[tokio::test]
    async fn test_server_with_registries() {
        let config = Config::default();
        let bus = MessageBus::new();
        let tmp = tempfile::tempdir().unwrap();
        let session_manager = SessionManager::new(tmp.path().to_path_buf()).unwrap();
        let providers = ProviderRegistry::new();
        let tools = ToolRegistry::new();
        let server =
            ApiServer::with_registries(config, bus, session_manager, providers, tools, 9090);
        let _router = server.router();
    }

    #[test]
    fn test_chat_completion_request_deserialize() {
        let json = r#"{
            "model": "gpt-4",
            "messages": [
                {"role": "user", "content": "Hi"}
            ],
            "temperature": 0.7,
            "max_tokens": 100,
            "stream": false
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.model, "gpt-4");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, "user");
        assert_eq!(req.temperature, Some(0.7));
        assert_eq!(req.max_tokens, Some(100));
        assert!(!req.stream);
    }

    #[test]
    fn test_chat_completion_request_minimal() {
        let json = r#"{
            "model": "test",
            "messages": []
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.model, "test");
        assert!(req.messages.is_empty());
        assert!(req.temperature.is_none());
        assert!(req.max_tokens.is_none());
        assert!(!req.stream);
    }

    #[test]
    fn test_api_message_serde() {
        let msg = ApiMessage {
            role: "user".to_string(),
            content: "Hello world".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: ApiMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.role, "user");
        assert_eq!(back.content, "Hello world");
    }
}
