use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use futures_util::StreamExt as _;

use crate::config::Config;
use crate::ids;

use crate::stream::{ApiFormat as StreamFormat, StreamTranslator};
use crate::translate::{build_zen_body, build_zen_body_anthropic, resolve_model};
use crate::wizard::{http_client, UPSTREAM_BASE, ZEN_USER_AGENT};

pub mod format {
    pub use crate::stream::ApiFormat;

    /// Which API shapes to serve.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ServeFormats {
        OpenAi,
        Anthropic,
        Both,
    }

    impl ServeFormats {
        pub fn parse(value: &str) -> Option<Self> {
            match value {
                "openai" => Some(Self::OpenAi),
                "anthropic" => Some(Self::Anthropic),
                "both" => Some(Self::Both),
                _ => None,
            }
        }

        pub fn serves(&self, format: ApiFormat) -> bool {
            match self {
                Self::OpenAi => format == ApiFormat::OpenAi,
                Self::Anthropic => format == ApiFormat::Anthropic,
                Self::Both => true,
            }
        }
    }
}

pub struct AppState {
    pub config: Config,
    pub http: reqwest::Client,
    pub formats: format::ServeFormats,
    /// Round-robin index over keys; advances on exhaustion.
    pub key_cursor: std::sync::atomic::AtomicUsize,
    /// Stable zen session per client conversation (hash of first user text).
    pub sessions: std::sync::Mutex<std::collections::HashMap<u64, String>>,
}

pub type SharedState = Arc<AppState>;

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/messages", post(anthropic_messages))
        .route("/v1/models", get(list_models))
        .route("/health", get(|| async { "ok" }))
        .with_state(state)
}

fn error_json(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({
            "error": { "message": message, "type": "invalid_request_error", "code": null }
        })),
    )
        .into_response()
}

fn error_anthropic(status: StatusCode, err_type: &str, message: &str) -> Response {
    (
        status,
        Json(json!({
            "type": "error",
            "error": { "type": err_type, "message": message }
        })),
    )
        .into_response()
}

fn authorized(state: &AppState, headers: &HeaderMap) -> bool {
    let expected = &state.config.gateway_key;
    if expected.is_empty() {
        return true;
    }
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let api_key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok());
    bearer
        .into_iter()
        .chain(api_key)
        .any(|key| constant_time_eq(key.as_bytes(), expected.as_bytes()))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn conversation_fingerprint(request: &Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    if let Some(messages) = request.get("messages").and_then(Value::as_array) {
        for message in messages.iter().take(3) {
            message.to_string().hash(&mut hasher);
        }
    }
    hasher.finish()
}

async fn list_models(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !authorized(&state, &headers) {
        return error_json(StatusCode::UNAUTHORIZED, "invalid gateway key");
    }
    let data: Vec<Value> = state
        .config
        .models
        .iter()
        .map(|id| {
            json!({
                "id": id,
                "object": "model",
                "created": 0,
                "owned_by": "opencode-zen",
            })
        })
        .collect();
    Json(json!({ "object": "list", "data": data })).into_response()
}

/// Send the zen body upstream with key rotation on exhaustion (402/429).
/// Shared by both API shapes.
async fn dispatch_upstream(
    state: &AppState,
    zen_body: &Value,
    session: &str,
) -> Result<reqwest::Response, Response> {
    let mut last_error = String::new();
    let total_keys = state.config.keys.len();
    let start = state.key_cursor.load(std::sync::atomic::Ordering::SeqCst);
    for attempt in 0..total_keys {
        let index = (start + attempt) % total_keys;
        let key = state.config.keys[index].clone();
        let send = state
            .http
            .post(format!("{UPSTREAM_BASE}/responses"))
            .bearer_auth(&key)
            .header("User-Agent", ZEN_USER_AGENT)
            .header("x-opencode-client", "cli")
            .header("x-opencode-project", "global")
            .header("x-opencode-session", session)
            .header("x-opencode-session-id", session)
            .header("x-opencode-request", ids::request_id())
            .json(zen_body)
            .send()
            .await;
        match send {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    state
                        .key_cursor
                        .store((index + 1) % total_keys.max(1), std::sync::atomic::Ordering::SeqCst);
                    return Ok(response);
                }
                if status.as_u16() == 402 || status.as_u16() == 429 {
                    let body_text = response.text().await.unwrap_or_default();
                    last_error = format!(
                        "key {index} exhausted ({status}): {}",
                        body_text.chars().take(160).collect::<String>()
                    );
                    continue;
                }
                let body_text = response.text().await.unwrap_or_default();
                return Err(error_json(
                    StatusCode::BAD_GATEWAY,
                    &format!("zen returned {status}: {}", body_text.chars().take(400).collect::<String>()),
                ));
            }
            Err(e) => {
                last_error = format!("upstream request failed: {e}");
            }
        }
    }
    Err(error_json(
        StatusCode::TOO_MANY_REQUESTS,
        &format!("all keys exhausted or unreachable. last error: {last_error}"),
    ))
}

