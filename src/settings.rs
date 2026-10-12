//! Settings window. It edits the config file, which the running pingtray
//! watches: saving applies the changes (pingtray restarts itself).

use std::hash::{BuildHasher, Hasher};
use std::net::SocketAddr;
use std::path::PathBuf;

use eframe::egui::{self, RichText};

use crate::config::{Config, LocationConfig, Target};

const DEFAULT_LISTEN: &str = "0.0.0.0:18765";

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Destinations,
    Pinging,
    Tray,
    Positions,
    Power,
}

pub struct SettingsApp {
    tab: Tab,
    path: PathBuf,
    cfg: Config,
    /// Last saved state, to know whether there is anything to save.
    saved: String,
    message: Option<(bool, String)>,
    lan_ip: Option<std::net::IpAddr>,
    /// Networks connected when the window opened (asking NetworkManager every frame is slow).
    networks_now: Vec<String>,
    snapshot: Option<(PathBuf, u32)>,
}

fn serialize(cfg: &Config) -> String {
    toml::to_string(cfg).unwrap_or_default()
}

/// 128 random bits as hex, without pulling in a crate.
fn new_token() -> String {
    let a = std::collections::hash_map::RandomState::new().build_hasher().finish();
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    format!("{a:016x}{:016x}", h.finish())
}

/// The address other devices on the network see (no packet is sent).
fn lan_ip() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    s.local_addr().ok().map(|a| a.ip())
}

