//! The phone's GPS as an NMEA stream on the local network, e.g. from
//! gpsdRelay (Android, F-Droid). Nothing to discover in either direction:
//!
//! - UDP: the phone broadcasts on its network (home Wi-Fi, or its own
//!   hotspot), pingtray listens;
//! - TCP: the phone runs a server, pingtray connects to it; by default at
//!   the gateway, which is the phone itself while tethered.
//!
//! Only the local link is used, so positions keep coming where there is no
//! mobile coverage, which is what pingtray is looking for.

use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::time::Duration;

use chrono::{NaiveDate, NaiveTime};

use crate::location::Fix;
use crate::store::Store;

const SOURCE: &str = "nmea";
/// GPS speaks once a second; one fix every few seconds is plenty.
const MIN_INTERVAL_MS: i64 = 3_000;

/// "4807.038" with "N" → 48.1173 (ddmm.mmmm or dddmm.mmmm).
fn coord(value: &str, hemisphere: &str) -> Option<f64> {
    let v: f64 = value.parse().ok()?;
    let degrees = (v / 100.0).trunc();
    let decimal = degrees + (v - degrees * 100.0) / 60.0;
    match hemisphere {
        "N" | "E" => Some(decimal),
        "S" | "W" => Some(-decimal),
        _ => None,
    }
}

fn checksum_ok(sentence: &str) -> bool {
    let Some((body, sum)) = sentence.trim_start_matches('$').split_once('*') else { return true };
    let computed = body.bytes().fold(0u8, |a, b| a ^ b);
    u8::from_str_radix(sum.trim(), 16).is_ok_and(|s| s == computed)
}

/// Accumulates RMC (position, time, speed) and GGA (altitude, precision).
#[derive(Default)]
pub struct Parser {
    hdop: Option<f64>,
    alt: Option<f64>,
}

impl Parser {
    /// A fix from a valid RMC sentence; GGA only refines the next ones.
    pub fn feed(&mut self, line: &str) -> Option<Fix> {
        let line = line.trim();
        if !line.starts_with('$') || !checksum_ok(line) {
            return None;
        }
        let body = line.trim_start_matches('$').split('*').next()?;
        let f: Vec<&str> = body.split(',').collect();
        let kind = f.first()?.get(2..)?;
        match kind {
            "GGA" if f.len() > 9 => {
                // quality 0 = no fix
                if f[6] != "0" {
                    self.hdop = f[8].parse().ok();
                    self.alt = f[9].parse().ok();
                }
                None
            }
            "RMC" if f.len() > 9 && f[2] == "A" => {
                let time = NaiveTime::parse_from_str(f[1].split('.').next()?, "%H%M%S").ok()?;
                let date = NaiveDate::parse_from_str(f[9], "%d%m%y").ok()?;
                let frac_ms = f[1].split('.').nth(1).and_then(|s| format!("0.{s}").parse::<f64>().ok()).unwrap_or(0.0);
                Some(Fix {
                    ts_ms: date.and_time(time).and_utc().timestamp_millis() + (frac_ms * 1000.0) as i64,
                    lat: coord(f[3], f[4])?,
                    lon: coord(f[5], f[6])?,
                    // HDOP × ~5 m is the usual rough conversion
                    acc_m: self.hdop.map(|h| (h * 5.0).max(3.0)),
                    alt_m: self.alt,
                    speed_ms: f[7].parse::<f64>().ok().map(|knots| knots * 0.514_444),
                })
            }
            _ => None,
        }
    }
}

/// Stores fixes, at most one every `MIN_INTERVAL_MS`.
struct Sink {
    store: Store,
    last_ms: i64,
}

impl Sink {
    fn take(&mut self, fix: Fix) {
        if fix.ts_ms - self.last_ms < MIN_INTERVAL_MS {
            return;
        }
        self.last_ms = fix.ts_ms;
        if let Err(e) = self.store.insert_fixes(&[fix], SOURCE) {
            tracing::warn!("NMEA positions: {e}");
        }
    }
}

