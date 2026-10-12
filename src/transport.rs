//! Guesses how you were travelling: speed patterns separate still / foot /
//! bike / motorised, then OpenStreetMap railways tell trains from cars.

use std::collections::{HashMap, HashSet};

use crate::location::{Fix, distance_m};
use crate::store::Store;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    Still,
    Foot,
    Bike,
    /// Motorised, railway data unavailable.
    Vehicle,
    Car,
    Train,
    Tram,
    Metro,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Still => "still",
            Mode::Foot => "on foot",
            Mode::Bike => "bike",
            Mode::Vehicle => "vehicle",
            Mode::Car => "car / bus",
            Mode::Train => "train",
            Mode::Tram => "tram",
            Mode::Metro => "metro",
        }
    }

    pub fn rgb(self) -> [u8; 3] {
        match self {
            // map only, where they replace the quality ramp: kept off it
            Mode::Still => [0x9a, 0x99, 0x96],
            Mode::Foot => [0xff, 0xff, 0xff],
            Mode::Bike => [0x33, 0xc7, 0xde],
            Mode::Vehicle => [0xb5, 0xa3, 0xff],
            Mode::Car => [0xdc, 0x8a, 0xdd],
            Mode::Train => [0x35, 0x84, 0xe4],
            Mode::Tram => [0x99, 0xc1, 0xf1],
            Mode::Metro => [0x61, 0x35, 0x83],
        }
    }
}

#[derive(Clone, Debug)]
pub struct Segment {
    pub from_ms: i64,
    pub to_ms: i64,
    pub mode: Mode,
    pub km: f64,
}

impl Segment {
    pub fn kmh(&self) -> f64 {
        let h = (self.to_ms - self.from_ms) as f64 / 3_600_000.0;
        if h > 0.0 { self.km / h } else { 0.0 }
    }
}

/// One straight piece of railway: two (lat, lon) ends and its kind.
type RailSeg = ((f64, f64), (f64, f64), Mode);

/// Railway lines near the track, indexed on a coarse grid.
pub struct Rails {
    segs: Vec<RailSeg>,
    grid: HashMap<(i32, i32), Vec<usize>>,
}

const CELL_DEG: f64 = 0.003; // about 330 m north-south

fn cell(lat: f64, lon: f64) -> (i32, i32) {
    ((lat / CELL_DEG).floor() as i32, (lon / CELL_DEG).floor() as i32)
}

impl Rails {
    pub fn new(ways: &[(Mode, Vec<(f64, f64)>)]) -> Rails {
        let mut segs = Vec::new();
        let mut grid: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
        for (kind, pts) in ways {
            for w in pts.windows(2) {
                let i = segs.len();
                segs.push((w[0], w[1], *kind));
                let (a, b) = (cell(w[0].0, w[0].1), cell(w[1].0, w[1].1));
                for y in a.0.min(b.0)..=a.0.max(b.0) {
                    for x in a.1.min(b.1)..=a.1.max(b.1) {
                        grid.entry((y, x)).or_default().push(i);
                    }
                }
            }
        }
        Rails { segs, grid }
    }

    /// Nearest railway within about one grid cell: distance in metres and kind.
    pub fn nearest(&self, lat: f64, lon: f64) -> Option<(f64, Mode)> {
        let (cy, cx) = cell(lat, lon);
        let mut seen = HashSet::new();
        let mut best: Option<(f64, Mode)> = None;
        for y in cy - 1..=cy + 1 {
            for x in cx - 1..=cx + 1 {
                for &i in self.grid.get(&(y, x)).into_iter().flatten() {
                    if !seen.insert(i) {
                        continue;
                    }
                    let (a, b, kind) = self.segs[i];
                    let d = point_segment_m((lat, lon), a, b);
                    if best.is_none_or(|(bd, _)| d < bd) {
                        best = Some((d, kind));
                    }
                }
            }
        }
        best
    }
}

