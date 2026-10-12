//! System tray front-end. The collector only talks to [`TrayHandle`], so porting
//! to another platform means adding a backend here (e.g. the `tray-icon` crate
//! for Windows/macOS) without touching the rest.

use std::path::PathBuf;

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
        fn icon_name(&self) -> String {
            // an absolute path: not in the spec, but the AppIndicator extension
            // and others accept it
            self.0.icon_file.as_ref().map(|p| p.display().to_string()).unwrap_or_default()
        }
        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            let (w, h, data) = &self.0.pixmap;
            if data.is_empty() {
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

    pub async fn update(&self, state: TrayState) {
        #[cfg(target_os = "linux")]
        self.inner.update(move |t| t.0 = state).await;
        #[cfg(not(target_os = "linux"))]
        let _ = state;
    }
}
