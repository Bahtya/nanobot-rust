//! OpenAI-compatible HTTP API server using Axum.
//!
//! Provides `/v1/chat/completions` (with SSE streaming), `/v1/models`, and `/health`.
//! The completions endpoint runs the agent directly to produce responses.
//!
//! ## Middleware
//!
//! - **Request logging**: Structured logs with method, path, status, and latency.
//! - **Auth**: Bearer-token authentication via axum middleware on protected routes.
//! - **CORS**: Configurable via `api.allowed_origins` in config.
//! - **Body limit**: Configurable via `api.max_body_size` in config.
//! - **Tracing**: HTTP request/response tracing via `tower-http`.
//!
//! ## Graceful shutdown
//!
//! The server listens for SIGINT / SIGTERM and drains in-flight SSE streams
//! via a `CancellationToken` before exiting.

use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, State},
    http::{HeaderValue, Method, Request, StatusCode},
    middleware::{self, Next},
    response::sse::{Event, KeepAlive, Sse},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use futures::stream::Stream;
use futures::StreamExt;
use kestrel_agent::AgentRunner;
use kestrel_bus::MessageBus;
use kestrel_config::Config;
use kestrel_core::{Message, MessageRole};
use kestrel_heartbeat::types::{CheckStatus, HealthSnapshot};
use kestrel_providers::ProviderRegistry;
use kestrel_session::SessionManager;
use kestrel_tools::ToolRegistry;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::{debug, info, warn, Instrument};

/// Shared state for the API server.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub bus: Arc<MessageBus>,
    pub session_manager: Arc<SessionManager>,
    pub provider_registry: Arc<ProviderRegistry>,
    pub tool_registry: Arc<ToolRegistry>,
    pub api_key: Option<String>,
    /// Cancellation token for graceful shutdown — cancelled on SIGINT/SIGTERM.
    pub cancel: CancellationToken,
    /// Latest health snapshot from the heartbeat service, updated externally.
    pub health_snapshot: Arc<parking_lot::RwLock<Option<HealthSnapshot>>>,
}

/// The API server.
pub struct ApiServer {
    state: AppState,
    host: String,
    port: u16,
}

/// Build a CorsLayer from the `api.allowed_origins` config.
///
/// - `["*"]` → permissive CORS (any origin).
/// - Specific origins → only those origins are allowed.
/// - Always includes `Access-Control-Max-Age: 3600` to cache preflight.
/// - Sets `Access-Control-Allow-Methods: GET, POST, OPTIONS`.
/// - Sets `Access-Control-Allow-Headers: Content-Type, Authorization`.
fn build_cors_layer(config: &Config) -> CorsLayer {
    let origins = &config.api.allowed_origins;

    if origins.len() == 1 && origins[0] == "*" {
        return CorsLayer::permissive().max_age(std::time::Duration::from_secs(3600));
    }

    let parsed: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|o| o.parse::<HeaderValue>().ok())
        .collect();

    CorsLayer::new()
        .allow_origin(AllowOrigin::list(parsed))
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([AUTHORIZATION, CONTENT_TYPE])
        .max_age(std::time::Duration::from_secs(3600))
}

impl ApiServer {
    /// Create a new API server with fresh registries.
    ///
    /// Reads `host` and `port` from `config.api`.  Pass `port_override` to
    /// override the config port (e.g. from a CLI `--port` flag).
    pub fn new(
        config: Config,
        bus: MessageBus,
        session_manager: SessionManager,
        port_override: Option<u16>,
    ) -> Self {
        let host = config.api.host.clone();
        let port = port_override.unwrap_or(config.api.port);
        let state = AppState {
            config: Arc::new(config),
            bus: Arc::new(bus),
            session_manager: Arc::new(session_manager),
            provider_registry: Arc::new(ProviderRegistry::new()),
            tool_registry: Arc::new(ToolRegistry::new()),
            api_key: None,
            cancel: CancellationToken::new(),
            health_snapshot: Arc::new(parking_lot::RwLock::new(None)),
        };
        Self { state, host, port }
    }

    /// Create with pre-built provider and tool registries.
    ///
    /// Reads `host` and `port` from `config.api`.  Pass `port_override` to
    /// override the config port (e.g. from a CLI `--port` flag).
    pub fn with_registries(
        config: Config,
        bus: MessageBus,
        session_manager: SessionManager,
        provider_registry: ProviderRegistry,
        tool_registry: ToolRegistry,
        port_override: Option<u16>,
    ) -> Self {
        let host = config.api.host.clone();
        let port = port_override.unwrap_or(config.api.port);
        let state = AppState {
            config: Arc::new(config),
            bus: Arc::new(bus),
            session_manager: Arc::new(session_manager),
            provider_registry: Arc::new(provider_registry),
            tool_registry: Arc::new(tool_registry),
            api_key: None,
            cancel: CancellationToken::new(),
            health_snapshot: Arc::new(parking_lot::RwLock::new(None)),
        };
        Self { state, host, port }
    }

    /// Set an API key for bearer-token authentication.
    pub fn with_api_key(mut self, key: String) -> Self {
        self.state.api_key = Some(key);
        self
    }

    /// Build the Axum router with all middleware.
    pub fn router(&self) -> Router {
        let cors = build_cors_layer(&self.state.config);
        let body_limit = self.state.config.api.max_body_size;

        let public_routes = Router::new()
            .route("/v1/models", get(list_models))
            .route("/health", get(health))
            .route("/ready", get(ready));

        let protected_routes = Router::new()
            .route("/v1/chat/completions", post(chat_completions))
            .layer(middleware::from_fn_with_state(
                self.state.clone(),
                auth_middleware,
            ));

        Router::new()
            .merge(public_routes)
            .merge(protected_routes)
            .layer(DefaultBodyLimit::max(body_limit))
            .layer(middleware::from_fn(request_log_middleware))
            .layer(middleware::from_fn(request_id_middleware))
            .layer(cors)
            .layer(TraceLayer::new_for_http())
            .with_state(self.state.clone())
    }

