pub mod setup;
mod ui;

use std::cell::Cell;
use std::collections::HashSet;
use std::io::stdout;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::DefaultTerminal;
use tokio::sync::watch;

use crate::clipboard;
use crate::config::Config;
use crate::hub::{self, hub, RequestRecord, ReqState};
use crate::server::{SharedState, UpstreamKey};
use crate::tunnel::{Tunnel, TunnelStatus};
use crate::wizard;

/// Everything the TUI needs from the running gateway.
pub struct Context {
    pub state: SharedState,
    pub tunnel: Arc<Tunnel>,
    pub http: reqwest::Client,
    pub runtime: tokio::runtime::Handle,
    pub shutdown: watch::Receiver<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Dashboard,
    Requests,
    Models,
    Keys,
    Logs,
}

impl Tab {
    const ALL: [Tab; 5] = [Tab::Dashboard, Tab::Requests, Tab::Models, Tab::Keys, Tab::Logs];

    fn title(&self) -> &'static str {
        match self {
            Tab::Dashboard => "Dashboard",
            Tab::Requests => "Requests",
            Tab::Models => "Models",
            Tab::Keys => "Keys",
            Tab::Logs => "Logs",
        }
    }

    fn index(&self) -> usize {
        Self::ALL.iter().position(|t| t == self).unwrap_or(0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestFilter {
    All,
    Errors,
    Active,
}

impl RequestFilter {
    fn label(&self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Errors => "errors",
            Self::Active => "active",
        }
    }

    fn next(&self) -> Self {
        match self {
            Self::All => Self::Errors,
            Self::Errors => Self::Active,
            Self::Active => Self::All,
        }
    }

    fn matches(&self, record: &RequestRecord) -> bool {
        match self {
            Self::All => true,
            Self::Errors => record.is_error(),
            Self::Active => record.state == ReqState::Active,
        }
    }
}

enum Confirm {
    RotateGatewayKey,
    RemoveKey(String),
    StopTunnel,
}

impl Confirm {
    fn question(&self) -> String {
        match self {
            Self::RotateGatewayKey => {
                "Generate a new gateway key? Clients using the current one stop working.".to_string()
            }
            Self::RemoveKey(key) => format!("Remove upstream key {}?", mask_key(key, false)),
            Self::StopTunnel => "Stop the public tunnel? Its URL will not come back.".to_string(),
        }
    }
}

enum Mode {
    Normal,
    Help,
    /// Typing an upstream key (or several, comma-separated) to add.
    AddKey(String),
    /// Typing a filter for the Models tab; applies live.
    FilterModels,
    Confirm(Confirm),
}

enum Catalog {
    Loading,
    Ready(Vec<String>),
    Failed(String),
}

/// Results of background work, delivered to the UI thread.
enum Msg {
    Catalog(Result<Vec<String>, String>),
    KeyChecked {
        key: String,
        /// Add the key on success (otherwise it is a re-check of an existing one).
        add: bool,
        result: Result<(), String>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToastKind {
    Good,
    Bad,
}

struct Toast {
    text: String,
    kind: ToastKind,
    at: Instant,
}

/// One row of the Models tab.
struct ModelRow {
    id: String,
    served: bool,
    in_catalog: bool,
}

struct App {
    ctx: Context,
    tab: Tab,
    mode: Mode,
    toast: Option<Toast>,
    reveal_key: bool,
    quit: bool,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    /// Rows the active list can show; set while drawing, used for paging.
    page: Cell<usize>,

    /// Selected request id; `None` follows the newest.
    request_selected: Option<u64>,
    request_filter: RequestFilter,

    catalog: Catalog,
    model_cursor: usize,
    model_query: String,
    /// Show the whole catalog instead of free-tier and served models.
    models_all: bool,

    key_cursor: usize,
    checking: HashSet<String>,

    /// Lines scrolled up from the newest log line; 0 follows.
    log_scroll: usize,
}

pub fn mask_key(key: &str, reveal: bool) -> String {
    if key == "public" {
        return "public (anonymous free tier)".to_string();
    }
    if reveal {
        return key.to_string();
    }
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 14 {
        return "•".repeat(chars.len().max(8));
    }
    let head: String = chars[..10].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}••••••••{tail}")
}

/// Run the full-screen app until the user quits or the gateway shuts down.
pub fn run(context: Context) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let _ = execute!(stdout(), EnableBracketedPaste);
    let result = App::new(context).run(&mut terminal);
    let _ = execute!(stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

impl App {
    fn new(ctx: Context) -> Self {
        let (tx, rx) = mpsc::channel();
        let mut app = Self {
            ctx,
            tab: Tab::Dashboard,
            mode: Mode::Normal,
            toast: None,
            reveal_key: false,
            quit: false,
            tx,
            rx,
            page: Cell::new(10),
            request_selected: None,
            request_filter: RequestFilter::All,
            catalog: Catalog::Loading,
            model_cursor: 0,
            model_query: String::new(),
            models_all: false,
            key_cursor: 0,
            checking: HashSet::new(),
            log_scroll: 0,
        };
        app.refresh_catalog();
        app
    }

    fn run(mut self, terminal: &mut DefaultTerminal) -> anyhow::Result<()> {
        loop {
            while let Ok(msg) = self.rx.try_recv() {
                self.on_msg(msg);
            }
            terminal.draw(|frame| ui::draw(frame, &self))?;
            if event::poll(Duration::from_millis(100))? {
                loop {
                    match event::read()? {
                        Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key),
                        Event::Paste(text) => self.on_paste(&text),
                        _ => {}
                    }
                    if !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }
            if self.quit || *self.ctx.shutdown.borrow() {
                return Ok(());
            }
        }
    }

    // ── derived views ────────────────────────────────────────────────

    /// Requests passing the filter, newest first.
    fn visible_requests(&self) -> Vec<RequestRecord> {
        let mut records = hub().requests();
        records.retain(|r| self.request_filter.matches(r));
        records.reverse();
        records
    }

    fn request_index(&self, records: &[RequestRecord]) -> usize {
        self.request_selected
            .and_then(|id| records.iter().position(|r| r.id == id))
            .unwrap_or(0)
    }

    fn model_rows(&self) -> Vec<ModelRow> {
        let served = self.ctx.state.models();
        let catalog: &[String] = match &self.catalog {
            Catalog::Ready(ids) => ids,
            _ => &[],
        };
        let mut ids: Vec<&String> = catalog
            .iter()
            .filter(|id| self.models_all || id.contains("-free") || served.contains(id))
            .collect();
        ids.extend(served.iter().filter(|id| !catalog.contains(id)));
        ids.sort();
        let query = self.model_query.to_lowercase();
        ids.into_iter()
            .filter(|id| query.is_empty() || id.to_lowercase().contains(&query))
            .map(|id| ModelRow {
                id: id.clone(),
                served: served.contains(id),
                in_catalog: catalog.contains(id),
            })
            .collect()
    }

    fn openai_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.ctx.state.port)
    }

    fn anthropic_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.ctx.state.port)
    }

    // ── feedback ─────────────────────────────────────────────────────

    fn toast(&mut self, kind: ToastKind, text: impl Into<String>) {
        self.toast = Some(Toast {
            text: text.into(),
            kind,
            at: Instant::now(),
        });
    }

    fn good(&mut self, text: impl Into<String>) {
        self.toast(ToastKind::Good, text);
    }

    fn bad(&mut self, text: impl Into<String>) {
        self.toast(ToastKind::Bad, text);
    }

    /// Copy `value`; `secret` values are never echoed when no clipboard
    /// tool is available.
    fn copy(&mut self, what: &str, value: &str, secret: bool) {
        if clipboard::copy(value) {
            self.good(format!("{what} copied to clipboard"));
        } else if secret {
            self.bad("no clipboard tool found (xclip, xsel, wl-copy, pbcopy) — press K to reveal");
        } else {
            self.bad(format!("no clipboard tool found — {what}: {value}"));
        }
    }

    fn save(&mut self, apply: impl FnOnce(&mut Config, Config)) {
        if let Err(e) = self.ctx.state.persist(apply) {
            hub::error(format!("could not save config: {e}"));
            self.bad(format!("could not save config: {e}"));
        }
    }

    // ── background work ──────────────────────────────────────────────

    fn refresh_catalog(&mut self) {
        self.catalog = Catalog::Loading;
        let http = self.ctx.http.clone();
        let key = self.ctx.state.keys().first().map(|k| k.value.clone());
        let tx = self.tx.clone();
        self.ctx.runtime.spawn(async move {
            let result = match key {
                Some(key) => wizard::fetch_models(&http, &key).await.map_err(|e| e.to_string()),
                None => Err("no upstream key".to_string()),
            };
            let _ = tx.send(Msg::Catalog(result));
        });
    }

    fn check_key(&mut self, key: String, add: bool) {
        if !self.checking.insert(key.clone()) {
            return;
        }
        let http = self.ctx.http.clone();
        let tx = self.tx.clone();
        self.ctx.runtime.spawn(async move {
            let result = wizard::fetch_models(&http, &key)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::KeyChecked { key, add, result });
        });
    }

    fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Catalog(Ok(ids)) => self.catalog = Catalog::Ready(ids),
            Msg::Catalog(Err(e)) => {
                hub::warn(format!("could not fetch the model catalog: {e}"));
                self.catalog = Catalog::Failed(e);
            }
            Msg::KeyChecked { key, add, result } => {
                self.checking.remove(&key);
                let label = mask_key(&key, false);
                match (result, add) {
                    (Ok(()), true) => {
                        {
                            let mut keys = self.ctx.state.keys.write().unwrap();
                            if !keys.iter().any(|k| k.value == key) {
                                keys.push(UpstreamKey::new(key));
                            }
                        }
                        self.save(|saved, live| saved.keys = live.keys);
                        hub::info(format!("upstream key {label} added"));
                        self.good(format!("key {label} is valid — added"));
                    }
                    (Ok(()), false) => {
                        if let Some(k) = self.ctx.state.keys.write().unwrap().iter_mut().find(|k| k.value == key) {
                            k.last_error = None;
                        }
                        self.good(format!("key {label} is valid"));
                    }
                    (Err(e), add) => {
                        if !add {
                            if let Some(k) =
                                self.ctx.state.keys.write().unwrap().iter_mut().find(|k| k.value == key)
                            {
                                k.last_error = Some(e.clone());
                            }
                        }
                        hub::warn(format!("upstream key {label} rejected: {e}"));
                        self.bad(format!("key {label} rejected — details in Logs"));
                    }
                }
            }
        }
    }

    // ── input ────────────────────────────────────────────────────────

    fn on_paste(&mut self, text: &str) {
        let clean: String = text.chars().filter(|c| !c.is_control()).collect();
        match &mut self.mode {
            Mode::AddKey(value) => value.push_str(&clean),
            Mode::FilterModels => {
                self.model_query.push_str(&clean);
                self.model_cursor = 0;
            }
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => self.on_normal_key(key),
            Mode::Help => {}
            Mode::AddKey(value) => self.on_add_key(key, value),
            Mode::FilterModels => self.on_filter_key(key),
            Mode::Confirm(confirm) => {
                if matches!(key.code, KeyCode::Char('y' | 'Y')) {
                    self.confirmed(confirm);
                }
            }
        }
    }

    fn on_add_key(&mut self, key: KeyEvent, mut value: String) {
        match key.code {
            KeyCode::Esc => {}
            KeyCode::Enter => {
                let existing: Vec<String> =
                    self.ctx.state.keys().into_iter().map(|k| k.value).collect();
                let mut queued = 0;
                for candidate in value.split(',').map(str::trim).filter(|k| !k.is_empty()) {
                    if existing.iter().any(|k| k == candidate) {
                        self.bad(format!("key {} is already added", mask_key(candidate, false)));
                    } else {
                        self.check_key(candidate.to_string(), true);
                        queued += 1;
                    }
                }
                if queued > 0 {
                    self.good(format!("validating {queued} key(s)…"));
                }
            }
            KeyCode::Backspace => {
                value.pop();
                self.mode = Mode::AddKey(value);
            }
            KeyCode::Char(c) => {
                value.push(c);
                self.mode = Mode::AddKey(value);
            }
            _ => self.mode = Mode::AddKey(value),
        }
    }

    fn on_filter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.model_query.clear(),
            KeyCode::Enter => {}
            KeyCode::Backspace => {
                self.model_query.pop();
                self.mode = Mode::FilterModels;
            }
            KeyCode::Char(c) => {
                self.model_query.push(c);
                self.mode = Mode::FilterModels;
            }
            KeyCode::Up | KeyCode::Down => {
                self.mode = Mode::FilterModels;
                self.on_models_key(key);
                return;
            }
            _ => self.mode = Mode::FilterModels,
        }
        self.model_cursor = 0;
    }

    fn confirmed(&mut self, confirm: Confirm) {
        match confirm {
            Confirm::RotateGatewayKey => {
                let fresh = Config::generate_gateway_key();
                *self.ctx.state.gateway_key.write().unwrap() = fresh.clone();
                self.save(|saved, live| saved.gateway_key = live.gateway_key);
                hub::info("gateway key rotated");
                if clipboard::copy(&fresh) {
                    self.good("new gateway key generated and copied to clipboard");
                } else {
                    self.good("new gateway key generated — press K to reveal it");
                }
            }
            Confirm::RemoveKey(key) => {
                {
                    let mut keys = self.ctx.state.keys.write().unwrap();
                    if keys.len() <= 1 {
                        return;
                    }
                    keys.retain(|k| k.value != key);
                }
                self.save(|saved, live| saved.keys = live.keys);
                hub::info(format!("upstream key {} removed", mask_key(&key, false)));
                self.good("upstream key removed");
            }
            Confirm::StopTunnel => {
                self.ctx.tunnel.stop();
                self.good("tunnel stopped");
            }
        }
    }

    fn on_normal_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') | KeyCode::F(1) => self.mode = Mode::Help,
            KeyCode::Tab | KeyCode::Right => self.switch_tab(1),
            KeyCode::BackTab | KeyCode::Left => self.switch_tab(Tab::ALL.len() - 1),
            KeyCode::Char(c @ '1'..='5') => self.tab = Tab::ALL[c as usize - '1' as usize],
            KeyCode::Char('s') => self.tab = Tab::Dashboard,
            KeyCode::Char('m') => self.tab = Tab::Models,
            KeyCode::Char('k') => {
                let value = self.ctx.state.gateway_key();
                self.copy("api key", &value, true);
            }
            KeyCode::Char('K') => self.reveal_key = !self.reveal_key,
            KeyCode::Char('u') => {
                let value = self.openai_url();
                self.copy("base url", &value, false);
            }
            KeyCode::Char('U') => {
                let value = self.anthropic_url();
                self.copy("anthropic base url", &value, false);
            }
            KeyCode::Char('t') => match self.ctx.tunnel.status() {
                TunnelStatus::Up(url) => self.copy("tunnel url", &url, false),
                TunnelStatus::Starting(_) => self.bad("tunnel is still starting"),
                _ => self.bad("no tunnel active — press T to start one"),
            },
            KeyCode::Char('T') => match self.ctx.tunnel.status() {
                TunnelStatus::Off | TunnelStatus::Failed(_) => {
                    self.ctx.tunnel.start();
                    hub::warn(
                        "tunnel requested: the gateway key is the only protection on the public URL",
                    );
                    self.good("starting tunnel…");
                }
                _ => self.mode = Mode::Confirm(Confirm::StopTunnel),
            },
            KeyCode::Char('R') => self.mode = Mode::Confirm(Confirm::RotateGatewayKey),
            _ => match self.tab {
                Tab::Dashboard => {}
                Tab::Requests => self.on_requests_key(key),
                Tab::Models => self.on_models_key(key),
                Tab::Keys => self.on_keys_key(key),
                Tab::Logs => self.on_logs_key(key),
            },
        }
    }

    fn switch_tab(&mut self, step: usize) {
        self.tab = Tab::ALL[(self.tab.index() + step) % Tab::ALL.len()];
    }

    /// Move a list cursor for the navigation keys; `None` for other keys.
    fn moved(&self, key: KeyEvent, cursor: usize, len: usize) -> Option<usize> {
        let page = self.page.get().max(1);
        let last = len.saturating_sub(1);
        let target = match key.code {
            KeyCode::Up => cursor.saturating_sub(1),
            KeyCode::Down => cursor + 1,
            KeyCode::PageUp => cursor.saturating_sub(page),
            KeyCode::PageDown => cursor + page,
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => last,
            _ => return None,
        };
        Some(target.min(last))
    }

    fn on_requests_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('f') => {
                self.request_filter = self.request_filter.next();
                self.request_selected = None;
            }
            KeyCode::Char('x') => {
                hub().clear_requests();
                self.request_selected = None;
                self.good("finished requests cleared");
            }
            _ => {
                let records = self.visible_requests();
                let cursor = self.request_index(&records);
                if let Some(target) = self.moved(key, cursor, records.len()) {
                    // The top row is "follow the newest".
                    self.request_selected = match target {
                        0 => None,
                        index => records.get(index).map(|r| r.id),
                    };
                }
            }
        }
    }

    fn on_models_key(&mut self, key: KeyEvent) {
        let rows = self.model_rows();
        let cursor = self.model_cursor.min(rows.len().saturating_sub(1));
        match key.code {
            KeyCode::Char('/') => self.mode = Mode::FilterModels,
            KeyCode::Esc => self.model_query.clear(),
            KeyCode::Char('f') => {
                self.models_all = !self.models_all;
                self.model_cursor = 0;
            }
            KeyCode::Char('r') => self.refresh_catalog(),
            KeyCode::Char('a') => {
                let value = self.ctx.state.models().join(",");
                self.copy("all served model ids", &value, false);
            }
            KeyCode::Enter | KeyCode::Char('c') => {
                if let Some(row) = rows.get(cursor) {
                    let id = row.id.clone();
                    self.copy(&format!("model id {id}"), &id, false);
                }
            }
            KeyCode::Char(' ') => {
                let Some(row) = rows.get(cursor) else { return };
                let changed = {
                    let mut models = self.ctx.state.models.write().unwrap();
                    if !row.served {
                        models.push(row.id.clone());
                        models.sort();
                        true
                    } else if models.len() > 1 {
                        models.retain(|id| id != &row.id);
                        true
                    } else {
                        false
                    }
                };
                if changed {
                    self.save(|saved, live| saved.models = live.models);
                    let verb = if row.served { "no longer served" } else { "now served" };
                    hub::info(format!("model {} {verb}", row.id));
                    self.good(format!("{} {verb}", row.id));
                } else {
                    self.bad("at least one model must stay served");
                }
            }
            _ => {
                if let Some(target) = self.moved(key, cursor, rows.len()) {
                    self.model_cursor = target;
                }
            }
        }
    }

    fn on_keys_key(&mut self, key: KeyEvent) {
        let keys = self.ctx.state.keys();
        let cursor = self.key_cursor.min(keys.len().saturating_sub(1));
        match key.code {
            KeyCode::Char('a') => self.mode = Mode::AddKey(String::new()),
            KeyCode::Char('d') | KeyCode::Delete => {
                if keys.len() <= 1 {
                    self.bad("the last upstream key cannot be removed — add another first");
                } else if let Some(entry) = keys.get(cursor) {
                    self.mode = Mode::Confirm(Confirm::RemoveKey(entry.value.clone()));
                }
            }
            KeyCode::Char('v') => {
                if let Some(entry) = keys.get(cursor) {
                    self.check_key(entry.value.clone(), false);
                }
            }
            _ => {
                if let Some(target) = self.moved(key, cursor, keys.len()) {
                    self.key_cursor = target;
                }
            }
        }
    }

    fn on_logs_key(&mut self, key: KeyEvent) {
        let page = self.page.get().max(1);
        let max = hub().logs().len().saturating_sub(page);
        let target = match key.code {
            KeyCode::Up => self.log_scroll + 1,
            KeyCode::Down => self.log_scroll.saturating_sub(1),
            KeyCode::PageUp => self.log_scroll + page,
            KeyCode::PageDown => self.log_scroll.saturating_sub(page),
            KeyCode::Home | KeyCode::Char('g') => max,
            KeyCode::End | KeyCode::Char('G') => 0,
            _ => return,
        };
        self.log_scroll = target.min(max);
    }
}