/// Distance from `p` to segment `a`-`b`, on a local flat projection.
fn point_segment_m(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let k = p.0.to_radians().cos();
    let xy = |q: (f64, f64)| ((q.1 - p.1) * k * 111_320.0, (q.0 - p.0) * 110_540.0);
    let (ax, ay) = xy(a);
    let (bx, by) = xy(b);
    let (dx, dy) = (bx - ax, by - ay);
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 0.0 { (-(ax * dx + ay * dy) / len2).clamp(0.0, 1.0) } else { 0.0 };
    let (cx, cy) = (ax + t * dx, ay + t * dy);
    (cx * cx + cy * cy).sqrt()
}

fn percentile(mut v: Vec<f64>, p: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    if v.is_empty() { 0.0 } else { crate::icon::percentile(&v, p) }
}

const SMOOTH_MS: i64 = 90_000;
const SPEED_BASE_MS: i64 = 60_000;
const MIN_RUN_MS: i64 = 120_000;

/// Splits the track into segments of one transport mode. `rails` is asked
/// for railway data around each motorised stretch.
pub fn classify(
    fixes: &[Fix],
    max_gap_ms: i64,
    mut rails: impl FnMut(&[Fix]) -> Option<Rails>,
) -> Vec<Segment> {
    let fixes: Vec<Fix> = fixes.iter().copied().filter(|f| f.acc_m.is_none_or(|a| a <= 150.0)).collect();
    if fixes.len() < 2 {
        return Vec::new();
    }
    // speed at each fix, m/s: the device's own when it reports one, else the
    // displacement over about a minute around the fix, so that GPS jitter
    // (metres from one fix to the next) does not read as movement
    let speeds: Vec<Option<f64>> = (0..fixes.len())
        .map(|i| {
            let f = &fixes[i];
            if let Some(s) = f.speed_ms {
                return Some(s);
            }
            let lo = fixes.partition_point(|g| g.ts_ms < f.ts_ms - SPEED_BASE_MS / 2);
            let hi = fixes.partition_point(|g| g.ts_ms <= f.ts_ms + SPEED_BASE_MS / 2).saturating_sub(1);
            let (a, b) = (&fixes[lo], &fixes[hi.max(lo)]);
            let dt = (b.ts_ms - a.ts_ms) as f64 / 1000.0;
            (dt >= 15.0).then(|| {
                // path length, but not shorter than the straight line minus
                // the jitter both ends may carry
                let path: f64 = fixes[lo..=hi.max(lo)]
                    .windows(2)
                    .map(|w| distance_m(w[0].lat, w[0].lon, w[1].lat, w[1].lon))
                    .sum();
                let straight = distance_m(a.lat, a.lon, b.lat, b.lon);
                let noise = a.acc_m.unwrap_or(10.0).min(50.0) + b.acc_m.unwrap_or(10.0).min(50.0);
                // noisy wiggles inflate the path: trust it only up to 1.5x the straight line
                path.min(straight * 1.5 + noise).max((straight - noise).max(0.0)) / dt
            })
        })
        .collect();

    // motion class per fix, from speeds in a window around it
    let classes: Vec<Mode> = (0..fixes.len())
        .map(|i| {
            let t = fixes[i].ts_ms;
            let lo = fixes.partition_point(|f| f.ts_ms < t - SMOOTH_MS);
            let hi = fixes.partition_point(|f| f.ts_ms <= t + SMOOTH_MS);
            let window: Vec<f64> = speeds[lo..hi].iter().flatten().copied().collect();
            let (med, p85) = (percentile(window.clone(), 0.5), percentile(window, 0.85));
            if p85 < 1.0 && med < 0.6 {
                Mode::Still
            } else if p85 < 2.5 {
                Mode::Foot
            } else if p85 < 8.0 && med < 7.0 {
                Mode::Bike
            } else {
                Mode::Vehicle
            }
        })
        .collect();

    // runs of one class, cut at gaps in the track
    let mut runs: Vec<(usize, usize, Mode)> = Vec::new(); // [start, end) fix indices
    for i in 0..fixes.len() {
        let gap = i > 0 && fixes[i].ts_ms - fixes[i - 1].ts_ms > max_gap_ms;
        match runs.last_mut() {
            Some(r) if !gap && r.2 == classes[i] => r.1 = i + 1,
            _ => runs.push((i, i + 1, classes[i])),
        }
    }
    let duration = |r: &(usize, usize, Mode)| fixes[r.1 - 1].ts_ms - fixes[r.0].ts_ms;
    let contiguous = |a: &(usize, usize, Mode), b: &(usize, usize, Mode)| {
        a.1 == b.0 && fixes[b.0].ts_ms - fixes[a.1 - 1].ts_ms <= max_gap_ms
    };
    // fold short runs into a neighbour: stops at red lights stay "car"
    loop {
        let candidate = (0..runs.len())
            .filter(|&i| duration(&runs[i]) < MIN_RUN_MS)
            .filter_map(|i| {
                let prev = i.checked_sub(1).filter(|&p| contiguous(&runs[p], &runs[i]));
                let next = (i + 1 < runs.len() && contiguous(&runs[i], &runs[i + 1])).then_some(i + 1);
                let target = match (prev, next) {
                    (Some(p), Some(n)) => {
                        if duration(&runs[p]) >= duration(&runs[n]) { p } else { n }
                    }
                    (Some(p), None) => p,
                    (None, Some(n)) => n,
                    // isolated short stretch: keep it, it is all we know
                    (None, None) => return None,
                };
                Some((i, target))
            })
            .min_by_key(|&(i, _)| duration(&runs[i]));
        let Some((i, target)) = candidate else { break };
        let (lo, hi) = (i.min(target), i.max(target));
        let mode = runs[target].2;
        runs[lo] = (runs[lo].0, runs[hi].1, mode);
        runs.remove(hi);
        // merge with same-mode neighbours created by the fold
        let mut j = 1;
        while j < runs.len() {
            if runs[j - 1].2 == runs[j].2 && contiguous(&runs[j - 1], &runs[j]) {
                runs[j - 1].1 = runs[j].1;
                runs.remove(j);
            } else {
                j += 1;
            }
        }
    }

    // trains and trams follow rails
    let mut segments: Vec<Segment> = runs
        .iter()
        .map(|&(a, b, mut mode)| {
            let track = &fixes[a..b];
            if mode == Mode::Vehicle {
                let top = percentile(speeds[a..b].iter().flatten().copied().collect(), 0.95);
                mode = match rails(track) {
                    Some(r) => rail_mode(&r, track),
                    None if top > 45.0 => Mode::Train, // > 160 km/h: no car does that here
                    None => Mode::Vehicle,
                };
            }
            let km = track.windows(2).map(|w| distance_m(w[0].lat, w[0].lon, w[1].lat, w[1].lon)).sum::<f64>() / 1000.0;
            Segment { from_ms: track[0].ts_ms, to_ms: track[track.len() - 1].ts_ms, mode, km }
        })
        .collect();
    segments.dedup_by(|b, a| {
        let same = a.mode == b.mode && b.from_ms - a.to_ms <= max_gap_ms;
        if same {
            a.to_ms = b.to_ms;
            a.km += b.km;
        }
        same
    });
    segments
}

