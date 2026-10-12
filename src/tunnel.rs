use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};


/// Shared handle for the tunnel URL (set once cloudflared reports it).
pub type TunnelUrl = Arc<Mutex<Option<String>>>;

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
fn resolve_cloudflared() -> Option<PathBuf> {
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
        println!("tunnel: darwin-arm64 requires a manual cloudflared install");
        println!("  https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/");
        return None;
    }

    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        let url = format!("{CLOUDFLARED_URL}{CLOUDFLARED_ASSET}");
        println!("tunnel: cloudflared not found — downloading {url}");
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
        let show_progress = std::io::stdout().is_terminal();
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
            if show_progress && total > 0 && downloaded - last_reported >= 262_144 {
                last_reported = downloaded;
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
            println!("tunnel: downloaded {:.1} MB", total as f64 / 1e6);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).ok()?;
        }
        println!("tunnel: cloudflared installed at {}", path.display());
        Some(path)
    }
}

pub struct TunnelGuard {
    child: Option<Child>,
}

impl TunnelGuard {
    pub fn none() -> Self {
        Self { child: None }
    }
}

impl Drop for TunnelGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
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

/// Start a cloudflared quick tunnel for the local port. Auto-installs
/// cloudflared on first use; no account or config needed.
pub fn start(port: u16, shared: TunnelUrl) -> (TunnelGuard, Option<String>) {
    let Some(cloudflared) = resolve_cloudflared() else {
        println!("tunnel: cloudflared unavailable — staying localhost-only");
        return (TunnelGuard::none(), None);
    };

    let mut child = match Command::new(&cloudflared)
        .args([
            "tunnel",
            "--url",
            &format!("http://127.0.0.1:{port}"),
            "--no-autoupdate",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            println!("tunnel: failed to start cloudflared ({e}) — staying localhost-only");
            return (TunnelGuard::none(), None);
        }
    };

    let stderr = child.stderr.take().expect("piped stderr");
    let watcher = Arc::clone(&shared);
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        // Drain stderr for the child's whole lifetime: once the URL is
        // found we stop parsing, but keep consuming so cloudflared's
        // pipe never fills and blocks the process (which kills the tunnel).
        for line in reader.lines().map_while(Result::ok) {
            let mut slot = watcher.lock().unwrap();
            if slot.is_none() {
                *slot = extract_url(&line);
            }
        }
    });

    // Wait up to 20s for the URL to appear.
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Some(url) = shared.lock().unwrap().clone() {
            println!("tunnel: {url}");
            return (
                TunnelGuard {
                    child: Some(child),
                },
                Some(url),
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    println!("tunnel: no URL received within 20s — staying localhost-only");
    let _ = child.kill();
    (TunnelGuard::none(), None)
}
