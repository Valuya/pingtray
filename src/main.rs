mod collector;
mod config;
mod icon;
mod pinger;
mod store;
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
    /// Start pingtray with the desktop session (XDG autostart).
    Autostart {
        /// Remove the autostart entry instead.
        #[arg(long)]
        remove: bool,
    },
}

/// Holds an exclusive lock on `name` in the data dir for the life of the process.
fn single_instance(name: &str) -> Result<Option<File>> {
    let path = config::data_dir().join(name);
    let file = File::create(&path).with_context(|| path.display().to_string())?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
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
            let Some(_lock) = single_instance("viewer.lock")? else {
                tracing::info!("viewer already open");
                return Ok(());
            };
            viewer::run(cfg)
        }
        Command::Autostart { remove } => autostart(remove, &config_path),
    }
}