impl SettingsApp {
    pub fn new(cfg: Config, path: PathBuf) -> SettingsApp {
        let saved = serialize(&cfg);
        SettingsApp {
            tab: Tab::Destinations,
            path,
            cfg,
            saved,
            message: None,
            lan_ip: lan_ip(),
            networks_now: crate::power::active_networks(),
            snapshot: std::env::var_os("PINGTRAY_SNAPSHOT").map(|p| (PathBuf::from(p), 0)),
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.cfg.targets.is_empty() {
            return Err("Add at least one destination.".into());
        }
        for (i, t) in self.cfg.targets.iter().enumerate() {
            if t.name.trim().is_empty() || t.host.trim().is_empty() {
                return Err(format!("Destination {} needs a name and a host.", i + 1));
            }
            if let Some(u) = &t.http
                && let Err(e) = crate::http_probe::Url::parse(u)
            {
                return Err(format!("{}: {e}", t.name));
            }
            if self.cfg.targets[..i].iter().any(|o| o.name.trim() == t.name.trim()) {
                return Err(format!("Two destinations are called \"{}\".", t.name.trim()));
            }
        }
        if let Some(listen) = &self.cfg.location.listen {
            let addr: SocketAddr = listen.parse().map_err(|_| format!("\"{listen}\" is not an address:port."))?;
            if !addr.ip().is_loopback() && self.cfg.location.token.as_deref().unwrap_or("").is_empty() {
                return Err("Receiving positions from the network needs a token.".into());
            }
        }
        Ok(())
    }

    fn save(&mut self) {
        if let Err(e) = self.validate() {
            self.message = Some((false, e));
            return;
        }
        for t in &mut self.cfg.targets {
            t.name = t.name.trim().to_string();
            t.host = t.host.trim().to_string();
        }
        let text = format!("# Written by pingtray's settings window.\n\n{}", serialize(&self.cfg));
        match std::fs::write(&self.path, text) {
            Ok(()) => {
                // the menu choice would otherwise override the one made here
                let _ = std::fs::remove_file(crate::config::data_dir().join("tray_target"));
                self.saved = serialize(&self.cfg);
                self.message = Some((true, "Saved. pingtray restarts with the new settings.".into()));
            }
            Err(e) => self.message = Some((false, format!("Cannot write {}: {e}", self.path.display()))),
        }
    }

    fn destinations(&mut self, ui: &mut egui::Ui) {
        let mut remove = None;
        let mut swap = None;
        let n = self.cfg.targets.len();
        ui.horizontal(|ui| {
            ui.add_sized([180.0, 18.0], egui::Label::new(RichText::new("Name").strong()));
            ui.add_sized([150.0, 18.0], egui::Label::new(RichText::new("Ping host / IP").strong()));
            ui.add_sized([260.0, 18.0], egui::Label::new(RichText::new("HTTP probe URL (optional)").strong()));
        });
        for (i, t) in self.cfg.targets.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.add_sized([180.0, 22.0], egui::TextEdit::singleline(&mut t.name).hint_text("Name"));
                ui.add_sized([150.0, 22.0], egui::TextEdit::singleline(&mut t.host).hint_text("8.8.8.8"));
                let url = t.http.get_or_insert_with(String::new);
                ui.add_sized(
                    [260.0, 22.0],
                    egui::TextEdit::singleline(url).hint_text("https://www.google.com/generate_204"),
                );
                if url.trim().is_empty() {
                    t.http = None;
                }
                if ui.add_enabled(i > 0, egui::Button::new("⏶")).on_hover_text("Move up").clicked() {
                    swap = Some((i - 1, i));
                }
                if ui.add_enabled(i + 1 < n, egui::Button::new("⏷")).on_hover_text("Move down").clicked() {
                    swap = Some((i, i + 1));
                }
                if ui.button("Remove").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some((a, b)) = swap {
            self.cfg.targets.swap(a, b);
        }
        if let Some(i) = remove {
            self.cfg.targets.remove(i);
        }
        if ui.button("+ Add destination").clicked() {
            self.cfg.targets.push(Target { name: String::new(), host: String::new(), http: None });
        }
        ui.weak("History is kept per name: renaming a destination starts a new history.");
        ui.weak("HTTP probes send a tiny HEAD request on a fresh connection, timing DNS, connect, TLS and the answer.");
    }

    fn pinging(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("ping").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("Ping every");
            ui.add(egui::DragValue::new(&mut self.cfg.interval_secs).range(0.5..=60.0).speed(0.1).suffix(" s"));
            ui.end_row();
            ui.label("Count as lost after");
            ui.add(egui::DragValue::new(&mut self.cfg.timeout_ms).range(100..=10_000).speed(10).suffix(" ms"));
            ui.end_row();
            ui.label("HTTP probe every");
            ui.add(egui::DragValue::new(&mut self.cfg.http_interval_secs).range(1.0..=600.0).speed(0.5).suffix(" s"));
            ui.end_row();
            ui.label("HTTP failed after");
            ui.add(egui::DragValue::new(&mut self.cfg.http_timeout_ms).range(500..=30_000).speed(50).suffix(" ms"));
            ui.end_row();
            ui.label("Keep history for");
            ui.add(egui::DragValue::new(&mut self.cfg.retention_days).range(1..=3650).suffix(" days"));
            ui.end_row();
            ui.label("Slow above");
            ui.add(egui::DragValue::new(&mut self.cfg.warn_ms).range(1.0..=10_000.0).suffix(" ms"));
            ui.end_row();
            ui.label("Very slow above");
            ui.add(egui::DragValue::new(&mut self.cfg.bad_ms).range(1.0..=10_000.0).suffix(" ms"));
            ui.end_row();
        });
    }

    fn tray(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("tray").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("Graph shows");
            let current = self.cfg.tray_target.clone().unwrap_or_else(|| {
                self.cfg.targets.first().map(|t| t.name.clone()).unwrap_or_default()
            });
            let shown = if current == "all" { "All destinations".to_string() } else { current.clone() };
            egui::ComboBox::from_id_salt("tray_target").selected_text(shown).show_ui(ui, |ui| {
                for t in &self.cfg.targets {
                    if ui.selectable_label(current == t.name, &t.name).clicked() {
                        self.cfg.tray_target = Some(t.name.clone());
                    }
                }
                if ui.selectable_label(current == "all", "All destinations").clicked() {
                    self.cfg.tray_target = Some("all".into());
                }
            });
            ui.end_row();
            ui.label("Columns");
            ui.add(egui::DragValue::new(&mut self.cfg.icon_columns).range(4..=120));
            ui.end_row();
            ui.label("One column per");
            ui.add(egui::DragValue::new(&mut self.cfg.icon_column_secs).range(1.0..=3600.0).suffix(" s"));
            ui.end_row();
            ui.label("Icon height");
            ui.add(egui::DragValue::new(&mut self.cfg.icon_height).range(9..=128).suffix(" px"));
            ui.end_row();
        });
        let minutes = self.cfg.icon_columns as f64 * self.cfg.icon_column_secs / 60.0;
        ui.weak(format!("The tray graph covers the last {minutes:.0} minutes."));
    }

    fn positions(&mut self, ui: &mut egui::Ui) {
        let loc: &mut LocationConfig = &mut self.cfg.location;
        let mut receive = loc.listen.is_some();
        if ui.checkbox(&mut receive, "Receive positions from my phone").changed() {
            loc.listen = receive.then(|| DEFAULT_LISTEN.to_string());
            if receive && loc.token.as_deref().unwrap_or("").is_empty() {
                loc.token = Some(new_token());
            }
        }
        if let Some(listen) = &mut loc.listen {
            egui::Grid::new("endpoint").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label("Listen on");
                ui.add(egui::TextEdit::singleline(listen).desired_width(180.0));
                ui.end_row();
                ui.label("Token");
                ui.horizontal(|ui| {
                    let token = loc.token.get_or_insert_with(String::new);
                    ui.add(egui::TextEdit::singleline(token).desired_width(260.0).font(egui::TextStyle::Monospace));
                    if ui.button("New").clicked() {
                        *token = new_token();
                    }
                });
                ui.end_row();
            });
            let port = listen.rsplit(':').next().unwrap_or("18765").to_string();
            let host = self.lan_ip.map_or("<this computer>".to_string(), |ip| ip.to_string());
            let token = loc.token.clone().unwrap_or_default();
            ui.add_space(4.0);
            ui.label("Phone setup (the phone must be able to reach this computer, e.g. same Wi-Fi or tethering):");
            let owntracks = format!("http://{host}:{port}/pub");
            let gpslogger = format!(
                "http://{host}:{port}/log?token={token}&lat=%LAT&lon=%LON&acc=%ACC&spd=%SPD&time=%TIME"
            );
            copy_row(ui, "OwnTracks (mode HTTP, password = token)", &owntracks);
            copy_row(ui, "GPSLogger (custom URL)", &gpslogger);
        }
        ui.add_space(6.0);
        let mut watch = loc.watch_dir.is_some();
        if ui.checkbox(&mut watch, "Import position files from a folder (GPX, CSV, OwnTracks)").changed() {
            loc.watch_dir = watch.then(|| PathBuf::from("~/Sync/gpslogger"));
        }
        if let Some(dir) = &mut loc.watch_dir {
            let mut text = dir.display().to_string();
            if ui.add(egui::TextEdit::singleline(&mut text).desired_width(360.0)).changed() {
                *dir = PathBuf::from(text);
            }
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label("Positions older or newer than");
            ui.add(egui::DragValue::new(&mut loc.max_gap_secs).range(30.0..=7200.0).suffix(" s"));
            ui.label("do not locate a ping");
        });
    }
}

