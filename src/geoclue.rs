//! Positions from the computer itself, through GeoClue: Wi-Fi positioning
//! (beaconDB), a phone's GPS shared on the local network (NMEA over mDNS,
//! which works over a hotspot link without any mobile coverage), and as a
//! last resort an IP-based guess. No setup, no account.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::OwnedObjectPath;

use crate::location::Fix;
use crate::store::Store;

const SERVICE: &str = "org.freedesktop.GeoClue2";
/// GeoClue's "exact": GPS when available.
const ACCURACY_EXACT: u32 = 8;
/// Re-record the current estimate this often while it does not change, so
/// that time spent in one place still has a position.
const RESTATE: Duration = Duration::from_secs(60);
const SOURCE: &str = "geoclue";
/// Coarser than this is an IP-based guess (often tens of km off): useless
/// for locating bad connectivity, and misleading on a map.
pub const USELESS_M: f64 = 5_000.0;

fn read_fix(conn: &Connection, path: &OwnedObjectPath) -> Result<Fix> {
    let loc = Proxy::new(conn, SERVICE, path.as_str(), "org.freedesktop.GeoClue2.Location")?;
    let (secs, usecs): (u64, u64) = loc.get_property("Timestamp")?;
    let known = |v: f64| (v.is_finite() && v > -1e300 && v >= 0.0).then_some(v);
    Ok(Fix {
        ts_ms: (secs * 1000 + usecs / 1000) as i64,
        lat: loc.get_property("Latitude")?,
        lon: loc.get_property("Longitude")?,
        acc_m: Some(loc.get_property("Accuracy")?),
        alt_m: loc.get_property::<f64>("Altitude").ok().filter(|v| *v > -1e300),
        speed_ms: loc.get_property::<f64>("Speed").ok().and_then(known),
    })
}

fn run(db: &std::path::Path) -> Result<()> {
    let conn = Connection::system()?;
    let manager = Proxy::new(&conn, SERVICE, "/org/freedesktop/GeoClue2/Manager", "org.freedesktop.GeoClue2.Manager")?;
    let path: OwnedObjectPath = manager.call("GetClient", &())?;
    let client = Proxy::new(&conn, SERVICE, path.as_str(), "org.freedesktop.GeoClue2.Client")?;
    // GeoClue authorises apps by desktop file: ~/.local/share/applications/pingtray.desktop
    client.set_property("DesktopId", "pingtray")?;
    client.set_property("RequestedAccuracyLevel", ACCURACY_EXACT)?;
    client.set_property("DistanceThreshold", 0u32)?;
    client.call_method("Start", &())?;
    tracing::info!("GeoClue started");

    let mut store = Store::open(db)?;
    let mut last: Option<(OwnedObjectPath, Fix)> = None;
    let mut restated = Instant::now();
    loop {
        let current: OwnedObjectPath = client.get_property("Location")?;
        if current.as_str() != "/" {
            let fresh = last.as_ref().is_none_or(|(p, _)| *p != current);
            if fresh {
                let fix = read_fix(&conn, &current)?;
                if fix.acc_m.is_some_and(|a| a > USELESS_M) {
                    tracing::debug!("GeoClue: only a rough guess ({:?} m), not recorded", fix.acc_m);
                    last = None;
                    std::thread::sleep(Duration::from_secs(10));
                    continue;
                }
                store.insert_fixes(&[fix], SOURCE)?;
                last = Some((current, fix));
                restated = Instant::now();
            } else if restated.elapsed() >= RESTATE
                && let Some((_, fix)) = &last
            {
                // still the best estimate: say so now
                let now = Fix { ts_ms: crate::pinger::now_ms(), speed_ms: None, ..*fix };
                store.insert_fixes(&[now], SOURCE)?;
                restated = Instant::now();
            }
        }
        std::thread::sleep(Duration::from_secs(10));
    }
}

/// Keeps a GeoClue client running, restarting it after errors.
pub fn spawn(db: PathBuf) {
    std::thread::spawn(move || {
        loop {
            if let Err(e) = run(&db) {
                tracing::warn!("GeoClue: {e:#}");
            }
            std::thread::sleep(Duration::from_secs(60));
        }
    });
}

/// Writes the desktop file GeoClue needs to authorise pingtray.
pub fn ensure_desktop_file() {
    let Some(base) = directories::BaseDirs::new() else { return };
    let path = base.data_dir().join("applications").join("pingtray.desktop");
    if path.exists() {
        return;
    }
    let exe = crate::tray::own_exe().unwrap_or_else(|_| PathBuf::from("pingtray"));
    let _ = std::fs::create_dir_all(path.parent().expect("has a parent"));
    let _ = std::fs::write(
        &path,
        format!(
            "[Desktop Entry]\nType=Application\nName=pingtray\nComment=Network latency in the tray, with history and map\n\
             Exec={} view\nIcon=network-wireless-signal-good-symbolic\nCategories=Network;Monitor;\n",
            exe.display()
        ),
    );
}
