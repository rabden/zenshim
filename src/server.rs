use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use axum::{
    body::Body,
    extract::{rejection::JsonRejection, ConnectInfo, DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::{Stream, StreamExt as _};
use serde_json::{json, Value};
use tokio::sync::{mpsc, OwnedSemaphorePermit};
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::bridge::{BridgePool, WireEvent};
use crate::config::Config;
use crate::hub::{self, Outcome, RequestInfo, Tracker};
use crate::ids;
use crate::keys::{Backend, Failure, KeyPool, Route};
use crate::limits::{Busy, Limits, RateLimiter, Slots};
use crate::responses::{self, Prepared, ResponseStore};
use crate::stream::{ApiFormat as StreamFormat, StreamTranslator};
use crate::translate::{
    bridge_prompt, build_zen_body_anthropic_for, build_zen_body_for, guess_upstream, learn_upstream,
    learned_upstream, resolve_model, Upstream,
};
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
                // Chat Completions and Responses are both the OpenAI dialect.
                Self::OpenAi => format != ApiFormat::Anthropic,
                Self::Anthropic => format == ApiFormat::Anthropic,
                Self::Both => true,
            }
        }
    }
}

/// Conversations remembered for stable upstream session ids.
const MAX_SESSIONS: usize = 4096;
/// How long upstream may take to start answering a direct request.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

/// Upstream events, whichever route they came by.
type EventStream = Pin<Box<dyn Stream<Item = WireEvent> + Send>>;

/// Stable zen session per client conversation, bounded: the oldest
/// conversation is forgotten first.
#[derive(Default)]
struct SessionCache {
    ids: HashMap<u64, String>,
    order: VecDeque<u64>,
}

impl SessionCache {
    fn get_or_create(&mut self, fingerprint: u64, create: impl FnOnce() -> String) -> String {
        if let Some(id) = self.ids.get(&fingerprint) {
            return id.clone();
        }
        if self.order.len() >= MAX_SESSIONS
            && let Some(oldest) = self.order.pop_front()
        {
            self.ids.remove(&oldest);
        }
        let id = create();
        self.ids.insert(fingerprint, id.clone());
        self.order.push_back(fingerprint);
        id
    }
}

/// What `--backend`, `--opencode` and the limit flags decide.
pub struct Options {
    pub backend: Backend,
    /// The OpenCode CLI to fall back to, if one was found.
    pub opencode: Option<PathBuf>,
    pub limits: Limits,
    /// Where key cooldowns are remembered; `None` keeps them in memory.
    pub state_path: Option<PathBuf>,
}

pub struct AppState {
    pub http: reqwest::Client,
    pub formats: format::ServeFormats,
    pub host: String,
    pub port: u16,
    /// Model ids served; editable while running.
    pub models: RwLock<Vec<String>>,
    /// Upstream keys and the policy for choosing between them.
    pub pool: KeyPool,
    /// The CLI route's backends.
    pub bridge: Arc<BridgePool>,
    /// Key clients must present to this gateway.
    pub gateway_key: RwLock<String>,
    pub limits: Limits,
    rate: RateLimiter,
    slots: Slots,
    upstream_base: String,
    sessions: Mutex<SessionCache>,
    /// Finished Responses-API answers, for `previous_response_id`.
    responses: ResponseStore,
}

pub type SharedState = Arc<AppState>;

impl AppState {
    pub fn new(
        config: Config,
        host: &str,
        formats: format::ServeFormats,
        options: Options,
    ) -> anyhow::Result<SharedState> {
        let bridge = BridgePool::new(options.opencode);
        let pool = KeyPool::new(config.keys, options.backend, options.state_path);
        pool.set_cli_ready(bridge.usable());
        Ok(Arc::new(AppState {
            http: http_client()?,
            formats,
            host: host.to_string(),
            port: config.port,
            models: RwLock::new(config.models),
            pool,
            bridge,
            gateway_key: RwLock::new(config.gateway_key),
            rate: RateLimiter::new(options.limits.rate_per_minute),
            slots: Slots::new(&options.limits),
            limits: options.limits,
            // Overridable so the failover paths can be exercised against a stand-in.
            upstream_base: std::env::var("ZENSHIM_UPSTREAM_BASE")
                .unwrap_or_else(|_| UPSTREAM_BASE.to_string()),
            sessions: Mutex::new(SessionCache::default()),
            responses: ResponseStore::default(),
        }))
    }

    pub fn models(&self) -> Vec<String> {
        self.models.read().unwrap().clone()
    }

    pub fn gateway_key(&self) -> String {
        self.gateway_key.read().unwrap().clone()
    }

    pub fn config(&self) -> Config {
        Config {
            keys: self.pool.values(),
            models: self.models(),
            port: self.port,
            gateway_key: self.gateway_key(),
        }
    }

    /// Write a runtime edit back to the config file. `apply` copies the
    /// edited field from the live config onto the saved one, so one-off
    /// flag overrides of other fields are not persisted along with it.
    pub fn persist(&self, apply: impl FnOnce(&mut Config, Config)) -> anyhow::Result<()> {
        let live = self.config();
        let mut saved = Config::load().unwrap_or_else(|| live.clone());
        apply(&mut saved, live);
        saved.save()
    }
}

pub fn router(state: SharedState) -> Router {
    let body_limit = state.limits.max_body_bytes;
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/messages", post(anthropic_messages))
        .route("/v1/responses", post(openai_responses))
        .route("/v1/responses/{id}", get(get_response).delete(delete_response))
        .route("/v1/models", get(list_models))
        .route("/metrics", get(metrics))
        .route("/health", get(|| async { "ok" }))
        .route("/ready", get(|| async { "ready" }))
        .layer(DefaultBodyLimit::max(body_limit))
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

fn anthropic_error_type(status: StatusCode) -> &'static str {
    match status.as_u16() {
        401 => "authentication_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        _ => "api_error",
    }
}

/// An error in the shape the dialect's clients expect.
fn reject(format: StreamFormat, status: StatusCode, message: &str) -> Response {
    match format {
        StreamFormat::OpenAi | StreamFormat::Responses => error_json(status, message),
        StreamFormat::Anthropic => error_anthropic(status, anthropic_error_type(status), message),
    }
}

