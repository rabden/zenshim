use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Write a file only its owner can read (it holds keys), replacing any
/// previous version atomically so a crash never leaves half a file.
pub fn write_private(path: &Path, contents: &str) -> anyhow::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let scratch = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options.open(&scratch).and_then(|mut file| {
        file.write_all(contents.as_bytes())?;
        file.sync_all()
    });
    let moved = written.and_then(|()| std::fs::rename(&scratch, path));
    if moved.is_err() {
        let _ = std::fs::remove_file(&scratch);
    }
    #[cfg(unix)]
    if moved.is_ok() {
        // Tighten files created before this was enforced.
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(moved?)
}

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

    /// Where key cooldowns are remembered across restarts.
    pub fn state_path() -> PathBuf {
        Self::path().with_file_name("state.json")
    }

    pub fn load() -> Option<Config> {
        let path = Self::path();
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        write_private(&Self::path(), &serde_json::to_string_pretty(self)?)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_files_are_owner_only_and_replaced_whole() {
        let dir = std::env::temp_dir().join(format!("zenshim-cfg-test-{}", std::process::id()));
        let path = dir.join("nested").join("config.json");
        write_private(&path, "first").unwrap();
        write_private(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let leftovers = std::fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1, "no scratch file may be left behind");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn gateway_keys_are_long_and_distinct() {
        let (a, b) = (Config::generate_gateway_key(), Config::generate_gateway_key());
        assert!(a.starts_with("sk-zenshim-") && a.len() == 11 + 64);
        assert_ne!(a, b);
    }
}
