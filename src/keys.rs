use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::hub;

/// How long an exhausted key route is left alone.
pub const EXHAUSTED_FOR: Duration = Duration::from_secs(24 * 3600);
/// Consecutive 403s from the direct wire before it is considered broken.
const GATE_THRESHOLD: u32 = 3;
/// How long the direct wire is skipped once broken, before one retry.
const GATE_BLOCK: Duration = Duration::from_secs(30 * 60);
/// When every route is cooling down, each may be re-tried this often.
const PROBE_INTERVAL: Duration = Duration::from_secs(60);

/// How a request reaches Zen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Route {
    /// zenshim's own imitation of the OpenCode wire format. Fast.
    Direct,
    /// A sandboxed `opencode serve`. Slower, but it is the real client.
    Cli,
}

impl Route {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Cli => "cli",
        }
    }
}

/// Which routes may be used at all (`--backend`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Direct first, the CLI when direct is exhausted or rejected.
    Auto,
    Direct,
    Cli,
}

impl Backend {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "direct" => Some(Self::Direct),
            "cli" => Some(Self::Cli),
            _ => None,
        }
    }

    fn allows(&self, route: Route) -> bool {
        match self {
            Self::Auto => true,
            Self::Direct => route == Route::Direct,
            Self::Cli => route == Route::Cli,
        }
    }
}

/// Why an upstream attempt did not produce an answer.
#[derive(Debug, Clone)]
pub enum Failure {
    /// Quota or rate limit (402/429): this route of this key rests.
    Exhausted { message: String, retry_after: Option<Duration> },
    /// Zen's free-tier gate refused the request shape (403).
    Gated(String),
    /// The key itself was refused (401).
    Auth(String),
    /// Upstream rejected the request itself (other 4xx); no other route
    /// would accept it either.
    Rejected { status: u16, message: String },
    /// Upstream or network trouble; another route may work.
    Upstream(String),
    /// The CLI backend could not be started or reached.
    Unavailable(String),
}

impl Failure {
    pub fn message(&self) -> &str {
        match self {
            Self::Exhausted { message, .. } | Self::Rejected { message, .. } => message,
            Self::Gated(m) | Self::Auth(m) | Self::Upstream(m) | Self::Unavailable(m) => m,
        }
    }
}

/// Health of one route of one key.
#[derive(Debug, Clone, Default)]
pub struct RouteState {
    pub served: u64,
    /// Exhausted until then.
    pub until: Option<SystemTime>,
    last_probe: Option<SystemTime>,
}

impl RouteState {
    /// Time left before this route is used again.
    pub fn cooling(&self, now: SystemTime) -> Option<Duration> {
        self.until.and_then(|until| until.duration_since(now).ok()).filter(|d| !d.is_zero())
    }
}

/// One upstream Zen key and how it has fared since startup.
#[derive(Debug, Clone)]
pub struct UpstreamKey {
    pub value: String,
    pub direct: RouteState,
    pub cli: RouteState,
    /// 402/429 responses (quota or rate limit).
    pub limited: u64,
    pub failed: u64,
    pub last_error: Option<String>,
}

impl UpstreamKey {
    pub fn new(value: String) -> Self {
        Self {
            value,
            direct: RouteState::default(),
            cli: RouteState::default(),
            limited: 0,
            failed: 0,
            last_error: None,
        }
    }

    pub fn served(&self) -> u64 {
        self.direct.served + self.cli.served
    }

    fn route_mut(&mut self, route: Route) -> &mut RouteState {
        match route {
            Route::Direct => &mut self.direct,
            Route::Cli => &mut self.cli,
        }
    }

    fn route(&self, route: Route) -> &RouteState {
        match route {
            Route::Direct => &self.direct,
            Route::Cli => &self.cli,
        }
    }
}

/// One thing to try for a request: a key over a route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    pub index: usize,
    pub key: String,
    pub route: Route,
}

#[derive(Default)]
struct Gate {
    consecutive: u32,
    blocked_until: Option<SystemTime>,
}

/// State of the direct wire as a whole, for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateState {
    Open,
    /// Rejected repeatedly; requests go through the CLI for this long.
    Blocked(Duration),
}

#[derive(Default, Serialize, Deserialize)]
struct SavedCooldown {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    direct: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cli: Option<u64>,
}

