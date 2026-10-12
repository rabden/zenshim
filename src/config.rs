use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Upstream Zen keys. "public" = anonymous free tier.
    pub keys: Vec<String>,
    /// Model ids to serve (as clients will name them).
    pub models: Vec<String>,
    pub port: u16,
    /// Key clients must present to this gateway.
    pub gateway_key: String,
}

impl Config {
    pub fn path() -> PathBuf {
        if let Ok(dir) = std::env::var("ZENSHIM_CONFIG_DIR") {
            return PathBuf::from(dir).join("config.json");
        }
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                PathBuf::from(home).join(".config")
            });
        base.join("zenshim").join("config.json")
    }

    pub fn load() -> Option<Config> {
        let path = Self::path();
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::path();
        std::fs::create_dir_all(path.parent().unwrap_or(&PathBuf::from(".")))?;
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn generate_gateway_key() -> String {
        use rand::Rng;
        let mut rng = rand::rng();
        let hex: String = (0..32)
            .map(|_| format!("{:02x}", rng.random::<u8>()))
            .collect();
        format!("sk-zenshim-{hex}")
    }
}
