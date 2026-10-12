mod bridge;
mod clipboard;
mod config;
mod hub;
mod ids;
mod keys;
mod limits;
mod responses;
mod server;
mod stream;
mod translate;
mod tui;
mod tunnel;
mod wizard;

use anyhow::Result;
use clap::Parser;
use std::io::IsTerminal;
use std::time::Duration;

use crate::tunnel::{Tunnel, TunnelStatus};

#[derive(Parser)]
#[command(
    name = "zenshim",
    version,
    about = "Local OpenAI- and Anthropic-compatible gateway for OpenCode Zen's free tier",
    disable_help_subcommand = true
)]
struct Cli {
    /// Skip the setup wizard; use flags/env as-is
    #[arg(long)]
    no_wizard: bool,

    /// Disable the full-screen TUI (print a summary and serve)
    #[arg(long)]
    no_tui: bool,

    /// Expose the gateway publicly via a cloudflared quick tunnel
    /// (requires the cloudflared binary; no account needed)
    #[arg(long, env = "ZENSHIM_TUNNEL")]
    tunnel: bool,

    /// Address to bind (default 127.0.0.1; use 0.0.0.0 to expose on LAN)
    #[arg(long, env = "ZENSHIM_HOST", default_value = "127.0.0.1")]
    host: String,

    /// Which API shapes to serve: openai, anthropic, or both
    #[arg(long, default_value = "both")]
    format: String,

    /// Comma-separated upstream Zen keys (blank or "public" = anonymous free tier)
    #[arg(long, env = "ZENSHIM_KEYS", value_delimiter = ',', hide_env_values = true)]
    keys: Option<Vec<String>>,

    /// Comma-separated models to serve (overrides the saved selection)
    #[arg(long, env = "ZENSHIM_MODELS", value_delimiter = ',')]
    models: Option<Vec<String>>,

    /// Serve every model from the live catalog instead of the saved selection
    #[arg(long)]
    all_models: bool,

    /// Override the saved gateway key (what clients authenticate with)
    #[arg(long, env = "ZENSHIM_GATEWAY_KEY", hide_env_values = true)]
    gateway_key: Option<String>,

    /// Port to listen on
    #[arg(long, env = "ZENSHIM_PORT", default_value_t = 8480)]
    port: u16,

    /// Print the saved gateway key and exit
    #[arg(long)]
    show_key: bool,

    /// Re-run the interactive setup wizard
    #[arg(long)]
    setup: bool,

    /// How requests reach Zen: auto (direct wire, falling back to a
    /// sandboxed OpenCode CLI when direct is exhausted or refused),
    /// direct, or cli
    #[arg(long, env = "ZENSHIM_BACKEND", default_value = "auto")]
    backend: String,

    /// Path to the OpenCode CLI used for the fallback (default: found on PATH)
    #[arg(long, env = "ZENSHIM_OPENCODE")]
    opencode: Option<String>,

    /// Largest request body accepted, in MB
    #[arg(long, env = "ZENSHIM_MAX_BODY_MB", default_value_t = 32)]
    max_body_mb: usize,

    /// Requests per minute allowed per remote client (0 = unlimited;
    /// clients on this machine are never limited)
    #[arg(long, env = "ZENSHIM_RATE_LIMIT", default_value_t = 120)]
    rate_limit: u32,

    /// Generations that may run at once
    #[arg(long, env = "ZENSHIM_MAX_CONCURRENT", default_value_t = 16)]
    max_concurrent: usize,

    /// Requests that may wait for a free slot before new ones get 429
    #[arg(long, env = "ZENSHIM_MAX_QUEUE", default_value_t = 64)]
    max_queue: usize,

    /// Longest a single generation may take, in seconds
    #[arg(long, env = "ZENSHIM_REQUEST_TIMEOUT", default_value_t = 600)]
    request_timeout: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.show_key {
        let Some(saved) = config::Config::load() else {
            anyhow::bail!("no config at {} yet — start zenshim once first", config::Config::path().display());
        };
        println!("{}", saved.gateway_key);
        return Ok(());
    }
    let http = wizard::http_client()?;
    let interactive =
        !cli.no_tui && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();

    let mut config = if cli.setup || (cli.no_wizard && config::Config::load().is_none() && cli.keys.is_none()) {
        None
    } else {
        config::Config::load()
    };

    if cli.setup || (config.is_none() && !cli.no_wizard && cli.keys.is_none() && cli.models.is_none()) {
        let port = cli.port;
        config = if interactive {
            let runtime = tokio::runtime::Handle::current();
            let setup = tokio::task::block_in_place(|| tui::setup::run(runtime, http.clone(), port))?;
            let Some(config) = setup else {
                println!("setup cancelled");
                return Ok(());
            };
            Some(config)
        } else {
            Some(wizard::run_wizard(&http, port).await?)
        };
    }

