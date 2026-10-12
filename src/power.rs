//! Battery charging mode, as in GNOME's "Battery Charging" setting: UPower's
//! charge threshold. Preserve = stop charging at the threshold (80 %),
//! Maximize = charge to 100 %. pingtray switches to Maximize when you plug in
//! while travelling (a socket on the train), and back when you unplug.

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
    /// Plugged in while travelling: charge to 100 %.
    pub maximize_when_travelling: bool,
    /// Unplugged: back to preserving the battery.
    pub preserve_when_unplugged: bool,
    /// Not travelling (at start, or when a journey ends): preserve too.
    pub preserve_when_not_travelling: bool,
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
            preserve_when_unplugged: true,
            preserve_when_not_travelling: true,
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

/// Acts on changes only (and once at start), so that a choice made by hand
/// (in GNOME's settings or the icon's menu) stays until the next change.
#[derive(Default)]
pub struct Policy {
    plugged: Option<bool>,
    travelling: bool,
}

impl Policy {
    pub fn step(&mut self, cfg: &PowerConfig, plugged: bool, travelling: bool) -> Option<Action> {
        let was_plugged = self.plugged.replace(plugged);
        let first = was_plugged.is_none();
        let was_travelling = std::mem::replace(&mut self.travelling, travelling);
        let plugged_in = plugged && was_plugged != Some(true);
        let started_travelling = plugged && travelling && !was_travelling;
        if cfg.maximize_when_travelling && plugged && travelling && (plugged_in || started_travelling) {
            return Some(Action::Maximize);
        }
        if cfg.preserve_when_unplugged && !plugged && was_plugged == Some(true) {
            return Some(Action::Preserve);
        }
        if cfg.preserve_when_not_travelling && !travelling && (first || was_travelling) {
            return Some(Action::Preserve);
        }
        None
    }
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
    /// Why we count as travelling; `None` when not travelling.
    pub reason: Option<String>,
    pub last_action: Option<(Action, String)>,
}

/// Polls the battery and the travel signals; applies the policy unless paused.
/// `report` receives the state after every poll.
pub fn spawn(cfg: PowerConfig, max_gap_ms: i64, paused: Arc<AtomicBool>, mut report: impl FnMut(&Status) + Send + 'static) {
    std::thread::spawn(move || {
        let upower = match Upower::connect() {
            Ok(u) => u,
            Err(e) => return tracing::warn!("battery charging mode: {e}"),
        };
        let store = crate::store::Store::open(&crate::config::db_path()).ok();
        let mut policy = Policy::default();
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
                    if let Some(action) = policy.step(&cfg, battery.plugged, travelling)
                        && !paused.load(Ordering::Relaxed)
                    {
                        let why = if action == Action::Maximize {
                            format!("plugged in while travelling ({})", reason.clone().unwrap_or_default())
                        } else {
                            "unplugged".to_string()
                        };
                        match upower.set_preserve(action == Action::Preserve) {
                            Ok(()) => {
                                tracing::info!("charging: {action:?}, {why}");
                                last_action = Some((action, why));
                            }
                            Err(e) => tracing::error!("{e:#}"),
                        }
                    }
                    let battery = upower.read().unwrap_or(battery);
                    report(&Status { battery, reason, last_action: last_action.clone() });
                }
                Err(e) => tracing::warn!("reading the battery: {e}"),
            }
            std::thread::sleep(Duration::from_secs(10));
        }
    });
}

/// Manual switch, from the icon's menu.
pub fn set_mode(preserve: bool) {
    if let Err(e) = Upower::connect().and_then(|u| u.set_preserve(preserve)) {
        tracing::error!("{e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plug_in_on_the_train_then_unplug() {
        let cfg = PowerConfig { enabled: true, ..Default::default() };
        let mut p = Policy::default();
        assert_eq!(p.step(&cfg, false, true), None); // starts on the train, on battery
        assert_eq!(p.step(&cfg, true, true), Some(Action::Maximize)); // found a socket
        assert_eq!(p.step(&cfg, true, true), None); // no repeat: manual changes stay
        assert_eq!(p.step(&cfg, false, true), Some(Action::Preserve)); // unplugged
        assert_eq!(p.step(&cfg, true, false), Some(Action::Preserve)); // home: journey over
    }

    #[test]
    fn at_home_preserves_at_start_and_after_a_journey() {
        let cfg = PowerConfig::default();
        let mut p = Policy::default();
        assert_eq!(p.step(&cfg, true, false), Some(Action::Preserve)); // starts at home
        assert_eq!(p.step(&cfg, true, false), None); // manual MAX stays
        assert_eq!(p.step(&cfg, true, true), Some(Action::Maximize));
        assert_eq!(p.step(&cfg, true, false), Some(Action::Preserve)); // journey over
        let off = PowerConfig { preserve_when_not_travelling: false, ..Default::default() };
        assert_eq!(Policy::default().step(&off, true, false), None);
    }

    #[test]
    fn plugged_in_then_the_journey_starts() {
        let cfg = PowerConfig { preserve_when_not_travelling: false, ..Default::default() };
        let mut p = Policy::default();
        assert_eq!(p.step(&cfg, true, false), None);
        assert_eq!(p.step(&cfg, true, true), Some(Action::Maximize));
        let cfg = PowerConfig { preserve_when_unplugged: false, ..Default::default() };
        assert_eq!(p.step(&cfg, false, true), None);
    }
}