fn listen_udp(port: u16, db: PathBuf) {
    std::thread::spawn(move || {
        let socket = match UdpSocket::bind(("0.0.0.0", port)) {
            Ok(s) => s,
            Err(e) => return tracing::error!("NMEA over UDP, port {port}: {e}"),
        };
        tracing::info!("listening for NMEA on UDP port {port}");
        let Ok(store) = Store::open(&db) else { return };
        let mut sink = Sink { store, last_ms: 0 };
        let mut parser = Parser::default();
        let mut buf = [0u8; 4096];
        loop {
            let Ok((n, _)) = socket.recv_from(&mut buf) else { continue };
            for line in String::from_utf8_lossy(&buf[..n]).lines() {
                if let Some(fix) = parser.feed(line) {
                    sink.take(fix);
                }
            }
        }
    });
}

/// The default gateway: the phone itself while tethered to its hotspot.
fn gateway() -> Option<std::net::IpAddr> {
    let routes = std::fs::read_to_string("/proc/net/route").ok()?;
    routes.lines().skip(1).find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.get(1) != Some(&"00000000") {
            return None;
        }
        let g = u32::from_str_radix(f.get(2)?, 16).ok()?;
        Some(std::net::IpAddr::V4(std::net::Ipv4Addr::from(g.to_le_bytes())))
    })
}

fn connect_tcp(host: Option<String>, port: u16, db: PathBuf) {
    std::thread::spawn(move || {
        let Ok(store) = Store::open(&db) else { return };
        let mut sink = Sink { store, last_ms: 0 };
        loop {
            let target: Option<SocketAddr> = match &host {
                Some(h) => std::net::ToSocketAddrs::to_socket_addrs(&(h.as_str(), port)).ok().and_then(|mut a| a.next()),
                None => gateway().map(|ip| SocketAddr::new(ip, port)),
            };
            if let Some(addr) = target
                && let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(3))
            {
                tracing::info!("reading NMEA from {addr}");
                let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                let mut parser = Parser::default();
                for line in BufReader::new(stream).lines() {
                    let Ok(line) = line else { break };
                    if let Some(fix) = parser.feed(&line) {
                        sink.take(fix);
                    }
                }
                tracing::info!("NMEA from {addr}: disconnected");
            }
            std::thread::sleep(Duration::from_secs(10));
        }
    });
}

pub fn spawn(cfg: &crate::config::NmeaConfig, db: PathBuf) {
    if let Some(port) = cfg.udp_port {
        listen_udp(port, db.clone());
    }
    if let Some(port) = cfg.tcp_port {
        connect_tcp(cfg.tcp_host.clone(), port, db);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rmc_and_gga() {
        let mut p = Parser::default();
        assert!(p.feed("$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*47").is_none());
        let f = p.feed("$GPRMC,123519,A,4807.038,N,01131.000,E,022.4,084.4,230394,003.1,W*6A").unwrap();
        assert!((f.lat - 48.1173).abs() < 1e-4 && (f.lon - 11.516_666).abs() < 1e-4);
        assert_eq!(f.alt_m, Some(545.4));
        assert_eq!(f.acc_m, Some(4.5));
        assert!((f.speed_ms.unwrap() - 11.52).abs() < 0.01);
        assert_eq!(f.ts_ms, 764_426_119_000);
        // void fix, bad checksum
        assert!(p.feed("$GPRMC,123519,V,,,,,,,230394,,*00").is_none());
        assert!(p.feed("$GPRMC,123519,A,4807.038,N,01131.000,E,022.4,084.4,230394,003.1,W*00").is_none());
    }

    #[test]
    fn southern_and_western() {
        assert!((coord("3345.000", "S").unwrap() + 33.75).abs() < 1e-9);
        assert!((coord("07030.000", "W").unwrap() + 70.5).abs() < 1e-9);
    }
}