/// Pump the zen SSE stream through a StreamTranslator into a channel.
fn spawn_translation_task(
    response: reqwest::Response,
    mut translator: StreamTranslator,
) -> Body {
    let (tx, rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut sse_buffer = String::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let Ok(bytes) = chunk else { break };
            sse_buffer.push_str(&String::from_utf8_lossy(&bytes));
            while let Some(split) = sse_buffer.find("\n\n") {
                let block: String = sse_buffer.drain(..split + 2).collect();
                let (event, data) = parse_sse_block(&block);
                for out in translator.feed(&event, &data) {
                    if tx.send(out).is_err() {
                        return;
                    }
                }
            }
        }
        for out in translator.close() {
            if tx.send(out).is_err() {
                return;
            }
        }
    });
    Body::from_stream(
        UnboundedReceiverStream::new(rx).map(|chunk| Ok::<String, std::convert::Infallible>(chunk)),
    )
}

async fn chat_completions(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: axum::extract::Json<Value>,
) -> Response {
    if !state.formats.serves(StreamFormat::OpenAi) {
        return error_json(StatusCode::NOT_FOUND, "openai format not served (see --format)");
    }
    if !authorized(&state, &headers) {
        return error_json(StatusCode::UNAUTHORIZED, "invalid gateway key");
    }
    let request = body.0;

    let requested = request.get("model").and_then(Value::as_str).unwrap_or_default();
    let Some(zen_model) = resolve_model(requested, &state.config.models) else {
        return error_json(
            StatusCode::NOT_FOUND,
            &format!(
                "model '{requested}' is not served. Available: {}",
                state.config.models.join(", ")
            ),
        );
    };

    let zen_body = build_zen_body(&request, &zen_model);
    let fingerprint = conversation_fingerprint(&request);
    let session = {
        let mut sessions = state.sessions.lock().unwrap();
        sessions.entry(fingerprint).or_insert_with(ids::session_id).clone()
    };

    let upstream = match dispatch_upstream(&state, &zen_body, &session).await {
        Ok(response) => response,
        Err(error) => return error,
    };

    let stream_requested = request.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let translator = StreamTranslator::new(StreamFormat::OpenAi, requested.to_string());

    if !stream_requested {
        let mut translator = translator;
        let mut sse_buffer = String::new();
        let mut stream = upstream.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let Ok(bytes) = chunk else { break };
            sse_buffer.push_str(&String::from_utf8_lossy(&bytes));
            while let Some(split) = sse_buffer.find("\n\n") {
                let block: String = sse_buffer.drain(..split + 2).collect();
                let (event, data) = parse_sse_block(&block);
                translator.feed(&event, &data);
            }
        }
        return Json(translator.into_completion()).into_response();
    }

    let body = spawn_translation_task(upstream, translator);
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(body)
        .unwrap()
}

async fn anthropic_messages(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: axum::extract::Json<Value>,
) -> Response {
    if !state.formats.serves(StreamFormat::Anthropic) {
        return error_anthropic(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "anthropic format not served (see --format)",
        );
    }
    if !authorized(&state, &headers) {
        return error_anthropic(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid gateway key",
        );
    }
    let request = body.0;

    let requested = request.get("model").and_then(Value::as_str).unwrap_or_default();
    let Some(zen_model) = resolve_model(requested, &state.config.models) else {
        return error_anthropic(
            StatusCode::NOT_FOUND,
            "not_found_error",
            &format!(
                "model: '{}' is not served. Available: {}",
                state.config.models.join(", "),
                requested
            ),
        );
    };

    let zen_body = build_zen_body_anthropic(&request, &zen_model);
    let fingerprint = conversation_fingerprint(&request);
    let session = {
        let mut sessions = state.sessions.lock().unwrap();
        sessions.entry(fingerprint).or_insert_with(ids::session_id).clone()
    };

    let upstream = match dispatch_upstream(&state, &zen_body, &session).await {
        Ok(response) => response,
        Err(error) => return error,
    };

    let stream_requested = request.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let translator = StreamTranslator::new(StreamFormat::Anthropic, requested.to_string());

    if !stream_requested {
        let mut translator = translator;
        let mut sse_buffer = String::new();
        let mut stream = upstream.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let Ok(bytes) = chunk else { break };
            sse_buffer.push_str(&String::from_utf8_lossy(&bytes));
            while let Some(split) = sse_buffer.find("\n\n") {
                let block: String = sse_buffer.drain(..split + 2).collect();
                let (event, data) = parse_sse_block(&block);
                translator.feed(&event, &data);
            }
        }
        return Json(translator.into_completion()).into_response();
    }

    let body = spawn_translation_task(upstream, translator);
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(body)
        .unwrap()
}

fn parse_sse_block(block: &str) -> (String, String) {
    let mut event = String::new();
    let mut data = String::new();
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            data.push_str(rest.trim_start());
        }
    }
    (event, data)
}

pub async fn serve(
    config: Config,
    host: &str,
    formats: format::ServeFormats,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let state = Arc::new(AppState {
        http: http_client()?,
        formats,
        key_cursor: std::sync::atomic::AtomicUsize::new(0),
        sessions: std::sync::Mutex::new(std::collections::HashMap::new()),
        config,
    });
    let addr = format!("{host}:{}", state.config.port);
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("zenshim listening on http://{addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown.changed().await;
        })
        .await?;
    Ok(())
}
