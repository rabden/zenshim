use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Shared handle for the tunnel URL (set once cloudflared reports it).
pub type TunnelUrl = Arc<Mutex<Option<String>>>;

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

/// Start a cloudflared quick tunnel for the local port. Requires the
/// `cloudflared` binary; no account or config needed.
pub fn start(port: u16, shared: TunnelUrl) -> (TunnelGuard, Option<String>) {
    if Command::new("cloudflared")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        println!("tunnel: cloudflared not found — staying localhost-only");
        println!("  install it from https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/");
        return (TunnelGuard::none(), None);
    }

    let mut child = match Command::new("cloudflared")
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
