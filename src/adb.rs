//! The phone's GPS through ADB (USB, or wireless debugging), over the local
//! link only: no internet, no account, no address to configure. Works in
//! dead zones, which are what pingtray is looking for.
//!
//! - reads the phone's latest fix (`dumpsys location`): GPS, fused or
//!   network, whichever is freshest and most accurate;
//! - keeps `adb reverse` up, so that apps on the phone (OwnTracks) can post
//!   to `http://127.0.0.1:<port>/pub`, an address that never changes.
//!
//! The phone has to be paired once (`adb pair`, done by hand).

use std::process::{Command, Stdio};
use std::time::Duration;

use crate::location::Fix;
use crate::store::Store;

const SOURCE: &str = "phone-adb";
const POLL: Duration = Duration::from_secs(15);
/// An older fix means the phone is not tracking (no app asks for location).
const MAX_AGE_MS: i64 = 90_000;

/// Runs adb with a timeout; `None` on failure.
fn adb(args: &[&str]) -> Option<String> {
    let mut child = Command::new("adb").args(args).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                std::io::Read::read_to_string(child.stdout.as_mut()?, &mut out).ok()?;
                return status.success().then_some(out);
            }
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// A connected phone, reconnecting a known wireless one when needed.
fn device() -> Option<String> {
    let connected = |out: &str| {
        out.lines()
            .skip(1)
            .filter_map(|l| l.split_once('\t'))
            .find(|(_, state)| state.trim() == "device")
            .map(|(serial, _)| serial.to_string())
    };
    if let Some(d) = connected(&adb(&["devices"])?) {
        return Some(d);
    }
    // wireless debugging: paired phones announce themselves over mDNS
    let services = adb(&["mdns", "services"])?;
    for line in services.lines().filter(|l| l.contains("_adb-tls-connect")) {
        if let Some(addr) = line.split_whitespace().last() {
            adb(&["connect", addr]);
        }
    }
    connected(&adb(&["devices"])?)
}

/// "+17d7h52m7s30ms" (time since boot) in ms.
fn parse_elapsed(s: &str) -> Option<i64> {
    let mut total = 0i64;
    let mut num = String::new();
    let mut chars = s.trim_start_matches('+').chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let n: i64 = num.parse().ok()?;
        num.clear();
        total += match c {
            'd' => n * 86_400_000,
            'h' => n * 3_600_000,
            'm' if chars.peek() == Some(&'s') => {
                chars.next();
                n
            }
            'm' => n * 60_000,
            's' => n * 1000,
            _ => return None,
        };
    }
    num.is_empty().then_some(total)
}

/// One "Location[gps 50.558310,4.680813 hAcc=2.04 et=+17d7h52m6s516ms alt=184.0 ... vel=0.0 ...]".
fn parse_location(line: &str, uptime_ms: i64, now_ms: i64) -> Option<(String, Fix)> {
    let inner = line.split("Location[").nth(1)?;
    let mut parts = inner.split_whitespace();
    let provider = parts.next()?.to_string();
    let (lat, lon) = parts.next()?.split_once(',')?;
    let field = |key: &str| {
        inner.split_whitespace().find_map(|p| p.strip_prefix(key)).map(|v| v.trim_end_matches(']').to_string())
    };
    let elapsed = parse_elapsed(&field("et=")?)?;
    let num = |key: &str| field(key).and_then(|v| v.parse::<f64>().ok());
    let fix = Fix {
        ts_ms: now_ms - (uptime_ms - elapsed),
        lat: lat.parse().ok()?,
        lon: lon.parse().ok()?,
        acc_m: num("hAcc="),
        alt_m: num("mslAlt=").or(num("alt=")),
        speed_ms: num("vel="),
    };
    Some((provider, fix))
}