impl SettingsApp {
    fn power(&mut self, ui: &mut egui::Ui) {
        let p = &mut self.cfg.power;
        ui.checkbox(&mut p.enabled, RichText::new("Manage battery charging").strong());
        ui.weak(
            "Switches GNOME's Battery Charging setting between Maximize Charge and Preserve Battery Health, \
             and shows a battery icon with the current mode in the tray.",
        );
        ui.add_space(8.0);
        ui.add_enabled_ui(p.enabled, |ui| {
            ui.checkbox(&mut p.maximize_when_travelling, "Maximize charge when plugged in while travelling");
            ui.weak("Typically: you found a socket on the train and want a full battery.");
            ui.checkbox(&mut p.preserve_when_unplugged, "Back to Preserve Battery Health when unplugged");
            ui.add_space(8.0);
            ui.label(RichText::new("Travelling means").strong());
            ui.checkbox(&mut p.moving, "Moving in a vehicle, according to the phone's positions (last 10 minutes)");
            ui.label("Or connected to one of these networks (e.g. the phone's hotspot):");
            let mut remove = None;
            for (i, n) in p.networks.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.add_sized([260.0, 22.0], egui::TextEdit::singleline(n));
                    if ui.button("Remove").clicked() {
                        remove = Some(i);
                    }
                });
            }
            if let Some(i) = remove {
                p.networks.remove(i);
            }
            ui.horizontal(|ui| {
                if ui.button("+ Add network").clicked() {
                    p.networks.push(String::new());
                }
                for current in &self.networks_now {
                    if !p.networks.contains(current)
                        && ui.button(format!("+ Add current: {current}")).clicked()
                    {
                        p.networks.push(current.clone());
                    }
                }
            });
            ui.add_space(8.0);
            ui.weak(
                "pingtray only switches when you plug in, unplug, or start travelling while plugged in: \
                 a mode you choose by hand (here, in GNOME or from the battery icon) stays until then.",
            );
        });
    }
}

