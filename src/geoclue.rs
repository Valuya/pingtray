//! Positions from the computer itself, through GeoClue: Wi-Fi positioning
//! (beaconDB), a phone's GPS shared on the local network (NMEA over mDNS,
//! which works over a hotspot link without any mobile coverage), and as a
//! last resort an IP-based guess. No setup, no account.
//!
//! Sampled, never left running: at street accuracy or better, GeoClue starts
//! a passive Wi-Fi scan 10 s after the previous one ends for as long as a
//! client is started, and each scan takes the radio off its channel for about
//! 4 s. A client left running cost a third of the time in latency spikes and
//! lost pings, the very thing pingtray measures. So: start, take one fix, stop.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::OwnedObjectPath;

use crate::location::Fix;
use crate::pinger::now_ms;
use crate::store::Store;

const SERVICE: &str = "org.freedesktop.GeoClue2";
/// GeoClue's "exact": GPS when available.
const ACCURACY_EXACT: u32 = 8;
/// Longest wait for a fix once started: the scan and the beaconDB lookup take
/// about 6 s, and GeoClue would start a second scan at about 14 s.
const SAMPLE_TIMEOUT: Duration = Duration::from_secs(12);
/// A location there sooner than this after starting may be GeoClue's cached
/// one; a later one comes from the new scan.
const FRESH_AFTER: Duration = Duration::from_secs(5);
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

/// The location GeoClue settles on: the first one from the new scan, or else
/// the cached one, which an unchanged scan leaves as it is.
fn wait_for_location(client: &Proxy, started: Instant) -> Result<Option<OwnedObjectPath>> {
    let mut latest: Option<OwnedObjectPath> = None;
    while started.elapsed() < SAMPLE_TIMEOUT {
        let current: OwnedObjectPath = client.get_property("Location")?;
        if current.as_str() != "/" && latest.as_ref() != Some(&current) {
            if started.elapsed() >= FRESH_AFTER {
                return Ok(Some(current));
            }
            latest = Some(current);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(latest)
}

/// One position: starts a client, waits for the fix from its scan, stops.
fn sample(conn: &Connection) -> Result<Option<Fix>> {
    let manager = Proxy::new(conn, SERVICE, "/org/freedesktop/GeoClue2/Manager", "org.freedesktop.GeoClue2.Manager")?;
    let path: OwnedObjectPath = manager.call("GetClient", &())?;
    let client = Proxy::new(conn, SERVICE, path.as_str(), "org.freedesktop.GeoClue2.Client")?;
    // GeoClue authorises apps by desktop file: ~/.local/share/applications/pingtray.desktop
    client.set_property("DesktopId", "pingtray")?;
    client.set_property("RequestedAccuracyLevel", ACCURACY_EXACT)?;
    client.set_property("DistanceThreshold", 0u32)?;
    let started_ms = now_ms();
    let started = Instant::now();
    client.call_method("Start", &())?;
    let found = wait_for_location(&client, started);
    // whatever happened: a started client keeps GeoClue scanning
    let _ = client.call_method("Stop", &());
    let Some(path) = found? else { return Ok(None) };
    let fix = read_fix(conn, &path)?;
    if fix.ts_ms < started_ms {
        // the estimate did not change: still true now
        return Ok(Some(Fix { ts_ms: now_ms(), speed_ms: None, ..fix }));
    }
    Ok(Some(fix))
}

fn run(db: &std::path::Path, every: Duration) -> Result<()> {
    let conn = Connection::system()?;
    let mut store = Store::open(db)?;
    loop {
        let elsewhere = store.last_fix_ms_except(SOURCE)?;
        if elsewhere.is_some_and(|t| now_ms() - t < every.as_millis() as i64) {
            tracing::debug!("GeoClue: recent positions from elsewhere, no scan");
        } else if let Some(fix) = sample(&conn)? {
            if fix.acc_m.is_some_and(|a| a > USELESS_M) {
                tracing::debug!("GeoClue: only a rough guess ({:?} m), not recorded", fix.acc_m);
            } else {
                store.insert_fixes(&[fix], SOURCE)?;
            }
        }
        std::thread::sleep(every);
    }
}

/// Takes a GeoClue position every `every`, unless other sources (the phone)
/// gave one meanwhile; starts again after errors.
pub fn spawn(db: PathBuf, every: Duration) {
    std::thread::spawn(move || {
        tracing::info!("GeoClue: a position every {} s", every.as_secs());
        loop {
            if let Err(e) = run(&db, every) {
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
