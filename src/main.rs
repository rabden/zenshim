mod config;
mod console;
mod ids;
mod server;
mod stream;
mod translate;
mod tunnel;
mod wizard;

use anyhow::Result;
use clap::Parser;
use std::io::IsTerminal;
use std::sync::{Arc, Mutex};

use crate::tunnel::TunnelUrl;

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

    /// Disable the interactive console (print summary and serve)
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

    let mut config = if cli.setup || (cli.no_wizard && config::Config::load().is_none() && cli.keys.is_none()) {
        None
    } else {
        config::Config::load()
    };

    if cli.setup || (config.is_none() && !cli.no_wizard && cli.keys.is_none() && cli.models.is_none()) {
        let port = cli.port;
        config = Some(wizard::run_wizard(&http, port).await?);
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

    if cli.tunnel || cli.host != "127.0.0.1" {
        println!(
            "warning: the gateway is reachable beyond localhost — the gateway \
             key is its only protection"
        );
    }

    let Some(formats) = server::format::ServeFormats::parse(&cli.format) else {
        anyhow::bail!("--format must be one of: openai, anthropic, both");
    };

    println!(
        "serving {} model(s) with {} upstream key(s)",
        config.models.len(),
        config.keys.len()
    );

    // Optional public tunnel.
    let tunnel_url: TunnelUrl = Arc::new(Mutex::new(None));
    let _tunnel_guard = if cli.tunnel {
        let (guard, url) = tunnel::start(config.port, Arc::clone(&tunnel_url));
        if url.is_some() {
            // cloudflared is already connecting; the server binds next.
        }
        guard
    } else {
        tunnel::TunnelGuard::none()
    };

    let base_url = format!("http://127.0.0.1:{}/v1", config.port);
    let console_state = console::ConsoleState {
        base_url,
        gateway_key: config.gateway_key.clone(),
        models: config.models.clone(),
        tunnel: Arc::clone(&tunnel_url),
    };

    // Shutdown: Ctrl-C or the console 'q' command.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let ctrl_c_tx = shutdown_tx.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        println!("\nshutting down");
        let _ = ctrl_c_tx.send(true);
    });

    let interactive = !cli.no_tui && std::io::stdin().is_terminal();
    if interactive {
        console::spawn(console_state, shutdown_tx);
    } else {
        // Give the tunnel a moment to report before printing the summary.
        for _ in 0..8 {
            if tunnel_url.lock().unwrap().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        console::print_plain(&console_state);
    }

    server::serve(config, &cli.host, formats, shutdown_rx).await
}
