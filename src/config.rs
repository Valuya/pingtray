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
# Tray graph: number of columns, seconds per column (all targets pooled),
# and icon height in pixels.
icon_columns = 20
icon_column_secs = 15.0
icon_height = 20

# Positions from your phone (see README). Uncomment to enable.
# [location]
# listen = "0.0.0.0:18765"
# token = "pick-a-long-random-string"
# watch_dir = "~/Sync/gpslogger"
# max_gap_secs = 600

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
    /// Seconds between two HTTP probes of the same destination.
    pub http_interval_secs: f64,
    pub http_timeout_ms: u64,
    pub retention_days: u32,
    pub warn_ms: f64,
    pub bad_ms: f64,
    pub icon_columns: usize,
    pub icon_column_secs: f64,
    pub icon_height: usize,
    /// Target shown in the tray graph (name), or "all"; changeable from the
    /// icon's menu, which then takes precedence.
    pub tray_target: Option<String>,
    #[serde(rename = "target")]
    pub targets: Vec<Target>,
    pub location: LocationConfig,
    pub power: crate::power::PowerConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Target {
    /// Display name, also the key under which history is stored.
    pub name: String,
    /// IP address or host name (resolved every few minutes).
    pub host: String,
    /// Optional URL for HTTP probes, e.g. "https://www.google.com/generate_204".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            interval_secs: 2.0,
            timeout_ms: 1500,
            http_interval_secs: 10.0,
            http_timeout_ms: 5000,
            retention_days: 180,
            warn_ms: 60.0,
            bad_ms: 200.0,
            icon_columns: 20,
            icon_column_secs: 15.0,
            icon_height: 20,
            tray_target: None,
            targets: Vec::new(),
            location: LocationConfig::default(),
            power: crate::power::PowerConfig::default(),
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
        cfg.http_interval_secs = cfg.http_interval_secs.max(1.0);
        for t in &mut cfg.targets {
            if t.http.as_deref().is_some_and(|u| u.trim().is_empty()) {
                t.http = None;
            }
        }
        cfg.icon_columns = cfg.icon_columns.clamp(4, 120);
        cfg.icon_column_secs = cfg.icon_column_secs.max(cfg.interval_secs);
        cfg.icon_height = cfg.icon_height.clamp(9, 128);
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

/// `PINGTRAY_DB` overrides it (tests, demos).
pub fn db_path() -> PathBuf {
    std::env::var_os("PINGTRAY_DB").map(PathBuf::from).unwrap_or_else(|| data_dir().join("pingtray.sqlite"))
}

/// Per-session scratch space (icons); falls back to the data dir.
pub fn runtime_dir() -> PathBuf {
    dirs().runtime_dir().map(Path::to_path_buf).unwrap_or_else(data_dir)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct LocationConfig {
    /// Address for the phone to post positions to, e.g. "0.0.0.0:18765".
    pub listen: Option<String>,
    /// Shared secret: `?token=…` or HTTP Basic password. Required unless loopback.
    pub token: Option<String>,
    /// Folder scanned every minute for GPX / CSV / OwnTracks files.
    pub watch_dir: Option<PathBuf>,
    /// A ping farther than this from any position gets none.
    pub max_gap_secs: f64,
    /// Positions from this computer's location service (GeoClue).
    pub geoclue: bool,
    /// Positions from a phone connected with ADB (USB or wireless debugging).
    pub adb: bool,
}

impl Default for LocationConfig {
    fn default() -> Self {
        LocationConfig { listen: None, token: None, watch_dir: None, max_gap_secs: 600.0, geoclue: true, adb: true }
    }
}

impl LocationConfig {
    pub fn max_gap_ms(&self) -> i64 {
        (self.max_gap_secs * 1000.0) as i64
    }

    pub fn watch_dir(&self) -> Option<PathBuf> {
        let dir = self.watch_dir.as_ref()?;
        match dir.strip_prefix("~") {
            Ok(rest) => directories::BaseDirs::new().map(|b| b.home_dir().join(rest)),
            Err(_) => Some(dir.clone()),
        }
    }
}
