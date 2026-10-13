//! System tray front-end. The collector only talks to [`TrayHandle`], so porting
//! to another platform means adding a backend here (e.g. the `tray-icon` crate
//! for Windows/macOS) without touching the rest.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// What the tray shows; rebuilt by the collector.
#[derive(Clone, Default)]
pub struct TrayState {
    /// Wide PNG of the icon. GNOME's AppIndicator extension only keeps the
    /// aspect ratio of icons given as files; raw pixmaps are squashed square.
    pub icon_file: Option<PathBuf>,
    /// The same image as ARGB32, for hosts that ignore file icons.
    pub pixmap: (usize, usize, Vec<u8>),
    pub tooltip: String,
    pub config_path: PathBuf,
    /// Status lines at the top of the menu: GNOME shows no tooltips.
    pub status: Vec<String>,
    /// Target names, for the "Tray graph" menu.
    pub choices: Vec<String>,
    /// Shared with the collector: 0 = all targets pooled, i + 1 = target i.
    pub selection: Arc<AtomicUsize>,
}

pub fn selected_target(selection: &AtomicUsize) -> Option<usize> {
    selection.load(Ordering::Relaxed).checked_sub(1)
}

fn selection_file() -> PathBuf {
    crate::config::data_dir().join("tray_target")
}

/// Last choice made in the menu, else the config's `tray_target`, else the first target.
pub fn load_selection(choices: &[String], configured: Option<&str>) -> Arc<AtomicUsize> {
    let saved = std::fs::read_to_string(selection_file()).ok();
    let wanted = saved.as_deref().map(str::trim).or(configured);
    let value = match wanted {
        Some("all") => 0,
        Some(name) => choices.iter().position(|c| c == name).map_or(1, |i| i + 1),
        None => 1,
    };
    Arc::new(AtomicUsize::new(value))
}

fn save_selection(choices: &[String], value: usize) {
    let name = value.checked_sub(1).and_then(|i| choices.get(i)).map_or("all", String::as_str);
    if let Err(e) = std::fs::write(selection_file(), name) {
        tracing::warn!("saving tray target: {e}");
    }
}

/// Opens the big graph window in a separate process, so the tray stays light.
pub fn open_viewer(config_path: &std::path::Path) {
    spawn_self(config_path, &["view"]);
}

pub fn open_settings(config_path: &std::path::Path) {
    spawn_self(config_path, &["settings"]);
}

pub fn open_note(config_path: &std::path::Path) {
    spawn_self(config_path, &["note"]);
}

pub fn open_settings_tab(config_path: &std::path::Path, tab: &str) {
    spawn_self(config_path, &["settings", "--tab", tab]);
}

pub fn open_power_settings(config_path: &std::path::Path) {
    spawn_self(config_path, &["settings", "--tab", "power"]);
}

/// This program's path, also after `cargo install` replaced the binary
/// (Linux then reports "<path> (deleted)").
pub fn own_exe() -> std::io::Result<PathBuf> {
    let p = std::env::current_exe()?;
    Ok(match p.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(s) => PathBuf::from(s),
        None => p,
    })
}

fn spawn_self(config_path: &std::path::Path, command: &[&str]) {
    let exe = match own_exe() {
        Ok(p) => p,
        Err(e) => return tracing::error!("cannot locate own executable: {e}"),
    };
    match std::process::Command::new(exe).arg("--config").arg(config_path).args(command).spawn() {
        // reap the child so it does not linger as a zombie
        Ok(mut child) => drop(std::thread::spawn(move || child.wait())),
        Err(e) => tracing::error!("cannot start viewer: {e}"),
    }
}

#[cfg(target_os = "linux")]
mod sni {
    use super::*;
    use ksni::TrayMethods;

    /// StatusNotifierItem over D-Bus: KDE, XFCE, wlroots bars, and GNOME with
    /// the AppIndicator extension (enabled by default on Ubuntu).
    pub struct SniTray(pub TrayState);