/// The freshest fix among the providers, the most accurate one when equally fresh.
fn best_fix(dump: &str, now_ms: i64) -> Option<Fix> {
    let mut lines = dump.lines();
    let uptime_ms = (lines.next()?.split_whitespace().next()?.parse::<f64>().ok()? * 1000.0) as i64;
    lines
        .filter(|l| l.contains("Location["))
        .filter_map(|l| parse_location(l, uptime_ms, now_ms))
        .map(|(_, f)| f)
        .filter(|f| now_ms - f.ts_ms <= MAX_AGE_MS && f.ts_ms <= now_ms + 5_000)
        .max_by(|a, b| {
            // within 5 s, accuracy wins; otherwise freshness
            if (a.ts_ms - b.ts_ms).abs() <= 5_000 {
                b.acc_m.unwrap_or(f64::MAX).total_cmp(&a.acc_m.unwrap_or(f64::MAX))
            } else {
                a.ts_ms.cmp(&b.ts_ms)
            }
        })
}

pub fn spawn(db: std::path::PathBuf, reverse_port: Option<u16>) {
    std::thread::spawn(move || {
        if adb(&["version"]).is_none() {
            return tracing::info!("adb not installed: no positions from the phone over ADB");
        }
        let mut store = match Store::open(&db) {
            Ok(s) => s,
            Err(e) => return tracing::error!("phone positions: {e}"),
        };
        let mut last_ts = 0;
        let mut reversed: Option<String> = None;
        loop {
            if let Some(dev) = device() {
                if let Some(port) = reverse_port
                    && reversed.as_deref() != Some(dev.as_str())
                {
                    let spec = format!("tcp:{port}");
                    if adb(&["-s", &dev, "reverse", &spec, &spec]).is_some() {
                        tracing::info!("phone {dev}: 127.0.0.1:{port} reaches pingtray");
                        reversed = Some(dev.clone());
                    }
                }
                let dump = adb(&["-s", &dev, "shell", "cat /proc/uptime; dumpsys location | grep 'Location\\['"]);
                if let Some(fix) = dump.as_deref().and_then(|d| best_fix(d, crate::pinger::now_ms()))
                    && fix.ts_ms > last_ts
                {
                    last_ts = fix.ts_ms;
                    if let Err(e) = store.insert_fixes(&[fix], SOURCE) {
                        tracing::warn!("phone positions: {e}");
                    }
                }
            } else {
                reversed = None;
            }
            std::thread::sleep(POLL);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &str = "1497127.50 2900000.00
      last location=Location[fused 50.558310,4.680827 hAcc=3.449 et=+17d7h52m7s30ms alt=193.4 vAcc=1.47 mslAlt=147.43 vel=0.069 sAcc=0.07]
      last location=Location[network 50.558356,4.680810 hAcc=17.673 et=+17d7h51m58s625ms alt=193.4]
      last location=Location[gps 50.558310,4.680813 hAcc=2.0408163 et=+17d7h52m6s516ms alt=184.0 mslAlt=138.04 vel=0.0 sAcc=0.13 {Bundle[{satellites=6}]}]";

    #[test]
    fn elapsed_times() {
        assert_eq!(parse_elapsed("+17d7h52m7s30ms"), Some(((17 * 24 + 7) * 3600 + 52 * 60 + 7) * 1000 + 30));
        assert_eq!(parse_elapsed("+2m5s"), Some(125_000));
    }

    #[test]
    fn picks_the_most_accurate_of_the_fresh_fixes() {
        // uptime 1497127.5 s = 17d7h52m7.5s: gps is 1 s old, fused 0.5 s, network 9 s
        let f = best_fix(DUMP, 1_000_000_000).unwrap();
        assert_eq!(f.acc_m, Some(2.0408163));
        assert_eq!(f.ts_ms, 1_000_000_000 - 984);
        assert_eq!(f.alt_m, Some(138.04));
        // the same dump read much later: everything too old
        assert!(best_fix(&DUMP.replacen("1497127.50", "1497500.00", 1), 1_000_000_000).is_none());
    }
}
