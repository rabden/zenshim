mod clipboard;
mod config;
mod hub;
mod ids;
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
    about = "OpenAI-compatible local gateway for OpenCode Zen's free tier",
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
    #[arg(long)]
    tunnel: bool,

    /// Address to bind (default 127.0.0.1; use 0.0.0.0 to expose on LAN)
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Which API shapes to serve: openai, anthropic, or both
    #[arg(long, default_value = "both")]
    format: String,

    /// Comma-separated upstream Zen keys (blank or "public" = anonymous free tier)
    #[arg(long, value_delimiter = ',')]
    keys: Option<Vec<String>>,

    /// Comma-separated models to serve (overrides the saved selection)
    #[arg(long, value_delimiter = ',')]
    models: Option<Vec<String>>,

    /// Serve every model from the live catalog instead of the saved selection
    #[arg(long)]
    all_models: bool,

    /// Override the saved gateway key (what clients authenticate with)
    #[arg(long)]
    gateway_key: Option<String>,

    /// Port to listen on
    #[arg(long, default_value_t = 8480)]
    port: u16,

    /// Re-run the interactive setup wizard
    #[arg(long)]
    setup: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
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

    let Some(formats) = server::format::ServeFormats::parse(&cli.format) else {
        anyhow::bail!("--format must be one of: openai, anthropic, both");
    };

    // Bind before the TUI takes over the screen so a busy port is a
    // plain error message.
    let listener = server::bind(&cli.host, config.port).await?;
    let state = server::AppState::new(config, &cli.host, formats)?;

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
        state.keys().len()
    ));

    // Optional public tunnel; the TUI can also start and stop it later.
    let tunnel = Tunnel::new(state.port);
    if cli.tunnel {
        tunnel.start();
    }

    // Shutdown: Ctrl-C / SIGINT, or quitting the TUI.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let ctrl_c_tx = shutdown_tx.clone();
    tokio::spawn(async move {
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