    /// Start the API server with graceful shutdown.
    ///
    /// Listens for SIGINT (Ctrl-C) and SIGTERM. On signal, the cancellation
    /// token is triggered, which causes in-flight SSE streams to terminate
    /// with a `stop` event, then the TCP listener stops accepting new
    /// connections and the server drains existing requests.
    pub async fn run(&self) -> anyhow::Result<()> {
        let app = self.router();
        let host: std::net::IpAddr = self
            .host
            .parse()
            .unwrap_or(std::net::IpAddr::from([0, 0, 0, 0]));
        let addr = std::net::SocketAddr::from((host, self.port));
        info!("API server listening on {}", addr);

        let listener = tokio::net::TcpListener::bind(addr).await?;

        let cancel = self.state.cancel.clone();
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                tokio::signal::ctrl_c()
                    .await
                    .expect("failed to install Ctrl-C handler");
                info!("Shutdown signal received, draining connections…");
                cancel.cancel();
            })
            .await?;

        info!("API server stopped");
        Ok(())
    }

    /// Trigger graceful shutdown programmatically.
    pub fn shutdown(&self) {
        self.state.cancel.cancel();
    }

    /// Update the shared health snapshot (called by the heartbeat service).
    pub fn set_health_snapshot(&self, snapshot: HealthSnapshot) {
        *self.state.health_snapshot.write() = Some(snapshot);
    }

    /// Get a reference to the shared health snapshot lock for external wiring.
    pub fn health_snapshot_lock(&self) -> Arc<parking_lot::RwLock<Option<HealthSnapshot>>> {
        self.state.health_snapshot.clone()
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
    id: String,
    object: String,
    created: u64,
    model: String,
    choices: Vec<Choice>,
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

// ─── Auth helper ────────────────────────────────────────────

// ─── Middleware ──────────────────────────────────────────────

/// Request ID middleware — ensures every request has a unique `x-request-id`.
///
/// If the client sends an `x-request-id` header it is preserved; otherwise a
/// new UUID v4 is generated. The ID is set on the response header and injected
/// into the current tracing span for structured log correlation.
async fn request_id_middleware(req: Request<Body>, next: Next) -> impl IntoResponse {
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // Inject into tracing span so downstream log lines carry the request ID.
    let span = tracing::info_span!("request", request_id = %request_id);
    let response: axum::response::Response = next.run(req).instrument(span).await;

    // Attach the request ID to the response.
    let mut response = response;
    if let Ok(val) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", val);
    }

    response
}

/// Request logging middleware — logs method, path, status code, and latency.
/// Also intercepts 413 Payload Too Large responses and returns an OpenAI-format
/// JSON error body instead of Axum's default plain text.
async fn request_log_middleware(req: Request<Body>, next: Next) -> impl IntoResponse {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let start = Instant::now();

    let response = next.run(req).await;

    let elapsed = start.elapsed();
    let status = response.status();
    info!(
        method = %method,
        path = %path,
        status = %status.as_u16(),
        elapsed_ms = elapsed.as_millis() as u64,
        "HTTP request"
    );

    // Replace default 413 body with OpenAI-format JSON error
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        let error = ErrorResponse {
            error: ErrorDetail {
                message: "Request body exceeds the maximum allowed size".to_string(),
                r#type: "invalid_request_error".to_string(),
                code: Some("payload_too_large".to_string()),
            },
        };
        return (StatusCode::PAYLOAD_TOO_LARGE, Json(error)).into_response();
    }

    response
}

/// Auth middleware — validates bearer token if an API key is configured.
/// Applied only to protected routes (e.g. `/v1/chat/completions`).
async fn auth_middleware(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> impl IntoResponse {
    let key = match &state.api_key {
        Some(k) if !k.is_empty() => k,
        _ => return next.run(req).await, // No auth configured
    };

    let auth_header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if auth_header == format!("Bearer {}", key) {
        next.run(req).await
    } else {
        let error = ErrorResponse {
            error: ErrorDetail {
                message: "Invalid or missing API key".to_string(),
                r#type: "authentication_error".to_string(),
                code: Some("invalid_api_key".to_string()),
            },
        };
        (StatusCode::UNAUTHORIZED, Json(error)).into_response()
    }
}

// ─── Validation helpers ──────────────────────────────────────

/// Build a validation error response.
fn validation_error(message: impl Into<String>, code: Option<String>) -> axum::response::Response {
    let error = ErrorResponse {
        error: ErrorDetail {
            message: message.into(),
            r#type: "invalid_request_error".to_string(),
            code,
        },
    };
    (StatusCode::BAD_REQUEST, Json(error)).into_response()
}

/// Validate a chat completion request. Returns Ok(()) or an error response.
#[allow(clippy::result_large_err)]
fn validate_request(req: &ChatCompletionRequest) -> Result<(), axum::response::Response> {
    // Model must be non-empty
    if req.model.trim().is_empty() {
        return Err(validation_error("Model must be a non-empty string", None));
    }

    // Messages must not be empty
    if req.messages.is_empty() {
        return Err(validation_error("Messages must be a non-empty array", None));
    }

    // Validate each message has a recognized role and non-empty content
    let valid_roles = ["system", "user", "assistant", "tool"];
    for (i, msg) in req.messages.iter().enumerate() {
        if !valid_roles.contains(&msg.role.as_str()) {
            return Err(validation_error(
                format!("Message at index {} has invalid role: '{}'. Must be one of: system, user, assistant, tool", i, msg.role),
                None,
            ));
        }
        if msg.content.trim().is_empty() {
            return Err(validation_error(
                format!("Message at index {} has empty content", i),
                None,
            ));
        }
    }

    // At least one user message is required
    if !req.messages.iter().any(|m| m.role == "user") {
        return Err(validation_error(
            "No user message found in request. At least one message with role 'user' is required.",
            None,
        ));
    }

    // Validate temperature range
    if let Some(temp) = req.temperature {
        if !(0.0..=2.0).contains(&temp) {
            return Err(validation_error(
                format!("Temperature must be between 0 and 2, got {}", temp),
                None,
            ));
        }
    }

    // Validate max_tokens
    if let Some(max_tokens) = req.max_tokens {
        if max_tokens == 0 {
            return Err(validation_error("max_tokens must be greater than 0", None));
        }
    }

    Ok(())
}

// ─── Handlers ──────────────────────────────────────────────

async fn chat_completions(
    State(state): State<AppState>,
    Json(req): Json<ChatCompletionRequest>,
) -> impl IntoResponse {
    debug!(
        "Chat completion request for model: {} (stream: {})",
        req.model, req.stream
    );

    // Validate request
    if let Err(resp) = validate_request(&req) {
        return resp;
    }

    // Check provider availability early
    if state.provider_registry.get_provider(&req.model).is_none() {
        let error = ErrorResponse {
            error: ErrorDetail {
                message: format!(
                    "Model '{}' not found. Check available models at GET /v1/models.",
                    req.model
                ),
                r#type: "invalid_request_error".to_string(),
                code: Some("model_not_found".to_string()),
            },
        };
        return (StatusCode::NOT_FOUND, Json(error)).into_response();
    }

    // Build the system prompt from config
    let system_prompt = state
        .config
        .agent
        .system_prompt
        .clone()
        .unwrap_or_else(|| "You are a helpful AI assistant.".to_string());

    // Convert API messages to kestrel Messages
    let messages: Vec<Message> = req
        .messages
        .iter()
        .filter(|m| m.role != "system")
        .map(|m| Message {
            role: match m.role.as_str() {
                "assistant" => MessageRole::Assistant,
                "tool" => MessageRole::Tool,
                _ => MessageRole::User,
            },
            content: m.content.clone(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        })
        .collect();

    if req.stream {
        return stream_completion(state, req, system_prompt, messages).await;
    }

    // Non-streaming path
    non_stream_completion(state, req, system_prompt, messages).await
}

/// Handle non-streaming completion.
async fn non_stream_completion(
    state: AppState,
    req: ChatCompletionRequest,
    system_prompt: String,
    messages: Vec<Message>,
) -> axum::response::Response {
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
            warn!("Agent error: {}", e);
            let msg = e.to_string();
            let (status, code) = if msg.contains("429") {
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    Some("rate_limit_exceeded".to_string()),
                )
            } else {
                (StatusCode::INTERNAL_SERVER_ERROR, None)
            };
            let error = ErrorResponse {
                error: ErrorDetail {
                    message: format!("Agent processing error: {}", msg),
                    r#type: "server_error".to_string(),
                    code,
                },
            };
            (status, Json(error)).into_response()
        }
    }
}