/// Add `Retry-After` so well-behaved clients back off by themselves.
fn with_retry_after(mut response: Response, wait: Option<Duration>) -> Response {
    if let Some(wait) = wait
        && let Ok(value) = wait.as_secs().max(1).to_string().parse()
    {
        response.headers_mut().insert("retry-after", value);
    }
    response
}

/// Who is calling. A proxy on this machine (the tunnel, a reverse proxy)
/// is believed about the address it forwards for; anyone else is not, or
/// a remote client could pick its own identity.
fn client_of(peer: SocketAddr, headers: &HeaderMap) -> (IpAddr, bool) {
    if !peer.ip().is_loopback() {
        return (peer.ip(), false);
    }
    let forwarded = ["cf-connecting-ip", "x-forwarded-for"]
        .iter()
        .find_map(|name| headers.get(*name).and_then(|v| v.to_str().ok()))
        .and_then(|v| v.split(',').next())
        .and_then(|v| v.trim().parse::<IpAddr>().ok());
    match forwarded {
        Some(remote) => (remote, false),
        None => (peer.ip(), true),
    }
}

fn authorized(state: &AppState, headers: &HeaderMap) -> bool {
    let expected = &state.gateway_key();
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
        .models()
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

/// Split complete SSE blocks off the front of `buffer`. Works on bytes
/// so a multi-byte character cut by a chunk boundary survives.
fn drain_sse_blocks(buffer: &mut Vec<u8>) -> Vec<WireEvent> {
    let mut events = Vec::new();
    while let Some(split) = buffer.windows(2).position(|pair| pair == b"\n\n") {
        let block: Vec<u8> = buffer.drain(..split + 2).collect();
        events.push(parse_sse_block(&String::from_utf8_lossy(&block)));
    }
    events
}

fn parse_sse_block(block: &str) -> WireEvent {
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

/// The direct wire's SSE body as a stream of events.
fn sse_events(response: reqwest::Response) -> EventStream {
    let chunks = Box::pin(response.bytes_stream());
    let state = (chunks, Vec::<u8>::new(), VecDeque::<WireEvent>::new());
    Box::pin(futures_util::stream::unfold(state, |(mut chunks, mut buffer, mut ready)| async move {
        loop {
            if let Some(event) = ready.pop_front() {
                return Some((event, (chunks, buffer, ready)));
            }
            let bytes = chunks.next().await?.ok()?;
            buffer.extend_from_slice(&bytes);
            ready.extend(drain_sse_blocks(&mut buffer));
        }
    }))
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let seconds: u64 = headers.get("retry-after")?.to_str().ok()?.trim().parse().ok()?;
    // Longer than a day is a quota, which the scheduler already assumes.
    (1..=24 * 3600).contains(&seconds).then(|| Duration::from_secs(seconds))
}

/// One attempt over zenshim's own imitation of the OpenCode wire.
async fn direct(
    state: &AppState,
    key: &str,
    zen_body: &Value,
    session: &str,
    path: &str,
) -> Result<EventStream, Failure> {
    let send = state
        .http
        .post(format!("{}{}", state.upstream_base, path))
        .bearer_auth(key)
        .header("User-Agent", ZEN_USER_AGENT)
        .header("x-opencode-client", "cli")
        .header("x-opencode-project", "global")
        .header("x-opencode-session", session)
        .header("x-opencode-session-id", session)
        .header("x-opencode-request", ids::request_id())
        .json(zen_body)
        .send();
    let response = match tokio::time::timeout(CONNECT_TIMEOUT, send).await {
        Ok(Ok(response)) => response,
        Ok(Err(e)) => return Err(Failure::Upstream(format!("upstream request failed: {e}"))),
        Err(_) => return Err(Failure::Upstream("upstream did not answer in time".to_string())),
    };
    let status = response.status();
    if status.is_success() {
        return Ok(sse_events(response));
    }
    let wait = retry_after(response.headers());
    let body: String = response.text().await.unwrap_or_default().chars().take(400).collect();
    let message = format!("zen returned {status}: {body}");
    Err(match status.as_u16() {
        402 | 429 => Failure::Exhausted { message, retry_after: wait },
        403 => Failure::Gated(message),
        401 => Failure::Auth(message),
        code @ 400..=499 => Failure::Rejected { status: code, message },
        _ => Failure::Upstream(message),
    })
}

/// What the client is told when no route produced an answer.
struct Refusal {
    status: StatusCode,
    message: String,
    retry_after: Option<Duration>,
    /// The upstream said the model lives on the other endpoint; the
    /// caller should retry once with the protocol flipped.
    wrong_protocol: bool,
}

/// Get an answer started, working through the scheduler's plan: each key
/// over the direct wire, then through the CLI, then the next key.
async fn dispatch_upstream(
    state: &AppState,
    zen_body: &Value,
    zen_model: &str,
    session: &str,
    tracker: &Tracker,
    upstream: Upstream,
) -> Result<EventStream, Refusal> {
    let plan = state.pool.plan(SystemTime::now());
    if plan.is_empty() {
        let wait = state.pool.next_available(SystemTime::now());
        return Err(Refusal {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: "all upstream keys are exhausted; they are retried as their quotas reset".to_string(),
            retry_after: wait.map(|w| w.min(Duration::from_secs(3600))),
            wrong_protocol: false,
        });
    }
    let mut last: Option<Failure> = None;
    // A key the upstream refused outright is not worth its other route.
    let mut refused_key: Option<String> = None;
    for attempt in plan {
        if refused_key.as_deref() == Some(attempt.key.as_str()) {
            continue;
        }
        let result = match attempt.route {
            Route::Direct => direct(state, &attempt.key, zen_body, session, upstream.path()).await,
            Route::Cli => state
                .bridge
                .generate(&attempt.key, zen_model, bridge_prompt(zen_body), state.limits.request_timeout)
                .await
                .map(|events| Box::pin(UnboundedReceiverStream::new(events)) as EventStream),
        };
        let failure = match result {
            Ok(events) => {
                state.pool.succeeded(&attempt);
                tracker.route(attempt.index, attempt.route);
                return Ok(events);
            }
            Err(failure) => failure,
        };
        state.pool.failed(&attempt, &failure, SystemTime::now());
        match &failure {
            Failure::Rejected { status, message } => {
                // The request itself is the problem; say so as upstream did.
                let status = match status {
                    404 => StatusCode::NOT_FOUND,
                    413 => StatusCode::PAYLOAD_TOO_LARGE,
                    _ => StatusCode::BAD_REQUEST,
                };
                // A protocol mismatch is retryable on the other endpoint.
                let wrong_protocol = status.as_u16() == 400
                    && (message.contains("ModelProtocolUnsupported")
                        || message.contains("Request validation failed"));
                return Err(Refusal {
                    status,
                    message: message.clone(),
                    retry_after: None,
                    wrong_protocol,
                });
            }
            Failure::Auth(_) => refused_key = Some(attempt.key.clone()),
            Failure::Unavailable(_) if !state.bridge.usable() => state.pool.set_cli_ready(false),
            _ => {}
        }
        last = Some(failure);
    }
    Err(match last {
        Some(Failure::Exhausted { message, .. }) => Refusal {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: format!("all upstream keys are exhausted. last error: {message}"),
            retry_after: Some(Duration::from_secs(60)),
            wrong_protocol: false,
        },
        Some(failure) => Refusal {
            status: StatusCode::BAD_GATEWAY,
            message: failure.message().to_string(),
            retry_after: None,
            wrong_protocol: false,
        },
        None => Refusal {
            status: StatusCode::BAD_GATEWAY,
            message: "no upstream route is available".to_string(),
            retry_after: None,
            wrong_protocol: false,
        },
    })
}

/// What the translator saw, as the activity log records it.
fn outcome_of(translator: &StreamTranslator, ending: Ending) -> Outcome {
    let (reason, usage, tool_calls) = translator.outcome();
    Outcome {
        status: 200,
        note: match ending {
            Ending::Completed => reason.to_string(),
            Ending::Cut => "upstream stream ended early".to_string(),
            Ending::TimedOut => "timed out".to_string(),
        },
        usage,
        tool_calls,
    }
}

/// How an upstream answer stopped.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// Upstream said it was done.
    Completed,
    /// The stream ended without a completion event.
    Cut,
    /// Silent for too long, or the whole request took too long.
    TimedOut,
}

/// Feed upstream events through the translator until the answer ends,
/// handing each rendered chunk to `emit` (which returns false once the
/// client is gone).
async fn pump(
    mut events: EventStream,
    translator: &mut StreamTranslator,
    limits: &Limits,
    started: Instant,
    mut emit: impl FnMut(String) -> bool,
) -> Option<Ending> {
    let deadline = tokio::time::Instant::from_std(started + limits.request_timeout);
    loop {
        let idle = tokio::time::Instant::now() + limits.idle_timeout;
        let next = tokio::time::timeout_at(deadline.min(idle), events.next()).await;
        let (event, data) = match next {
            Ok(Some(event)) => event,
            Ok(None) => break,
            Err(_) => return Some(Ending::TimedOut),
        };
        for out in translator.feed(&event, &data) {
            if !emit(out) {
                return None;
            }
        }
        if translator.is_finished() {
            return Some(Ending::Completed);
        }
    }
    Some(if translator.is_finished() { Ending::Completed } else { Ending::Cut })
}

async fn chat_completions(
    State(state): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    handle(state, peer, headers, body, StreamFormat::OpenAi).await
}

async fn openai_responses(
    State(state): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    handle(state, peer, headers, body, StreamFormat::Responses).await
}

async fn get_response(State(state): State<SharedState>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if !authorized(&state, &headers) {
        return error_json(StatusCode::UNAUTHORIZED, "invalid gateway key");
    }
    match state.responses.get(&id) {
        Some(response) => Json(response).into_response(),
        None => error_json(StatusCode::NOT_FOUND, &format!("response '{id}' was not found")),
    }
}

async fn delete_response(State(state): State<SharedState>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if !authorized(&state, &headers) {
        return error_json(StatusCode::UNAUTHORIZED, "invalid gateway key");
    }
    match state.responses.delete(&id) {
        true => Json(json!({ "id": id, "object": "response", "deleted": true })).into_response(),
        false => error_json(StatusCode::NOT_FOUND, &format!("response '{id}' was not found")),
    }
}

/// Counters for dashboards and alerts: JSON, or Prometheus text with
/// `?format=prometheus` or an `Accept: text/plain` scrape.
async fn metrics(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if !authorized(&state, &headers) {
        return error_json(StatusCode::UNAUTHORIZED, "invalid gateway key");
    }
    let hub = hub::hub();
    let totals = hub.totals();
    let now = SystemTime::now();
    let keys = state.pool.snapshot();
    let resting = |route: &crate::keys::RouteState| route.cooling(now).map_or(0, |left| left.as_secs());
    let blocked = match state.pool.gate(now) {
        crate::keys::GateState::Blocked(left) => left.as_secs(),
        crate::keys::GateState::Open => 0,
    };
    let active = totals.requests - totals.completed;
    let wants_text = query.get("format").is_some_and(|f| f == "prometheus")
        || headers
            .get("accept")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|accept| accept.contains("text/plain") && !accept.contains("json"));
    if !wants_text {
        let keys: Vec<Value> = keys
            .iter()
            .enumerate()
            .map(|(i, key)| {
                json!({
                    "key": i + 1,
                    "direct": { "served": key.direct.served, "resting_seconds": resting(&key.direct) },
                    "cli": { "served": key.cli.served, "resting_seconds": resting(&key.cli) },
                    "limited": key.limited,
                    "failed": key.failed,
                })
            })
            .collect();
        return Json(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_seconds": hub.uptime().as_secs(),
            "requests": { "total": totals.requests, "active": active, "errors": totals.errors },
            "tokens": { "input": totals.tokens_in, "output": totals.tokens_out },
            "average_seconds": if totals.ok > 0 { totals.ok_millis as f64 / totals.ok as f64 / 1000.0 } else { 0.0 },
            "backend": {
                "cli_available": state.pool.cli_usable(),
                "opencode_version": state.bridge.version(),
                "direct_blocked_seconds": blocked,
            },
            "keys": keys,
        }))
        .into_response();
    }
    let mut text = String::new();
    let mut metric = |name: &str, kind: &str, help: &str, samples: Vec<(String, u64)>| {
        text.push_str(&format!("# HELP zenshim_{name} {help}\n# TYPE zenshim_{name} {kind}\n"));
        for (labels, value) in samples {
            text.push_str(&format!("zenshim_{name}{labels} {value}\n"));
        }
    };
    let plain = |value: u64| vec![(String::new(), value)];
    metric("uptime_seconds", "gauge", "Seconds since the gateway started.", plain(hub.uptime().as_secs()));
    metric("requests_total", "counter", "Requests received.", plain(totals.requests));
    metric("requests_failed_total", "counter", "Requests answered with an error.", plain(totals.errors));
    metric("requests_active", "gauge", "Requests in flight.", plain(active));
    metric(
        "tokens_total",
        "counter",
        "Tokens reported by upstream.",
        vec![("{direction=\"input\"}".to_string(), totals.tokens_in), ("{direction=\"output\"}".to_string(), totals.tokens_out)],
    );
    metric("request_seconds_sum_ms", "counter", "Milliseconds spent on successful requests.", plain(totals.ok_millis));
    metric("requests_succeeded_total", "counter", "Requests answered successfully.", plain(totals.ok));
    metric("direct_blocked_seconds", "gauge", "Seconds until the direct wire is retried (0 = in use).", plain(blocked));
    let per_route = |pick: &dyn Fn(&crate::keys::RouteState) -> u64| -> Vec<(String, u64)> {
        keys.iter()
            .enumerate()
            .flat_map(|(i, key)| {
                [("direct", &key.direct), ("cli", &key.cli)]
                    .map(|(route, state)| (format!("{{key=\"{}\",route=\"{route}\"}}", i + 1), pick(state)))
            })
            .collect()
    };
    metric("key_served_total", "counter", "Requests served per upstream key and route.", per_route(&|route| route.served));
    metric("key_resting_seconds", "gauge", "Seconds an exhausted key route still rests.", per_route(&resting));
    let per_key = |pick: &dyn Fn(&crate::keys::UpstreamKey) -> u64| -> Vec<(String, u64)> {
        keys.iter().enumerate().map(|(i, key)| (format!("{{key=\"{}\"}}", i + 1), pick(key))).collect()
    };
    metric("key_limited_total", "counter", "Quota or rate-limit responses per upstream key.", per_key(&|key| key.limited));
    metric("key_failed_total", "counter", "Other failures per upstream key.", per_key(&|key| key.failed));
    ([("content-type", "text/plain; version=0.0.4; charset=utf-8")], text).into_response()
}

