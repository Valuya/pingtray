//! User configuration, read from a TOML file kept by hand.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

/// Written on first start when no config file exists.
pub const DEFAULT_CONFIG: &str = r#"# pingtray configuration. Restart pingtray after editing.

# Seconds between two pings to the same target.
interval_secs = 2.0
# A reply slower than this counts as lost (capped below interval_secs).
timeout_ms = 1500
# History older than this is purged from the database.
retention_days = 180
# Colour thresholds for the tray icon, in milliseconds.
warn_ms = 60.0
bad_ms = 200.0
# Number of bars in the tray icon (one per ping, worst target wins).
icon_samples = 16

[[target]]
name = "Google DNS"
host = "8.8.8.8"

[[target]]
name = "Cloudflare"
host = "1.1.1.1"
"#;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub interval_secs: f64,
    pub timeout_ms: u64,
    pub retention_days: u32,
    pub warn_ms: f64,
    pub bad_ms: f64,
    pub icon_samples: usize,
    #[serde(rename = "target")]
    pub targets: Vec<Target>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Target {
    /// Display name, also the key under which history is stored.
    pub name: String,
    /// IP address or host name (resolved every few minutes).
    pub host: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            interval_secs: 2.0,
            timeout_ms: 1500,
            retention_days: 180,
            warn_ms: 60.0,
            bad_ms: 200.0,
            icon_samples: 16,
            targets: Vec::new(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        if !path.exists() {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(path, DEFAULT_CONFIG)?;
            tracing::info!("wrote default config to {}", path.display());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.interval_secs = cfg.interval_secs.max(0.2);
        cfg.icon_samples = cfg.icon_samples.clamp(4, 64);
        Ok(cfg)
    }

    pub fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(self.interval_secs)
    }

    /// Ping timeout, kept below the interval so ticks never pile up.
    pub fn timeout(&self) -> std::time::Duration {
        let cap = self.interval().mul_f64(0.9);
        std::time::Duration::from_millis(self.timeout_ms).min(cap)
    }
}

fn dirs() -> ProjectDirs {
    ProjectDirs::from("be", "valuya", "pingtray").expect("no home directory")
}

pub fn default_config_path() -> PathBuf {
    dirs().config_dir().join("config.toml")
}

pub fn data_dir() -> PathBuf {
    let dir = dirs().data_dir().to_path_buf();
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn db_path() -> PathBuf {
    data_dir().join("pingtray.sqlite")
}