    impl ksni::Tray for SniTray {
        fn id(&self) -> String {
            "pingtray".into()
        }
        fn title(&self) -> String {
            "pingtray".into()
        }
        fn icon_name(&self) -> String {
            // an absolute path: not in the spec, but the AppIndicator extension
            // and others accept it
            self.0.icon_file.as_ref().map(|p| p.display().to_string()).unwrap_or_default()
        }
        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            let (w, h, data) = &self.0.pixmap;
            // with a file, no pixmap: GNOME's extension would draw it after the
            // wide file image, squeezed into a square
            if data.is_empty() || self.0.icon_file.is_some() {
                return Vec::new();
            }
            vec![ksni::Icon { width: *w as i32, height: *h as i32, data: data.clone() }]
        }
        fn tool_tip(&self) -> ksni::ToolTip {
            ksni::ToolTip {
                title: "pingtray".into(),
                description: self.0.tooltip.clone(),
                ..Default::default()
            }
        }
        fn activate(&mut self, _x: i32, _y: i32) {
            open_viewer(&self.0.config_path);
        }
        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            use ksni::menu::{RadioGroup, RadioItem, StandardItem, SubMenu};
            let n = self.0.choices.len();
            let selected = selected_target(&self.0.selection).unwrap_or(n);
            let mut options: Vec<RadioItem> =
                self.0.choices.iter().map(|c| RadioItem { label: c.clone(), ..Default::default() }).collect();
            options.push(RadioItem { label: "All targets".into(), ..Default::default() });
            let mut items: Vec<ksni::MenuItem<Self>> = self
                .0
                .status
                .iter()
                .map(|line| StandardItem { label: line.replace('_', "__"), enabled: false, ..Default::default() }.into())
                .collect();
            if !items.is_empty() {
                items.push(ksni::MenuItem::Separator);
            }
            items.extend(vec![
                StandardItem {
                    label: "Show graph".into(),
                    activate: Box::new(|t: &mut Self| open_viewer(&t.0.config_path)),
                    ..Default::default()
                }
                .into(),
                SubMenu {
                    label: "Tray graph".into(),
                    submenu: vec![
                        RadioGroup {
                            selected,
                            select: Box::new(|t: &mut Self, i: usize| {
                                let value = if i < t.0.choices.len() { i + 1 } else { 0 };
                                t.0.selection.store(value, Ordering::Relaxed);
                                save_selection(&t.0.choices, value);
                            }),
                            options,
                        }
                        .into(),
                    ],
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: "Add note…".into(),
                    activate: Box::new(|t: &mut Self| open_note(&t.0.config_path)),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: "Settings…".into(),
                    activate: Box::new(|t: &mut Self| open_settings(&t.0.config_path)),
                    ..Default::default()
                }
                .into(),
                ksni::MenuItem::Separator,
                StandardItem {
                    label: "Quit".into(),
                    activate: Box::new(|_| std::process::exit(0)),
                    ..Default::default()
                }
                .into(),
            ]);
            items
        }
    }

    pub async fn spawn(state: TrayState) -> Option<ksni::Handle<SniTray>> {
        // at login pingtray may start before the panel: wait for it
        match SniTray(state).assume_sni_available(true).spawn().await {
            Ok(h) => Some(h),
            Err(e) => {
                tracing::warn!("no system tray available ({e}); collecting without an icon");
                None
            }
        }
    }
}

pub struct TrayHandle {
    #[cfg(target_os = "linux")]
    inner: ksni::Handle<sni::SniTray>,
}

impl TrayHandle {
    #[cfg(target_os = "linux")]
    pub async fn spawn(state: TrayState) -> Option<TrayHandle> {
        sni::spawn(state).await.map(|inner| TrayHandle { inner })
    }

    #[cfg(not(target_os = "linux"))]
    pub async fn spawn(_state: TrayState) -> Option<TrayHandle> {
        tracing::warn!("no tray backend on this platform yet; use `pingtray view`");
        None
    }

    pub async fn update(&self, state: TrayState) {
        #[cfg(target_os = "linux")]
        self.inner.update(move |t| t.0 = state).await;
        #[cfg(not(target_os = "linux"))]
        let _ = state;
    }
}

