//! The CLI route: answer a request by driving a sandboxed `opencode serve`
//! instead of imitating one. Each upstream key gets its own backend
//! process, started on first use and stopped when idle.
//!
//! The backend is the real OpenCode, so Zen's free-tier gate accepts it
//! whatever the wire recipe of the day is. It is kept harmless: it runs in
//! a throwaway home with none of the user's configuration, and every tool
//! it asks permission for is refused, so nothing runs a command or reads a
//! file on this machine.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, oneshot, Notify};

use crate::hub;
use crate::keys::Failure;

const ROOT_PREFIX: &str = "zenshim-oc-";
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// A backend nobody has used for this long is stopped.
const IDLE_FOR: Duration = Duration::from_secs(10 * 60);
const AGENT: &str = "plan";
const PROVIDER: &str = "opencode";

/// Sent back to the model whenever it tries to use an OpenCode tool.
const TOOL_REJECTION: &str = "OpenCode tools are disabled by this gateway. Do not call OpenCode \
     tools; answer directly. If the user supplied their own tools, use the tool-protocol block \
     described in your instructions.";

/// Environment variables the backend legitimately needs from the host.
const PASSTHROUGH_ENV: [&str; 24] = [
    "PATH", "Path", "PATHEXT", "SystemRoot", "SYSTEMROOT", "windir", "WINDIR", "ComSpec",
    "SystemDrive", "TEMP", "TMP", "TMPDIR", "LANG", "LC_ALL", "TZ", "HTTP_PROXY", "HTTPS_PROXY",
    "NO_PROXY", "http_proxy", "https_proxy", "no_proxy", "NODE_EXTRA_CA_CERTS", "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

/// One upstream event in the vocabulary the stream translator already
/// reads from the direct wire: (event name, JSON data).
pub type WireEvent = (String, String);

/// What the CLI route sends: session instructions, one text, attachments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgePrompt {
    /// The client's system text and tool protocol.
    pub system: String,
    pub text: String,
    /// Attachments as (data: or https: URI, file name if known).
    pub attachments: Vec<(String, Option<String>)>,
}

/// OpenCode configuration for the backend. Every tool permission is "ask"
/// (tools stay advertised exactly as in stock OpenCode, which the gate
/// checks) and the bridge answers every ask with "reject".
fn backend_config() -> Value {
    json!({
        "$schema": "https://opencode.ai/config.json",
        "autoupdate": false,
        "share": "disabled",
        "snapshot": false,
        "permission": {
            "*": "ask", "read": "ask", "edit": "ask", "bash": "ask", "shell": "ask",
            "glob": "ask", "grep": "ask", "list": "ask", "task": "ask", "webfetch": "ask",
            "websearch": "ask", "external_directory": "ask", "skill": "ask", "lsp": "ask",
        },
        "experimental": { "continue_loop_on_deny": true },
    })
}

/// Minimal child environment: isolated homes, none of the gateway's own
/// secrets, and the upstream key this backend is for.
fn sandbox_env(
    root: &Path,
    password: &str,
    key: &str,
    host: impl Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    let dir = |name: &str| root.join(name).to_string_lossy().into_owned();
    let mut env: Vec<(String, String)> = PASSTHROUGH_ENV
        .iter()
        .filter_map(|name| host(name).map(|value| (name.to_string(), value)))
        .collect();
    let home = dir("home");
    let fixed = [
        ("HOME", home.clone()),
        ("USERPROFILE", home.clone()),
        ("APPDATA", format!("{home}/AppData/Roaming")),
        ("LOCALAPPDATA", format!("{home}/AppData/Local")),
        ("XDG_DATA_HOME", dir("data")),
        ("XDG_CONFIG_HOME", dir("config")),
        ("XDG_CACHE_HOME", dir("cache")),
        ("XDG_STATE_HOME", dir("state")),
        ("OPENCODE_SERVER_USERNAME", "opencode".to_string()),
        ("OPENCODE_SERVER_PASSWORD", password.to_string()),
        ("OPENCODE_CONFIG_CONTENT", backend_config().to_string()),
        ("OPENCODE_DISABLE_AUTOUPDATE", "1".to_string()),
        ("OPENCODE_DISABLE_SHARE", "1".to_string()),
        ("OPENCODE_DISABLE_LSP_DOWNLOAD", "1".to_string()),
        ("OPENCODE_DISABLE_PROJECT_CONFIG", "1".to_string()),
        ("OPENCODE_DISABLE_CLAUDE_CODE", "1".to_string()),
        ("OPENCODE_DISABLE_EXTERNAL_SKILLS", "1".to_string()),
        ("OPENCODE_DISABLE_DEFAULT_PLUGINS", "1".to_string()),
    ];
    env.extend(fixed.into_iter().map(|(name, value)| (name.to_string(), value)));
    // The anonymous tier is what OpenCode uses with no key at all.
    if key != "public" {
        env.push(("OPENCODE_API_KEY".to_string(), key.to_string()));
    }
    env
}

