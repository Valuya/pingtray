//! Battery charging mode, as in GNOME's "Battery Charging" setting: UPower's
//! charge threshold. Preserve = stop charging at the threshold (80 %),
//! Maximize = charge to 100 %.
//!
//! Either automatic (pingtray: Maximize when plugged in while travelling, a
//! socket on the train; Preserve otherwise) or manual (chosen by hand in the
//! icon's menu, or changed elsewhere such as GNOME's settings: kept as is).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::OwnedObjectPath;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct PowerConfig {
    /// Manage the charging mode (and show the battery icon).
    pub enabled: bool,
    /// In automatic mode: charge to 100 % when plugged in while travelling
    /// (otherwise preserve).
    pub maximize_when_travelling: bool,
    /// Travelling when the phone's positions show a vehicle lately.
    pub moving: bool,
    /// Travelling when connected to one of these networks (e.g. the phone's hotspot).
    pub networks: Vec<String>,
}

impl Default for PowerConfig {
    fn default() -> Self {
        PowerConfig {
            enabled: false,
            maximize_when_travelling: true,
            moving: true,
            networks: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Battery {
    pub percentage: f64,
    pub plugged: bool,
    pub charging: bool,
    /// Charge threshold on: "Preserve Battery Health".
    pub preserve: bool,
    pub end_threshold: u32,
}

pub struct Upower {
    conn: Connection,
    battery: OwnedObjectPath,
}

const UPOWER: &str = "org.freedesktop.UPower";
const DEVICE: &str = "org.freedesktop.UPower.Device";

impl Upower {
    /// The laptop battery, if it supports a charge threshold.
    pub fn connect() -> Result<Upower> {
        let conn = Connection::system()?;
        let root = Proxy::new(&conn, UPOWER, "/org/freedesktop/UPower", UPOWER)?;
        let devices: Vec<OwnedObjectPath> = root.call("EnumerateDevices", &())?;
        for path in devices {
            let suitable = {
                let dev = Proxy::new(&conn, UPOWER, path.as_str(), DEVICE)?;
                // Type 2 = battery; PowerSupply excludes mice and headsets
                dev.get_property::<u32>("Type").ok() == Some(2)
                    && dev.get_property::<bool>("PowerSupply").unwrap_or(false)
                    && dev.get_property::<bool>("ChargeThresholdSupported").unwrap_or(false)
            };
            if suitable {
                return Ok(Upower { conn, battery: path });
            }
        }
        anyhow::bail!("no battery with a charge threshold (needs UPower ≥ 1.90.5 and a supported laptop)")
    }

    fn device(&self) -> Result<Proxy<'_>> {
        Ok(Proxy::new(&self.conn, UPOWER, self.battery.as_str(), DEVICE)?)
    }

    pub fn read(&self) -> Result<Battery> {
        let root = Proxy::new(&self.conn, UPOWER, "/org/freedesktop/UPower", UPOWER)?;
        let dev = self.device()?;
        // State: 1 charging, 2 discharging, 4 fully charged, 5 pending charge
        let state: u32 = dev.get_property("State")?;
        Ok(Battery {
            percentage: dev.get_property("Percentage")?,
            plugged: !root.get_property::<bool>("OnBattery")?,
            charging: state == 1,
            preserve: dev.get_property("ChargeThresholdEnabled")?,
            end_threshold: dev.get_property("ChargeEndThreshold").unwrap_or(80),
        })
    }

    pub fn set_preserve(&self, preserve: bool) -> Result<()> {
        self.device()?.call_method("EnableChargeThreshold", &(preserve,)).context("changing the charging mode")?;
        Ok(())
    }
}

/// Names of the active network connections (for Wi-Fi: usually the SSID).
pub fn active_networks() -> Vec<String> {
    let names = || -> Result<Vec<String>> {
        let conn = Connection::system()?;
        let nm = Proxy::new(&conn, "org.freedesktop.NetworkManager", "/org/freedesktop/NetworkManager", "org.freedesktop.NetworkManager")?;
        let active: Vec<OwnedObjectPath> = nm.get_property("ActiveConnections")?;
        Ok(active
            .iter()
            .filter_map(|p| {
                let c = Proxy::new(&conn, "org.freedesktop.NetworkManager", p.as_str(), "org.freedesktop.NetworkManager.Connection.Active").ok()?;
                let kind: String = c.get_property("Type").ok()?;
                (kind != "loopback").then(|| c.get_property::<String>("Id").ok()).flatten()
            })
            .collect())
    };
    names().unwrap_or_default()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Maximize,
    Preserve,
}

/// Automatic mode's memory, to tell its own changes from changes made elsewhere.
#[derive(Default)]
pub struct Auto {
    /// Mode last applied (true = preserve), and the inputs it was for.
    applied: Option<(bool, bool, bool)>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// Already as wanted.
    Keep,
    /// Switch to this mode.
    Apply(Action),
    /// Changed elsewhere since we set it, while nothing changed here: the
    /// user chose by hand, go manual.
    ChangedElsewhere,
}

impl Auto {
    pub fn want(cfg: &PowerConfig, plugged: bool, travelling: bool) -> Action {
        if cfg.maximize_when_travelling && plugged && travelling { Action::Maximize } else { Action::Preserve }
    }

    pub fn step(&mut self, cfg: &PowerConfig, plugged: bool, travelling: bool, preserving: bool) -> Step {
        let want = Self::want(cfg, plugged, travelling);
        let want_preserve = want == Action::Preserve;
        if preserving == want_preserve {
            self.applied = Some((want_preserve, plugged, travelling));
            return Step::Keep;
        }
        if self.applied == Some((want_preserve, plugged, travelling)) {
            return Step::ChangedElsewhere;
        }
        self.applied = Some((want_preserve, plugged, travelling));
        Step::Apply(want)
    }

    /// Forget: after switching back to automatic, apply without second-guessing.
    pub fn reset(&mut self) {
        self.applied = None;
    }
}

fn mode_file() -> std::path::PathBuf {
    crate::config::data_dir().join("power_mode")
}

/// Automatic unless the last choice was manual.
pub fn load_automatic() -> bool {
    std::fs::read_to_string(mode_file()).map_or(true, |s| s.trim() != "manual")
}

pub fn save_automatic(automatic: bool) {
    let _ = std::fs::write(mode_file(), if automatic { "automatic" } else { "manual" });
}

/// Recent positions show a vehicle (no railway lookup: train or car alike).
fn moving_lately(store: &crate::store::Store, max_gap_ms: i64) -> bool {
    let now = crate::pinger::now_ms();
    let Ok(fixes) = store.fixes(now - 20 * 60_000, now + 1) else { return false };
    crate::transport::classify(&fixes, max_gap_ms, |_| None).iter().any(|s| {
        s.to_ms >= now - 10 * 60_000
            && matches!(
                s.mode,
                crate::transport::Mode::Vehicle
                    | crate::transport::Mode::Car
                    | crate::transport::Mode::Train
                    | crate::transport::Mode::Tram
                    | crate::transport::Mode::Metro
            )
    })
}

pub struct Status {
    pub battery: Battery,
    pub automatic: bool,
    /// Why we count as travelling; `None` when not travelling.
    pub reason: Option<String>,
    pub last_action: Option<(Action, String)>,
}

/// Set to make the power loop read the battery again right away.
static POKE: AtomicBool = AtomicBool::new(false);

pub fn poke() {
    POKE.store(true, Ordering::Relaxed);
}

/// Polls the battery and the travel signals; in automatic mode, applies the
/// rule. `automatic` is shared with the icon's menu. `report` receives the
/// state after every poll.
pub fn spawn(cfg: PowerConfig, max_gap_ms: i64, automatic: Arc<AtomicBool>, mut report: impl FnMut(&Status) + Send + 'static) {
    std::thread::spawn(move || {
        let upower = match Upower::connect() {
            Ok(u) => u,
            Err(e) => return tracing::warn!("battery charging mode: {e}"),
        };
        let store = crate::store::Store::open(&crate::config::db_path()).ok();
        let mut auto = Auto::default();
        let mut was_automatic = false;
        let mut last_action = None;
        loop {
            match upower.read() {
                Ok(battery) => {
                    let networks = active_networks();
                    let on_network = networks.iter().find(|n| cfg.networks.contains(n)).cloned();
                    let moving = cfg.moving && store.as_ref().is_some_and(|s| moving_lately(s, max_gap_ms));
                    let travelling = moving || on_network.is_some();
                    let reason = match (moving, &on_network) {
                        (true, _) => Some("moving".to_string()),
                        (false, Some(n)) => Some(format!("on {n}")),
                        _ => None,
                    };
                    let is_automatic = automatic.load(Ordering::Relaxed);
                    if is_automatic && !was_automatic {
                        auto.reset();
                    }
                    was_automatic = is_automatic;
                    if is_automatic {
                        match auto.step(&cfg, battery.plugged, travelling, battery.preserve) {
                            Step::Keep => {}
                            Step::Apply(action) => {
                                let why = match (action, &reason) {
                                    (Action::Maximize, Some(r)) => format!("plugged in while travelling ({r})"),
                                    (Action::Maximize, None) => "plugged in while travelling".to_string(),
                                    (Action::Preserve, _) if !battery.plugged => "on battery".to_string(),
                                    (Action::Preserve, _) => "not travelling".to_string(),
                                };
                                match upower.set_preserve(action == Action::Preserve) {
                                    Ok(()) => {
                                        tracing::info!("charging: {action:?}, {why}");
                                        last_action = Some((action, why));
                                    }
                                    Err(e) => tracing::error!("{e:#}"),
                                }
                            }
                            Step::ChangedElsewhere => {
                                tracing::info!("charging mode changed elsewhere: manual");
                                automatic.store(false, Ordering::Relaxed);
                                save_automatic(false);
                                was_automatic = false;
                            }
                        }
                    }
                    let battery = upower.read().unwrap_or(battery);
                    let automatic = automatic.load(Ordering::Relaxed);
                    report(&Status { battery, automatic, reason, last_action: last_action.clone() });
                }
                Err(e) => tracing::warn!("reading the battery: {e}"),
            }
            // every 10 s, or at once after a change from the menu
            for _ in 0..40 {
                if POKE.swap(false, Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    });
}

/// Manual choice, from the icon's menu. On its own thread: the menu runs
/// inside the async runtime, where zbus's blocking calls must not run.
pub fn set_mode(preserve: bool) {
    std::thread::spawn(move || {
        match Upower::connect().and_then(|u| u.set_preserve(preserve)) {
            Ok(()) => tracing::info!("charging set by hand: {}", if preserve { "preserve" } else { "maximize" }),
            Err(e) => tracing::error!("{e:#}"),
        }
        poke();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_follows_plug_and_travel() {
        let cfg = PowerConfig { enabled: true, ..Default::default() };
        let mut a = Auto::default();
        // at home, plugged, set to maximize before: preserve
        assert_eq!(a.step(&cfg, true, false, false), Step::Apply(Action::Preserve));
        assert_eq!(a.step(&cfg, true, false, true), Step::Keep);
        // on the train, on battery, then a socket
        assert_eq!(a.step(&cfg, false, true, true), Step::Keep);
        assert_eq!(a.step(&cfg, true, true, true), Step::Apply(Action::Maximize));
        assert_eq!(a.step(&cfg, true, true, false), Step::Keep);
        // unplugged
        assert_eq!(a.step(&cfg, false, true, false), Step::Apply(Action::Preserve));
    }

    #[test]
    fn a_change_made_elsewhere_means_manual() {
        let cfg = PowerConfig::default();
        let mut a = Auto::default();
        assert_eq!(a.step(&cfg, true, true, true), Step::Apply(Action::Maximize));
        assert_eq!(a.step(&cfg, true, true, false), Step::Keep);
        // GNOME's settings switched it back while nothing changed here
        assert_eq!(a.step(&cfg, true, true, true), Step::ChangedElsewhere);
        // back to automatic: applies again
        a.reset();
        assert_eq!(a.step(&cfg, true, true, true), Step::Apply(Action::Maximize));
    }
}