async fn anthropic_messages(
    State(state): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    handle(state, peer, headers, body, StreamFormat::Anthropic).await
}

/// The request path both dialects share; `format` picks the body
/// translation and the response/error shapes.
async fn handle(
    state: SharedState,
    peer: SocketAddr,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
    format: StreamFormat,
) -> Response {
    let started = Instant::now();
    let (client, local) = client_of(peer, &headers);
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => {
            // Too large, or not JSON: answer in the dialect's error shape.
            return reject(format, rejection.status(), &rejection.body_text());
        }
    };
    let requested = request.get("model").and_then(Value::as_str).unwrap_or_default();
    let stream_requested = request.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let count = |field: &str| request.get(field).and_then(Value::as_array).map_or(0, Vec::len);
    let tracker = hub::hub().begin(RequestInfo {
        format,
        model: requested.to_string(),
        client: if local { "local".to_string() } else { client.to_string() },
        stream: stream_requested,
        messages: match format {
            StreamFormat::Responses => count("input").max(1),
            _ => count("messages"),
        },
        tools: count("tools"),
    });
    let fail = |tracker: Tracker, status: StatusCode, message: &str, wait: Option<Duration>| {
        tracker.finish(Outcome::error(status.as_u16(), message));
        with_retry_after(reject(format, status, message), wait)
    };

    if !state.formats.serves(format) {
        let message = format!("{} format not served (see --format)", format.label());
        return fail(tracker, StatusCode::NOT_FOUND, &message, None);
    }
    // Before the key check, so guessing keys is rate limited too.
    if !local
        && let Err(wait) = state.rate.check(client, started)
    {
        let message = format!(
            "rate limit of {} requests per minute reached; retry shortly",
            state.limits.rate_per_minute
        );
        return fail(tracker, StatusCode::TOO_MANY_REQUESTS, &message, Some(wait));
    }
    if !authorized(&state, &headers) {
        return fail(tracker, StatusCode::UNAUTHORIZED, "invalid gateway key", None);
    }

    // A Responses request is converted to the chat shape first.
    let prepared: Option<Prepared> = match format {
        StreamFormat::Responses => match responses::prepare(&state.responses, &request) {
            Ok(prepared) => Some(prepared),
            Err(message) => return fail(tracker, StatusCode::BAD_REQUEST, &message, None),
        },
        _ => None,
    };
    let chat = prepared.as_ref().map_or(&request, |prepared| &prepared.chat);

    let models = state.models();
    let Some(zen_model) = resolve_model(requested, &models) else {
        let message = format!(
            "model '{requested}' is not served. Available: {}",
            models.join(", ")
        );
        return fail(tracker, StatusCode::NOT_FOUND, &message, None);
    };

    // Held until the answer has been fully delivered.
    let slot = match state.slots.acquire().await {
        Ok(slot) => slot,
        Err(busy) => {
            let message = match busy {
                Busy::QueueFull => "the gateway is at capacity; retry shortly",
                Busy::TimedOut => "the gateway stayed at capacity while this request waited; retry shortly",
            };
            return fail(tracker, StatusCode::TOO_MANY_REQUESTS, message, Some(Duration::from_secs(5)));
        }
    };

    let fingerprint = conversation_fingerprint(chat);
    let session = state.sessions.lock().unwrap().get_or_create(fingerprint, ids::session_id);

    // Which zen endpoint serves this model: learned from a live request
    // if we have seen it, otherwise a guess (muse = Responses, other
    // frees = chat) — corrected after one mismatch round-trip.
    let mut upstream = learned_upstream(&zen_model).unwrap_or_else(|| guess_upstream(&zen_model));
    let (events, served_on) = loop {
        let zen_body = match format {
            StreamFormat::OpenAi | StreamFormat::Responses => build_zen_body_for(chat, &zen_model, upstream),
            StreamFormat::Anthropic => build_zen_body_anthropic_for(chat, &zen_model, upstream),
        };
        match dispatch_upstream(&state, &zen_body, &zen_model, &session, &tracker, upstream).await {
            Ok(events) => break (events, upstream),
            Err(refusal) => {
                if refusal.wrong_protocol {
                    learn_upstream(&zen_model, upstream.flip());
                    upstream = upstream.flip();
                    continue;
                }
                return fail(tracker, refusal.status, &refusal.message, refusal.retry_after);
            }
        }
    };

    let mut translator = StreamTranslator::new(format, requested.to_string());
    translator.set_upstream(served_on);
    if let Some(prepared) = &prepared {
        translator.echo(prepared.echo.clone());
    }

    if !stream_requested {
        let ending = pump(events, &mut translator, &state.limits, started, |_| true).await;
        let ending = ending.unwrap_or(Ending::Cut);
        translator.close();
        if ending == Ending::TimedOut {
            return fail(tracker, StatusCode::GATEWAY_TIMEOUT, "the model took too long to answer", None);
        }
        tracker.finish(outcome_of(&translator, ending));
        drop(slot);
        remember(&state, prepared, &mut translator);
        return Json(translator.into_completion()).into_response();
    }

    let keep = prepared.map(|prepared| (Arc::clone(&state), prepared));
    let body = spawn_translation_task(events, translator, tracker, slot, state.limits.clone(), started, keep);
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("x-accel-buffering", "no")
        .body(body)
        .unwrap()
}