fn copy_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(label);
        if ui.button("Copy").clicked() {
            ui.ctx().copy_text(value.to_string());
        }
    });
    ui.add(egui::Label::new(RichText::new(value).monospace().weak()).wrap());
}

impl eframe::App for SettingsApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        crate::viewer::snapshot_step(ui, &mut self.snapshot, true);
        let dirty = serialize(&self.cfg) != self.saved;
        egui::Panel::bottom("actions").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(dirty, egui::Button::new("Save")).clicked() {
                    self.save();
                }
                if ui.add_enabled(dirty, egui::Button::new("Revert")).clicked() {
                    if let Ok(cfg) = toml::from_str::<Config>(&self.saved) {
                        self.cfg = cfg;
                    }
                    self.message = None;
                }
                match &self.message {
                    Some((true, m)) => drop(ui.label(RichText::new(m).color(egui::Color32::from_rgb(0x2e, 0xc2, 0x7e)))),
                    Some((false, m)) => drop(ui.label(RichText::new(m).color(egui::Color32::from_rgb(0xe0, 0x1b, 0x24)))),
                    None if dirty => drop(ui.weak("Unsaved changes")),
                    None => {}
                }
            });
            ui.add_space(4.0);
        });
        egui::Panel::top("tabs").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                for (tab, label) in [
                    (Tab::Destinations, "Destinations"),
                    (Tab::Pinging, "Pinging"),
                    (Tab::Tray, "Tray icon"),
                    (Tab::Positions, "Phone positions"),
                    (Tab::Power, "Power"),
                ] {
                    ui.selectable_value(&mut self.tab, tab, label);
                }
            });
            ui.add_space(2.0);
        });
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| match self.tab {
                Tab::Destinations => self.destinations(ui),
                Tab::Pinging => self.pinging(ui),
                Tab::Tray => self.tray(ui),
                Tab::Positions => self.positions(ui),
                Tab::Power => self.power(ui),
            });
        });
    }
}

pub fn run(cfg: Config, path: PathBuf, tab: Option<&str>) -> anyhow::Result<()> {
    let tab = match tab {
        Some("pinging") => Tab::Pinging,
        Some("tray") => Tab::Tray,
        Some("positions") => Tab::Positions,
        Some("power") => Tab::Power,
        _ => Tab::Destinations,
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("pingtray settings")
            .with_app_id("pingtray-settings")
            .with_inner_size([640.0, 720.0]),
        ..Default::default()
    };
    eframe::run_native("pingtray settings", options, Box::new(move |_| {
        let mut app = SettingsApp::new(cfg, path);
        app.tab = tab;
        Ok(Box::new(app))
    }))
        .map_err(|e| anyhow::anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrips_through_toml() {
        let mut cfg = Config::default();
        cfg.targets = vec![Target { name: "UCLouvain".into(), host: "130.104.1.1".into(), http: Some("https://uclouvain.be/".into()) }];
        cfg.tray_target = Some("UCLouvain".into());
        cfg.location.listen = Some(DEFAULT_LISTEN.into());
        cfg.location.token = Some(new_token());
        let text = serialize(&cfg);
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(serialize(&back), text);
        assert_eq!(back.targets[0].host, "130.104.1.1");
    }
}
