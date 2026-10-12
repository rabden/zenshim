use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::hub;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const CLOUDFLARED_ASSET: &str = "cloudflared-linux-amd64";
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const CLOUDFLARED_ASSET: &str = "cloudflared-linux-arm64";
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const CLOUDFLARED_ASSET: &str = "cloudflared-darwin-amd64";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const CLOUDFLARED_ASSET: &str = "cloudflared-darwin-arm64.tgz";
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
const CLOUDFLARED_ASSET: &str = "cloudflared-windows-amd64.exe";

const CLOUDFLARED_URL: &str = "https://github.com/cloudflare/cloudflared/releases/latest/download/";

fn cloudflared_cache_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("ZENSHIM_CLOUDFLARED") {
        return Some(PathBuf::from(explicit));
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".local/share/zenshim/cloudflared"))
}

fn cloudflared_exists_on_path() -> bool {
    Command::new("cloudflared")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// Resolve a cloudflared binary: env override → PATH → cached copy →
/// download from the official release into the cache (auto-install).
fn resolve_cloudflared(progress: &dyn Fn(String)) -> Option<PathBuf> {
    if let Some(path) = cloudflared_cache_path() {
        if path.exists() {
            return Some(path);
        }
    }
    if cloudflared_exists_on_path() {
        return Some(PathBuf::from("cloudflared"));
    }
    let path = cloudflared_cache_path()?;

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        // The darwin-arm64 release ships a .tgz; extraction is not wired up.
        let _ = progress;
        hub::warn("tunnel: darwin-arm64 requires a manual cloudflared install");
        hub::warn("  https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/");
        return None;
    }

    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        let url = format!("{CLOUDFLARED_URL}{CLOUDFLARED_ASSET}");
        hub::info(format!("tunnel: cloudflared not found — downloading {url}"));
        progress("downloading cloudflared".to_string());
        let client = reqwest::blocking::Client::builder()
            .user_agent(crate::wizard::ZEN_USER_AGENT)
            .build()
            .ok()?;
        let mut response = client.get(&url).send().ok()?.error_for_status().ok()?;
        let total = response.content_length().unwrap_or(0);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok()?;
        }
        let mut file = std::fs::File::create(&path).ok()?;
        let mut downloaded: u64 = 0;
        let show_progress = !hub::hub().tui_active() && std::io::stdout().is_terminal();
        let mut last_reported: u64 = 0;
        let mut buffer = [0u8; 65536];
        use std::io::Read;
        loop {
            let read = response.read(&mut buffer).ok()?;
            if read == 0 {
                break;
            }
            file.write_all(&buffer[..read]).ok()?;
            downloaded += read as u64;
            if total > 0 && downloaded - last_reported >= 262_144 {
                last_reported = downloaded;
                progress(format!(
                    "downloading cloudflared {:.1} / {:.1} MB",
                    downloaded as f64 / 1e6,
                    total as f64 / 1e6
                ));
            }
            if show_progress && total > 0 && last_reported == downloaded {
                print!(
                    "\r  downloaded {:.1} / {:.1} MB",
                    downloaded as f64 / 1e6,
                    total as f64 / 1e6
                );
                let _ = std::io::stdout().flush();
            }
        }
        if show_progress {
            println!();
        } else if total > 0 {
            hub::info(format!("tunnel: downloaded {:.1} MB", total as f64 / 1e6));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).ok()?;
        }
        hub::info(format!("tunnel: cloudflared installed at {}", path.display()));
        Some(path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelStatus {
    Off,
    /// Resolving/downloading cloudflared or waiting for its URL.
    Starting(String),
    Up(String),
    Failed(String),
}

struct Inner {
    status: TunnelStatus,
    child: Option<Child>,
    /// Bumped on every start/stop so stale worker threads stand down.
    generation: u64,
}

/// A cloudflared quick tunnel for the local port that can be started
/// and stopped at runtime. Auto-installs cloudflared on first use; no
/// account or config needed.
pub struct Tunnel {
    port: u16,
    inner: Mutex<Inner>,
}

impl Tunnel {
    pub fn new(port: u16) -> Arc<Self> {
        Arc::new(Self {
            port,
            inner: Mutex::new(Inner {
                status: TunnelStatus::Off,
                child: None,
                generation: 0,
            }),
        })
    }

    pub fn status(&self) -> TunnelStatus {
        self.inner.lock().unwrap().status.clone()
    }

    pub fn url(&self) -> Option<String> {
        match self.status() {
            TunnelStatus::Up(url) => Some(url),
            _ => None,
        }
    }

    /// Set the status if `generation` is still the live run.
    fn transition(&self, generation: u64, status: TunnelStatus) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if inner.generation != generation {
            return false;
        }
        inner.status = status;
        true
    }

    fn fail(&self, generation: u64, reason: &str) {
        let mut inner = self.inner.lock().unwrap();
        if inner.generation != generation {
            return;
        }
        if let Some(mut child) = inner.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        inner.status = TunnelStatus::Failed(reason.to_string());
        drop(inner);
        hub::warn(format!("tunnel: {reason} — staying localhost-only"));
    }

    /// Start the tunnel in the background; a no-op while one is
    /// starting or up. Progress is visible through [`Tunnel::status`].
    pub fn start(self: &Arc<Self>) {
        let generation = {
            let mut inner = self.inner.lock().unwrap();
            if matches!(inner.status, TunnelStatus::Starting(_) | TunnelStatus::Up(_)) {
                return;
            }
            inner.generation += 1;
            inner.status = TunnelStatus::Starting("locating cloudflared".to_string());
            inner.generation
        };
        let tunnel = Arc::clone(self);
        std::thread::spawn(move || tunnel.run(generation));
    }

    fn run(self: Arc<Self>, generation: u64) {
        let progress = |message: String| {
            self.transition(generation, TunnelStatus::Starting(message));
        };
        let Some(cloudflared) = resolve_cloudflared(&progress) else {
            self.fail(generation, "cloudflared unavailable");
            return;
        };
        if !self.transition(generation, TunnelStatus::Starting("connecting".to_string())) {
            return;
        }

        let mut child = match Command::new(&cloudflared)
            .args([
                "tunnel",
                "--url",
                &format!("http://127.0.0.1:{}", self.port),
                "--no-autoupdate",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                self.fail(generation, &format!("failed to start cloudflared ({e})"));
                return;
            }
        };
        let stderr = child.stderr.take().expect("piped stderr");
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.generation != generation {
                // Stopped while cloudflared was being resolved.
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            inner.child = Some(child);
        }

        let watcher = Arc::clone(&self);
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            let mut found = false;
            // Drain stderr for the child's whole lifetime: once the URL is
            // found we stop parsing, but keep consuming so cloudflared's
            // pipe never fills and blocks the process (which kills the tunnel).
            for line in reader.lines().map_while(Result::ok) {
                if found {
                    continue;
                }
                if let Some(url) = extract_url(&line) {
                    found = true;
                    if watcher.transition(generation, TunnelStatus::Up(url.clone())) {
                        hub::info(format!("tunnel: {url}"));
                    }
                }
            }
            // EOF: cloudflared exited. Only news if nobody asked for it.
            let live = {
                let inner = watcher.inner.lock().unwrap();
                inner.generation == generation
                    && matches!(inner.status, TunnelStatus::Starting(_) | TunnelStatus::Up(_))
            };
            if live {
                watcher.fail(generation, "cloudflared exited");
            }
        });

        // Wait up to 20s for the URL to appear.
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            let inner = self.inner.lock().unwrap();
            if inner.generation != generation || !matches!(inner.status, TunnelStatus::Starting(_)) {
                return;
            }
            drop(inner);
            std::thread::sleep(Duration::from_millis(250));
        }
        let waiting = {
            let inner = self.inner.lock().unwrap();
            inner.generation == generation && matches!(inner.status, TunnelStatus::Starting(_))
        };
        if waiting {
            self.fail(generation, "no URL received within 20s");
        }
    }

    pub fn stop(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.generation += 1;
        let was_running = inner.status != TunnelStatus::Off;
        inner.status = TunnelStatus::Off;
        if let Some(mut child) = inner.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        drop(inner);
        if was_running {
            hub::info("tunnel: stopped");
        }
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        if let Some(mut child) = self.inner.get_mut().unwrap().child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn extract_url(line: &str) -> Option<String> {
    let mut rest = line;
    while let Some(pos) = rest.find("https://") {
        let after = &rest[pos..];
        let end = after
            .find(|c: char| c.is_whitespace() || c == '|')
            .unwrap_or(after.len());
        let url = &after[..end];
        if url.contains(".trycloudflare.com") {
            return Some(url.to_string());
        }
        rest = &after[end..];
    }
    None
}