/// The port from OpenCode's "listening on http://127.0.0.1:PORT" line.
fn listening_port(line: &str) -> Option<u16> {
    let rest = &line[line.find("listening on http://127.0.0.1:")? + 30..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok().filter(|port| *port > 0)
}

/// Sort an OpenCode error into what the key scheduler should do about it.
fn classify(status: Option<u64>, message: &str) -> Failure {
    let message: String = message.chars().take(300).collect();
    let lower = message.to_lowercase();
    let quota = ["rate limit", "rate_limit", "quota", "usage limit", "too many requests"]
        .iter()
        .any(|needle| lower.contains(needle));
    match status {
        Some(402 | 429) => Failure::Exhausted { message, retry_after: None },
        Some(403) => Failure::Gated(message),
        Some(401) => Failure::Auth(message),
        Some(status @ 400..=499) => Failure::Rejected { status: status as u16, message },
        _ if quota => Failure::Exhausted { message, retry_after: None },
        _ => Failure::Upstream(message),
    }
}

fn error_of(value: &Value) -> Option<Failure> {
    let error = value.get("error").filter(|e| e.is_object())?;
    let message = error.get("message").and_then(Value::as_str).unwrap_or("unknown error");
    Some(classify(error.get("status").and_then(Value::as_u64), message))
}

fn usage_of(tokens: &Value) -> (u64, u64) {
    let count = |pointer: &str| tokens.pointer(pointer).and_then(Value::as_u64).unwrap_or(0);
    (count("/input") + count("/cache/read"), count("/output") + count("/reasoning"))
}

/// What one session event means for the turn in progress.
#[derive(Debug, PartialEq)]
enum Step {
    Delta(String),
    /// Something happened; nothing to forward.
    Activity,
    /// The agent loop ended; the messages hold the result.
    Ended,
}

/// Accumulates one answer from session events, then squares it with the
/// stored messages so nothing is lost or repeated if events were missed.
#[derive(Default)]
struct Turn {
    streamed: String,
    last_message: Option<String>,
}

/// The finished answer as read back from the session.
#[derive(Debug, PartialEq)]
struct Answer {
    /// Text the events did not deliver.
    remainder: String,
    usage: Option<(u64, u64)>,
    truncated: bool,
}

impl Turn {
    fn on_event(&mut self, event: &Value) -> Step {
        let data = event.get("data").unwrap_or(&Value::Null);
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "session.text.delta" => {
                let Some(delta) = data.get("delta").and_then(Value::as_str) else {
                    return Step::Activity;
                };
                // After a refused tool the model answers in a new message.
                let message = data.get("assistantMessageID").and_then(Value::as_str).map(String::from);
                let mut text = String::new();
                if !self.streamed.is_empty() && self.last_message != message {
                    text.push_str("\n\n");
                }
                self.last_message = message;
                text.push_str(delta);
                self.streamed.push_str(&text);
                Step::Delta(text)
            }
            "session.execution.succeeded" | "session.execution.failed" | "session.execution.interrupted" => {
                Step::Ended
            }
            _ => Step::Activity,
        }
    }

    /// Read the turn's result from the session's messages (newest first).
    fn reconcile(&mut self, messages: &Value) -> Result<Answer, Failure> {
        let empty = Vec::new();
        let list = messages.get("data").and_then(Value::as_array).unwrap_or(&empty);
        let mut texts: Vec<String> = Vec::new();
        let mut failure = None;
        let mut usage: Option<(u64, u64)> = None;
        let mut truncated = false;
        for message in list.iter().rev() {
            if message.get("type").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let text: String = message
                .get("content")
                .and_then(Value::as_array)
                .unwrap_or(&empty)
                .iter()
                .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect();
            if !text.is_empty() {
                texts.push(text);
            }
            if let Some(tokens) = message.get("tokens") {
                let (input, output) = usage_of(tokens);
                let total = usage.get_or_insert((0, 0));
                // Each step re-sends the conversation; the last input is the prompt size.
                total.0 = total.0.max(input);
                total.1 += output;
            }
            failure = error_of(message).or(failure);
            truncated = message.get("finish").and_then(Value::as_str) == Some("length");
        }
        let full = texts.join("\n\n");
        if let Some(failure) = failure {
            if full.is_empty() && self.streamed.is_empty() {
                return Err(failure);
            }
            // Part of an answer already went out; the rest is lost.
            truncated = true;
        } else if full.is_empty() && self.streamed.is_empty() {
            return Err(Failure::Upstream("OpenCode returned no answer".to_string()));
        }
        let remainder = match full.strip_prefix(self.streamed.as_str()) {
            Some(rest) => rest.to_string(),
            None if self.streamed.is_empty() => full,
            None => String::new(),
        };
        self.streamed.push_str(&remainder);
        Ok(Answer { remainder, usage, truncated })
    }
}

/// True once the session has gone idle after working (newest message first).
fn settled(messages: &Value) -> bool {
    messages
        .pointer("/data/0/type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind == "idle")
}

fn random_token() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    (0..32).map(|_| format!("{:02x}", rng.random::<u8>())).collect()
}