fn rail_mode(rails: &Rails, track: &[Fix]) -> Mode {
    let mut on_rail = 0;
    let mut kinds: HashMap<Mode, usize> = HashMap::new();
    for f in track {
        let tolerance = f.acc_m.unwrap_or(30.0).clamp(30.0, 100.0);
        if let Some((d, kind)) = rails.nearest(f.lat, f.lon)
            && d <= tolerance {
                on_rail += 1;
                *kinds.entry(kind).or_default() += 1;
            }
    }
    if on_rail * 4 >= track.len() * 3 {
        kinds.into_iter().max_by_key(|(_, n)| *n).map(|(k, _)| k).unwrap_or(Mode::Train)
    } else {
        Mode::Car
    }
}

// ---- OpenStreetMap railways, fetched from Overpass and cached per tile ----

const RAIL_ZOOM: u32 = 12;
const RAIL_MAX_AGE_MS: i64 = 90 * 86_400_000;
/// Public Overpass servers, tried in turn: they are often busy.
const OVERPASS_URLS: [&str; 3] = [
    "https://overpass-api.de/api/interpreter",
    "https://overpass.private.coffee/api/interpreter",
    "https://maps.mail.ru/osm/tools/overpass/api/interpreter",
];

fn tile_of(lat: f64, lon: f64) -> (u32, u32) {
    let n = 2f64.powi(RAIL_ZOOM as i32);
    let x = ((lon + 180.0) / 360.0 * n).floor();
    let r = lat.to_radians();
    let y = ((1.0 - (r.tan() + 1.0 / r.cos()).ln() / std::f64::consts::PI) / 2.0 * n).floor();
    (x.clamp(0.0, n - 1.0) as u32, y.clamp(0.0, n - 1.0) as u32)
}

