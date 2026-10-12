//! Positions: where the pings were sent from. They come from the phone, either
//! pushed live (OwnTracks HTTP mode, GPSLogger custom URL) or as files (GPX,
//! CSV, OwnTracks .rec, JSON) imported by hand or picked up from a folder.

use std::collections::HashMap;
use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::store::Store;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fix {
    pub ts_ms: i64,
    pub lat: f64,
    pub lon: f64,
    /// Horizontal accuracy, metres.
    pub acc_m: Option<f64>,
    pub alt_m: Option<f64>,
    /// Speed reported by the device, m/s.
    pub speed_ms: Option<f64>,
}

impl Fix {
    fn valid(&self) -> bool {
        self.lat.is_finite()
            && self.lon.is_finite()
            && self.lat.abs() <= 90.0
            && self.lon.abs() <= 180.0
            && !(self.lat == 0.0 && self.lon == 0.0)
            && self.ts_ms > 946_684_800_000 // 2000-01-01
    }
}

/// Unix seconds or milliseconds as a number, or an RFC 3339 / ISO 8601 date.
pub fn parse_time(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Ok(n) = s.parse::<f64>() {
        return Some(if n > 1e11 { n as i64 } else { (n * 1000.0) as i64 });
    }
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(t.timestamp_millis());
    }
    // "2026-10-12 08:00:00" or "2026-10-12T08:00:00" without zone: assume UTC
    let s = s.trim_end_matches('Z').replace(' ', "T");
    chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S%.f")
        .ok()
        .map(|t| t.and_utc().timestamp_millis())
}