fn create_root() -> std::io::Result<PathBuf> {
    let root = std::env::temp_dir().join(format!(
        "{ROOT_PREFIX}{}-{}",
        std::process::id(),
        &random_token()[..12]
    ));
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    for name in ["home", "workspace", "data", "config", "cache", "state"] {
        builder.create(root.join(name))?;
    }
    Ok(root)
}

fn remove_root(root: &Path) {
    let ours = root
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(ROOT_PREFIX));
    if ours {
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(unix)]
fn process_alive(pid: i32) -> bool {
    // Signal 0 only checks; EPERM still means the process exists.
    let signalled = unsafe { libc::kill(pid, 0) == 0 };
    signalled || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Without a way to tell, assume the owner is still running.
#[cfg(not(unix))]
fn process_alive(_pid: i32) -> bool {
    true
}

/// Remove scratch trees left by gateway processes that no longer exist.
fn sweep_stale_roots() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(rest) = name.to_str().and_then(|n| n.strip_prefix(ROOT_PREFIX)) else { continue };
        let Some(pid) = rest.split('-').next().and_then(|p| p.parse::<i32>().ok()) else { continue };
        if pid != std::process::id() as i32 && !process_alive(pid) {
            remove_root(&entry.path());
        }
    }
}

/// Find the OpenCode CLI: an explicit path, then PATH, then its installer's
/// default location.
pub fn find_opencode(explicit: Option<&str>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }
    let name = if cfg!(windows) { "opencode.exe" } else { "opencode" };
    let on_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join(name));
    let home = std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".opencode/bin").join(name));
    on_path.chain(home).find(|candidate| candidate.is_file())
}

/// One sandboxed `opencode serve`, serving one upstream key.
struct Backend {
    base: String,
    password: String,
    http: reqwest::Client,
    root: PathBuf,
    pid: Option<u32>,
    version: String,
    /// Session id → where that session's events go.
    sessions: Mutex<HashMap<String, mpsc::UnboundedSender<Value>>>,
    alive: AtomicBool,
    stop: Notify,
    active: AtomicUsize,
    last_used: Mutex<Instant>,
}