fn tile_bbox(x: u32, y: u32) -> (f64, f64, f64, f64) {
    let n = 2f64.powi(RAIL_ZOOM as i32);
    let lon = |x: f64| x / n * 360.0 - 180.0;
    let lat = |y: f64| (std::f64::consts::PI * (1.0 - 2.0 * y / n)).sinh().atan().to_degrees();
    (lat(y as f64 + 1.0), lon(x as f64), lat(y as f64), lon(x as f64 + 1.0)) // s, w, n, e
}

fn kind_of(tag: &str) -> Mode {
    match tag {
        "tram" => Mode::Tram,
        "subway" => Mode::Metro,
        _ => Mode::Train,
    }
}

/// Railways of one tile as `[[kind, [[lat, lon], …]], …]`.
fn fetch_tile(x: u32, y: u32) -> anyhow::Result<String> {
    let (s, w, n, e) = tile_bbox(x, y);
    let query = format!(
        "[out:json][timeout:60];way[\"railway\"~\"^(rail|light_rail|subway|tram|narrow_gauge)$\"]({s},{w},{n},{e});out geom;"
    );
    let mut last_err = anyhow::anyhow!("no Overpass server");
    let mut v = None;
    for url in OVERPASS_URLS {
        let attempt = ureq::post(url)
            .header("User-Agent", concat!("pingtray/", env!("CARGO_PKG_VERSION")))
            .send_form([("data", query.as_str())])
            .map_err(anyhow::Error::from)
            .and_then(|mut r| Ok(r.body_mut().read_to_string()?))
            // a busy server answers 200 with an HTML error page
            .and_then(|text| Ok(serde_json::from_str::<serde_json::Value>(&text)?));
        match attempt {
            Ok(json) => {
                v = Some(json);
                break;
            }
            Err(e) => last_err = e.context(url),
        }
    }
    let Some(v) = v else { return Err(last_err) };
    let ways: Vec<serde_json::Value> = v["elements"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|el| {
            let tag = el.pointer("/tags/railway")?.as_str()?;
            let pts: Vec<[f64; 2]> = el["geometry"]
                .as_array()?
                .iter()
                .filter_map(|p| Some([p["lat"].as_f64()?, p["lon"].as_f64()?]))
                .collect();
            Some(serde_json::json!([tag, pts]))
        })
        .collect();
    Ok(serde_json::Value::Array(ways).to_string())
}

fn parse_tile(json: &str) -> Vec<(Mode, Vec<(f64, f64)>)> {
    let Ok(serde_json::Value::Array(ways)) = serde_json::from_str(json) else { return Vec::new() };
    ways.iter()
        .filter_map(|w| {
            let kind = kind_of(w.get(0)?.as_str()?);
            let pts = w.get(1)?.as_array()?.iter().filter_map(|p| Some((p.get(0)?.as_f64()?, p.get(1)?.as_f64()?))).collect();
            Some((kind, pts))
        })
        .collect()
}