/// The charging-mode icon: what the second tray item shows.
#[derive(Clone, Default)]
pub struct PowerTrayState {
    pub icon_file: Option<PathBuf>,
    pub pixmap: (usize, usize, Vec<u8>),
    pub status: Vec<String>,
    pub preserve: bool,
    pub threshold: u32,
    /// Shared with the power loop: automatic (pingtray decides) or manual.
    pub automatic: Arc<std::sync::atomic::AtomicBool>,
    pub config_path: PathBuf,
}

#[cfg(target_os = "linux")]
mod power_sni {
    use super::*;
    use ksni::TrayMethods;

    pub struct PowerTray(pub PowerTrayState);

    impl ksni::Tray for PowerTray {
        fn id(&self) -> String {
            "pingtray-power".into()
        }
        fn title(&self) -> String {
            "pingtray charging".into()
        }
        fn icon_name(&self) -> String {
            self.0.icon_file.as_ref().map(|p| p.display().to_string()).unwrap_or_default()
        }
        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            let (w, h, data) = &self.0.pixmap;
            // with a file, no pixmap: GNOME's extension would draw it after the
            // wide file image, squeezed into a square
            if data.is_empty() || self.0.icon_file.is_some() {
                return Vec::new();
            }
            vec![ksni::Icon { width: *w as i32, height: *h as i32, data: data.clone() }]
        }
        fn tool_tip(&self) -> ksni::ToolTip {
            ksni::ToolTip { title: "Battery charging".into(), description: self.0.status.join("\n"), ..Default::default() }
        }
        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            use ksni::menu::{RadioGroup, RadioItem, StandardItem};
            let mut items: Vec<ksni::MenuItem<Self>> = self
                .0
                .status
                .iter()
                .map(|line| StandardItem { label: line.replace('_', "__"), enabled: false, ..Default::default() }.into())
                .collect();
            items.push(ksni::MenuItem::Separator);
            let automatic = self.0.automatic.load(Ordering::Relaxed);
            items.push(
                RadioGroup {
                    selected: if automatic { 0 } else if self.0.preserve { 2 } else { 1 },
                    select: Box::new(|t: &mut Self, i: usize| {
                        let automatic = i == 0;
                        t.0.automatic.store(automatic, Ordering::Relaxed);
                        crate::power::save_automatic(automatic);
                        if automatic {
                            crate::power::poke();
                        } else {
                            t.0.preserve = i == 2;
                            crate::power::set_mode(i == 2);
                        }
                    }),
                    options: vec![
                        RadioItem { label: "Automatic".into(), ..Default::default() },
                        RadioItem { label: "Maximize charge".into(), ..Default::default() },
                        RadioItem {
                            label: format!("Preserve battery health ({} %)", self.0.threshold),
                            ..Default::default()
                        },
                    ],
                }
                .into(),
            );
            items.push(
                StandardItem {
                    label: "Settings…".into(),
                    activate: Box::new(|t: &mut Self| open_power_settings(&t.0.config_path)),
                    ..Default::default()
                }
                .into(),
            );
            items
        }
    }

    pub async fn spawn(state: PowerTrayState) -> Option<ksni::Handle<PowerTray>> {
        PowerTray(state)
            .assume_sni_available(true)
            .spawn()
            .await
            .map_err(|e| tracing::warn!("charging icon: {e}"))
            .ok()
    }
}

#[derive(Clone)]
pub struct PowerTrayHandle {
    #[cfg(target_os = "linux")]
    inner: ksni::Handle<power_sni::PowerTray>,
}

impl PowerTrayHandle {
    #[cfg(target_os = "linux")]
    pub async fn spawn(state: PowerTrayState) -> Option<PowerTrayHandle> {
        power_sni::spawn(state).await.map(|inner| PowerTrayHandle { inner })
    }

    #[cfg(not(target_os = "linux"))]
    pub async fn spawn(_state: PowerTrayState) -> Option<PowerTrayHandle> {
        None
    }

    pub async fn update(&self, state: PowerTrayState) {
        #[cfg(target_os = "linux")]
        self.inner.update(move |t| t.0 = state).await;
        #[cfg(not(target_os = "linux"))]
        let _ = state;
    }
}
