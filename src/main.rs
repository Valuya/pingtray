mod collector;
mod colors;
mod config;
mod icon;
mod location;
mod map;
mod pinger;
mod settings;
mod store;
mod transport;
mod tray;
mod viewer;

use std::fs::File;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

/// Pings a few hosts, shows a tiny graph in the system tray and the full history on click.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Configuration file (default: ~/.config/pingtray/config.toml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Ping and record, with the tray icon (default).
    Run,
    /// Ping and record without any icon (servers, unsupported desktops).
    Collect,
    /// Open the graph window.
    View,
    /// Open the settings window.
    Settings,
    /// Import positions from GPX, CSV, OwnTracks .rec or JSON files.
    Import {
        files: Vec<PathBuf>,
    },
    /// List journeys with their guessed transport mode.
    Trips {
        /// How far back, in hours.
        #[arg(long, default_value_t = 24.0)]
        hours: f64,
    },
    /// Start pingtray with the desktop session (XDG autostart).
    Autostart {
        /// Remove the autostart entry instead.
        #[arg(long)]
        remove: bool,
    },
}

/// Holds an exclusive lock on `name` in the data dir for the life of the process.
/// After a restart for new settings, the old process may hold the lock a
/// moment longer: wait for it then.
fn single_instance(name: &str) -> Result<Option<File>> {
    let path = config::data_dir().join(name);
    let file = File::create(&path).with_context(|| path.display().to_string())?;
    let patience = if std::env::var_os(collector::RESTARTED).is_some() { 50 } else { 1 };
    for _ in 0..patience {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    }
    Ok(None)
}

fn autostart(remove: bool, config_path: &std::path::Path) -> Result<()> {
    let dir = directories::BaseDirs::new().context("no home")?.config_dir().join("autostart");
    let file = dir.join("pingtray.desktop");
    if remove {
        let _ = std::fs::remove_file(&file);
        println!("removed {}", file.display());
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        &file,
        format!(
            "[Desktop Entry]\nType=Application\nName=pingtray\nComment=Network latency in the tray\n\
             Exec=\"{}\" --config \"{}\" run\nX-GNOME-Autostart-enabled=true\nNoDisplay=true\n",
            exe.display(),
            config_path.display()
        ),
    )?;
    println!("wrote {}", file.display());
    Ok(())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pingtray=info".into()),
        )
        .init();
    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(config::default_config_path);
    let cfg = config::Config::load(&config_path)?;

    match cli.command.unwrap_or(Command::Run) {
        cmd @ (Command::Run | Command::Collect) => {
            let Some(_lock) = single_instance("collector.lock")? else {
                tracing::info!("pingtray is already running");
                return Ok(());
            };
            let with_tray = matches!(cmd, Command::Run);
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(collector::run(cfg, config_path, with_tray))
        }
        Command::View => {
            // a snapshot (debug aid) may run next to an open window
            let lock = if std::env::var_os("PINGTRAY_SNAPSHOT").is_some() {
                Some(None)
            } else {
                single_instance("viewer.lock")?.map(Some)
            };
            let Some(_lock) = lock else {
                tracing::info!("viewer already open");
                return Ok(());
            };
            viewer::run(cfg, config_path)
        }
        Command::Autostart { remove } => autostart(remove, &config_path),
        Command::Settings => {
            let Some(_lock) = single_instance("settings.lock")? else {
                tracing::info!("settings already open");
                return Ok(());
            };
            settings::run(cfg, config_path)
        }
        Command::Trips { hours } => {
            let store = store::Store::open(&config::db_path())?;
            let now = pinger::now_ms();
            let fixes = store.fixes(now - (hours * 3_600_000.0) as i64, now + 1)?;
            let gap = cfg.location.max_gap_ms();
            let segments = transport::classify(&fixes, gap, |track| transport::rails_for(&store, track));
            if segments.is_empty() {
                println!("no positions in the last {hours} h");
            }
            let t = |ms: i64| {
                chrono::TimeZone::timestamp_millis_opt(&chrono::Local, ms)
                    .single()
                    .map(|d| d.format("%a %d/%m %H:%M").to_string())
                    .unwrap_or_default()
            };
            for s in segments {
                println!(
                    "{} - {}  {:<10} {:>6.1} km  {:>4.0} km/h  {:>4} min",
                    t(s.from_ms),
                    t(s.to_ms),
                    s.mode.label(),
                    s.km,
                    s.kmh(),
                    (s.to_ms - s.from_ms) / 60_000
                );
            }
            Ok(())
        }
        Command::Import { files } => {
            let mut store = store::Store::open(&config::db_path())?;
            location::import_files(&mut store, &files)
        }
    }
}