/// Railways around a stretch of track, from the cache or Overpass.
/// `None` when some tile could not be obtained (offline, rate-limited).
pub fn rails_for(store: &Store, track: &[Fix]) -> Option<Rails> {
    let tiles: HashSet<(u32, u32)> = track.iter().map(|f| tile_of(f.lat, f.lon)).collect();
    let now = crate::pinger::now_ms();
    let mut ways = Vec::new();
    for (x, y) in tiles {
        let cached = store.rail_tile(RAIL_ZOOM, x, y).ok().flatten();
        let json = match cached {
            Some((fetched, json)) if now - fetched < RAIL_MAX_AGE_MS => json,
            _ => match fetch_tile(x, y) {
                Ok(json) => {
                    let _ = store.set_rail_tile(RAIL_ZOOM, x, y, now, &json);
                    std::thread::sleep(std::time::Duration::from_millis(500)); // be gentle with Overpass
                    json
                }
                Err(e) => {
                    tracing::warn!("railways for tile {x}/{y}: {e}");
                    match cached {
                        Some((_, stale)) => stale,
                        None => return None,
                    }
                }
            },
        };
        ways.extend(parse_tile(&json));
    }
    Some(Rails::new(&ways))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A straight track going east from (50.5, 4.0) at `speed` m/s, one fix per `step` s.
    fn track(t0_ms: i64, secs: i64, step: i64, speed: f64, lon0: f64) -> Vec<Fix> {
        let m_per_deg = 111_320.0 * 50.5f64.to_radians().cos();
        (0..=secs / step)
            .map(|i| Fix {
                ts_ms: t0_ms + i * step * 1000,
                lat: 50.5,
                lon: lon0 + speed * (i * step) as f64 / m_per_deg,
                acc_m: Some(10.0),
                alt_m: None,
                speed_ms: None,
            })
            .collect()
    }

    #[test]
    fn jittery_walk_is_on_foot() {
        // 8 m of GPS noise every 5 s used to read as cycling
        let mut fixes = track(0, 900, 5, 1.3, 4.0);
        for (i, f) in fixes.iter_mut().enumerate() {
            let n = ((i * 7919) % 17) as f64 / 17.0 - 0.5;
            f.lat += n * 16.0 / 110_540.0;
            f.lon -= n * 12.0 / 71_000.0;
        }
        let segs = classify(&fixes, 600_000, |_| Some(Rails::new(&[])));
        assert_eq!(segs.iter().map(|s| s.mode).collect::<Vec<_>>(), vec![Mode::Foot]);
    }

    #[test]
    fn walk_then_drive() {
        let mut fixes = track(0, 600, 5, 1.4, 4.0);
        let last = fixes.last().unwrap().lon;
        fixes.extend(track(605_000, 900, 5, 25.0, last).into_iter());
        let segs = classify(&fixes, 600_000, |_| Some(Rails::new(&[])));
        let modes: Vec<Mode> = segs.iter().map(|s| s.mode).collect();
        assert_eq!(modes, vec![Mode::Foot, Mode::Car]);
        assert!((segs[1].kmh() - 90.0).abs() < 10.0, "{}", segs[1].kmh());
    }

    #[test]
    fn along_rails_is_a_train() {
        let fixes = track(0, 900, 5, 30.0, 4.0);
        let line = vec![(Mode::Train, vec![(50.5002, 3.9), (50.5002, 4.6)])];
        let segs = classify(&fixes, 600_000, |_| Some(Rails::new(&line)));
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].mode, Mode::Train);
    }

    #[test]
    fn tile_math_roundtrip() {
        let (x, y) = tile_of(50.56, 4.69);
        let (s, w, n, e) = tile_bbox(x, y);
        assert!(s <= 50.56 && 50.56 <= n && w <= 4.69 && 4.69 <= e);
    }
}