/// Keep a finished Responses answer so a later request can continue from
/// it or read it back.
fn remember(state: &AppState, prepared: Option<Prepared>, translator: &mut StreamTranslator) {
    let Some(prepared) = prepared.filter(|prepared| prepared.store) else { return };
    let response = translator.completion();
    let mut conversation = prepared.conversation;
    conversation.extend(responses::output_messages(&response));
    state.responses.put(response, conversation);
}

/// Pump upstream events through the translator into the response body.
fn spawn_translation_task(
    events: EventStream,
    mut translator: StreamTranslator,
    tracker: Tracker,
    slot: OwnedSemaphorePermit,
    limits: Limits,
    started: Instant,
    keep: Option<(SharedState, Prepared)>,
) -> Body {
    let (tx, rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let _slot = slot;
        let ending = pump(events, &mut translator, &limits, started, |out| tx.send(out).is_ok()).await;
        // `None`: the client went away; dropping the tracker records it.
        let Some(ending) = ending else { return };
        for out in translator.close() {
            if tx.send(out).is_err() {
                return;
            }
        }
        tracker.finish(outcome_of(&translator, ending));
        if let Some((state, prepared)) = keep {
            remember(&state, Some(prepared), &mut translator);
        }
    });
    Body::from_stream(
        UnboundedReceiverStream::new(rx).map(Ok::<String, std::convert::Infallible>),
    )
}