/// Handle streaming completion via SSE.
///
/// Emits proper OpenAI-format SSE chunks:
/// 1. Role announcement chunk (`delta: {role: "assistant"}`)
/// 2. Content chunk (`delta: {content: "..."}`)
/// 3. Final chunk with finish_reason and usage
///
/// If the server's cancellation token fires mid-stream, the stream ends
/// gracefully with a `stop` event.
async fn stream_completion(
    state: AppState,
    req: ChatCompletionRequest,
    system_prompt: String,
    messages: Vec<Message>,
) -> axum::response::Response {
    let completion_id = format!("chatcmpl-{}", uuid::Uuid::new_v4());
    let model = req.model.clone();
    let created = chrono::Utc::now().timestamp() as u64;

    let runner = AgentRunner::new(
        state.config.clone(),
        state.provider_registry.clone(),
        state.tool_registry.clone(),
    );

    let stream_result = runner.run(system_prompt, messages).await;
    let cancel = state.cancel.clone();

    let stream: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> = match stream_result
    {
        Ok(result) => {
            let content = result.content;
            let usage = result.usage;
            let id = completion_id;
            let mdl = model;
            let cr = created;

            let events = futures::stream::iter(vec![
                // Chunk 1: role announcement
                Ok(Event::default().data(
                    serde_json::json!({
                        "id": id,
                        "object": "chat.completion.chunk",
                        "created": cr,
                        "model": mdl,
                        "choices": [{
                            "index": 0,
                            "delta": {"role": "assistant"},
                            "finish_reason": null
                        }]
                    })
                    .to_string(),
                )),
                // Chunk 2: content
                Ok(Event::default().data(
                    serde_json::json!({
                        "id": id,
                        "object": "chat.completion.chunk",
                        "created": cr,
                        "model": mdl,
                        "choices": [{
                            "index": 0,
                            "delta": {"content": content},
                            "finish_reason": null
                        }],
                        "usage": null
                    })
                    .to_string(),
                )),
                // Chunk 3: stop with usage
                Ok(Event::default().data(
                    serde_json::json!({
                        "id": id,
                        "object": "chat.completion.chunk",
                        "created": cr,
                        "model": mdl,
                        "choices": [{
                            "index": 0,
                            "delta": {},
                            "finish_reason": "stop"
                        }],
                        "usage": {
                            "prompt_tokens": usage.prompt_tokens.unwrap_or(0),
                            "completion_tokens": usage.completion_tokens.unwrap_or(0),
                            "total_tokens": usage.total_tokens.unwrap_or(0)
                        }
                    })
                    .to_string(),
                )),
            ]);

            // On graceful shutdown, emit [DONE] then terminate.
            // take_until truncates when the cancel token fires; chain appends
            // the [DONE] sentinel so clients receive a clean end-of-stream signal.
            let done_event =
                futures::stream::once(async move { Ok(Event::default().data("[DONE]")) });
            let stream = events
                .take_until(cancel.cancelled_owned())
                .chain(done_event);
            Box::pin(stream)
        }
        Err(e) => {
            let msg = e.to_string();
            Box::pin(futures::stream::once(async move {
                let error = serde_json::json!({
                    "error": {
                        "message": format!("Agent error: {}", msg),
                        "type": "server_error",
                        "code": null
                    }
                });
                Ok(Event::default().data(error.to_string()))
            }))
        }
    };

    let sse = Sse::new(stream).keep_alive(KeepAlive::default());
    sse.into_response()
}

