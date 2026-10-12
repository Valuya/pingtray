//! System tray front-end. The collector only talks to [`TrayHandle`], so porting
//! to another platform means adding a backend here (e.g. the `tray-icon` crate
//! for Windows/macOS) without touching the rest.

use std::path::PathBuf;

use crate::icon::{Bar, Thresholds};

/// What the tray shows; rebuilt by the collector after every sample.
#[derive(Clone)]
pub struct TrayState {
    pub bars: Vec<Bar>,
    pub tooltip: String,
    pub thresholds: Thresholds,
    pub config_path: PathBuf,
}

/// Opens the big graph window in a separate process, so the tray stays light.
pub fn open_viewer(config_path: &std::path::Path) {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return tracing::error!("cannot locate own executable: {e}"),
    };
    match std::process::Command::new(exe).arg("--config").arg(config_path).arg("view").spawn() {
        // reap the child so it does not linger as a zombie
        Ok(mut child) => drop(std::thread::spawn(move || child.wait())),
        Err(e) => tracing::error!("cannot start viewer: {e}"),
    }
}

fn open_in_editor(path: &std::path::Path) {
    #[cfg(target_os = "linux")]
    let opener = "xdg-open";
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(windows)]
    let opener = "explorer";
    if let Ok(mut child) = std::process::Command::new(opener).arg(path).spawn() {
        std::thread::spawn(move || child.wait());
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
        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            [32, 64]
                .into_iter()
                .map(|size| ksni::Icon {
                    width: size as i32,
                    height: size as i32,
                    data: crate::icon::render_argb(&self.0.bars, size, self.0.thresholds),
                })
                .collect()
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
            use ksni::menu::StandardItem;
            vec![
                StandardItem {
                    label: "Show graph".into(),
                    activate: Box::new(|t: &mut Self| open_viewer(&t.0.config_path)),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: "Edit configuration".into(),
                    activate: Box::new(|t: &mut Self| open_in_editor(&t.0.config_path)),
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
            ]
        }
    }

    pub async fn spawn(state: TrayState) -> Option<ksni::Handle<SniTray>> {
        match SniTray(state).spawn().await {
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

    pub async fn update(&self, bars: Vec<Bar>, tooltip: String) {
        #[cfg(target_os = "linux")]
        self.inner
            .update(move |t| {
                t.0.bars = bars;
                t.0.tooltip = tooltip;
            })
            .await;
        #[cfg(not(target_os = "linux"))]
        let _ = (bars, tooltip);
    }
}
