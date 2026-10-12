use anyhow::{Context, Result};
use dialoguer::{Input, MultiSelect};
use serde::Deserialize;

use crate::config::Config;

pub const UPSTREAM_BASE: &str = "https://opencode.ai/zen/v1";
pub const ZEN_USER_AGENT: &str = "opencode/1.19.0 ai-sdk/provider-utils/4.0.40 runtime/bun/1.3.14";

#[derive(Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

pub fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(ZEN_USER_AGENT)
        .build()
        .context("build http client")
}

pub async fn fetch_models(client: &reqwest::Client, key: &str) -> Result<Vec<String>> {
    let response = client
        .get(format!("{UPSTREAM_BASE}/models"))
        .bearer_auth(key)
        .send()
        .await
        .context("request to /models failed")?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("GET /models returned {status}: {}", truncate(&text, 200));
    }
    let parsed: ModelsResponse = serde_json::from_str(&text).context("parse /models response")?;
    let mut ids: Vec<String> = parsed.data.into_iter().map(|m| m.id).collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

fn truncate(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Interactive first-run setup. Returns the saved config.
pub async fn run_wizard(client: &reqwest::Client, port: u16) -> Result<Config> {
    println!("zenshim setup — OpenAI-compatible gateway for OpenCode Zen's free tier\n");

    let keys_raw: String = Input::new()
        .with_prompt("Zen API keys, comma-separated (blank = anonymous free tier)")
        .allow_empty(true)
        .default(String::new())
        .interact_text()?;

    let mut keys: Vec<String> = keys_raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if keys.is_empty() {
        println!("using anonymous free tier (Bearer public)");
        keys.push("public".to_string());
    }

    // Validate each key against /models; drop the broken ones.
    let mut valid = Vec::new();
    for key in &keys {
        match fetch_models(client, key).await {
            Ok(_) => {
                println!("  key {}…{}: OK", &key[..key.len().min(6)], &key[key.len().saturating_sub(4)..]);
                valid.push(key.clone());
            }
            Err(e) => println!("  key {}…{}: INVALID ({e})", &key[..key.len().min(6)], &key[key.len().saturating_sub(4)..]),
        }
    }
    if valid.is_empty() {
        anyhow::bail!("no usable keys");
    }
    keys = valid;

    let all = fetch_models(client, &keys[0]).await?;
    let free: Vec<&String> = all.iter().filter(|id| id.contains("-free")).collect();
    println!("\n{} models total, {} free-tier models available", all.len(), free.len());

    let selections = if free.is_empty() {
        Vec::new()
    } else {
        let items: Vec<String> = free.iter().map(|s| (*s).clone()).collect();
        let defaults: Vec<bool> = items.iter().map(|_| true).collect();
        MultiSelect::new()
            .with_prompt("Models to serve (space toggles, enter confirms)")
            .items(&items)
            .defaults(&defaults)
            .interact()?
    };

    let models: Vec<String> = if selections.is_empty() {
        free.iter().map(|s| (*s).clone()).collect()
    } else {
        selections.into_iter().map(|i| free[i].clone()).collect()
    };
    if models.is_empty() {
        anyhow::bail!("no models selected");
    }
    println!("\nserving {} models", models.len());

    let gateway_key = Config::generate_gateway_key();
    let config = Config {
        keys,
        models,
        port,
        gateway_key,
    };
    config.save()?;
    println!("\nconfig saved to {}", Config::path().display());
    Ok(config)
}
