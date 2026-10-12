use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::stream::ApiFormat;

const MAX_LOGS: usize = 1000;
const MAX_REQUESTS: usize = 500;
/// Seconds of per-second request counts kept for the traffic sparkline.
pub const TIMELINE_SECS: usize = 240;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone)]
pub struct LogLine {
    /// Time since startup.
    pub at: Duration,
    pub level: Level,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReqState {
    Active,
    Done(u16),
}

#[derive(Debug, Clone)]
pub struct RequestRecord {
    pub id: u64,
    pub started: Instant,
    pub format: ApiFormat,
    pub model: String,
    pub client: String,
    pub stream: bool,
    pub messages: usize,
    pub tools: usize,
    /// Index of the upstream key that served it.
    pub key: Option<usize>,
    pub state: ReqState,
    pub duration: Option<Duration>,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tool_calls: usize,
    /// Finish reason on success, error message otherwise.
    pub note: String,
}

impl RequestRecord {
    pub fn is_error(&self) -> bool {
        matches!(self.state, ReqState::Done(status) if status >= 400)
    }

    pub fn elapsed(&self) -> Duration {
        self.duration.unwrap_or_else(|| self.started.elapsed())
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Totals {
    pub requests: u64,
    pub completed: u64,
    pub errors: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Summed duration of successful requests, for the average.
    pub ok_millis: u64,
    pub ok: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ModelStat {
    pub requests: u64,
    pub tokens_out: u64,
}

struct Timeline {
    /// One slot per second; the back slot is `head_sec`.
    slots: VecDeque<u64>,
    head_sec: u64,
}

impl Timeline {
    fn advance(&mut self, now_sec: u64) {
        let gap = now_sec.saturating_sub(self.head_sec).min(TIMELINE_SECS as u64);
        for _ in 0..gap {
            self.slots.pop_front();
            self.slots.push_back(0);
        }
        self.head_sec = self.head_sec.max(now_sec);
    }
}

/// Process-wide activity sink: log lines and request telemetry. The
/// server and tunnel write to it; the TUI reads snapshots each frame.
/// Without the TUI, log lines go straight to stdout.
pub struct Hub {
    started: Instant,
    tui: AtomicBool,
    next_id: AtomicU64,
    logs: Mutex<VecDeque<LogLine>>,
    requests: Mutex<VecDeque<RequestRecord>>,
    totals: Mutex<Totals>,
    models: Mutex<HashMap<String, ModelStat>>,
    timeline: Mutex<Timeline>,
}

static HUB: LazyLock<Hub> = LazyLock::new(|| Hub {
    started: Instant::now(),
    tui: AtomicBool::new(false),
    next_id: AtomicU64::new(1),
    logs: Mutex::new(VecDeque::new()),
    requests: Mutex::new(VecDeque::new()),
    totals: Mutex::new(Totals::default()),
    models: Mutex::new(HashMap::new()),
    timeline: Mutex::new(Timeline {
        slots: std::iter::repeat_n(0, TIMELINE_SECS).collect(),
        head_sec: 0,
    }),
});

pub fn hub() -> &'static Hub {
    &HUB
}

pub fn info(text: impl Into<String>) {
    hub().log(Level::Info, text.into());
}

pub fn warn(text: impl Into<String>) {
    hub().log(Level::Warn, text.into());
}

pub fn error(text: impl Into<String>) {
    hub().log(Level::Error, text.into());
}

/// What a handler knows about a request when it arrives.
pub struct RequestInfo {
    pub format: ApiFormat,
    pub model: String,
    pub client: String,
    pub stream: bool,
    pub messages: usize,
    pub tools: usize,
}

/// Client-supplied text ends up on screen: drop control characters and
/// cap the length.
fn printable(text: &str, max: usize) -> String {
    text.chars().filter(|c| !c.is_control()).take(max).collect()
}

impl Hub {
    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    /// While the TUI owns the terminal, log lines are only buffered.
    pub fn set_tui(&self, active: bool) {
        self.tui.store(active, Ordering::SeqCst);
    }

    pub fn tui_active(&self) -> bool {
        self.tui.load(Ordering::SeqCst)
    }

    fn log(&self, level: Level, text: String) {
        if !self.tui_active() {
            println!("{text}");
        }
        let mut logs = self.logs.lock().unwrap();
        if logs.len() == MAX_LOGS {
            logs.pop_front();
        }
        logs.push_back(LogLine {
            at: self.uptime(),
            level,
            text,
        });
    }

    pub fn logs(&self) -> Vec<LogLine> {
        self.logs.lock().unwrap().iter().cloned().collect()
    }

    /// Requests, oldest first.
    pub fn requests(&self) -> Vec<RequestRecord> {
        self.requests.lock().unwrap().iter().cloned().collect()
    }

    pub fn totals(&self) -> Totals {
        *self.totals.lock().unwrap()
    }

    pub fn model_stats(&self) -> HashMap<String, ModelStat> {
        self.models.lock().unwrap().clone()
    }

    /// Requests per second, oldest first, ending at the current second.
    pub fn timeline(&self) -> Vec<u64> {
        let mut timeline = self.timeline.lock().unwrap();
        timeline.advance(self.uptime().as_secs());
        timeline.slots.iter().copied().collect()
    }

    pub fn clear_requests(&self) {
        self.requests
            .lock()
            .unwrap()
            .retain(|r| r.state == ReqState::Active);
    }

    pub fn begin(&self, info: RequestInfo) -> Tracker {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let record = RequestRecord {
            id,
            started: Instant::now(),
            format: info.format,
            model: printable(&info.model, 80),
            client: printable(&info.client, 45),
            stream: info.stream,
            messages: info.messages,
            tools: info.tools,
            key: None,
            state: ReqState::Active,
            duration: None,
            tokens_in: 0,
            tokens_out: 0,
            tool_calls: 0,
            note: String::new(),
        };
        {
            let mut requests = self.requests.lock().unwrap();
            if requests.len() == MAX_REQUESTS {
                requests.pop_front();
            }
            requests.push_back(record);
        }
        self.totals.lock().unwrap().requests += 1;
        {
            let mut timeline = self.timeline.lock().unwrap();
            timeline.advance(self.uptime().as_secs());
            if let Some(slot) = timeline.slots.back_mut() {
                *slot += 1;
            }
        }
        Tracker { id, done: false }
    }

    fn update(&self, id: u64, apply: impl FnOnce(&mut RequestRecord)) -> Option<RequestRecord> {
        let mut requests = self.requests.lock().unwrap();
        let record = requests.iter_mut().rev().find(|r| r.id == id)?;
        apply(record);
        Some(record.clone())
    }
}

/// Outcome of a served request, reported once by its [`Tracker`].
pub struct Outcome {
    pub status: u16,
    pub note: String,
    /// (input, output) tokens when upstream reported usage.
    pub usage: Option<(u64, u64)>,
    pub tool_calls: usize,
}

impl Outcome {
    pub fn error(status: u16, note: impl Into<String>) -> Self {
        Self {
            status,
            note: note.into(),
            usage: None,
            tool_calls: 0,
        }
    }
}

/// Handle on an in-flight request. Dropping it unfinished (the client
/// went away and axum dropped the handler) records a 499.
pub struct Tracker {
    id: u64,
    done: bool,
}

impl Tracker {
    pub fn key(&self, index: usize) {
        hub().update(self.id, |r| r.key = Some(index));
    }

    pub fn finish(mut self, outcome: Outcome) {
        self.done = true;
        Self::record(self.id, outcome);
    }

    fn record(id: u64, outcome: Outcome) {
        let hub = hub();
        let (tokens_in, tokens_out) = outcome.usage.unwrap_or((0, 0));
        let failed = outcome.status >= 400;
        let record = hub.update(id, |r| {
            r.state = ReqState::Done(outcome.status);
            r.duration = Some(r.started.elapsed());
            r.tokens_in = tokens_in;
            r.tokens_out = tokens_out;
            r.tool_calls = outcome.tool_calls;
            r.note = printable(&outcome.note, 400);
        });
        {
            let mut totals = hub.totals.lock().unwrap();
            totals.completed += 1;
            totals.tokens_in += tokens_in;
            totals.tokens_out += tokens_out;
            if failed {
                totals.errors += 1;
            } else if let Some(record) = &record {
                totals.ok += 1;
                totals.ok_millis += record.elapsed().as_millis() as u64;
            }
        }
        let Some(record) = record else { return };
        if failed {
            if outcome.status != 499 {
                hub.log(
                    Level::Warn,
                    format!("request #{id} {} → {}: {}", record.model, outcome.status, record.note),
                );
            }
        } else {
            let mut models = hub.models.lock().unwrap();
            let stat = models.entry(record.model.clone()).or_default();
            stat.requests += 1;
            stat.tokens_out += tokens_out;
        }
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        if !self.done {
            Self::record(self.id, Outcome::error(499, "client disconnected"));
        }
    }
}