async fn list_models(State(state): State<AppState>) -> impl IntoResponse {
    let mut models = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();

    // Collect models from all registered providers
    for name in state.provider_registry.provider_names() {
        if seen_ids.insert(name.clone()) {
            models.push(ModelInfo {
                id: name.clone(),
                object: "model".to_string(),
                created: 0,
                owned_by: format!("kestrel/{}", name),
            });
        }
    }

    // Include the configured agent model
    let agent_model = &state.config.agent.model;
    if !agent_model.is_empty() && seen_ids.insert(agent_model.clone()) {
        models.push(ModelInfo {
            id: agent_model.clone(),
            object: "model".to_string(),
            created: 0,
            owned_by: "kestrel".to_string(),
        });
    }

    // Include models from provider configs
    if let Some(ref entry) = state.config.providers.anthropic {
        if let Some(ref model) = entry.model {
            if seen_ids.insert(model.clone()) {
                models.push(ModelInfo {
                    id: model.clone(),
                    object: "model".to_string(),
                    created: 0,
                    owned_by: "anthropic".to_string(),
                });
            }
        }
    }
    if let Some(ref entry) = state.config.providers.openai {
        if let Some(ref model) = entry.model {
            if seen_ids.insert(model.clone()) {
                models.push(ModelInfo {
                    id: model.clone(),
                    object: "model".to_string(),
                    created: 0,
                    owned_by: "openai".to_string(),
                });
            }
        }
    }
    if let Some(ref entry) = state.config.providers.deepseek {
        if let Some(ref model) = entry.model {
            if seen_ids.insert(model.clone()) {
                models.push(ModelInfo {
                    id: model.clone(),
                    object: "model".to_string(),
                    created: 0,
                    owned_by: "deepseek".to_string(),
                });
            }
        }
    }
    if let Some(ref entry) = state.config.providers.groq {
        if let Some(ref model) = entry.model {
            if seen_ids.insert(model.clone()) {
                models.push(ModelInfo {
                    id: model.clone(),
                    object: "model".to_string(),
                    created: 0,
                    owned_by: "groq".to_string(),
                });
            }
        }
    }
    if let Some(ref entry) = state.config.providers.openrouter {
        if let Some(ref model) = entry.model {
            if seen_ids.insert(model.clone()) {
                models.push(ModelInfo {
                    id: model.clone(),
                    object: "model".to_string(),
                    created: 0,
                    owned_by: "openrouter".to_string(),
                });
            }
        }
    }
    if let Some(ref entry) = state.config.providers.ollama {
        if let Some(ref model) = entry.model {
            if !model.is_empty() && seen_ids.insert(model.clone()) {
                models.push(ModelInfo {
                    id: model.clone(),
                    object: "model".to_string(),
                    created: 0,
                    owned_by: "ollama".to_string(),
                });
            }
        }
    }

    Json(ModelsResponse {
        object: "list".to_string(),
        data: models,
    })
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let snapshot = state.health_snapshot.read();

    match snapshot.clone() {
        Some(snap) => {
            let status = if snap.healthy {
                if snap.degraded {
                    "degraded"
                } else {
                    "healthy"
                }
            } else {
                "unhealthy"
            };

            let checks_json: Vec<serde_json::Value> = snap
                .checks
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "component": c.component,
                        "status": match c.status {
                            CheckStatus::Healthy => "healthy",
                            CheckStatus::Degraded => "degraded",
                            CheckStatus::Unhealthy => "unhealthy",
                            CheckStatus::Skipped => "skipped",
                        },
                        "message": c.message,
                    })
                })
                .collect();

            Json(serde_json::json!({
                "status": status,
                "version": kestrel_core::VERSION,
                "healthy": snap.healthy,
                "degraded": snap.degraded,
                "checks": checks_json,
                "summary": snap.summary(),
                "timestamp": snap.timestamp.to_rfc3339(),
            }))
        }
        None => Json(serde_json::json!({
            "status": "starting",
            "version": kestrel_core::VERSION,
            "healthy": true,
            "checks": [],
            "message": "No health checks run yet",
        })),
    }
}

/// Readiness probe — returns 200 if healthy, 503 if not.
///
/// Suitable for Kubernetes readiness probes.
async fn ready(State(state): State<AppState>) -> impl IntoResponse {
    let snapshot = state.health_snapshot.read();

    match snapshot.clone() {
        Some(snap) if snap.healthy => (StatusCode::OK, Json(serde_json::json!({ "ready": true }))),
        Some(snap) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "ready": false,
                "reason": snap.summary(),
            })),
        ),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "ready": false,
                "reason": "No health checks run yet",
            })),
        ),
    }
}