#[derive(Default, Serialize, Deserialize)]
struct SavedState {
    #[serde(default)]
    cooldowns: HashMap<String, SavedCooldown>,
}

/// A stable stand-in for a key in the state file, so the key itself is
/// stored in one place only.
fn fingerprint(key: &str) -> String {
    // FNV-1a: stable across builds, unlike the std hasher.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in key.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn epoch_secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The upstream keys and the policy for choosing between them:
///
/// - a key is used over the direct wire until that is exhausted (402/429),
///   then through the CLI, and when that is exhausted too the next key
///   takes over;
/// - an exhausted route rests for 24 hours (or what upstream's
///   `Retry-After` says), remembered across restarts;
/// - if the direct wire keeps being refused (403), everything goes
///   through the CLI and direct is retried every half hour;
/// - if everything is resting, the route due back soonest is tried
///   anyway, at most once a minute, so a wrong guess cannot lock the
///   gateway out for a day.
pub struct KeyPool {
    keys: RwLock<Vec<UpstreamKey>>,
    cursor: AtomicUsize,
    gate: Mutex<Gate>,
    mode: Backend,
    cli_ready: AtomicBool,
    state_path: Option<PathBuf>,
}

impl KeyPool {
    pub fn new(keys: Vec<String>, mode: Backend, state_path: Option<PathBuf>) -> Self {
        let pool = Self {
            keys: RwLock::new(keys.into_iter().map(UpstreamKey::new).collect()),
            cursor: AtomicUsize::new(0),
            gate: Mutex::new(Gate::default()),
            mode,
            cli_ready: AtomicBool::new(false),
            state_path,
        };
        pool.load();
        pool
    }

    pub fn mode(&self) -> Backend {
        self.mode
    }

    /// Whether an OpenCode CLI is available to fall back to.
    pub fn set_cli_ready(&self, ready: bool) {
        self.cli_ready.store(ready, Ordering::SeqCst);
    }

    pub fn cli_usable(&self) -> bool {
        self.mode.allows(Route::Cli) && self.cli_ready.load(Ordering::SeqCst)
    }

    pub fn snapshot(&self) -> Vec<UpstreamKey> {
        self.keys.read().unwrap().clone()
    }

    pub fn values(&self) -> Vec<String> {
        self.keys.read().unwrap().iter().map(|k| k.value.clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.keys.read().unwrap().len()
    }

    /// Add a key; false if it is already there.
    pub fn add(&self, value: String) -> bool {
        let mut keys = self.keys.write().unwrap();
        if keys.iter().any(|k| k.value == value) {
            return false;
        }
        keys.push(UpstreamKey::new(value));
        true
    }

    /// Remove a key; the last one stays.
    pub fn remove(&self, value: &str) -> bool {
        let mut keys = self.keys.write().unwrap();
        if keys.len() <= 1 || !keys.iter().any(|k| k.value == value) {
            return false;
        }
        keys.retain(|k| k.value != value);
        true
    }

    pub fn set_error(&self, value: &str, error: Option<String>) {
        if let Some(key) = self.keys.write().unwrap().iter_mut().find(|k| k.value == value) {
            key.last_error = error;
        }
    }

    /// Forget a key's cooldowns, making both routes usable now.
    pub fn reset(&self, value: &str) {
        if let Some(key) = self.keys.write().unwrap().iter_mut().find(|k| k.value == value) {
            key.direct.until = None;
            key.cli.until = None;
            key.last_error = None;
        }
        self.save();
    }

    pub fn gate(&self, now: SystemTime) -> GateState {
        let gate = self.gate.lock().unwrap();
        match gate.blocked_until.and_then(|until| until.duration_since(now).ok()) {
            Some(left) if !left.is_zero() && self.cli_usable() => GateState::Blocked(left),
            _ => GateState::Open,
        }
    }

    /// What to try for one request, in order.
    pub fn plan(&self, now: SystemTime) -> Vec<Attempt> {
        let gate = self.gate(now);
        let cli = self.cli_usable();
        let mut keys = self.keys.write().unwrap();
        let count = keys.len();
        if count == 0 {
            return Vec::new();
        }
        let start = self.cursor.load(Ordering::SeqCst) % count;
        let mut ready = Vec::new();
        // Routes that are resting, with when they are due back.
        let mut resting: Vec<(SystemTime, Attempt)> = Vec::new();
        for offset in 0..count {
            let index = (start + offset) % count;
            let key = &keys[index];
            for route in [Route::Direct, Route::Cli] {
                if !self.mode.allows(route) || (route == Route::Cli && !cli) {
                    continue;
                }
                let state = key.route(route);
                let attempt = Attempt { index, key: key.value.clone(), route };
                let gated = match gate {
                    GateState::Blocked(left) if route == Route::Direct => Some(now + left),
                    _ => None,
                };
                let due_back = match (state.cooling(now), gated) {
                    (None, None) => {
                        ready.push(attempt);
                        continue;
                    }
                    (Some(left), gated) => gated.map_or(now + left, |g| g.max(now + left)),
                    (None, Some(gated)) => gated,
                };
                let probed_recently = state
                    .last_probe
                    .and_then(|at| now.duration_since(at).ok())
                    .is_some_and(|since| since < PROBE_INTERVAL);
                if !probed_recently {
                    resting.push((due_back, attempt));
                }
            }
        }
        if ready.is_empty() {
            if let Some((_, attempt)) = resting.into_iter().min_by_key(|(due, _)| *due) {
                keys[attempt.index].route_mut(attempt.route).last_probe = Some(now);
                ready.push(attempt);
            }
        }
        ready
    }

    /// Earliest moment any route is due back, when none is usable now.
    pub fn next_available(&self, now: SystemTime) -> Option<Duration> {
        let keys = self.keys.read().unwrap();
        keys.iter()
            .flat_map(|k| [k.direct.cooling(now), k.cli.cooling(now)])
            .flatten()
            .min()
    }

    fn with_key(&self, attempt: &Attempt, apply: impl FnOnce(&mut UpstreamKey)) {
        // Keys can be edited mid-request, so find it by value.
        if let Some(key) = self.keys.write().unwrap().iter_mut().find(|k| k.value == attempt.key) {
            apply(key);
        }
    }

    pub fn succeeded(&self, attempt: &Attempt) {
        let count = self.len().max(1);
        self.cursor.store((attempt.index + 1) % count, Ordering::SeqCst);
        let mut was_resting = false;
        self.with_key(attempt, |key| {
            let state = key.route_mut(attempt.route);
            state.served += 1;
            was_resting = state.until.take().is_some();
            key.last_error = None;
        });
        if attempt.route == Route::Direct {
            let mut gate = self.gate.lock().unwrap();
            if gate.blocked_until.take().is_some() {
                hub::info("direct wire is accepted again — leaving the CLI fallback");
            }
            gate.consecutive = 0;
        }
        if was_resting {
            self.save();
        }
    }

    pub fn failed(&self, attempt: &Attempt, failure: &Failure, now: SystemTime) {
        let label = format!("key {} ({})", attempt.index + 1, attempt.route.label());
        match failure {
            Failure::Exhausted { message, retry_after } => {
                let rest = retry_after.unwrap_or(EXHAUSTED_FOR);
                self.with_key(attempt, |key| {
                    key.limited += 1;
                    key.last_error = Some(message.clone());
                    key.route_mut(attempt.route).until = Some(now + rest);
                });
                hub::warn(format!(
                    "{label} exhausted — resting {}: {message}",
                    describe(rest)
                ));
                self.save();
            }
            Failure::Gated(message) if attempt.route == Route::Direct => {
                let mut gate = self.gate.lock().unwrap();
                gate.consecutive += 1;
                if gate.consecutive >= GATE_THRESHOLD && self.cli_usable() {
                    if gate.blocked_until.is_none_or(|until| until <= now) {
                        hub::warn(format!(
                            "direct wire refused {} times in a row ({message}) — \
                             using the OpenCode CLI for {}",
                            gate.consecutive,
                            describe(GATE_BLOCK)
                        ));
                    }
                    gate.blocked_until = Some(now + GATE_BLOCK);
                }
                drop(gate);
                self.with_key(attempt, |key| key.last_error = Some(message.clone()));
            }
            other => {
                self.with_key(attempt, |key| {
                    key.failed += 1;
                    key.last_error = Some(other.message().to_string());
                });
                hub::warn(format!("{label} failed: {}", other.message()));
            }
        }
    }

    fn load(&self) {
        let Some(path) = &self.state_path else { return };
        let Ok(text) = std::fs::read_to_string(path) else { return };
        let Ok(saved) = serde_json::from_str::<SavedState>(&text) else { return };
        let at = |secs: Option<u64>| secs.map(|s| UNIX_EPOCH + Duration::from_secs(s));
        for key in self.keys.write().unwrap().iter_mut() {
            if let Some(cooldown) = saved.cooldowns.get(&fingerprint(&key.value)) {
                key.direct.until = at(cooldown.direct);
                key.cli.until = at(cooldown.cli);
            }
        }
    }

    fn save(&self) {
        let Some(path) = &self.state_path else { return };
        let now = SystemTime::now();
        let mut saved = SavedState::default();
        for key in self.keys.read().unwrap().iter() {
            let live = |state: &RouteState| state.cooling(now).and(state.until).map(epoch_secs);
            let cooldown = SavedCooldown { direct: live(&key.direct), cli: live(&key.cli) };
            if cooldown.direct.is_some() || cooldown.cli.is_some() {
                saved.cooldowns.insert(fingerprint(&key.value), cooldown);
            }
        }
        if let Ok(text) = serde_json::to_string_pretty(&saved) {
            let _ = crate::config::write_private(path, &text);
        }
    }
}

/// "24h", "30m", "45s" — for log lines and the Keys tab.
pub fn describe(duration: Duration) -> String {
    match duration.as_secs() {
        s @ 0..60 => format!("{s}s"),
        s @ 60..3600 => format!("{}m", s.div_ceil(60)),
        s => format!("{}h", s.div_ceil(3600)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(keys: &[&str], mode: Backend, cli: bool) -> KeyPool {
        let pool = KeyPool::new(keys.iter().map(|k| k.to_string()).collect(), mode, None);
        pool.set_cli_ready(cli);
        pool
    }

    fn routes(plan: &[Attempt]) -> Vec<(usize, Route)> {
        plan.iter().map(|a| (a.index, a.route)).collect()
    }

    fn exhausted() -> Failure {
        Failure::Exhausted { message: "quota".into(), retry_after: None }
    }

    #[test]
    fn direct_first_then_cli_then_next_key() {
        let pool = pool(&["a", "b"], Backend::Auto, true);
        let now = SystemTime::now();
        assert_eq!(
            routes(&pool.plan(now)),
            [(0, Route::Direct), (0, Route::Cli), (1, Route::Direct), (1, Route::Cli)]
        );
    }

    #[test]
    fn without_a_cli_only_direct_is_planned() {
        let pool = pool(&["a", "b"], Backend::Auto, false);
        assert_eq!(routes(&pool.plan(SystemTime::now())), [(0, Route::Direct), (1, Route::Direct)]);
    }

    #[test]
    fn exhausted_direct_falls_to_the_same_keys_cli_for_a_day() {
        let pool = pool(&["a", "b"], Backend::Auto, true);
        let now = SystemTime::now();
        let plan = pool.plan(now);
        pool.failed(&plan[0], &exhausted(), now);
        assert_eq!(
            routes(&pool.plan(now)),
            [(0, Route::Cli), (1, Route::Direct), (1, Route::Cli)]
        );
        // Still resting just before the day is over, back just after.
        let almost = now + EXHAUSTED_FOR - Duration::from_secs(1);
        assert_eq!(routes(&pool.plan(almost))[0], (0, Route::Cli));
        let after = now + EXHAUSTED_FOR + Duration::from_secs(1);
        assert_eq!(routes(&pool.plan(after))[0], (0, Route::Direct));
    }

    #[test]
    fn a_key_exhausted_both_ways_is_skipped() {
        let pool = pool(&["a", "b"], Backend::Auto, true);
        let now = SystemTime::now();
        let plan = pool.plan(now);
        pool.failed(&plan[0], &exhausted(), now);
        pool.failed(&plan[1], &exhausted(), now);
        assert_eq!(routes(&pool.plan(now)), [(1, Route::Direct), (1, Route::Cli)]);
    }

    #[test]
    fn retry_after_shortens_the_rest() {
        let pool = pool(&["a"], Backend::Direct, false);
        let now = SystemTime::now();
        let plan = pool.plan(now);
        let failure = Failure::Exhausted {
            message: "slow down".into(),
            retry_after: Some(Duration::from_secs(30)),
        };
        pool.failed(&plan[0], &failure, now);
        assert_eq!(pool.next_available(now), Some(Duration::from_secs(30)));
        assert_eq!(routes(&pool.plan(now + Duration::from_secs(31))), [(0, Route::Direct)]);
    }

    #[test]
    fn when_everything_rests_the_soonest_route_is_probed_once_a_minute() {
        let pool = pool(&["a"], Backend::Auto, true);
        let now = SystemTime::now();
        let plan = pool.plan(now);
        pool.failed(&plan[0], &exhausted(), now);
        let later = now + Duration::from_secs(10);
        pool.failed(&plan[1], &exhausted(), later);

        // Direct is due back first, so it is the one probed.
        assert_eq!(routes(&pool.plan(later)), [(0, Route::Direct)]);
        // Within the minute the other route gets its turn, then nothing.
        assert_eq!(routes(&pool.plan(later)), [(0, Route::Cli)]);
        assert!(pool.plan(later).is_empty());
        assert_eq!(routes(&pool.plan(later + PROBE_INTERVAL)), [(0, Route::Direct)]);
    }

    #[test]
    fn success_clears_the_rest_and_rotates_keys() {
        let pool = pool(&["a", "b"], Backend::Direct, false);
        let now = SystemTime::now();
        let plan = pool.plan(now);
        pool.failed(&plan[0], &exhausted(), now);
        pool.succeeded(&plan[0]);
        let keys = pool.snapshot();
        assert!(keys[0].direct.until.is_none());
        assert_eq!(keys[0].direct.served, 1);
        assert_eq!(routes(&pool.plan(now))[0], (1, Route::Direct));
    }

    #[test]
    fn repeated_403s_move_everything_to_the_cli() {
        let pool = pool(&["a", "b"], Backend::Auto, true);
        let now = SystemTime::now();
        let direct = pool.plan(now)[0].clone();
        for _ in 0..GATE_THRESHOLD {
            assert_eq!(pool.gate(now), GateState::Open);
            pool.failed(&direct, &Failure::Gated("free tier".into()), now);
        }
        assert!(matches!(pool.gate(now), GateState::Blocked(_)));
        assert_eq!(routes(&pool.plan(now)), [(0, Route::Cli), (1, Route::Cli)]);

        // Direct gets another chance after the block, and a success reopens it.
        let after = now + GATE_BLOCK + Duration::from_secs(1);
        assert_eq!(routes(&pool.plan(after))[0], (0, Route::Direct));
        pool.succeeded(&direct);
        assert_eq!(pool.gate(now), GateState::Open);
    }

    #[test]
    fn a_403_does_not_block_direct_without_a_cli() {
        let pool = pool(&["a"], Backend::Auto, false);
        let now = SystemTime::now();
        let direct = pool.plan(now)[0].clone();
        for _ in 0..GATE_THRESHOLD + 2 {
            pool.failed(&direct, &Failure::Gated("free tier".into()), now);
        }
        assert_eq!(routes(&pool.plan(now)), [(0, Route::Direct)]);
    }

    #[test]
    fn backend_mode_restricts_routes() {
        let cli = pool(&["a"], Backend::Cli, true);
        assert_eq!(routes(&cli.plan(SystemTime::now())), [(0, Route::Cli)]);
        let direct = pool(&["a"], Backend::Direct, true);
        assert_eq!(routes(&direct.plan(SystemTime::now())), [(0, Route::Direct)]);
    }

    #[test]
    fn the_last_key_cannot_be_removed() {
        let pool = pool(&["a", "b"], Backend::Auto, false);
        assert!(!pool.add("a".into()));
        assert!(pool.remove("a"));
        assert!(!pool.remove("b"));
        assert_eq!(pool.values(), ["b"]);
    }

    #[test]
    fn cooldowns_survive_a_restart() {
        let dir = std::env::temp_dir().join(format!("zenshim-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let now = SystemTime::now();
        {
            let pool = KeyPool::new(vec!["a".into()], Backend::Auto, Some(path.clone()));
            pool.set_cli_ready(true);
            let plan = pool.plan(now);
            pool.failed(&plan[0], &exhausted(), now);
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("\"a\""), "the key itself must not be written");
        let pool = KeyPool::new(vec!["a".into()], Backend::Auto, Some(path));
        pool.set_cli_ready(true);
        assert_eq!(routes(&pool.plan(now)), [(0, Route::Cli)]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