impl Backend {
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.base))
            .basic_auth("opencode", Some(&self.password))
    }

    async fn json(&self, request: reqwest::RequestBuilder) -> Result<Value, (u16, String)> {
        let response = request.send().await.map_err(|e| (0, e.to_string()))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("message").and_then(Value::as_str).map(String::from))
                .unwrap_or_else(|| text.chars().take(200).collect());
            return Err((status.as_u16(), message));
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    fn reject_permission(self: &Arc<Self>, session: String, request: String) {
        let backend = Arc::clone(self);
        tokio::spawn(async move {
            let path = format!("/api/session/{session}/permission/{request}/reply");
            let body = json!({ "decision": "reject", "message": TOOL_REJECTION });
            let reply = backend.request(reqwest::Method::POST, &path).json(&body);
            // 404: already answered, which is all that matters.
            if let Err((status, message)) = backend.json(reply.timeout(Duration::from_secs(5))).await
                && status != 404
            {
                hub::warn(format!("cli: could not refuse an OpenCode tool request: {message}"));
            }
        });
    }

    /// Refuse whatever is pending: permission requests raised while the
    /// event stream was down, and questions the model put to the user.
    async fn sweep_pending(self: &Arc<Self>) {
        let pending = self.request(reqwest::Method::GET, "/api/permission/request");
        if let Ok(list) = self.json(pending.timeout(Duration::from_secs(5))).await {
            for item in list.get("data").and_then(Value::as_array).into_iter().flatten() {
                let id = item.get("id").and_then(Value::as_str);
                let session = item.get("sessionID").and_then(Value::as_str);
                if let (Some(id), Some(session)) = (id, session) {
                    self.reject_permission(session.to_string(), id.to_string());
                }
            }
        }
        let forms = self.request(reqwest::Method::GET, "/api/form");
        if let Ok(list) = self.json(forms.timeout(Duration::from_secs(5))).await {
            for item in list.get("data").and_then(Value::as_array).into_iter().flatten() {
                let id = item.get("id").and_then(Value::as_str);
                let session = item.get("sessionID").and_then(Value::as_str);
                if let (Some(id), Some(session)) = (id, session) {
                    let path = format!("/api/session/{session}/form/{id}");
                    let cancel = self.request(reqwest::Method::DELETE, &path);
                    let _ = self.json(cancel.timeout(Duration::from_secs(5))).await;
                }
            }
        }
    }

    fn dispatch(self: &Arc<Self>, event: Value) {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        let session = event.pointer("/data/sessionID").and_then(Value::as_str).map(String::from);
        if kind == "permission.asked" {
            let id = event.pointer("/data/id").and_then(Value::as_str);
            if let (Some(id), Some(session)) = (id, &session) {
                self.reject_permission(session.clone(), id.to_string());
            }
        } else if kind.starts_with("form.") || kind.contains(".form.") {
            let backend = Arc::clone(self);
            tokio::spawn(async move { backend.sweep_pending().await });
        }
        if let Some(session) = session
            && let Some(listener) = self.sessions.lock().unwrap().get(&session)
        {
            let _ = listener.send(event);
        }
    }

    /// One shared subscription to the backend's event stream, fanned out
    /// to per-session listeners, for as long as the backend lives.
    async fn pump_events(self: Arc<Self>) {
        let mut backoff = Duration::from_millis(250);
        while self.alive.load(Ordering::SeqCst) {
            let request = self
                .request(reqwest::Method::GET, "/api/event")
                .header("accept", "text/event-stream");
            if let Ok(response) = request.send().await.and_then(|r| r.error_for_status()) {
                backoff = Duration::from_millis(250);
                self.sweep_pending().await;
                // Bytes, so a character split across chunks survives.
                let mut buffer: Vec<u8> = Vec::new();
                let mut stream = response.bytes_stream();
                loop {
                    let chunk = tokio::select! {
                        chunk = stream.next() => chunk,
                        _ = self.stop.notified() => return,
                    };
                    let Some(Ok(bytes)) = chunk else { break };
                    buffer.extend_from_slice(&bytes);
                    while let Some(split) = buffer.windows(2).position(|pair| pair == b"\n\n") {
                        let block: Vec<u8> = buffer.drain(..split + 2).collect();
                        let data: String = String::from_utf8_lossy(&block)
                            .lines()
                            .filter_map(|line| line.strip_prefix("data:"))
                            .map(str::trim_start)
                            .collect();
                        if let Ok(event) = serde_json::from_str::<Value>(&data) {
                            self.dispatch(event);
                        }
                    }
                }
            }
            if !self.alive.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(5));
        }
    }

    fn shutdown(&self) {
        if !self.alive.swap(false, Ordering::SeqCst) {
            return;
        }
        // Stored permits, so both waiters hear it whenever they next look.
        self.stop.notify_one();
        self.stop.notify_waiters();
        // OpenCode does not exit promptly on SIGTERM; it holds nothing
        // worth a graceful stop, so take the whole process group down.
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            unsafe { libc::killpg(pid as i32, libc::SIGKILL) };
        }
        self.sessions.lock().unwrap().clear();
        remove_root(&self.root);
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn start_backend(binary: &Path, key: &str) -> Result<Arc<Backend>, String> {
    let root = create_root().map_err(|e| format!("could not create the sandbox: {e}"))?;
    let password = random_token();
    let mut command = tokio::process::Command::new(binary);
    command
        .args(["serve", "--hostname", "127.0.0.1", "--port", "0"])
        .current_dir(root.join("workspace"))
        .env_clear()
        .envs(sandbox_env(&root, &password, key, |name| std::env::var(name).ok()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(target_os = "linux")]
    unsafe {
        // If the gateway is killed outright, the backend goes with it.
        command.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            remove_root(&root);
            return Err(format!("could not start {}: {e}", binary.display()));
        }
    };
    let pid = child.id();
    let abandon = |root: &Path, reason: String| {
        #[cfg(unix)]
        if let Some(pid) = pid {
            unsafe { libc::killpg(pid as i32, libc::SIGKILL) };
        }
        remove_root(root);
        reason
    };

    // OpenCode picks the port itself and reports it once bound, so nothing
    // else can take it in between and pose as the backend.
    let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    let port = tokio::time::timeout(START_TIMEOUT, async {
        while let Ok(Some(line)) = stdout.next_line().await {
            if let Some(port) = listening_port(&line) {
                return Some(port);
            }
        }
        None
    })
    .await;
    let Ok(Some(port)) = port else {
        let _ = child.start_kill();
        return Err(abandon(&root, "OpenCode did not start listening".to_string()));
    };
    // Keep draining both pipes so OpenCode never blocks on a full one.
    tokio::spawn(async move { while let Ok(Some(_)) = stdout.next_line().await {} });
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(_)) = lines.next_line().await {}
        });
    }

    let http = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| abandon(&root, e.to_string()))?;
    let mut backend = Backend {
        base: format!("http://127.0.0.1:{port}"),
        password,
        http,
        root: root.clone(),
        pid,
        version: String::new(),
        sessions: Mutex::new(HashMap::new()),
        alive: AtomicBool::new(true),
        stop: Notify::new(),
        active: AtomicUsize::new(0),
        last_used: Mutex::new(Instant::now()),
    };

    // Ready when it identifies itself and has loaded its model list.
    let deadline = Instant::now() + START_TIMEOUT;
    let info = loop {
        let request = backend.request(reqwest::Method::GET, "/api/info").timeout(Duration::from_secs(3));
        match backend.json(request).await {
            Ok(info) if info.get("version").is_some() => break info,
            // Answered, but not with the v2 API: an older OpenCode.
            Ok(_) | Err((404, _)) => {
                let _ = child.start_kill();
                backend.shutdown();
                return Err("unsupported: this OpenCode has no v2 server API (OpenCode 2 or newer is needed)".to_string());
            }
            Err(_) if Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(200)).await,
            Err((_, message)) => {
                let _ = child.start_kill();
                backend.shutdown();
                return Err(format!("OpenCode did not become ready: {message}"));
            }
        }
    };
    backend.version = info.get("version").and_then(Value::as_str).unwrap_or("?").to_string();
    let models_deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < models_deadline {
        let request = backend.request(reqwest::Method::GET, "/api/model").timeout(Duration::from_secs(5));
        let loaded = backend.json(request).await.ok().is_some_and(|models| {
            models.get("data").and_then(Value::as_array).is_some_and(|list| !list.is_empty())
        });
        if loaded {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    let backend = Arc::new(backend);
    tokio::spawn(Arc::clone(&backend).pump_events());
    // Reap the child, and notice if it dies on its own.
    tokio::spawn({
        let backend = Arc::clone(&backend);
        async move {
            tokio::select! {
                _ = child.wait() => {
                    if backend.alive.load(Ordering::SeqCst) {
                        hub::warn("cli: the OpenCode backend exited; it restarts on the next request");
                    }
                }
                _ = backend.stop.notified() => {
                    let _ = child.kill().await;
                }
            }
            backend.shutdown();
        }
    });
    Ok(backend)
}

/// The CLI backends, one per upstream key.
pub struct BridgePool {
    binary: Option<PathBuf>,
    backends: tokio::sync::Mutex<HashMap<String, Arc<Backend>>>,
    /// False once the installed OpenCode turned out to be unusable.
    usable: AtomicBool,
    version: Mutex<Option<String>>,
}

impl BridgePool {
    pub fn new(binary: Option<PathBuf>) -> Arc<Self> {
        if binary.is_some() {
            sweep_stale_roots();
        }
        Arc::new(Self {
            usable: AtomicBool::new(binary.is_some()),
            binary,
            backends: tokio::sync::Mutex::new(HashMap::new()),
            version: Mutex::new(None),
        })
    }

    pub fn binary(&self) -> Option<&Path> {
        self.binary.as_deref()
    }

    pub fn usable(&self) -> bool {
        self.usable.load(Ordering::SeqCst)
    }

    /// Version of the OpenCode last started, once one has been.
    pub fn version(&self) -> Option<String> {
        self.version.lock().unwrap().clone()
    }

    async fn backend(&self, key: &str) -> Result<Arc<Backend>, Failure> {
        let Some(binary) = self.binary.as_deref().filter(|_| self.usable()) else {
            return Err(Failure::Unavailable("the OpenCode CLI is not available".to_string()));
        };
        // Held across the start so concurrent first requests share one backend.
        let mut backends = self.backends.lock().await;
        if let Some(backend) = backends.get(key).filter(|b| b.alive.load(Ordering::SeqCst)) {
            return Ok(Arc::clone(backend));
        }
        hub::info("cli: starting a sandboxed OpenCode backend");
        let started = Instant::now();
        match start_backend(binary, key).await {
            Ok(backend) => {
                hub::info(format!(
                    "cli: OpenCode {} backend ready in {:.1}s (isolated, tools refused)",
                    backend.version,
                    started.elapsed().as_secs_f64()
                ));
                *self.version.lock().unwrap() = Some(backend.version.clone());
                backends.insert(key.to_string(), Arc::clone(&backend));
                Ok(backend)
            }
            Err(reason) => {
                if reason.starts_with("unsupported") {
                    self.usable.store(false, Ordering::SeqCst);
                }
                hub::error(format!("cli: {reason}"));
                Err(Failure::Unavailable(reason))
            }
        }
    }

    /// Stop backends that have sat unused, and forget dead ones.
    pub async fn reap_idle(&self) {
        let mut backends = self.backends.lock().await;
        backends.retain(|_, backend| {
            let idle = backend.active.load(Ordering::SeqCst) == 0
                && backend.last_used.lock().unwrap().elapsed() >= IDLE_FOR;
            if idle && backend.alive.load(Ordering::SeqCst) {
                hub::info("cli: stopping an idle OpenCode backend");
                backend.shutdown();
            }
            backend.alive.load(Ordering::SeqCst)
        });
    }

    pub async fn shutdown(&self) {
        for (_, backend) in self.backends.lock().await.drain() {
            backend.shutdown();
        }
    }

    /// Stop the backend of a key that was removed.
    pub async fn forget(&self, key: &str) {
        if let Some(backend) = self.backends.lock().await.remove(key) {
            backend.shutdown();
        }
    }

    /// Run one prompt in a throwaway session. Resolves once the answer has
    /// started (or failed before producing anything), so the caller can
    /// still move to another route; the rest arrives on the channel.
    pub async fn generate(
        &self,
        key: &str,
        model: &str,
        prompt: BridgePrompt,
        timeout: Duration,
    ) -> Result<mpsc::UnboundedReceiver<WireEvent>, Failure> {
        let backend = self.backend(key).await?;
        let deadline = tokio::time::Instant::now() + timeout;
        backend.active.fetch_add(1, Ordering::SeqCst);
        *backend.last_used.lock().unwrap() = Instant::now();
        let turn = SessionTurn::open(&backend, model, prompt).await;
        let turn = match turn {
            Ok(turn) => turn,
            Err(failure) => {
                backend.active.fetch_sub(1, Ordering::SeqCst);
                return Err(failure);
            }
        };
        let (out, receiver) = mpsc::unbounded_channel();
        let (started, first) = oneshot::channel();
        tokio::spawn(turn.run(out, started, deadline));
        match tokio::time::timeout_at(deadline, first).await {
            Ok(Ok(Ok(()))) => Ok(receiver),
            Ok(Ok(Err(failure))) => Err(failure),
            Ok(Err(_)) => Err(Failure::Upstream("the OpenCode backend stopped mid-request".to_string())),
            Err(_) => Err(Failure::Upstream("the OpenCode backend timed out".to_string())),
        }
    }
}

/// How a turn's event loop ended.
enum Ending {
    /// The agent loop finished (or the backend went away); read the result.
    Finished,
    /// The client stopped listening.
    Cancelled,
    TimedOut,
}

/// One request's session on a backend.
struct SessionTurn {
    backend: Arc<Backend>,
    session: String,
    events: mpsc::UnboundedReceiver<Value>,
}

impl SessionTurn {
    async fn open(backend: &Arc<Backend>, model: &str, prompt: BridgePrompt) -> Result<Self, Failure> {
        let unreachable = |(status, message): (u16, String)| match status {
            // OpenCode's own verdict on the request, e.g. an unknown model.
            400 | 404 | 409 | 422 => Failure::Rejected { status: 400, message },
            _ => Failure::Unavailable(format!("OpenCode backend: {message}")),
        };
        let create = backend
            .request(reqwest::Method::POST, "/api/session")
            .json(&json!({
                "title": "zenshim",
                "agent": AGENT,
                "model": { "id": model, "providerID": PROVIDER },
            }))
            .timeout(Duration::from_secs(30));
        let created = backend.json(create).await.map_err(unreachable)?;
        let Some(session) = created.pointer("/data/id").and_then(Value::as_str).map(String::from) else {
            return Err(Failure::Unavailable("OpenCode did not return a session id".to_string()));
        };
        let (listener, events) = mpsc::unbounded_channel();
        backend.sessions.lock().unwrap().insert(session.clone(), listener);
        let turn = Self { backend: Arc::clone(backend), session, events };

        // Session instructions are OpenCode's system channel. If this
        // (experimental) endpoint is gone, the text carries them instead.
        let mut text = prompt.text;
        if !prompt.system.is_empty() {
            let path = format!("/api/experimental/session/{}/instructions/entries/client-system", turn.session);
            let put = turn
                .backend
                .request(reqwest::Method::PUT, &path)
                .json(&json!({ "value": prompt.system }))
                .timeout(Duration::from_secs(15));
            if turn.backend.json(put).await.is_err() {
                text = format!("<system-instructions>\n{}\n</system-instructions>\n\n{text}", prompt.system);
            }
        }

        let files: Vec<Value> = prompt
            .attachments
            .iter()
            .enumerate()
            .map(|(i, (uri, name))| {
                let name = name.clone().unwrap_or_else(|| format!("attachment-{}", i + 1));
                json!({ "uri": uri, "name": name })
            })
            .collect();
        let mut body = json!({ "text": text });
        if !files.is_empty() {
            body["files"] = json!(files);
        }
        let send = turn
            .backend
            .request(reqwest::Method::POST, &format!("/api/session/{}/prompt", turn.session))
            .json(&body)
            .timeout(Duration::from_secs(60));
        if let Err(error) = turn.backend.json(send).await {
            turn.close(false).await;
            return Err(unreachable(error));
        }
        Ok(turn)
    }

    async fn messages(&self) -> Result<Value, (u16, String)> {
        let request = self
            .backend
            .request(reqwest::Method::GET, &format!("/api/session/{}/message", self.session))
            .timeout(Duration::from_secs(15));
        self.backend.json(request).await
    }

    /// Delete the session; `interrupt` first if it may still be running.
    async fn close(&self, interrupt: bool) {
        self.backend.sessions.lock().unwrap().remove(&self.session);
        if !self.backend.alive.load(Ordering::SeqCst) {
            return;
        }
        if interrupt {
            let path = format!("/api/session/{}/interrupt", self.session);
            let stop = self.backend.request(reqwest::Method::POST, &path);
            let _ = self.backend.json(stop.timeout(Duration::from_secs(5))).await;
        }
        let path = format!("/api/session/{}", self.session);
        let delete = self.backend.request(reqwest::Method::DELETE, &path);
        let _ = self.backend.json(delete.timeout(Duration::from_secs(10))).await;
    }

    async fn run(
        mut self,
        out: mpsc::UnboundedSender<WireEvent>,
        started: oneshot::Sender<Result<(), Failure>>,
        deadline: tokio::time::Instant,
    ) {
        let mut started = Some(started);
        let mut turn = Turn::default();
        // The agent loop going idle is the authoritative end of the turn;
        // events only make it stream.
        let wait_path = format!("/api/experimental/session/{}/wait", self.session);
        let wait = {
            let backend = Arc::clone(&self.backend);
            async move { backend.json(backend.request(reqwest::Method::POST, &wait_path)).await }
        };
        tokio::pin!(wait);
        let mut waiting = true;

        let ending = loop {
            tokio::select! {
                event = self.events.recv() => {
                    let Some(event) = event else { break Ending::Finished };
                    match turn.on_event(&event) {
                        Step::Delta(text) => {
                            if let Some(started) = started.take() {
                                let _ = started.send(Ok(()));
                            }
                            let data = json!({ "delta": text }).to_string();
                            if out.send(("response.output_text.delta".to_string(), data)).is_err() {
                                break Ending::Cancelled;
                            }
                        }
                        Step::Activity => {
                            // Keeps the caller's idle timer from firing while the model thinks.
                            if started.is_none() && out.send(("ping".to_string(), "{}".to_string())).is_err() {
                                break Ending::Cancelled;
                            }
                        }
                        Step::Ended => break Ending::Finished,
                    }
                }
                result = &mut wait, if waiting => {
                    waiting = false;
                    // An error here just means events alone decide.
                    if result.is_ok() {
                        break Ending::Finished;
                    }
                }
                _ = out.closed(), if started.is_none() => break Ending::Cancelled,
                _ = async { started.as_mut().unwrap().closed().await }, if started.is_some() => {
                    break Ending::Cancelled;
                }
                _ = tokio::time::sleep_until(deadline) => break Ending::TimedOut,
            }
        };

        let result = match ending {
            Ending::Cancelled => {
                self.finish(true).await;
                return;
            }
            Ending::TimedOut => Err(Failure::Upstream("the OpenCode backend timed out".to_string())),
            Ending::Finished => self.result(&mut turn, deadline).await,
        };
        let interrupt = matches!(ending, Ending::TimedOut);
        match result {
            Ok(answer) => {
                if let Some(started) = started.take() {
                    let _ = started.send(Ok(()));
                }
                if !answer.remainder.is_empty() {
                    let data = json!({ "delta": answer.remainder }).to_string();
                    let _ = out.send(("response.output_text.delta".to_string(), data));
                }
                let (input, output) = answer.usage.unwrap_or((0, 0));
                let data = match answer.usage {
                    Some(_) => json!({ "response": { "usage": { "input_tokens": input, "output_tokens": output } } }),
                    None => json!({ "response": {} }),
                };
                let event = if answer.truncated { "response.incomplete" } else { "response.completed" };
                let _ = out.send((event.to_string(), data.to_string()));
            }
            Err(failure) => match started.take() {
                Some(started) => {
                    let _ = started.send(Err(failure));
                }
                None => {
                    hub::warn(format!("cli: answer cut short: {}", failure.message()));
                    let _ = out.send(("response.failed".to_string(), json!({ "response": {} }).to_string()));
                }
            },
        }
        self.finish(interrupt).await;
    }

    /// The turn's result from the stored messages, once the session has
    /// settled (the wait can return a moment before the loop starts).
    async fn result(&self, turn: &mut Turn, deadline: tokio::time::Instant) -> Result<Answer, Failure> {
        let mut last = Value::Null;
        for _ in 0..40 {
            match self.messages().await {
                Ok(messages) if settled(&messages) => return turn.reconcile(&messages),
                Ok(messages) => last = messages,
                Err((_, message)) => {
                    if !self.backend.alive.load(Ordering::SeqCst) {
                        return Err(Failure::Unavailable(format!("OpenCode backend: {message}")));
                    }
                }
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        turn.reconcile(&last)
    }

    async fn finish(self, interrupt: bool) {
        self.close(interrupt).await;
        self.backend.active.fetch_sub(1, Ordering::SeqCst);
        *self.backend.last_used.lock().unwrap() = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(message: &str, text: &str) -> Value {
        json!({ "type": "session.text.delta", "data": { "sessionID": "ses_1", "assistantMessageID": message, "delta": text } })
    }

    fn assistant(text: &str, extra: Value) -> Value {
        let mut message = json!({
            "type": "assistant",
            "content": [{ "type": "reasoning", "text": "" }, { "type": "text", "text": text }],
            "finish": "stop",
        });
        for (key, value) in extra.as_object().unwrap() {
            message[key] = value.clone();
        }
        message
    }

    #[test]
    fn deltas_stream_and_messages_add_only_what_was_missed() {
        let mut turn = Turn::default();
        assert_eq!(turn.on_event(&delta("m1", "Hel")), Step::Delta("Hel".into()));
        assert_eq!(turn.on_event(&delta("m1", "lo")), Step::Delta("lo".into()));
        let messages = json!({ "data": [
            { "type": "idle", "outcome": "succeeded" },
            assistant("Hello world", json!({ "tokens": { "input": 100, "output": 5, "reasoning": 2, "cache": { "read": 20 } } })),
            { "type": "user", "text": "hi" },
        ]});
        let answer = turn.reconcile(&messages).unwrap();
        assert_eq!(answer, Answer { remainder: " world".into(), usage: Some((120, 7)), truncated: false });
    }

    #[test]
    fn nothing_streamed_means_the_whole_answer_comes_from_messages() {
        let mut turn = Turn::default();
        let messages = json!({ "data": [{ "type": "idle" }, assistant("pong", json!({}))] });
        assert_eq!(turn.reconcile(&messages).unwrap().remainder, "pong");
    }

    #[test]
    fn a_second_message_after_a_refused_tool_is_set_apart() {
        let mut turn = Turn::default();
        turn.on_event(&delta("m1", "Checking."));
        assert_eq!(turn.on_event(&delta("m2", "Done.")), Step::Delta("\n\nDone.".into()));
        // Newest first, as OpenCode lists them.
        let messages = json!({ "data": [
            { "type": "idle" },
            assistant("Done.", json!({})),
            assistant("Checking.", json!({ "finish": "tool-calls" })),
        ]});
        assert_eq!(turn.reconcile(&messages).unwrap().remainder, "");
    }

    #[test]
    fn other_events_are_activity_and_execution_end_ends_the_turn() {
        let mut turn = Turn::default();
        let event = |kind: &str| json!({ "type": kind, "data": { "sessionID": "ses_1" } });
        assert_eq!(turn.on_event(&event("session.reasoning.started")), Step::Activity);
        assert_eq!(turn.on_event(&event("session.execution.succeeded")), Step::Ended);
        assert_eq!(turn.on_event(&event("session.execution.failed")), Step::Ended);
    }

    #[test]
    fn upstream_errors_map_to_what_the_scheduler_should_do() {
        let failed = |status: u64, message: &str| {
            let error = json!({ "type": "provider", "message": message, "status": status });
            let messages = json!({ "data": [
                { "type": "idle", "outcome": "failed" },
                { "type": "assistant", "content": [], "finish": "error", "error": error },
            ]});
            Turn::default().reconcile(&messages).unwrap_err()
        };
        assert!(matches!(failed(429, "slow down"), Failure::Exhausted { .. }));
        assert!(matches!(failed(402, "out of credit"), Failure::Exhausted { .. }));
        assert!(matches!(failed(403, "free tier only from within OpenCode"), Failure::Gated(_)));
        assert!(matches!(failed(401, "Reconnect OpenCode Console"), Failure::Auth(_)));
        assert!(matches!(failed(400, "context too long"), Failure::Rejected { status: 400, .. }));
        assert!(matches!(failed(500, "boom"), Failure::Upstream(_)));
        assert!(matches!(classify(None, "Rate limit reached"), Failure::Exhausted { .. }));
    }

    #[test]
    fn an_error_after_text_truncates_instead_of_failing() {
        let mut turn = Turn::default();
        turn.on_event(&delta("m1", "Partial"));
        let messages = json!({ "data": [
            { "type": "idle", "outcome": "failed" },
            assistant("Partial", json!({ "finish": "error", "error": { "message": "boom", "status": 500 } })),
        ]});
        assert!(turn.reconcile(&messages).unwrap().truncated);
    }

    #[test]
    fn an_empty_session_is_a_failure_not_an_empty_answer() {
        let messages = json!({ "data": [{ "type": "user", "text": "hi" }] });
        assert!(!settled(&messages));
        assert!(Turn::default().reconcile(&messages).is_err());
    }

    #[test]
    fn the_listening_line_gives_the_port() {
        assert_eq!(listening_port("server listening on http://127.0.0.1:36689"), Some(36689));
        assert_eq!(listening_port("\u{1b}[32mopencode server listening on http://127.0.0.1:8 \u{1b}[0m"), Some(8));
        assert_eq!(listening_port("listening on http://0.0.0.0:80"), None);
        assert_eq!(listening_port("starting…"), None);
    }

    #[test]
    fn the_sandbox_gets_no_host_secrets() {
        let host = |name: &str| match name {
            "PATH" => Some("/usr/bin".to_string()),
            "HOME" => Some("/home/me".to_string()),
            "ANTHROPIC_API_KEY" | "ZENSHIM_GATEWAY_KEY" | "OPENCODE_API_KEY" => Some("secret".to_string()),
            _ => None,
        };
        let root = Path::new("/tmp/zenshim-oc-test");
        let env: HashMap<String, String> = sandbox_env(root, "pw", "public", host).into_iter().collect();
        let rooted = |name: &str| root.join(name).to_string_lossy().into_owned();
        assert_eq!(env["PATH"], "/usr/bin");
        assert_eq!(env["HOME"], rooted("home"));
        assert_eq!(env["XDG_CONFIG_HOME"], rooted("config"));
        assert!(!env.values().any(|value| value == "secret"));
        assert!(!env.contains_key("OPENCODE_API_KEY"), "anonymous tier sends no key");

        let keyed: HashMap<String, String> = sandbox_env(root, "pw", "sk-mine", host).into_iter().collect();
        assert_eq!(keyed["OPENCODE_API_KEY"], "sk-mine");
    }

    #[test]
    fn every_tool_permission_is_ask() {
        let config = backend_config();
        let permissions = config["permission"].as_object().unwrap();
        assert!(permissions.len() > 10);
        assert!(permissions.values().all(|effect| effect == "ask"));
    }
}