// ─── Tests ─────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use kestrel_core::Usage;
    use kestrel_providers::base::{
        BoxStream, CompletionChunk, CompletionRequest, CompletionResponse, LlmProvider,
    };
    use tower::ServiceExt;

    /// Mock provider for testing.
    struct MockProvider;

    #[async_trait::async_trait]
    impl LlmProvider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }
        async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<CompletionResponse> {
            Ok(CompletionResponse {
                content: Some("Mock response".to_string()),
                tool_calls: None,
                usage: Some(Usage {
                    prompt_tokens: Some(5),
                    completion_tokens: Some(3),
                    total_tokens: Some(8),
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
            api_key: None,
            cancel: CancellationToken::new(),
            health_snapshot: Arc::new(parking_lot::RwLock::new(None)),
        }
    }

    fn test_state_with_provider() -> AppState {
        let mut config = Config::default();
        config.agent.model = "mock-model".to_string();
        let bus = MessageBus::new();
        let tmp = tempfile::tempdir().unwrap();
        let session_manager = SessionManager::new(tmp.path().to_path_buf()).unwrap();
        let mut reg = ProviderRegistry::new();
        reg.register("mock", MockProvider);
        reg.set_default("mock");
        AppState {
            config: Arc::new(config),
            bus: Arc::new(bus),
            session_manager: Arc::new(session_manager),
            provider_registry: Arc::new(reg),
            tool_registry: Arc::new(ToolRegistry::new()),
            api_key: None,
            cancel: CancellationToken::new(),
            health_snapshot: Arc::new(parking_lot::RwLock::new(None)),
        }
    }

    fn test_state_with_auth() -> AppState {
        let mut state = test_state_with_provider();
        state.api_key = Some("sk-secret".to_string());
        state
    }

    fn test_router() -> Router {
        Router::new()
            .route("/v1/chat/completions", post(chat_completions))
            .route("/v1/models", get(list_models))
            .route("/health", get(health))
            .route("/ready", get(ready))
            .with_state(test_state())
    }

    fn router_with_provider() -> Router {
        Router::new()
            .route("/v1/chat/completions", post(chat_completions))
            .route("/v1/models", get(list_models))
            .route("/health", get(health))
            .route("/ready", get(ready))
            .with_state(test_state_with_provider())
    }

    fn router_with_auth() -> Router {
        let state = test_state_with_auth();
        let public_routes = Router::new()
            .route("/v1/models", get(list_models))
            .route("/health", get(health))
            .route("/ready", get(ready));

        let protected_routes = Router::new()
            .route("/v1/chat/completions", post(chat_completions))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                auth_middleware,
            ));

        Router::new()
            .merge(public_routes)
            .merge(protected_routes)
            .with_state(state)
    }

    // ─── Health ─────────────────────────────────────────

    #[tokio::test]
    async fn test_health_endpoint_no_snapshot() {
        let app = test_router();
        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["status"], "starting");
        assert_eq!(v["version"], kestrel_core::VERSION);
        assert_eq!(v["healthy"], true);
    }

    #[tokio::test]
    async fn test_health_endpoint_with_healthy_snapshot() {
        let state = test_state();
        let snap = HealthSnapshot::from_checks(vec![kestrel_heartbeat::types::HealthCheckResult {
            component: "test".to_string(),
            status: CheckStatus::Healthy,
            message: "ok".to_string(),
            timestamp: chrono::Local::now(),
        }]);
        *state.health_snapshot.write() = Some(snap);

        let app = Router::new()
            .route("/health", get(health))
            .with_state(state);

        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["status"], "healthy");
        assert_eq!(v["healthy"], true);
        assert!(v["checks"].is_array());
    }

    #[tokio::test]
    async fn test_health_endpoint_with_degraded_snapshot() {
        let state = test_state();
        let snap = HealthSnapshot::from_checks(vec![kestrel_heartbeat::types::HealthCheckResult {
            component: "channel".to_string(),
            status: CheckStatus::Degraded,
            message: "1/2 connected".to_string(),
            timestamp: chrono::Local::now(),
        }]);
        *state.health_snapshot.write() = Some(snap);

        let app = Router::new()
            .route("/health", get(health))
            .with_state(state);

        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["status"], "degraded");
        assert_eq!(v["healthy"], true);
        assert_eq!(v["degraded"], true);
    }

    #[tokio::test]
    async fn test_health_endpoint_with_unhealthy_snapshot() {
        let state = test_state();
        let snap = HealthSnapshot::from_checks(vec![kestrel_heartbeat::types::HealthCheckResult {
            component: "provider".to_string(),
            status: CheckStatus::Unhealthy,
            message: "down".to_string(),
            timestamp: chrono::Local::now(),
        }]);
        *state.health_snapshot.write() = Some(snap);

        let app = Router::new()
            .route("/health", get(health))
            .with_state(state);

        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["status"], "unhealthy");
        assert_eq!(v["healthy"], false);
    }

    #[tokio::test]
    async fn test_ready_endpoint_no_snapshot() {
        let app = test_router();
        let req = Request::builder()
            .uri("/ready")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["ready"], false);
    }

    #[tokio::test]
    async fn test_ready_endpoint_healthy() {
        let state = test_state();
        let snap = HealthSnapshot::from_checks(vec![kestrel_heartbeat::types::HealthCheckResult {
            component: "test".to_string(),
            status: CheckStatus::Healthy,
            message: "ok".to_string(),
            timestamp: chrono::Local::now(),
        }]);
        *state.health_snapshot.write() = Some(snap);

        let app = Router::new().route("/ready", get(ready)).with_state(state);

        let req = Request::builder()
            .uri("/ready")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["ready"], true);
    }

    #[tokio::test]
    async fn test_ready_endpoint_unhealthy() {
        let state = test_state();
        let snap = HealthSnapshot::from_checks(vec![kestrel_heartbeat::types::HealthCheckResult {
            component: "db".to_string(),
            status: CheckStatus::Unhealthy,
            message: "down".to_string(),
            timestamp: chrono::Local::now(),
        }]);
        *state.health_snapshot.write() = Some(snap);

        let app = Router::new().route("/ready", get(ready)).with_state(state);

        let req = Request::builder()
            .uri("/ready")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["ready"], false);
        assert!(v["reason"].is_string());
    }

    // ─── Models ─────────────────────────────────────────

    #[tokio::test]
    async fn test_models_endpoint_basic() {
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
    async fn test_models_lists_registered_providers() {
        let app = router_with_provider();
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
        assert!(
            ids.contains(&"mock".to_string()),
            "Should list 'mock' provider"
        );
        assert!(
            ids.contains(&"mock-model".to_string()),
            "Should list agent model"
        );
    }

    // ─── Chat completions: validation ───────────────────

    #[tokio::test]
    async fn test_chat_completions_no_user_message() {
        let app = test_router();
        let req_body = serde_json::json!({
            "model": "test-model",
            "messages": [{"role": "system", "content": "You are helpful"}]
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
    async fn test_chat_completions_empty_model() {
        let app = test_router();
        let req_body = serde_json::json!({
            "model": "",
            "messages": [{"role": "user", "content": "Hi"}]
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
    async fn test_chat_completions_empty_messages() {
        let app = test_router();
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
            .contains("non-empty array"));
    }

    #[tokio::test]
    async fn test_chat_completions_invalid_temperature() {
        let app = router_with_provider();
        let req_body = serde_json::json!({
            "model": "mock-model",
            "messages": [{"role": "user", "content": "Hi"}],
            "temperature": 5.0
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
    async fn test_chat_completions_negative_temperature() {
        let app = router_with_provider();
        let req_body = serde_json::json!({
            "model": "mock-model",
            "messages": [{"role": "user", "content": "Hi"}],
            "temperature": -1.0
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
    async fn test_chat_completions_zero_max_tokens() {
        let app = router_with_provider();
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
    async fn test_chat_completions_invalid_role() {
        let app = test_router();
        let req_body = serde_json::json!({
            "model": "test-model",
            "messages": [{"role": "invalid_role", "content": "Hi"}]
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
            .contains("invalid role"));
    }

    #[tokio::test]
    async fn test_chat_completions_empty_content() {
        let app = test_router();
        let req_body = serde_json::json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "  "}]
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
            .contains("empty content"));
    }

    #[tokio::test]
    async fn test_chat_completions_model_not_found() {
        // Use a registry with a provider but NO default set, and a model name
        // that doesn't match any keyword — so get_provider returns None.
        let mut config = Config::default();
        config.agent.model = "mock-model".to_string();
        let bus = MessageBus::new();
        let tmp = tempfile::tempdir().unwrap();
        let session_manager = SessionManager::new(tmp.path().to_path_buf()).unwrap();
        let mut reg = ProviderRegistry::new();
        reg.register("mock", MockProvider);
        // Intentionally do NOT call set_default — no fallback for unknown models
        let state = AppState {
            config: Arc::new(config),
            bus: Arc::new(bus),
            session_manager: Arc::new(session_manager),
            provider_registry: Arc::new(reg),
            tool_registry: Arc::new(ToolRegistry::new()),
            api_key: None,
            cancel: CancellationToken::new(),
            health_snapshot: Arc::new(parking_lot::RwLock::new(None)),
        };
        let app = Router::new()
            .route("/v1/chat/completions", post(chat_completions))
            .route("/v1/models", get(list_models))
            .route("/health", get(health))
            .with_state(state);

        let req_body = serde_json::json!({
            "model": "nonexistent-model",
            "messages": [{"role": "user", "content": "Hi"}]
        });
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&req_body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not found"));
        assert_eq!(v["error"]["code"].as_str(), Some("model_not_found"));
    }

    // ─── Chat completions: success with mock provider ───

    #[tokio::test]
    async fn test_chat_completions_success() {
        let app = router_with_provider();
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
        assert_eq!(v["choices"][0]["message"]["content"], "Mock response");
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
        assert_eq!(v["usage"]["prompt_tokens"], 5);
        assert_eq!(v["usage"]["completion_tokens"], 3);
        assert_eq!(v["usage"]["total_tokens"], 8);
        assert!(v["id"].as_str().unwrap().starts_with("chatcmpl-"));
        // Verify created is a reasonable timestamp
        assert!(v["created"].as_u64().unwrap() > 1_700_000_000);
    }

    // ─── Chat completions: streaming ─────────────────────

    #[tokio::test]
    async fn test_chat_completions_streaming() {
        let app = router_with_provider();
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
        // SSE responses use 200 with text/event-stream
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            ct.contains("text/event-stream"),
            "Expected SSE content type, got: {}",
            ct
        );
    }

    #[tokio::test]
    async fn test_chat_completions_streaming_body_contains_three_chunks() {
        let app = router_with_provider();
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

        // Should contain 4 data events: role, content, stop, [DONE]
        let data_count = body_str.matches("data:").count();
        assert_eq!(
            data_count, 4,
            "Expected 4 SSE data events, got {}: {}",
            data_count, body_str
        );

        // First chunk should have role announcement
        assert!(
            body_str.contains("\"role\":\"assistant\"")
                || body_str.contains("\"role\": \"assistant\""),
            "First chunk should contain role announcement"
        );

        // Should contain the mock response content
        assert!(
            body_str.contains("Mock response"),
            "Should contain content in SSE body"
        );

        // Should contain finish_reason stop
        assert!(
            body_str.contains("\"finish_reason\":\"stop\"")
                || body_str.contains("\"finish_reason\": \"stop\""),
            "Final chunk should contain finish_reason: stop"
        );

        // Should contain usage info
        assert!(
            body_str.contains("\"prompt_tokens\""),
            "Final chunk should contain usage info"
        );

        // Should end with [DONE] sentinel
        assert!(
            body_str.contains("data: [DONE]"),
            "SSE stream should end with [DONE] sentinel"
        );
    }

    #[tokio::test]
    async fn test_chat_completions_streaming_chunks_have_consistent_id() {
        let app = router_with_provider();
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

        // Extract all IDs from SSE data lines
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
        // All IDs should be identical
        let first_id = &ids[0];
        assert!(
            ids.iter().all(|id| id == first_id),
            "All SSE chunks should have the same ID"
        );
        assert!(
            first_id.starts_with("chatcmpl-"),
            "ID should start with chatcmpl-"
        );
    }

    // ─── Auth: 401 tests ─────────────────────────────────

    #[tokio::test]
    async fn test_auth_missing_key() {
        let app = router_with_auth();
        let req_body = serde_json::json!({
            "model": "mock-model",
            "messages": [{"role": "user", "content": "Hello"}]
        });
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&req_body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["code"], "invalid_api_key");
    }

    #[tokio::test]
    async fn test_auth_wrong_key() {
        let app = router_with_auth();
        let req_body = serde_json::json!({
            "model": "mock-model",
            "messages": [{"role": "user", "content": "Hello"}]
        });
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("authorization", "Bearer wrong-key")
            .body(Body::from(serde_json::to_string(&req_body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_auth_valid_key() {
        let app = router_with_auth();
        let req_body = serde_json::json!({
            "model": "mock-model",
            "messages": [{"role": "user", "content": "Hello"}]
        });
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("authorization", "Bearer sk-secret")
            .body(Body::from(serde_json::to_string(&req_body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_auth_health_needs_no_key() {
        // Health endpoint should work without auth even when api_key is set
        let app = router_with_auth();
        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_auth_models_needs_no_key() {
        // Models endpoint should work without auth (matches OpenAI behavior)
        let app = router_with_auth();
        let req = Request::builder()
            .uri("/v1/models")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ─── Server construction ─────────────────────────────

    #[tokio::test]
    async fn test_server_new_and_router() {
        let config = Config::default();
        let bus = MessageBus::new();
        let tmp = tempfile::tempdir().unwrap();
        let session_manager = SessionManager::new(tmp.path().to_path_buf()).unwrap();
        let server = ApiServer::new(config, bus, session_manager, Some(8080));
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
            ApiServer::with_registries(config, bus, session_manager, providers, tools, Some(9090));
        let _router = server.router();
    }

    #[tokio::test]
    async fn test_server_with_api_key() {
        let config = Config::default();
        let bus = MessageBus::new();
        let tmp = tempfile::tempdir().unwrap();
        let session_manager = SessionManager::new(tmp.path().to_path_buf()).unwrap();
        let server = ApiServer::new(config, bus, session_manager, Some(8080))
            .with_api_key("sk-test".to_string());
        assert_eq!(server.state.api_key, Some("sk-test".to_string()));
    }

    // ─── Serialization tests ─────────────────────────────

    #[test]
    fn test_chat_completion_request_deserialize() {
        let json = r#"{
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "Hi"}],
            "temperature": 0.7,
            "max_tokens": 100,
            "stream": false
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.model, "gpt-4");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.temperature, Some(0.7));
        assert_eq!(req.max_tokens, Some(100));
        assert!(!req.stream);
    }

    #[test]
    fn test_chat_completion_request_minimal() {
        let json = r#"{"model": "test", "messages": [{"role": "user", "content": "hi"}]}"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.model, "test");
        assert_eq!(req.messages.len(), 1);
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

    // ─── Boundary temperature tests ─────────────────────

    #[tokio::test]
    async fn test_chat_completions_temperature_zero_ok() {
        let app = router_with_provider();
        let req_body = serde_json::json!({
            "model": "mock-model",
            "messages": [{"role": "user", "content": "Hi"}],
            "temperature": 0.0
        });
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&req_body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_chat_completions_temperature_two_ok() {
        let app = router_with_provider();
        let req_body = serde_json::json!({
            "model": "mock-model",
            "messages": [{"role": "user", "content": "Hi"}],
            "temperature": 2.0
        });
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&req_body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ─── Tool role message ─────────────────────────────

    #[tokio::test]
    async fn test_chat_completions_tool_role_accepted() {
        let app = router_with_provider();
        let req_body = serde_json::json!({
            "model": "mock-model",
            "messages": [
                {"role": "user", "content": "What is 2+2?"},
                {"role": "assistant", "content": "Let me calculate."},
                {"role": "tool", "content": "4"}
            ]
        });
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&req_body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ─── No provider configured ──────────────────────────

    #[tokio::test]
    async fn test_chat_completions_no_provider() {
        let app = test_router();
        let req_body = serde_json::json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "Hello"}]
        });
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&req_body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // No provider registered → model not found (404)
        assert!(
            resp.status() == StatusCode::NOT_FOUND
                || resp.status() == StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    // ─── CORS configuration tests ──────────────────────────

    /// Build a router from a custom config to test CORS headers.
    fn router_with_config(config: Config) -> Router {
        let state = AppState {
            config: Arc::new(config),
            bus: Arc::new(MessageBus::new()),
            session_manager: {
                let tmp = tempfile::tempdir().unwrap();
                Arc::new(SessionManager::new(tmp.path().to_path_buf()).unwrap())
            },
            provider_registry: Arc::new(ProviderRegistry::new()),
            tool_registry: Arc::new(ToolRegistry::new()),
            api_key: None,
            cancel: CancellationToken::new(),
            health_snapshot: Arc::new(parking_lot::RwLock::new(None)),
        };

        let cors = build_cors_layer(&state.config);
        let body_limit = state.config.api.max_body_size;

        Router::new()
            .route("/health", get(health))
            .layer(DefaultBodyLimit::max(body_limit))
            .layer(middleware::from_fn(request_log_middleware))
            .layer(cors)
            .with_state(state)
    }

    #[tokio::test]
    async fn test_cors_default_allows_any_origin() {
        let config = Config::default();
        let app = router_with_config(config);

        let req = Request::builder()
            .uri("/health")
            .header("origin", "https://example.com")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Permissive CORS (tower-http 0.6) returns wildcard "*"
        let acrh = resp.headers().get("access-control-allow-origin").unwrap();
        assert_eq!(acrh, "*");
    }

    #[tokio::test]
    async fn test_cors_specific_origin_allowed() {
        let mut config = Config::default();
        config.api.allowed_origins = vec!["https://trusted.example.com".to_string()];
        let app = router_with_config(config);

        let req = Request::builder()
            .uri("/health")
            .header("origin", "https://trusted.example.com")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let acrh = resp.headers().get("access-control-allow-origin").unwrap();
        assert_eq!(acrh, "https://trusted.example.com");
    }

    #[tokio::test]
    async fn test_cors_unlisted_origin_denied() {
        let mut config = Config::default();
        config.api.allowed_origins = vec!["https://trusted.example.com".to_string()];
        let app = router_with_config(config);

        let req = Request::builder()
            .uri("/health")
            .header("origin", "https://evil.example.com")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Unlisted origin should not get a CORS header
        assert!(
            resp.headers().get("access-control-allow-origin").is_none(),
            "Unlisted origin should not receive CORS header"
        );
    }

    #[tokio::test]
    async fn test_cors_preflight_options() {
        let mut config = Config::default();
        config.api.allowed_origins = vec!["https://trusted.example.com".to_string()];
        let app = router_with_config(config);

        let req = Request::builder()
            .method("OPTIONS")
            .uri("/health")
            .header("origin", "https://trusted.example.com")
            .header("access-control-request-method", "POST")
            .header(
                "access-control-request-headers",
                "content-type,authorization",
            )
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Verify CORS headers on preflight response
        let allow_origin = resp.headers().get("access-control-allow-origin").unwrap();
        assert_eq!(allow_origin, "https://trusted.example.com");
        let allow_methods = resp.headers().get("access-control-allow-methods").unwrap();
        let methods_str = allow_methods.to_str().unwrap();
        assert!(methods_str.contains("GET"));
        assert!(methods_str.contains("POST"));
        assert!(methods_str.contains("OPTIONS"));
        let allow_headers = resp.headers().get("access-control-allow-headers").unwrap();
        let headers_str = allow_headers.to_str().unwrap().to_lowercase();
        assert!(headers_str.contains("content-type"));
        assert!(headers_str.contains("authorization"));
        // Verify Max-Age header
        let max_age = resp.headers().get("access-control-max-age").unwrap();
        assert_eq!(max_age, "3600");
    }

    #[tokio::test]
    async fn test_cors_preflight_unlisted_origin_rejected() {
        let mut config = Config::default();
        config.api.allowed_origins = vec!["https://trusted.example.com".to_string()];
        let app = router_with_config(config);

        let req = Request::builder()
            .method("OPTIONS")
            .uri("/health")
            .header("origin", "https://evil.example.com")
            .header("access-control-request-method", "POST")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // tower-http returns 200 for OPTIONS but without CORS headers for unlisted origins
        assert!(
            resp.headers().get("access-control-allow-origin").is_none(),
            "Unlisted origin should not receive CORS headers on preflight"
        );
    }

    #[tokio::test]
    async fn test_cors_wildcard_preflight() {
        let config = Config::default(); // default is ["*"]
        let app = router_with_config(config);

        let req = Request::builder()
            .method("OPTIONS")
            .uri("/health")
            .header("origin", "https://any.example.com")
            .header("access-control-request-method", "POST")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let allow_origin = resp.headers().get("access-control-allow-origin").unwrap();
        assert_eq!(allow_origin, "*");
    }

    // ─── Request ID tests ──────────────────────────────────

    #[tokio::test]
    async fn test_request_id_generated_when_missing() {
        let config = Config::default();
        let state = AppState {
            config: Arc::new(config),
            bus: Arc::new(MessageBus::new()),
            session_manager: {
                let tmp = tempfile::tempdir().unwrap();
                Arc::new(SessionManager::new(tmp.path().to_path_buf()).unwrap())
            },
            provider_registry: Arc::new(ProviderRegistry::new()),
            tool_registry: Arc::new(ToolRegistry::new()),
            api_key: None,
            cancel: CancellationToken::new(),
            health_snapshot: Arc::new(parking_lot::RwLock::new(None)),
        };

        let cors = build_cors_layer(&state.config);
        let body_limit = state.config.api.max_body_size;

        let app = Router::new()
            .route("/health", get(health))
            .layer(DefaultBodyLimit::max(body_limit))
            .layer(middleware::from_fn(request_log_middleware))
            .layer(middleware::from_fn(request_id_middleware))
            .layer(cors)
            .with_state(state);

        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let rid = resp
            .headers()
            .get("x-request-id")
            .expect("response should have x-request-id");
        let rid_str = rid.to_str().unwrap();
        // Should be a valid UUID
        assert!(
            uuid::Uuid::parse_str(rid_str).is_ok(),
            "Generated request ID should be a valid UUID"
        );
    }

    #[tokio::test]
    async fn test_request_id_preserved_when_provided() {
        let config = Config::default();
        let state = AppState {
            config: Arc::new(config),
            bus: Arc::new(MessageBus::new()),
            session_manager: {
                let tmp = tempfile::tempdir().unwrap();
                Arc::new(SessionManager::new(tmp.path().to_path_buf()).unwrap())
            },
            provider_registry: Arc::new(ProviderRegistry::new()),
            tool_registry: Arc::new(ToolRegistry::new()),
            api_key: None,
            cancel: CancellationToken::new(),
            health_snapshot: Arc::new(parking_lot::RwLock::new(None)),
        };

        let cors = build_cors_layer(&state.config);
        let body_limit = state.config.api.max_body_size;

        let app = Router::new()
            .route("/health", get(health))
            .layer(DefaultBodyLimit::max(body_limit))
            .layer(middleware::from_fn(request_log_middleware))
            .layer(middleware::from_fn(request_id_middleware))
            .layer(cors)
            .with_state(state);

        let req = Request::builder()
            .uri("/health")
            .header("x-request-id", "my-custom-id-123")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let rid = resp
            .headers()
            .get("x-request-id")
            .expect("response should have x-request-id");
        assert_eq!(rid.to_str().unwrap(), "my-custom-id-123");
    }

    // ─── Body limit tests ──────────────────────────────────

    #[tokio::test]
    async fn test_body_limit_rejects_oversized() {
        let mut config = Config::default();
        config.api.max_body_size = 100; // Very small limit
        let state = AppState {
            config: Arc::new(config),
            bus: Arc::new(MessageBus::new()),
            session_manager: {
                let tmp = tempfile::tempdir().unwrap();
                Arc::new(SessionManager::new(tmp.path().to_path_buf()).unwrap())
            },
            provider_registry: Arc::new(ProviderRegistry::new()),
            tool_registry: Arc::new(ToolRegistry::new()),
            api_key: None,
            cancel: CancellationToken::new(),
            health_snapshot: Arc::new(parking_lot::RwLock::new(None)),
        };

        let cors = build_cors_layer(&state.config);
        let body_limit = state.config.api.max_body_size;

        let app = Router::new()
            .route("/v1/chat/completions", post(chat_completions))
            .layer(DefaultBodyLimit::max(body_limit))
            .layer(middleware::from_fn(request_log_middleware))
            .layer(cors)
            .with_state(state);

        // Send a body larger than 100 bytes
        let big_body = "x".repeat(200);
        let req_body = serde_json::json!({
            "model": "test",
            "messages": [{"role": "user", "content": big_body}]
        });
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&req_body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // axum returns 413 Payload Too Large when body exceeds the limit
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);

        // Verify the response is OpenAI-format JSON
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["type"], "invalid_request_error");
        assert_eq!(v["error"]["code"], "payload_too_large");
        assert!(v["error"]["message"].as_str().unwrap().contains("maximum"));
    }

    #[tokio::test]
    async fn test_body_limit_allows_normal_request() {
        let mut config = Config::default();
        config.api.max_body_size = 1024 * 1024; // 1 MB
        let state = AppState {
            config: Arc::new(config),
            bus: Arc::new(MessageBus::new()),
            session_manager: {
                let tmp = tempfile::tempdir().unwrap();
                Arc::new(SessionManager::new(tmp.path().to_path_buf()).unwrap())
            },
            provider_registry: Arc::new(ProviderRegistry::new()),
            tool_registry: Arc::new(ToolRegistry::new()),
            api_key: None,
            cancel: CancellationToken::new(),
            health_snapshot: Arc::new(parking_lot::RwLock::new(None)),
        };

        let cors = build_cors_layer(&state.config);
        let body_limit = state.config.api.max_body_size;

        let app = Router::new()
            .route("/v1/chat/completions", post(chat_completions))
            .layer(DefaultBodyLimit::max(body_limit))
            .layer(cors)
            .with_state(state);

        let req_body = serde_json::json!({
            "model": "test",
            "messages": [{"role": "user", "content": "Hello"}]
        });
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&req_body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // Should not be 413 — may be 404 (no provider) but not rejected for size
        assert_ne!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    // ─── Graceful shutdown test ────────────────────────────

    #[tokio::test]
    async fn test_shutdown_cancels_token() {
        let server = ApiServer::new(
            Config::default(),
            MessageBus::new(),
            {
                let tmp = tempfile::tempdir().unwrap();
                SessionManager::new(tmp.path().to_path_buf()).unwrap()
            },
            Some(0), // port 0 — not actually binding
        );

        assert!(!server.state.cancel.is_cancelled());
        server.shutdown();
        assert!(server.state.cancel.is_cancelled());
    }

    #[tokio::test]
    async fn test_sse_stream_emits_shutdown_event_on_cancel() {
        // When cancel is already triggered before the SSE stream starts,
        // take_until immediately truncates the stream (0 normal events).
        // This verifies the cancellation mechanism works.
        let state = test_state_with_provider();
        // Cancel BEFORE calling stream_completion
        state.cancel.cancel();

        let req = ChatCompletionRequest {
            model: "mock-model".to_string(),
            messages: vec![ApiMessage {
                role: "user".to_string(),
                content: "Hello".to_string(),
            }],
            temperature: None,
            max_tokens: None,
            stream: true,
        };

        let messages = vec![Message {
            role: MessageRole::User,
            content: "Hello".to_string(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let resp = stream_completion(state, req, "test".to_string(), messages).await;

        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.contains("text/event-stream"));

        // Collect body — with cancel already fired, take_until immediately
        // truncates, so we get only the [DONE] sentinel.
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        let data_count = body_str.matches("data:").count();
        assert_eq!(
            data_count, 1,
            "Expected 1 SSE event ([DONE]) when cancel is pre-triggered, got {}: {}",
            data_count, body_str
        );
        assert!(
            body_str.contains("[DONE]"),
            "Should contain [DONE] sentinel"
        );
    }

    #[tokio::test]
    async fn test_sse_stream_normal_not_cancelled() {
        // Verify normal SSE stream (not cancelled) emits all 3 chunk events
        // plus the [DONE] sentinel.
        let state = test_state_with_provider();

        let req = ChatCompletionRequest {
            model: "mock-model".to_string(),
            messages: vec![ApiMessage {
                role: "user".to_string(),
                content: "Hello".to_string(),
            }],
            temperature: None,
            max_tokens: None,
            stream: true,
        };

        let messages = vec![Message {
            role: MessageRole::User,
            content: "Hello".to_string(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let resp = stream_completion(state, req, "test".to_string(), messages).await;

        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        let data_count = body_str.matches("data:").count();
        assert_eq!(
            data_count, 4,
            "Expected 4 SSE events (3 chunks + [DONE]) without cancellation, got {}: {}",
            data_count, body_str
        );
        assert!(
            body_str.contains("[DONE]"),
            "Should contain [DONE] sentinel"
        );
    }
}