fn num(v: &serde_json::Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// One OwnTracks message, or an array of them. Non-location messages are skipped.
pub fn parse_owntracks(body: &[u8]) -> Result<Vec<Fix>> {
    let v: serde_json::Value = serde_json::from_slice(body).context("invalid JSON")?;
    let items = match v {
        serde_json::Value::Array(a) => a,
        other => vec![other],
    };
    Ok(items.iter().filter_map(owntracks_fix).collect())
}

fn owntracks_fix(v: &serde_json::Value) -> Option<Fix> {
    if v.get("_type").and_then(|t| t.as_str()).is_some_and(|t| t != "location") {
        return None;
    }
    let fix = Fix {
        ts_ms: (num(v.get("tst")?)? * 1000.0) as i64,
        lat: num(v.get("lat")?)?,
        lon: num(v.get("lon")?)?,
        acc_m: v.get("acc").and_then(num),
        alt_m: v.get("alt").and_then(num),
        // OwnTracks reports km/h
        speed_ms: v.get("vel").and_then(num).filter(|s| *s >= 0.0).map(|kmh| kmh / 3.6),
    };
    fix.valid().then_some(fix)
}

/// Key/value pairs as sent by GPSLogger's custom URL (or any similar app).
pub fn parse_params(params: &HashMap<String, String>) -> Option<Fix> {
    let get = |keys: &[&str]| keys.iter().find_map(|k| params.get(*k)).map(|s| s.as_str());
    let f = |keys: &[&str]| get(keys).and_then(|s| s.trim().parse::<f64>().ok()).filter(|v| v.is_finite());
    let ts_ms = match get(&["timestamp", "ts", "time", "tst", "date"]) {
        Some(t) => parse_time(t)?,
        None => crate::pinger::now_ms(),
    };
    let fix = Fix {
        ts_ms,
        lat: f(&["lat", "latitude"])?,
        lon: f(&["lon", "lng", "longitude"])?,
        acc_m: f(&["acc", "accuracy"]),
        alt_m: f(&["alt", "altitude", "ele", "elevation"]),
        speed_ms: f(&["spd", "speed"]).filter(|s| *s >= 0.0),
    };
    fix.valid().then_some(fix)
}

fn parse_gpx(text: &str) -> Result<Vec<Fix>> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(text);
    reader.config_mut().trim_text(true);
    let mut out = Vec::new();
    let mut cur: Option<(f64, f64)> = None;
    let mut fields: HashMap<String, String> = HashMap::new();
    let mut tag = String::new();
    loop {
        match reader.read_event()? {
            Event::Start(e) | Event::Empty(e)
                if matches!(e.local_name().as_ref(), "trkpt" | "wpt" | "rtept") =>
            {
                let mut lat = None;
                let mut lon = None;
                for a in e.attributes().flatten() {
                    let v = a.value.parse::<f64>().ok();
                    match a.key.local_name().as_ref() {
                        "lat" => lat = v,
                        "lon" => lon = v,
                        _ => {}
                    }
                }
                cur = lat.zip(lon);
                fields.clear();
            }
            Event::Start(e) => tag = e.local_name().as_ref().to_string(),
            Event::Text(t) if cur.is_some() => {
                fields.insert(tag.clone(), t.into_inner().trim().to_string());
            }
            Event::End(e) if matches!(e.local_name().as_ref(), "trkpt" | "wpt" | "rtept") => {
                if let (Some((lat, lon)), Some(ts_ms)) =
                    (cur.take(), fields.get("time").and_then(|t| parse_time(t)))
                {
                    let f = |k: &str| fields.get(k).and_then(|v| v.parse::<f64>().ok());
                    let fix = Fix {
                        ts_ms,
                        lat,
                        lon,
                        acc_m: f("hdop").map(|h| h * 5.0).or(f("accuracy")),
                        alt_m: f("ele"),
                        speed_ms: f("speed"),
                    };
                    if fix.valid() {
                        out.push(fix);
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

/// CSV with a header naming the columns (GPSLogger's format among others).
fn parse_csv(text: &str) -> Vec<Fix> {
    let mut lines = text.lines();
    let Some(header) = lines.next() else { return Vec::new() };
    let names: Vec<String> = header.split(',').map(|h| h.trim().trim_matches('"').to_lowercase()).collect();
    lines
        .filter_map(|line| {
            let params = names
                .iter()
                .cloned()
                .zip(line.split(',').map(|v| v.trim().trim_matches('"').to_string()))
                .filter(|(_, v)| !v.is_empty())
                .collect::<HashMap<_, _>>();
            // without a time column the row cannot be placed
            ["timestamp", "ts", "time", "tst", "date"].iter().any(|k| params.contains_key(*k)).then_some(())?;
            parse_params(&params)
        })
        .collect()
}

/// OwnTracks recorder files: `<iso time>\t<topic>\t<json>` per line.
fn parse_rec(text: &str) -> Vec<Fix> {
    text.lines()
        .filter_map(|line| line.rsplit('\t').next())
        .filter_map(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .filter_map(|v| owntracks_fix(&v))
        .collect()
}

/// GeoJSON point features with a time property, or OwnTracks JSON.
fn parse_json(text: &str) -> Result<Vec<Fix>> {
    let v: serde_json::Value = serde_json::from_str(text)?;
    if let Some(features) = v.get("features").and_then(|f| f.as_array()) {
        return Ok(features
            .iter()
            .filter_map(|f| {
                let c = f.pointer("/geometry/coordinates")?.as_array()?;
                let p = f.get("properties")?;
                let t = ["time", "timestamp", "date"].iter().find_map(|k| p.get(*k))?;
                let ts_ms = match t {
                    serde_json::Value::String(s) => parse_time(s)?,
                    other => parse_time(&other.to_string())?,
                };
                let fix = Fix {
                    ts_ms,
                    lon: num(c.first()?)?,
                    lat: num(c.get(1)?)?,
                    alt_m: c.get(2).and_then(num),
                    acc_m: p.get("accuracy").and_then(num),
                    speed_ms: p.get("speed").and_then(num),
                };
                fix.valid().then_some(fix)
            })
            .collect());
    }
    parse_owntracks(text.as_bytes())
}

pub const FILE_EXTENSIONS: [&str; 5] = ["gpx", "csv", "rec", "json", "geojson"];

pub fn parse_file(path: &Path) -> Result<Vec<Fix>> {
    let text = std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    match ext.as_str() {
        "gpx" => parse_gpx(&text),
        "csv" | "txt" => Ok(parse_csv(&text)),
        "rec" => Ok(parse_rec(&text)),
        "json" | "geojson" => parse_json(&text),
        _ => anyhow::bail!("{}: unknown format (expected .gpx, .csv, .rec or .json)", path.display()),
    }
}

/// Imports files given on the command line.
pub fn import_files(store: &mut Store, paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        let fixes = parse_file(path)?;
        let added = store.insert_fixes(&fixes, &format!("file:{}", path.display()))?;
        println!("{}: {} positions, {} new", path.display(), fixes.len(), added);
    }
    Ok(())
}

/// Scans `dir` (recursively) for new or changed position files.
pub fn scan_dir(store: &mut Store, dir: &Path) -> Result<usize> {
    let mut stack = vec![dir.to_path_buf()];
    let mut added = 0;
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)?.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
            if !FILE_EXTENSIONS.contains(&ext.as_str()) {
                continue;
            }
            let stamp = format!(
                "{}:{}",
                meta.len(),
                meta.modified().ok().and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs())
            );
            let key = path.display().to_string();
            if store.file_stamp(&key)?.as_deref() == Some(stamp.as_str()) {
                continue;
            }
            match parse_file(&path) {
                Ok(fixes) => {
                    let n = store.insert_fixes(&fixes, &format!("file:{key}"))?;
                    if n > 0 {
                        tracing::info!("{key}: {n} new positions");
                    }
                    added += n;
                }
                Err(e) => tracing::warn!("{key}: {e}"),
            }
            store.set_file_stamp(&key, &stamp)?;
        }
    }
    Ok(added)
}

pub fn spawn_dir_watcher(dir: PathBuf, db: PathBuf) {
    std::thread::spawn(move || {
        let mut store = match Store::open(&db) {
            Ok(s) => s,
            Err(e) => return tracing::error!("location watcher: {e}"),
        };
        loop {
            if let Err(e) = scan_dir(&mut store, &dir) {
                tracing::warn!("scanning {}: {e}", dir.display());
            }
            std::thread::sleep(Duration::from_secs(60));
        }
    });
}

fn query_params(url: &str) -> HashMap<String, String> {
    let query = url.split_once('?').map(|(_, q)| q).unwrap_or("");
    form_params(query)
}

fn form_params(s: &str) -> HashMap<String, String> {
    s.split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (percent_decode(k).to_lowercase(), percent_decode(v)))
        .collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()).ok_or(()) {
                    Ok(b) => {
                        out.push(b);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn basic_auth_password(header: &str) -> Option<String> {
    use base64_lite::decode;
    let b64 = header.trim().strip_prefix("Basic ")?;
    let raw = decode(b64.trim())?;
    let text = String::from_utf8(raw).ok()?;
    text.split_once(':').map(|(_, p)| p.to_string())
}

/// Minimal base64 decoding, enough for HTTP Basic auth.
mod base64_lite {
    pub fn decode(s: &str) -> Option<Vec<u8>> {
        let val = |c: u8| -> Option<u32> {
            Some(match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'+' | b'-' => 62,
                b'/' | b'_' => 63,
                _ => return None,
            } as u32)
        };
        let clean: Vec<u8> = s.bytes().filter(|&c| c != b'=').collect();
        let mut out = Vec::new();
        for chunk in clean.chunks(4) {
            let mut acc = 0u32;
            for (i, &c) in chunk.iter().enumerate() {
                acc |= val(c)? << (18 - 6 * i);
            }
            let n = chunk.len() * 6 / 8;
            for i in 0..n {
                out.push((acc >> (16 - 8 * i)) as u8);
            }
        }
        Some(out)
    }
}

/// HTTP endpoint for phone apps. Any path works; the format is guessed:
/// a JSON body is OwnTracks, otherwise query/form parameters (GPSLogger).
/// Authentication: `?token=…`, or HTTP Basic with the token as password.
pub fn spawn_server(listen: SocketAddr, token: Option<String>, db: PathBuf) -> Result<()> {
    if token.is_none() && !listen.ip().is_loopback() {
        anyhow::bail!("[location] listen on {listen} needs a token");
    }
    let server = tiny_http::Server::http(listen).map_err(|e| anyhow::anyhow!("listening on {listen}: {e}"))?;
    tracing::info!("location endpoint on http://{listen}/");
    std::thread::spawn(move || {
        let mut store = match Store::open(&db) {
            Ok(s) => s,
            Err(e) => return tracing::error!("location endpoint: {e}"),
        };
        for mut req in server.incoming_requests() {
            let (status, body) = handle(&mut req, token.as_deref(), &mut store);
            let resp = tiny_http::Response::from_string(body).with_status_code(status).with_header(
                "Content-Type: application/json".parse::<tiny_http::Header>().expect("static header"),
            );
            let _ = req.respond(resp);
        }
    });
    Ok(())
}

fn handle(req: &mut tiny_http::Request, token: Option<&str>, store: &mut Store) -> (u16, String) {
    let mut params = query_params(req.url());
    if let Some(expected) = token {
        let basic = req
            .headers()
            .iter()
            .find(|h| h.field.equiv("Authorization"))
            .and_then(|h| basic_auth_password(h.value.as_str()));
        if params.get("token").map(String::as_str) != Some(expected) && basic.as_deref() != Some(expected) {
            return (401, r#"{"error":"unauthorized"}"#.into());
        }
    }
    let mut body = Vec::new();
    if req.as_reader().take(1 << 20).read_to_end(&mut body).is_err() {
        return (400, r#"{"error":"unreadable body"}"#.into());
    }
    let trimmed = String::from_utf8_lossy(&body).trim().to_string();
    let (fixes, source) = if trimmed.starts_with('{') || trimmed.starts_with('[') {
        match parse_owntracks(trimmed.as_bytes()) {
            Ok(f) => (f, "owntracks"),
            Err(e) => return (400, format!(r#"{{"error":"{e}"}}"#)),
        }
    } else {
        if !trimmed.is_empty() {
            params.extend(form_params(&trimmed));
        }
        (parse_params(&params).into_iter().collect(), "http")
    };
    if fixes.is_empty() && source == "http" && req.url().trim_start_matches('/').is_empty() {
        return (200, r#"{"pingtray":"location endpoint"}"#.into());
    }
    match store.insert_fixes(&fixes, source) {
        Ok(n) => {
            tracing::debug!("{n} positions via {source}");
            // OwnTracks expects a JSON array back
            (200, "[]".into())
        }
        Err(e) => (500, format!(r#"{{"error":"{e}"}}"#)),
    }
}

/// A position with its uncertainty radius (metres).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Estimate {
    pub lat: f64,
    pub lon: f64,
    pub uncertainty_m: f64,
}

/// Accuracy assumed when the device did not report one.
pub const DEFAULT_ACC_M: f64 = 30.0;
/// Drift assumed away from a fix, when only one side is known.
const DRIFT_MS: f64 = 1.5;

/// Like [`position_at`], with an honest uncertainty: the fixes' own accuracy,
/// plus the leeway of interpolating between them (the route may have bent),
/// plus drift when only one side is known.
pub fn estimate_at(fixes: &[Fix], ts_ms: i64, max_gap_ms: i64) -> Option<Estimate> {
    let i = fixes.partition_point(|f| f.ts_ms <= ts_ms);
    let acc = |f: &Fix| f.acc_m.unwrap_or(DEFAULT_ACC_M);
    match (i.checked_sub(1).map(|j| &fixes[j]), fixes.get(i)) {
        (Some(a), Some(b)) if b.ts_ms - a.ts_ms <= max_gap_ms => {
            let r = (ts_ms - a.ts_ms) as f64 / (b.ts_ms - a.ts_ms).max(1) as f64;
            let leeway = 0.5 * distance_m(a.lat, a.lon, b.lat, b.lon) * (4.0 * r * (1.0 - r)).sqrt();
            Some(Estimate {
                lat: a.lat + (b.lat - a.lat) * r,
                lon: a.lon + (b.lon - a.lon) * r,
                uncertainty_m: acc(a) + (acc(b) - acc(a)) * r + leeway * 0.5,
            })
        }
        (Some(a), _) if ts_ms - a.ts_ms <= max_gap_ms / 2 => Some(Estimate {
            lat: a.lat,
            lon: a.lon,
            uncertainty_m: acc(a) + DRIFT_MS * (ts_ms - a.ts_ms) as f64 / 1000.0,
        }),
        (_, Some(b)) if b.ts_ms - ts_ms <= max_gap_ms / 2 => Some(Estimate {
            lat: b.lat,
            lon: b.lon,
            uncertainty_m: acc(b) + DRIFT_MS * (b.ts_ms - ts_ms) as f64 / 1000.0,
        }),
        _ => None,
    }
}

/// Distance in metres (haversine).
pub fn distance_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = p2 - p1;
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * 6_371_000.0 * a.sqrt().asin()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owntracks_message() {
        let body = br#"{"_type":"location","tst":1791792000,"lat":50.56,"lon":4.69,"acc":12,"vel":36}"#;
        let f = parse_owntracks(body).unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].ts_ms, 1_791_792_000_000);
        assert!((f[0].speed_ms.unwrap() - 10.0).abs() < 1e-9);
        assert!(parse_owntracks(br#"{"_type":"transition"}"#).unwrap().is_empty());
    }

    #[test]
    fn gpslogger_params_and_csv() {
        let p = query_params("/log?lat=50.5&lon=4.7&time=2026-10-12T08:00:00Z&spd=3.5&acc=8");
        let f = parse_params(&p).unwrap();
        assert_eq!(f.ts_ms, 1_791_792_000_000);
        assert_eq!(f.speed_ms, Some(3.5));
        let csv = "time,lat,lon,elevation,accuracy,bearing,speed\n2026-10-12T08:00:00.000Z,50.5,4.7,120,5,,1.2\n";
        assert_eq!(parse_csv(csv).len(), 1);
    }

    #[test]
    fn gpx_track() {
        let gpx = r#"<?xml version="1.0"?><gpx version="1.1"><trk><trkseg>
            <trkpt lat="50.5" lon="4.7"><ele>100</ele><time>2026-10-12T08:00:00Z</time></trkpt>
            <trkpt lat="50.6" lon="4.8"><time>2026-10-12T08:01:00Z</time><speed>2.5</speed></trkpt>
            </trkseg></trk></gpx>"#;
        let f = parse_gpx(gpx).unwrap();
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].alt_m, Some(100.0));
        assert_eq!(f[1].speed_ms, Some(2.5));
    }

    #[test]
    fn basic_auth_and_percent_decoding() {
        // "user:s3cret"
        assert_eq!(basic_auth_password("Basic dXNlcjpzM2NyZXQ=").as_deref(), Some("s3cret"));
        assert_eq!(percent_decode("2026-10-12T08%3A00%3A00Z"), "2026-10-12T08:00:00Z");
    }

    #[test]
    fn interpolation() {
        let fix = |ts_ms, lat| Fix { ts_ms, lat, lon: 4.0, acc_m: None, alt_m: None, speed_ms: None };
        let fixes = [fix(0, 50.0), fix(10_000, 51.0)];
        let mid = estimate_at(&fixes, 5_000, 60_000).unwrap();
        assert_eq!((mid.lat, mid.lon), (50.5, 4.0));
        assert_eq!(estimate_at(&fixes, 5_000, 5_000), None);
        let e = estimate_at(&fixes, 5_000, 60_000).unwrap();
        // half-way between fixes 111 km apart: far less certain than either fix
        assert!(e.uncertainty_m > 1000.0, "{e:?}");
        let near = estimate_at(&fixes, 0, 60_000).unwrap();
        assert_eq!(near.uncertainty_m, DEFAULT_ACC_M);
    }
}

#[cfg(test)]
mod endpoint_tests {
    use super::*;

    #[test]
    fn owntracks_and_gpslogger_over_http() {
        let dir = std::env::temp_dir().join(format!("pingtray-loc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("t.sqlite");
        let _ = std::fs::remove_file(&db);
        Store::open(&db).unwrap();
        let addr: SocketAddr = "127.0.0.1:38765".parse().unwrap();
        spawn_server(addr, Some("s3cret".into()), db.clone()).unwrap();
        let base = format!("http://{addr}");

        let denied = ureq::post(&format!("{base}/pub")).send("{}").map(|r| r.status().as_u16());
        assert!(matches!(denied, Err(ureq::Error::StatusCode(401))), "{denied:?}");

        let body = r#"{"_type":"location","tst":1791792000,"lat":50.56,"lon":4.69,"acc":12,"vel":36}"#;
        let r = ureq::post(&format!("{base}/pub"))
            .header("Authorization", "Basic dXNlcjpzM2NyZXQ=")
            .send(body)
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);

        let r = ureq::get(&format!("{base}/log?token=s3cret&lat=50.57&lon=4.70&time=2026-10-12T08%3A01%3A00Z&spd=3"))
            .call()
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);

        let fixes = Store::open(&db).unwrap().fixes(0, i64::MAX).unwrap();
        assert_eq!(fixes.len(), 2);
        assert_eq!(fixes[1].speed_ms, Some(3.0));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
