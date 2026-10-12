mod config;
mod ids;
mod server;
mod stream;
mod translate;
mod wizard;

use anyhow::Result;
use clap::Parser;

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
            // Non-interactive path with no saved config: build from flags.
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

    println!(
        "serving {} model(s) with {} upstream key(s)",
        config.models.len(),
        config.keys.len()
    );
    server::serve(config).await
}