    // Built from flags alone (services, containers): saved below so the
    // generated gateway key is the same on the next start.
    let created = config.is_none();
    let mut config = match config {
        Some(c) => c,
        None => {
            let keys = cli
                .keys
                .clone()
                .unwrap_or_else(|| vec!["public".to_string()]);
            let models = match (&cli.models, cli.all_models) {
                (Some(list), _) => list.clone(),
                (None, true) => wizard::fetch_models(&http, &keys[0]).await?,
                (None, false) => wizard::fetch_models(&http, &keys[0])
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|id| id.contains("-free"))
                    .collect::<Vec<_>>(),
            };
            if models.is_empty() {
                anyhow::bail!("no models available; pass --models or run interactively");
            }
            config::Config {
                keys,
                models,
                port: cli.port,
                gateway_key: config::Config::generate_gateway_key(),
            }
        }
    };

    // Flag overrides.
    if let Some(keys) = &cli.keys {
        config.keys = keys
            .iter()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .collect::<Vec<_>>();
        if config.keys.is_empty() {
            config.keys.push("public".to_string());
        }
    }
    if let Some(models) = &cli.models {
        config.models = models.clone();
    }
    if cli.all_models && cli.models.is_none() {
        config.models = wizard::fetch_models(&http, &config.keys[0]).await?;
    }
    if let Some(key) = &cli.gateway_key {
        config.gateway_key = key.clone();
    }
    config.port = cli.port;
    if created {
        match config.save() {
            Ok(()) => println!(
                "config saved to {} — `zenshim --show-key` prints the gateway key",
                config::Config::path().display()
            ),
            Err(e) => println!("warning: could not save the config ({e}); the gateway key will change on restart"),
        }
    }

    let Some(formats) = server::format::ServeFormats::parse(&cli.format) else {
        anyhow::bail!("--format must be one of: openai, anthropic, both");
    };

    // Bind before the TUI takes over the screen so a busy port is a
    // plain error message.
    let Some(backend) = keys::Backend::parse(&cli.backend) else {
        anyhow::bail!("--backend must be one of: auto, direct, cli");
    };
    let opencode = match backend {
        keys::Backend::Direct => None,
        _ => bridge::find_opencode(cli.opencode.as_deref()),
    };
    if opencode.is_none() && (backend == keys::Backend::Cli || cli.opencode.is_some()) {
        anyhow::bail!("the OpenCode CLI was not found; install it or pass --opencode <path>");
    }
    let options = server::Options {
        backend,
        opencode,
        limits: limits::Limits {
            max_body_bytes: cli.max_body_mb.max(1) * 1024 * 1024,
            rate_per_minute: cli.rate_limit,
            max_concurrent: cli.max_concurrent,
            max_queue: cli.max_queue,
            request_timeout: Duration::from_secs(cli.request_timeout.max(1)),
            ..limits::Limits::default()
        },
        state_path: Some(config::Config::state_path()),
    };

    let listener = server::bind(&cli.host, config.port).await?;
    let state = server::AppState::new(config, &cli.host, formats, options)?;

    // From here on log lines belong to the TUI's Logs tab.
    hub::hub().set_tui(interactive);
    if cli.tunnel || cli.host != "127.0.0.1" {
        hub::warn(
            "warning: the gateway is reachable beyond localhost — the gateway \
             key is its only protection",
        );
    }
    hub::info(format!(
        "serving {} model(s) with {} upstream key(s)",
        state.models().len(),
        state.pool.len()
    ));
    match (state.pool.mode(), state.bridge.binary()) {
        (keys::Backend::Direct, _) => hub::info("backend: direct wire only"),
        (keys::Backend::Cli, _) => hub::info("backend: OpenCode CLI only (sandboxed)"),
        (_, Some(path)) => hub::info(format!(
            "backend: direct wire, falling back to a sandboxed OpenCode CLI ({})",
            path.display()
        )),
        (_, None) => hub::info("backend: direct wire (no OpenCode CLI found, so no fallback)"),
    }

    // Optional public tunnel; the TUI can also start and stop it later.
    let tunnel = Tunnel::new(state.port);
    if cli.tunnel {
        tunnel.start();
    }

    // Shutdown: Ctrl-C / SIGINT, or quitting the TUI.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let ctrl_c_tx = shutdown_tx.clone();
    tokio::spawn(async move {
        // SIGTERM too, so service managers get the same clean stop.
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            match signal(SignalKind::terminate()) {
                Ok(mut terminate) => tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                },
                Err(_) => {
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
        hub::info("shutting down");
        let _ = ctrl_c_tx.send(true);
    });

    if !interactive {
        // Give the tunnel time to come up (or give up) before the summary.
        while matches!(tunnel.status(), TunnelStatus::Starting(_)) && !*shutdown_rx.borrow() {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        print_plain(&state, &tunnel);
        let served = server::serve(listener, state, shutdown_rx).await;
        tunnel.stop();
        return served;
    }

    // The server stopping for any reason also closes the TUI.
    let server = tokio::spawn({
        let state = state.clone();
        let rx = shutdown_rx.clone();
        let tx = shutdown_tx.clone();
        async move {
            let served = server::serve(listener, state, rx).await;
            let _ = tx.send(true);
            served
        }
    });
    let ui = tokio::task::spawn_blocking({
        let context = tui::Context {
            state,
            tunnel: tunnel.clone(),
            http,
            runtime: tokio::runtime::Handle::current(),
            shutdown: shutdown_rx,
        };
        move || tui::run(context)
    });
    let ui_result = ui.await;
    hub::hub().set_tui(false);
    let _ = shutdown_tx.send(true);
    tunnel.stop();
    // Open streams can hold graceful shutdown up; don't wait on them forever.
    let served = tokio::time::timeout(Duration::from_secs(3), server).await;
    ui_result??;
    if let Ok(served) = served {
        served??;
    }
    Ok(())
}

/// Non-interactive summary for pipes/scripts/TTY-less environments.
fn print_plain(state: &server::AppState, tunnel: &Tunnel) {
    println!("base url : http://127.0.0.1:{}/v1", state.port);
    if let Some(url) = tunnel.url() {
        println!("tunnel   : {url} (public)");
    }
    println!("api key  : hidden — saved in the config; pass --gateway-key to override");
    println!("models   : {}", state.models().join(", "));
}