pub async fn bind(host: &str, port: u16) -> anyhow::Result<tokio::net::TcpListener> {
    let addr = format!("{host}:{port}");
    tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| anyhow::anyhow!("cannot listen on {addr}: {e}"))
}

pub async fn serve(
    listener: tokio::net::TcpListener,
    state: SharedState,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    hub::info(format!("zenshim listening on http://{}:{}", state.host, state.port));
    // Idle CLI backends are stopped; they restart on demand.
    let reaper = tokio::spawn({
        let bridge = Arc::clone(&state.bridge);
        async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                bridge.reap_idle().await;
            }
        }
    });
    let bridge = Arc::clone(&state.bridge);
    let served = axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = shutdown.wait_for(|stop| *stop).await;
    })
    .await;
    reaper.abort();
    bridge.shutdown().await;
    Ok(served?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::EXHAUSTED_FOR;

    const ANSWER: &str = "event: response.output_text.delta\ndata: {\"delta\":\"po\"}\n\n\
        event: response.output_text.delta\ndata: {\"delta\":\"ng \u{2713}\"}\n\n\
        event: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n";

    const CHAT_ANSWER: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"po\"}}]}\n\n\
        data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ng \u{2713}\"}}]}\n\n\
        data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
        data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\n\
        data: [DONE]\n\n";

    /// A stand-in for Zen: answers per upstream key as `behave` says.
    async fn fake_zen(behave: fn(&str) -> (u16, &'static str)) -> String {
        let app = Router::new()
            .route(
                "/responses",
                post(move |headers: HeaderMap| async move {
                    let key = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.strip_prefix("Bearer "))
                        .unwrap_or_default();
                    let (status, body) = behave(key);
                    let mut response = (StatusCode::from_u16(status).unwrap(), body).into_response();
                    if status == 200 {
                        response.headers_mut().insert("content-type", "text/event-stream".parse().unwrap());
                    }
                    response
                }),
            )
            .route(
                "/chat/completions",
                post(move |headers: HeaderMap| async move {
                    let key = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.strip_prefix("Bearer "))
                        .unwrap_or_default();
                    let (status, body) = behave(key);
                    let body = if status == 200 { CHAT_ANSWER } else { body };
                    let mut response = (StatusCode::from_u16(status).unwrap(), body).into_response();
                    if status == 200 {
                        response.headers_mut().insert("content-type", "text/event-stream".parse().unwrap());
                    }
                    response
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}")
    }

    /// A gateway on a free port, talking to `upstream` over the direct wire.
    async fn gateway(upstream: String, keys: &[&str], limits: Limits) -> (String, SharedState) {
        let pool = KeyPool::new(keys.iter().map(|k| k.to_string()).collect(), Backend::Direct, None);
        let state = Arc::new(AppState {
            http: reqwest::Client::new(),
            formats: format::ServeFormats::Both,
            host: "127.0.0.1".to_string(),
            port: 0,
            models: RwLock::new(vec!["test-model-free".to_string()]),
            pool,
            bridge: BridgePool::new(None),
            gateway_key: RwLock::new("gateway-secret".to_string()),
            rate: RateLimiter::new(limits.rate_per_minute),
            slots: Slots::new(&limits),
            limits,
            upstream_base: upstream,
            sessions: Mutex::new(SessionCache::default()),
            responses: ResponseStore::default(),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = router(state.clone()).into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}"), state)
    }

    /// Like `gateway` but with a chosen served model (protocol guessing
    /// is model-dependent).
    async fn gateway_model(upstream: String, keys: &[&str], model: &str) -> String {
        let pool = KeyPool::new(keys.iter().map(|k| k.to_string()).collect(), Backend::Direct, None);
        let state = Arc::new(AppState {
            http: reqwest::Client::new(),
            formats: format::ServeFormats::Both,
            host: "127.0.0.1".to_string(),
            port: 0,
            models: RwLock::new(vec![model.to_string()]),
            pool,
            bridge: BridgePool::new(None),
            gateway_key: RwLock::new("gateway-secret".to_string()),
            rate: RateLimiter::new(Limits::default().rate_per_minute),
            slots: Slots::new(&Limits::default()),
            limits: Limits::default(),
            upstream_base: upstream,
            sessions: Mutex::new(SessionCache::default()),
            responses: ResponseStore::default(),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = router(state.clone()).into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}")
    }

    /// A stand-in where /responses answers ModelProtocolUnsupported and
    /// /chat/completions serves the model: the fallback path.
    async fn fake_zen_chat_only() -> String {
        let app = Router::new()
            .route(
                "/responses",
                post(|| async {
                    (
                        StatusCode::BAD_REQUEST,
                        r#"{"type":"error","error":{"type":"ModelProtocolUnsupported","message":"Model does not support this protocol."}}"#,
                    )
                }),
            )
            .route(
                "/chat/completions",
                post(|| async {
                    let mut response = CHAT_ANSWER.to_string().into_response();
                    response.headers_mut().insert("content-type", "text/event-stream".parse().unwrap());
                    response
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn a_protocol_mismatch_falls_back_to_the_other_endpoint_and_is_learned() {
        // muse-* guesses Responses; the stand-in only serves chat.
        let base = gateway_model(fake_zen_chat_only().await, &["k1"], "muse-spark-1.3-contributor-free").await;
        let response = chat_model(&base, "muse-spark-1.3-contributor-free").send().await.unwrap();
        assert_eq!(response.status(), 200);
        // Non-streaming client: a complete chat.completion object.
        let completion: Value = response.json().await.unwrap();
        assert_eq!(completion["choices"][0]["message"]["content"], "pong \u{2713}");
        assert_eq!(completion["usage"]["total_tokens"], 5);
        // Learned: a second request goes straight to the chat endpoint.
        assert_eq!(
            crate::translate::learned_upstream("muse-spark-1.3-contributor-free"),
            Some(crate::translate::Upstream::Chat)
        );
        let response = chat_model(&base, "muse-spark-1.3-contributor-free").send().await.unwrap();
        assert_eq!(response.status(), 200);
    }

    fn chat(base: &str) -> reqwest::RequestBuilder {
        chat_model(base, "test-model-free")
    }

    fn chat_model(base: &str, model: &str) -> reqwest::RequestBuilder {
        reqwest::Client::new()
            .post(format!("{base}/v1/chat/completions"))
            .bearer_auth("gateway-secret")
            .json(&json!({ "model": model, "messages": [{ "role": "user", "content": "ping" }] }))
    }

    async fn body(response: reqwest::Response) -> Value {
        response.json().await.unwrap()
    }

    #[tokio::test]
    async fn an_answer_is_translated_for_both_dialects() {
        let (base, state) = gateway(fake_zen(|_| (200, ANSWER)).await, &["k1"], Limits::default()).await;
        let response = chat(&base).send().await.unwrap();
        assert_eq!(response.status(), 200);
        let completion = body(response).await;
        assert_eq!(completion["choices"][0]["message"]["content"], "pong \u{2713}");
        assert_eq!(completion["choices"][0]["finish_reason"], "stop");
        assert_eq!(completion["usage"]["total_tokens"], 5);

        let response = reqwest::Client::new()
            .post(format!("{base}/v1/messages"))
            .header("x-api-key", "gateway-secret")
            .json(&json!({ "model": "test-model-free", "max_tokens": 64, "messages": [{ "role": "user", "content": "ping" }] }))
            .send()
            .await
            .unwrap();
        let message = body(response).await;
        assert_eq!(message["content"][0]["text"], "pong \u{2713}");
        assert_eq!(message["stop_reason"], "end_turn");
        assert_eq!(state.pool.snapshot()[0].direct.served, 2);
    }

    #[tokio::test]
    async fn streaming_delivers_the_text_and_ends() {
        let (base, _) = gateway(fake_zen(|_| (200, ANSWER)).await, &["k1"], Limits::default()).await;
        let mut request: Value = json!({ "model": "test-model-free", "stream": true });
        request["messages"] = json!([{ "role": "user", "content": "ping" }]);
        let response = reqwest::Client::new()
            .post(format!("{base}/v1/chat/completions"))
            .bearer_auth("gateway-secret")
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        let text = response.text().await.unwrap();
        let said: String = text
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str::<Value>(data).ok())
            .filter_map(|chunk| chunk["choices"][0]["delta"]["content"].as_str().map(String::from))
            .collect();
        assert_eq!(said, "pong \u{2713}");
        assert!(text.contains("\"finish_reason\":\"stop\""));
        assert!(text.trim_end().ends_with("[DONE]"));
    }

    #[tokio::test]
    async fn an_exhausted_key_rests_and_the_next_one_answers() {
        let zen = fake_zen(|key| if key == "k1" { (429, "quota used up") } else { (200, ANSWER) }).await;
        let (base, state) = gateway(zen, &["k1", "k2"], Limits::default()).await;
        assert_eq!(chat(&base).send().await.unwrap().status(), 200);
        let keys = state.pool.snapshot();
        let rest = keys[0].direct.cooling(SystemTime::now()).expect("k1 rests");
        assert!(rest > EXHAUSTED_FOR - Duration::from_secs(60));
        assert_eq!((keys[0].limited, keys[1].direct.served), (1, 1));
        // The resting key is not asked again.
        assert_eq!(chat(&base).send().await.unwrap().status(), 200);
        assert_eq!(state.pool.snapshot()[0].limited, 1);
    }

    #[tokio::test]
    async fn when_every_key_is_exhausted_the_client_is_told_to_retry() {
        let (base, _) = gateway(fake_zen(|_| (402, "no credit")).await, &["k1", "k2"], Limits::default()).await;
        let response = chat(&base).send().await.unwrap();
        assert_eq!(response.status(), 429);
        assert!(response.headers().contains_key("retry-after"));
        assert!(body(response).await["error"]["message"].as_str().unwrap().contains("exhausted"));
    }

    #[tokio::test]
    async fn upstream_refusals_and_bad_requests_surface_with_fitting_statuses() {
        let (base, _) = gateway(fake_zen(|_| (403, "free tier gate")).await, &["k1"], Limits::default()).await;
        let response = chat(&base).send().await.unwrap();
        assert_eq!(response.status(), 502);
        assert!(body(response).await["error"]["message"].as_str().unwrap().contains("free tier gate"));

        let (base, state) = gateway(fake_zen(|_| (400, "context too long")).await, &["k1", "k2"], Limits::default()).await;
        assert_eq!(chat(&base).send().await.unwrap().status(), 400);
        // A request upstream rejects is not retried on the other key.
        assert_eq!(state.pool.snapshot()[1].failed, 0);
    }

    #[tokio::test]
    async fn a_failing_upstream_is_skipped_for_the_next_key() {
        let zen = fake_zen(|key| if key == "k1" { (500, "boom") } else { (200, ANSWER) }).await;
        let (base, state) = gateway(zen, &["k1", "k2"], Limits::default()).await;
        assert_eq!(chat(&base).send().await.unwrap().status(), 200);
        let keys = state.pool.snapshot();
        assert_eq!((keys[0].failed, keys[0].direct.until), (1, None));
    }

    #[tokio::test]
    async fn requests_are_refused_before_reaching_upstream() {
        let limits = Limits { max_body_bytes: 2048, ..Limits::default() };
        let (base, state) = gateway(fake_zen(|_| (200, ANSWER)).await, &["k1"], limits).await;
        let client = reqwest::Client::new();
        let url = format!("{base}/v1/chat/completions");

        let wrong_key = client.post(&url).bearer_auth("nope").json(&json!({ "model": "test-model-free" }));
        assert_eq!(wrong_key.send().await.unwrap().status(), 401);

        let unknown = client.post(&url).bearer_auth("gateway-secret").json(&json!({ "model": "gpt-9" }));
        let response = unknown.send().await.unwrap();
        assert_eq!(response.status(), 404);
        assert!(body(response).await["error"]["message"].as_str().unwrap().contains("test-model-free"));

        let huge = json!({ "model": "test-model-free", "messages": [{ "role": "user", "content": "x".repeat(4096) }] });
        let response = client.post(&url).bearer_auth("gateway-secret").json(&huge).send().await.unwrap();
        assert_eq!(response.status(), 413);

        let garbage = client
            .post(format!("{base}/v1/messages"))
            .header("x-api-key", "gateway-secret")
            .header("content-type", "application/json")
            .body("{not json");
        let response = garbage.send().await.unwrap();
        assert_eq!(response.status(), 400);
        assert_eq!(body(response).await["type"], "error", "errors use the dialect's shape");

        assert_eq!(state.pool.snapshot()[0].served(), 0);
    }

    #[tokio::test]
    async fn remote_clients_are_rate_limited_and_local_ones_are_not() {
        let limits = Limits { rate_per_minute: 2, ..Limits::default() };
        let (base, _) = gateway(fake_zen(|_| (200, ANSWER)).await, &["k1"], limits).await;
        // As the tunnel would forward a remote caller.
        let remote = |ip: &'static str| chat(&base).header("cf-connecting-ip", ip);
        assert_eq!(remote("203.0.113.7").send().await.unwrap().status(), 200);
        assert_eq!(remote("203.0.113.7").send().await.unwrap().status(), 200);
        let limited = remote("203.0.113.7").send().await.unwrap();
        assert_eq!(limited.status(), 429);
        assert!(limited.headers().contains_key("retry-after"));
        assert_eq!(remote("203.0.113.8").send().await.unwrap().status(), 200);
        for _ in 0..4 {
            assert_eq!(chat(&base).send().await.unwrap().status(), 200);
        }
    }

    #[tokio::test]
    async fn guessing_gateway_keys_is_rate_limited_too() {
        let limits = Limits { rate_per_minute: 3, ..Limits::default() };
        let (base, _) = gateway(fake_zen(|_| (200, ANSWER)).await, &["k1"], limits).await;
        let guess = || {
            reqwest::Client::new()
                .post(format!("{base}/v1/chat/completions"))
                .header("x-forwarded-for", "198.51.100.9, 10.0.0.1")
                .bearer_auth("wrong")
                .json(&json!({ "model": "test-model-free" }))
        };
        for _ in 0..3 {
            assert_eq!(guess().send().await.unwrap().status(), 401);
        }
        assert_eq!(guess().send().await.unwrap().status(), 429);
    }

    #[tokio::test]
    async fn a_full_gateway_answers_429_instead_of_piling_up() {
        let limits = Limits { max_concurrent: 1, max_queue: 0, ..Limits::default() };
        let (base, state) = gateway(fake_zen(|_| (200, ANSWER)).await, &["k1"], limits).await;
        let held = state.slots.acquire().await.unwrap();
        assert_eq!(chat(&base).send().await.unwrap().status(), 429);
        drop(held);
        assert_eq!(chat(&base).send().await.unwrap().status(), 200);
    }

    #[tokio::test]
    async fn the_responses_api_answers_stores_continues_and_deletes() {
        let (base, _) = gateway(fake_zen(|_| (200, ANSWER)).await, &["k1"], Limits::default()).await;
        let client = reqwest::Client::new();
        let post = |request: Value| {
            client.post(format!("{base}/v1/responses")).bearer_auth("gateway-secret").json(&request).send()
        };
        let first = body(post(json!({ "model": "test-model-free", "instructions": "Be terse.", "input": "ping" })).await.unwrap()).await;
        assert_eq!(first["object"], "response");
        assert_eq!(first["status"], "completed");
        assert_eq!(first["output"][0]["content"][0]["text"], "pong \u{2713}");
        assert_eq!(first["instructions"], "Be terse.");
        assert_eq!(first["usage"]["total_tokens"], 5);
        let id = first["id"].as_str().unwrap().to_string();

        let url = format!("{base}/v1/responses/{id}");
        let fetched = body(client.get(&url).bearer_auth("gateway-secret").send().await.unwrap()).await;
        assert_eq!(fetched["id"], id.as_str());
        assert_eq!(client.get(&url).send().await.unwrap().status(), 401);

        let next = post(json!({ "model": "test-model-free", "previous_response_id": id, "input": "again" })).await.unwrap();
        assert_eq!(next.status(), 200);
        assert_eq!(body(next).await["previous_response_id"], id.as_str());

        let unknown = post(json!({ "model": "test-model-free", "previous_response_id": "resp_nope", "input": "x" })).await.unwrap();
        assert_eq!(unknown.status(), 400);

        let unstored = body(post(json!({ "model": "test-model-free", "store": false, "input": "ping" })).await.unwrap()).await;
        let gone = format!("{base}/v1/responses/{}", unstored["id"].as_str().unwrap());
        assert_eq!(client.get(&gone).bearer_auth("gateway-secret").send().await.unwrap().status(), 404);

        assert_eq!(client.delete(&url).bearer_auth("gateway-secret").send().await.unwrap().status(), 200);
        assert_eq!(client.get(&url).bearer_auth("gateway-secret").send().await.unwrap().status(), 404);
    }

    #[tokio::test]
    async fn a_streamed_response_ends_with_the_completed_object_and_is_stored() {
        let (base, _) = gateway(fake_zen(|_| (200, ANSWER)).await, &["k1"], Limits::default()).await;
        let client = reqwest::Client::new();
        let text = client
            .post(format!("{base}/v1/responses"))
            .bearer_auth("gateway-secret")
            .json(&json!({ "model": "test-model-free", "input": "ping", "stream": true }))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(text.starts_with("event: response.created"));
        let last = text.trim_end().rsplit("\n\n").next().unwrap();
        assert!(last.starts_with("event: response.completed"));
        let completed: Value = serde_json::from_str(last.split_once("data: ").unwrap().1).unwrap();
        let id = completed["response"]["id"].as_str().unwrap();
        assert_eq!(completed["response"]["output"][0]["content"][0]["text"], "pong \u{2713}");
        // Stored just after the stream closes.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let fetched = client.get(format!("{base}/v1/responses/{id}")).bearer_auth("gateway-secret").send().await.unwrap();
        assert_eq!(fetched.status(), 200);
    }

    #[tokio::test]
    async fn metrics_need_the_key_and_come_as_json_or_prometheus_text() {
        let (base, _) = gateway(fake_zen(|_| (200, ANSWER)).await, &["k1", "k2"], Limits::default()).await;
        assert_eq!(chat(&base).send().await.unwrap().status(), 200);
        let client = reqwest::Client::new();
        assert_eq!(client.get(format!("{base}/metrics")).send().await.unwrap().status(), 401);

        let json = body(client.get(format!("{base}/metrics")).bearer_auth("gateway-secret").send().await.unwrap()).await;
        assert!(json["requests"]["total"].as_u64().unwrap() >= 1);
        assert_eq!(json["keys"].as_array().unwrap().len(), 2);
        assert_eq!(json["keys"][0]["direct"]["served"], 1);

        let text = client
            .get(format!("{base}/metrics?format=prometheus"))
            .bearer_auth("gateway-secret")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(text.contains("# TYPE zenshim_requests_total counter"));
        assert!(text.contains("zenshim_key_served_total{key=\"1\",route=\"direct\"} 1"));
        assert!(text.contains("zenshim_key_resting_seconds{key=\"2\",route=\"cli\"} 0"));

        assert_eq!(client.get(format!("{base}/ready")).send().await.unwrap().text().await.unwrap(), "ready");
    }

    #[test]
    fn sse_blocks_survive_any_chunking_including_split_characters() {
        let wire = "event: response.output_text.delta\ndata: {\"delta\":\"h\u{e9}llo \u{2713}\"}\n\nevent: done\ndata: {}\n\n".as_bytes();
        for chunk in 1..wire.len() {
            let mut buffer = Vec::new();
            let mut events = Vec::new();
            for piece in wire.chunks(chunk) {
                buffer.extend_from_slice(piece);
                events.extend(drain_sse_blocks(&mut buffer));
            }
            assert_eq!(events.len(), 2, "chunk size {chunk}");
            assert_eq!(events[0].0, "response.output_text.delta");
            assert!(events[0].1.contains("h\u{e9}llo \u{2713}"), "chunk size {chunk}");
            assert!(buffer.is_empty());
        }
    }

    #[test]
    fn forwarded_addresses_are_believed_only_from_this_machine() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.5, 10.0.0.1".parse().unwrap());
        let loopback: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let lan: SocketAddr = "192.168.1.20:5000".parse().unwrap();
        assert_eq!(client_of(loopback, &headers), ("203.0.113.5".parse().unwrap(), false));
        assert_eq!(client_of(lan, &headers), ("192.168.1.20".parse().unwrap(), false));
        assert_eq!(client_of(loopback, &HeaderMap::new()), ("127.0.0.1".parse().unwrap(), true));
    }

    #[test]
    fn remembered_conversations_are_bounded() {
        let mut cache = SessionCache::default();
        let first = cache.get_or_create(0, || "s0".to_string());
        assert_eq!(cache.get_or_create(0, || "other".to_string()), first);
        for fingerprint in 1..=MAX_SESSIONS as u64 {
            cache.get_or_create(fingerprint, || format!("s{fingerprint}"));
        }
        assert_eq!(cache.ids.len(), MAX_SESSIONS);
        assert_eq!(cache.get_or_create(0, || "fresh".to_string()), "fresh", "the oldest was forgotten");
    }

    #[test]
    fn gateway_keys_compare_in_full() {
        assert!(constant_time_eq(b"sk-zenshim-abc", b"sk-zenshim-abc"));
        assert!(!constant_time_eq(b"sk-zenshim-abc", b"sk-zenshim-abd"));
        assert!(!constant_time_eq(b"sk-zenshim-abc", b"sk-zenshim-ab"));
    }
}
