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
use crate::stream::StreamTranslator;
use crate::translate::{build_zen_body, resolve_model};
use crate::wizard::{http_client, UPSTREAM_BASE, ZEN_USER_AGENT};

pub struct AppState {
    pub config: Config,
    pub http: reqwest::Client,
    /// Round-robin index over keys; advances on exhaustion.
    pub key_cursor: std::sync::atomic::AtomicUsize,
    /// Stable zen session per client conversation (hash of first user text).
    pub sessions: std::sync::Mutex<std::collections::HashMap<u64, String>>,
}

pub type SharedState = Arc<AppState>;

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
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

fn authorized(state: &AppState, headers: &HeaderMap) -> bool {
    let expected = &state.config.gateway_key;
    if expected.is_empty() {
        return true;
    }
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|key| constant_time_eq(key.as_bytes(), expected.as_bytes()))
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

async fn chat_completions(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: axum::extract::Json<Value>,
) -> Response {
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

    // Stable zen session per conversation for routing/cache affinity.
    let fingerprint = conversation_fingerprint(&request);
    let session = {
        let mut sessions = state.sessions.lock().unwrap();
        sessions
            .entry(fingerprint)
            .or_insert_with(ids::session_id)
            .clone()
    };

    // Upstream request with key rotation on exhaustion (402/429).
    let mut last_error = String::new();
    let mut upstream_response: Option<reqwest::Response> = None;
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
            .header("x-opencode-session", &session)
            .header("x-opencode-session-id", &session)
            .header("x-opencode-request", ids::request_id())
            .json(&zen_body)
            .send()
            .await;
        match send {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    // Advance past any keys that failed before this one.
                    state
                        .key_cursor
                        .store((index + 1) % total_keys.max(1), std::sync::atomic::Ordering::SeqCst);
                    upstream_response = Some(response);
                    break;
                }
                if status.as_u16() == 402 || status.as_u16() == 429 {
                    let body_text = response.text().await.unwrap_or_default();
                    last_error = format!("key {index} exhausted ({status}): {}", body_text.chars().take(160).collect::<String>());
                    continue;
                }
                let body_text = response.text().await.unwrap_or_default();
                return error_json(
                    StatusCode::BAD_GATEWAY,
                    &format!("zen returned {status}: {}", body_text.chars().take(400).collect::<String>()),
                );
            }
            Err(e) => {
                last_error = format!("upstream request failed: {e}");
            }
        }
    }
    let Some(response) = upstream_response else {
        return error_json(
            StatusCode::TOO_MANY_REQUESTS,
            &format!("all keys exhausted or unreachable. last error: {last_error}"),
        );
    };

    let stream_requested = request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if !stream_requested {
        // Buffer the zen stream and assemble one completion object.
        let mut translator = StreamTranslator::new(requested.to_string());
        let mut sse_buffer = String::new();
        let mut stream = response.bytes_stream();
        use futures_util::StreamExt;
        while let Some(chunk) = stream.next().await {
            let Ok(bytes) = chunk else { break };
            sse_buffer.push_str(&String::from_utf8_lossy(&bytes));
            loop {
                let Some(split) = sse_buffer.find("\n\n") else { break };
                let block: String = sse_buffer.drain(..split + 2).collect();
                let (event, data) = parse_sse_block(&block);
                translator.feed(&event, &data);
            }
        }
        return Json(translator.into_completion()).into_response();
    }

    // Streaming: translate zen SSE -> OpenAI chunks through a channel.
    let (tx, rx) = mpsc::unbounded_channel::<String>();
    let mut translator = StreamTranslator::new(requested.to_string());
    tokio::spawn(async move {
        let mut sse_buffer = String::new();
        let mut stream = response.bytes_stream();
        use futures_util::StreamExt;
        while let Some(chunk) = stream.next().await {
            let Ok(bytes) = chunk else { break };
            sse_buffer.push_str(&String::from_utf8_lossy(&bytes));
            loop {
                let Some(split) = sse_buffer.find("\n\n") else { break };
                let block: String = sse_buffer.drain(..split + 2).collect();
                let (event, data) = parse_sse_block(&block);
                for out in translator.feed(&event, &data) {
                    if tx.send(out).is_err() {
                        return;
                    }
                }
            }
        }
        // If the stream ended without a completion event, still finish cleanly.
        for out in translator.close() {
            if tx.send(out).is_err() {
                return;
            }
        }
    });

    let body = Body::from_stream(
        UnboundedReceiverStream::new(rx).map(|chunk| Ok::<String, std::convert::Infallible>(chunk)),
    );
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
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let state = Arc::new(AppState {
        http: http_client()?,
        key_cursor: std::sync::atomic::AtomicUsize::new(0),
        sessions: std::sync::Mutex::new(std::collections::HashMap::new()),
        config,
    });
    let addr = format!("{host}:{}", state.config.port);
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("zenshim listening on http://{addr}/v1");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown.changed().await;
        })
        .await?;
    Ok(())
}
